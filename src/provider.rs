use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use console::style;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::config::{
    Config, EndpointConfig, MirrorConfig, NamespaceKind, ProviderKind, RepoNameFilter, SiteConfig,
    Visibility,
};

#[derive(Clone, Debug)]
pub struct RemoteRepo {
    pub name: String,
    pub clone_url: String,
    pub private: bool,
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EndpointRepo {
    pub endpoint: EndpointConfig,
    pub repo: RemoteRepo,
}

#[derive(Clone, Debug)]
pub struct PullRequestRequest {
    pub title: String,
    pub body: String,
    pub head_branch: String,
    pub base_branch: String,
}

#[derive(Clone, Debug)]
pub struct PullRequestInfo {
    pub url: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct GitlabProtectedBranch {
    allow_force_push: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookInstallOutcome {
    Created,
    Existing,
}

pub fn list_mirror_repos(
    config: &Config,
    mirror: &MirrorConfig,
    repo_filter: &RepoNameFilter,
    jobs: usize,
) -> Result<Vec<EndpointRepo>> {
    let endpoint_jobs = mirror
        .endpoints
        .iter()
        .cloned()
        .enumerate()
        .collect::<Vec<_>>();
    let worker_count = jobs.min(endpoint_jobs.len());
    if worker_count > 1 {
        crate::logln!(
            "  {} listing repositories with {} workers",
            style("jobs").cyan().bold(),
            worker_count
        );
    }

    let mut listed = crate::parallel::map(endpoint_jobs, jobs, |(index, endpoint)| {
        let site = config.site(&endpoint.site).unwrap();
        let client = ProviderClient::new(site)?;
        crate::logln!(
            "  {} {}",
            style("list").cyan().bold(),
            style(endpoint.label()).dim()
        );
        let repos = client
            .list_repos(&endpoint)
            .with_context(|| format!("failed to list repos for {}", endpoint.label()))?;
        let repos = repos
            .into_iter()
            .filter(|repo| mirror.sync_visibility.matches_private(repo.private))
            .filter(|repo| repo_filter.matches(&repo.name))
            .map(|repo| EndpointRepo {
                endpoint: endpoint.clone(),
                repo,
            })
            .collect::<Vec<_>>();
        Ok((index, repos))
    })?;
    listed.sort_by_key(|(index, _)| *index);

    Ok(listed.into_iter().flat_map(|(_, repos)| repos).collect())
}

pub struct ProviderClient<'a> {
    site: &'a SiteConfig,
    token: String,
    http: Client,
}

macro_rules! dispatch_provider {
    ($provider:expr, github => $github:expr, gitlab => $gitlab:expr, gitea_like => $gitea_like:expr $(,)?) => {
        match $provider {
            ProviderKind::Github => $github,
            ProviderKind::Gitlab => $gitlab,
            ProviderKind::Gitea | ProviderKind::Forgejo => $gitea_like,
        }
    };
}

macro_rules! owned_repos {
    ($client:expr, $repo:ty, $url:expr, $namespace:expr) => {{
        Ok($client
            .paged_get::<$repo>($url)?
            .into_iter()
            .filter(|repo: &$repo| repo.owner.login.eq_ignore_ascii_case($namespace))
            .map(Into::into)
            .collect())
    }};
}

macro_rules! json_method {
    ($name:ident, $method:literal, $request:ident) => {
        fn $name<T>(&self, url: &str, body: &serde_json::Value) -> Result<T>
        where
            T: for<'de> Deserialize<'de>,
        {
            self.send_json($method, self.http.$request(url), url, body)
        }
    };
}

impl<'a> ProviderClient<'a> {
    pub fn new(site: &'a SiteConfig) -> Result<Self> {
        let token = site.token()?;
        Ok(Self {
            site,
            token,
            http: Client::builder().build()?,
        })
    }

    pub fn list_repos(&self, endpoint: &EndpointConfig) -> Result<Vec<RemoteRepo>> {
        dispatch_provider!(self.site.provider,
            github => self.github_list_repos(endpoint),
            gitlab => self.gitlab_list_repos(endpoint),
            gitea_like => self.gitea_list_repos(endpoint),
        )
    }

    pub fn create_repo(
        &self,
        endpoint: &EndpointConfig,
        name: &str,
        visibility: &Visibility,
        description: Option<&str>,
    ) -> Result<RemoteRepo> {
        dispatch_provider!(self.site.provider,
            github => self.github_create_repo(endpoint, name, visibility, description),
            gitlab => self.gitlab_create_repo(endpoint, name, visibility, description),
            gitea_like => self.gitea_create_repo(endpoint, name, visibility, description),
        )
    }

    pub fn delete_repo(&self, endpoint: &EndpointConfig, repo_name: &str) -> Result<()> {
        dispatch_provider!(self.site.provider,
            github => self.github_delete_repo(endpoint, repo_name),
            gitlab => self.gitlab_delete_repo(endpoint, repo_name),
            gitea_like => self.gitea_delete_repo(endpoint, repo_name),
        )
    }

    pub fn set_default_branch(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        branch: &str,
    ) -> Result<()> {
        dispatch_provider!(self.site.provider,
            github => self.github_set_default_branch(endpoint, repo_name, branch),
            gitlab => self.gitlab_set_default_branch(endpoint, repo_name, branch),
            gitea_like => self.gitea_set_default_branch(endpoint, repo_name, branch),
        )
    }

    pub fn gitlab_protected_branch_allow_force_push(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        branch: &str,
    ) -> Result<Option<bool>> {
        if self.site.provider != ProviderKind::Gitlab {
            bail!("protected branch force-push lookup is only supported for GitLab");
        }
        let url = self.gitlab_protected_branch_url(endpoint, repo_name, branch);
        match self.get_json::<GitlabProtectedBranch>(&url) {
            Ok(protected_branch) => Ok(Some(protected_branch.allow_force_push)),
            Err(error) if is_not_found_error(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn set_gitlab_protected_branch_allow_force_push(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        branch: &str,
        allow_force_push: bool,
    ) -> Result<()> {
        if self.site.provider != ProviderKind::Gitlab {
            bail!("protected branch force-push update is only supported for GitLab");
        }
        let url = self.gitlab_protected_branch_url(endpoint, repo_name, branch);
        self.patch_json::<serde_json::Value>(&url, &json!({ "allow_force_push": allow_force_push }))
            .map(|_| ())
    }

    pub fn install_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        url: &str,
        secret: &str,
    ) -> Result<WebhookInstallOutcome> {
        dispatch_provider!(self.site.provider,
            github => self.github_install_webhook(endpoint, repo, url, secret),
            gitlab => self.gitlab_install_webhook(endpoint, repo, url, secret),
            gitea_like => self.gitea_install_webhook(endpoint, repo, url, secret),
        )
    }

    pub fn uninstall_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        url: &str,
    ) -> Result<bool> {
        dispatch_provider!(self.site.provider,
            github => self.github_uninstall_webhook(endpoint, repo_name, url),
            gitlab => self.gitlab_uninstall_webhook(endpoint, repo_name, url),
            gitea_like => self.gitea_uninstall_webhook(endpoint, repo_name, url),
        )
    }

    pub fn open_pull_request(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        request: &PullRequestRequest,
    ) -> Result<PullRequestInfo> {
        dispatch_provider!(self.site.provider,
            github => self.github_open_pull_request(endpoint, repo, request),
            gitlab => self.gitlab_open_pull_request(endpoint, repo, request),
            gitea_like => self.gitea_open_pull_request(endpoint, repo, request),
        )
    }

    pub fn close_pull_requests_by_head_prefix(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        base_branch: &str,
        head_prefix: &str,
    ) -> Result<usize> {
        dispatch_provider!(self.site.provider,
            github => self.github_close_pull_requests_by_head_prefix(endpoint, repo, base_branch, head_prefix),
            gitlab => self.gitlab_close_pull_requests_by_head_prefix(endpoint, repo, base_branch, head_prefix),
            gitea_like => self.gitea_close_pull_requests_by_head_prefix(endpoint, repo, base_branch, head_prefix),
        )
    }

    pub fn validate_token(&self) -> Result<()> {
        let url = format!("{}/user", self.site.api_base());
        self.get(&url).map(|_| ())
    }

    pub fn detect_namespace_kind(&self, namespace: &str) -> Result<Option<NamespaceKind>> {
        dispatch_provider!(self.site.provider,
            github => self.github_detect_namespace_kind(namespace),
            gitlab => self.gitlab_detect_namespace_kind(namespace),
            gitea_like => self.gitea_detect_namespace_kind(namespace),
        )
    }

    pub fn authenticated_clone_url(&self, clone_url: &str) -> Result<String> {
        let mut url = Url::parse(clone_url)
            .or_else(|_| Url::parse(&format!("{}/{}", self.site.base_url, clone_url)))
            .with_context(|| format!("failed to parse clone URL '{clone_url}'"))?;
        if url.scheme() != "https" && url.scheme() != "http" {
            bail!("only HTTP(S) clone URLs are supported, got '{clone_url}'");
        }

        let username = self
            .site
            .git_username
            .clone()
            .unwrap_or_else(|| default_git_username(&self.site.provider).to_string());
        url.set_username(&username)
            .map_err(|_| anyhow!("failed to set username on clone URL"))?;
        url.set_password(Some(&self.token))
            .map_err(|_| anyhow!("failed to set token on clone URL"))?;
        Ok(url.to_string())
    }

    fn github_list_repos(&self, endpoint: &EndpointConfig) -> Result<Vec<RemoteRepo>> {
        match endpoint.kind {
            NamespaceKind::User => {
                let url = format!(
                    "{}/user/repos?affiliation=owner&visibility=all&per_page=100",
                    self.site.api_base()
                );
                owned_repos!(self, GithubRepo, &url, &endpoint.namespace)
            }
            NamespaceKind::Org => {
                let url = format!(
                    "{}/orgs/{}/repos?type=all&per_page=100",
                    self.site.api_base(),
                    endpoint.namespace
                );
                self.paged_remote_repos::<GithubRepo>(&url)
            }
            NamespaceKind::Group => bail!("GitHub endpoints use kind 'user' or 'org'"),
        }
    }

    fn github_create_repo(
        &self,
        endpoint: &EndpointConfig,
        name: &str,
        visibility: &Visibility,
        description: Option<&str>,
    ) -> Result<RemoteRepo> {
        let url = match endpoint.kind {
            NamespaceKind::User => format!("{}/user/repos", self.site.api_base()),
            NamespaceKind::Org => {
                format!("{}/orgs/{}/repos", self.site.api_base(), endpoint.namespace)
            }
            NamespaceKind::Group => bail!("GitHub endpoints use kind 'user' or 'org'"),
        };
        let body = json!({
            "name": name,
            "private": matches!(visibility, Visibility::Private),
            "description": description.unwrap_or(""),
        });
        self.post_json::<GithubRepo>(&url, &body).map(Into::into)
    }

    fn github_detect_namespace_kind(&self, namespace: &str) -> Result<Option<NamespaceKind>> {
        let url = format!("{}/users/{namespace}", self.site.api_base());
        let value: serde_json::Value = self.get_json(&url)?;
        Ok(match value.get("type").and_then(|value| value.as_str()) {
            Some("Organization") => Some(NamespaceKind::Org),
            Some("User") => Some(NamespaceKind::User),
            _ => None,
        })
    }

    fn github_delete_repo(&self, endpoint: &EndpointConfig, repo_name: &str) -> Result<()> {
        let url = self.repo_url(endpoint, repo_name, "GitHub")?;
        self.delete(&url).map(|_| ())
    }

    fn github_set_default_branch(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        branch: &str,
    ) -> Result<()> {
        let url = self.repo_url(endpoint, repo_name, "GitHub")?;
        self.patch_json::<serde_json::Value>(&url, &json!({ "default_branch": branch }))
            .map(|_| ())
    }

    fn github_install_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        url: &str,
        secret: &str,
    ) -> Result<WebhookInstallOutcome> {
        let hooks_url = self.repo_hooks_url(endpoint, &repo.name, "GitHub")?;
        let body = json!({
            "name": "web",
            "active": true,
            "events": ["push"],
            "config": {
                "url": url,
                "content_type": "json",
                "secret": secret,
                "insecure_ssl": "0",
            },
        });
        self.upsert_hook(&hooks_url, url, &body, false)
    }

    fn github_uninstall_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        url: &str,
    ) -> Result<bool> {
        let hooks_url = self.repo_hooks_url(endpoint, repo_name, "GitHub")?;
        self.delete_matching_hook(&hooks_url, url)
    }

    fn github_open_pull_request(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        request: &PullRequestRequest,
    ) -> Result<PullRequestInfo> {
        let url = self.repo_pulls_url(endpoint, &repo.name, "GitHub")?;
        if let Some(existing) =
            self.github_find_open_pull_request(&url, &request.head_branch, &request.base_branch)?
        {
            return Ok(PullRequestInfo {
                url: existing.url(),
            });
        }
        let body = json!({
            "title": request.title,
            "body": request.body,
            "head": request.head_branch,
            "base": request.base_branch,
        });
        let created: ProviderPullRequest = self.post_json(&url, &body)?;
        Ok(PullRequestInfo { url: created.url() })
    }

    fn github_find_open_pull_request(
        &self,
        pulls_url: &str,
        head_branch: &str,
        base_branch: &str,
    ) -> Result<Option<ProviderPullRequest>> {
        let url = format!(
            "{pulls_url}?state=open&base={}&per_page=100",
            urlencoding(base_branch)
        );
        let pulls: Vec<ProviderPullRequest> = self.paged_get(&url)?;
        Ok(pulls
            .into_iter()
            .find(|pull| pull.head_ref() == Some(head_branch)))
    }

    fn github_close_pull_requests_by_head_prefix(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        base_branch: &str,
        head_prefix: &str,
    ) -> Result<usize> {
        let pulls_url = self.repo_pulls_url(endpoint, &repo.name, "GitHub")?;
        let url = format!(
            "{pulls_url}?state=open&base={}&per_page=100",
            urlencoding(base_branch)
        );
        let pulls: Vec<ProviderPullRequest> = match self.paged_get(&url) {
            Ok(pulls) => pulls,
            Err(error) if is_not_found_error(&error) => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut closed = 0;
        for pull in pulls.into_iter().filter(|pull| {
            pull.head_ref()
                .is_some_and(|head| head.starts_with(head_prefix))
        }) {
            let Some(number) = pull.number else {
                continue;
            };
            let update_url = format!("{pulls_url}/{number}");
            self.patch_json::<serde_json::Value>(&update_url, &json!({ "state": "closed" }))?;
            closed += 1;
        }
        Ok(closed)
    }

    fn gitlab_list_repos(&self, endpoint: &EndpointConfig) -> Result<Vec<RemoteRepo>> {
        match endpoint.kind {
            NamespaceKind::User => {
                let url = format!(
                    "{}/users/{}/projects?simple=true&per_page=100&owned=true",
                    self.site.api_base(),
                    endpoint.namespace
                );
                let mut projects = self.paged_get::<GitlabProject>(&url)?;
                let owned_url = format!(
                    "{}/projects?owned=true&simple=true&per_page=100",
                    self.site.api_base()
                );
                for project in self.paged_get::<GitlabProject>(&owned_url)? {
                    if project.is_in_namespace(&endpoint.namespace)
                        && !projects.iter().any(|existing| existing.same_path(&project))
                    {
                        projects.push(project);
                    }
                }
                Ok(projects
                    .into_iter()
                    .filter(|project| !project.is_deletion_scheduled())
                    .map(Into::into)
                    .collect())
            }
            NamespaceKind::Org | NamespaceKind::Group => {
                let encoded = urlencoding(&endpoint.namespace);
                let url = format!(
                    "{}/groups/{}/projects?simple=true&include_subgroups=false&per_page=100",
                    self.site.api_base(),
                    encoded
                );
                Ok(self
                    .paged_get::<GitlabProject>(&url)?
                    .into_iter()
                    .filter(|project| !project.is_deletion_scheduled())
                    .map(Into::into)
                    .collect())
            }
        }
    }

    fn gitlab_create_repo(
        &self,
        endpoint: &EndpointConfig,
        name: &str,
        visibility: &Visibility,
        description: Option<&str>,
    ) -> Result<RemoteRepo> {
        let mut body = serde_json::Map::from_iter([
            ("name".to_string(), json!(name)),
            ("path".to_string(), json!(name)),
            (
                "visibility".to_string(),
                json!(match visibility {
                    Visibility::Private => "private",
                    Visibility::Public => "public",
                }),
            ),
            ("description".to_string(), json!(description.unwrap_or(""))),
        ]);

        if matches!(endpoint.kind, NamespaceKind::Org | NamespaceKind::Group) {
            let group = self.gitlab_group(&endpoint.namespace)?;
            body.insert("namespace_id".to_string(), json!(group.id));
        }

        let url = format!("{}/projects", self.site.api_base());
        match self.post_json::<GitlabProject>(&url, &serde_json::Value::Object(body)) {
            Ok(project) => Ok(project.into()),
            Err(error) if is_already_taken_error(&error) => {
                let existing_url = self.gitlab_project_url(endpoint, name);
                self.get_json::<GitlabProject>(&existing_url)
                    .map(Into::into)
                    .with_context(|| {
                        format!(
                            "GitLab reported that {name} already exists, but failed to fetch {existing_url}: {error:#}"
                        )
                    })
            }
            Err(error) => Err(error),
        }
    }

    fn gitlab_delete_repo(&self, endpoint: &EndpointConfig, repo_name: &str) -> Result<()> {
        let url = self.gitlab_project_url(endpoint, repo_name);
        self.delete(&url).map(|_| ())
    }

    fn gitlab_set_default_branch(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        branch: &str,
    ) -> Result<()> {
        let url = self.gitlab_project_url(endpoint, repo_name);
        self.put_json::<serde_json::Value>(&url, &json!({ "default_branch": branch }))
            .map(|_| ())
    }

    fn gitlab_group(&self, namespace: &str) -> Result<GitlabGroup> {
        let url = format!("{}/groups/{}", self.site.api_base(), urlencoding(namespace));
        self.get_json(&url)
    }

    fn gitlab_detect_namespace_kind(&self, namespace: &str) -> Result<Option<NamespaceKind>> {
        let group_url = format!("{}/groups/{}", self.site.api_base(), urlencoding(namespace));
        if self.get(&group_url).is_ok() {
            return Ok(Some(NamespaceKind::Group));
        }

        let username = namespace.rsplit('/').next().unwrap_or(namespace);
        let user_url = format!(
            "{}/users?username={}",
            self.site.api_base(),
            urlencoding(username)
        );
        let users: serde_json::Value = self.get_json(&user_url)?;
        Ok(users
            .as_array()
            .is_some_and(|items| !items.is_empty())
            .then_some(NamespaceKind::User))
    }

    fn gitlab_install_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        url: &str,
        secret: &str,
    ) -> Result<WebhookInstallOutcome> {
        let hooks_url = self.gitlab_hooks_url(endpoint, &repo.name);
        let body = json!({
            "url": url,
            "push_events": true,
            "tag_push_events": true,
            "token": secret,
            "enable_ssl_verification": true,
        });
        self.upsert_hook(&hooks_url, url, &body, true)
    }

    fn gitlab_uninstall_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        url: &str,
    ) -> Result<bool> {
        let hooks_url = self.gitlab_hooks_url(endpoint, repo_name);
        self.delete_matching_hook(&hooks_url, url)
    }

    fn gitlab_open_pull_request(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        request: &PullRequestRequest,
    ) -> Result<PullRequestInfo> {
        let url = self.gitlab_merge_requests_url(endpoint, &repo.name);
        if let Some(existing) =
            self.gitlab_find_open_merge_request(&url, &request.head_branch, &request.base_branch)?
        {
            return Ok(PullRequestInfo {
                url: existing.url(),
            });
        }
        let body = json!({
            "title": request.title,
            "description": request.body,
            "source_branch": request.head_branch,
            "target_branch": request.base_branch,
        });
        let created: ProviderPullRequest = self.post_json(&url, &body)?;
        Ok(PullRequestInfo { url: created.url() })
    }

    fn gitlab_find_open_merge_request(
        &self,
        merge_requests_url: &str,
        source_branch: &str,
        target_branch: &str,
    ) -> Result<Option<ProviderPullRequest>> {
        let url = format!(
            "{merge_requests_url}?state=opened&source_branch={}&target_branch={}&per_page=100",
            urlencoding(source_branch),
            urlencoding(target_branch)
        );
        let pulls: Vec<ProviderPullRequest> = self.paged_get(&url)?;
        Ok(pulls
            .into_iter()
            .find(|pull| pull.head_ref() == Some(source_branch)))
    }

    fn gitlab_close_pull_requests_by_head_prefix(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        base_branch: &str,
        head_prefix: &str,
    ) -> Result<usize> {
        let merge_requests_url = self.gitlab_merge_requests_url(endpoint, &repo.name);
        let url = format!(
            "{merge_requests_url}?state=opened&target_branch={}&per_page=100",
            urlencoding(base_branch)
        );
        let pulls: Vec<ProviderPullRequest> = self.paged_get(&url)?;
        let mut closed = 0;
        for pull in pulls.into_iter().filter(|pull| {
            pull.head_ref()
                .is_some_and(|head| head.starts_with(head_prefix))
        }) {
            let Some(number) = pull.iid.or(pull.number) else {
                continue;
            };
            let update_url = format!("{merge_requests_url}/{number}");
            self.put_json::<serde_json::Value>(&update_url, &json!({ "state_event": "close" }))?;
            closed += 1;
        }
        Ok(closed)
    }

    fn gitea_list_repos(&self, endpoint: &EndpointConfig) -> Result<Vec<RemoteRepo>> {
        match endpoint.kind {
            NamespaceKind::User => {
                let url = format!("{}/user/repos", self.site.api_base());
                Ok(self
                    .gitea_paged_get::<GiteaRepo>(&url)?
                    .into_iter()
                    .filter(|repo| repo.owner.login.eq_ignore_ascii_case(&endpoint.namespace))
                    .map(Into::into)
                    .collect())
            }
            NamespaceKind::Org => {
                let url = format!("{}/orgs/{}/repos", self.site.api_base(), endpoint.namespace);
                Ok(self
                    .gitea_paged_get::<GiteaRepo>(&url)?
                    .into_iter()
                    .map(Into::into)
                    .collect())
            }
            NamespaceKind::Group => bail!("Gitea/Forgejo endpoints use kind 'user' or 'org'"),
        }
    }

    fn gitea_create_repo(
        &self,
        endpoint: &EndpointConfig,
        name: &str,
        visibility: &Visibility,
        description: Option<&str>,
    ) -> Result<RemoteRepo> {
        let url = match endpoint.kind {
            NamespaceKind::User => format!("{}/user/repos", self.site.api_base()),
            NamespaceKind::Org => {
                format!("{}/orgs/{}/repos", self.site.api_base(), endpoint.namespace)
            }
            NamespaceKind::Group => bail!("Gitea/Forgejo endpoints use kind 'user' or 'org'"),
        };
        let body = json!({
            "name": name,
            "private": matches!(visibility, Visibility::Private),
            "description": description.unwrap_or(""),
            "auto_init": false,
        });
        match self.post_json::<GiteaRepo>(&url, &body) {
            Ok(repo) => Ok(repo.into()),
            Err(error) if is_conflict_error(&error) => {
                let repo_url = self.repo_url(endpoint, name, "Gitea/Forgejo")?;
                self.get_json::<GiteaRepo>(&repo_url).map(Into::into)
            }
            Err(error) => Err(error),
        }
    }

    fn gitea_detect_namespace_kind(&self, namespace: &str) -> Result<Option<NamespaceKind>> {
        let org_url = format!("{}/orgs/{namespace}", self.site.api_base());
        if self.get(&org_url).is_ok() {
            return Ok(Some(NamespaceKind::Org));
        }

        let user_url = format!("{}/users/{namespace}", self.site.api_base());
        if self.get(&user_url).is_ok() {
            return Ok(Some(NamespaceKind::User));
        }

        Ok(None)
    }

    fn gitea_delete_repo(&self, endpoint: &EndpointConfig, repo_name: &str) -> Result<()> {
        let url = self.repo_url(endpoint, repo_name, "Gitea/Forgejo")?;
        self.delete(&url).map(|_| ())
    }

    fn gitea_set_default_branch(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        branch: &str,
    ) -> Result<()> {
        let url = self.repo_url(endpoint, repo_name, "Gitea/Forgejo")?;
        self.patch_json::<serde_json::Value>(&url, &json!({ "default_branch": branch }))
            .map(|_| ())
    }

    fn gitea_install_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        url: &str,
        secret: &str,
    ) -> Result<WebhookInstallOutcome> {
        let hooks_url = self.repo_hooks_url(endpoint, &repo.name, "Gitea/Forgejo")?;
        let body = json!({
            "type": "gitea",
            "active": true,
            "events": ["push"],
            "config": {
                "url": url,
                "content_type": "json",
                "secret": secret,
            },
        });
        self.upsert_hook(&hooks_url, url, &body, false)
    }

    fn gitea_uninstall_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        url: &str,
    ) -> Result<bool> {
        let hooks_url = self.repo_hooks_url(endpoint, repo_name, "Gitea/Forgejo")?;
        self.delete_matching_hook(&hooks_url, url)
    }

    fn gitea_open_pull_request(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        request: &PullRequestRequest,
    ) -> Result<PullRequestInfo> {
        let url = self.repo_pulls_url(endpoint, &repo.name, "Gitea/Forgejo")?;
        if let Some(existing) =
            self.gitea_find_open_pull_request(&url, &request.head_branch, &request.base_branch)?
        {
            return Ok(PullRequestInfo {
                url: existing.url(),
            });
        }
        let body = json!({
            "title": request.title,
            "body": request.body,
            "head": request.head_branch,
            "base": request.base_branch,
        });
        let created: ProviderPullRequest = self.post_json(&url, &body)?;
        Ok(PullRequestInfo { url: created.url() })
    }

    fn gitea_find_open_pull_request(
        &self,
        pulls_url: &str,
        head_branch: &str,
        base_branch: &str,
    ) -> Result<Option<ProviderPullRequest>> {
        let url = format!(
            "{pulls_url}?state=open&base={}&head={}&limit=50",
            urlencoding(base_branch),
            urlencoding(head_branch)
        );
        let pulls: Vec<ProviderPullRequest> = self.paged_get(&url)?;
        Ok(pulls
            .into_iter()
            .find(|pull| pull.head_ref() == Some(head_branch)))
    }

    fn gitea_close_pull_requests_by_head_prefix(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        base_branch: &str,
        head_prefix: &str,
    ) -> Result<usize> {
        let pulls_url = self.repo_pulls_url(endpoint, &repo.name, "Gitea/Forgejo")?;
        let url = format!(
            "{pulls_url}?state=open&base={}&limit=50",
            urlencoding(base_branch)
        );
        let pulls: Vec<ProviderPullRequest> = match self.paged_get(&url) {
            Ok(pulls) => pulls,
            Err(error) if is_not_found_error(&error) => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut closed = 0;
        for pull in pulls.into_iter().filter(|pull| {
            pull.head_ref()
                .is_some_and(|head| head.starts_with(head_prefix))
        }) {
            let Some(number) = pull.number.or(pull.index) else {
                continue;
            };
            let update_url = format!("{pulls_url}/{number}");
            self.patch_json::<serde_json::Value>(&update_url, &json!({ "state": "closed" }))?;
            closed += 1;
        }
        Ok(closed)
    }

    fn repo_url(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        provider: &str,
    ) -> Result<String> {
        if matches!(endpoint.kind, NamespaceKind::Group) {
            bail!("{provider} endpoints use kind 'user' or 'org'");
        }
        Ok(format!(
            "{}/repos/{}/{repo_name}",
            self.site.api_base(),
            endpoint.namespace
        ))
    }

    fn repo_hooks_url(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        provider: &str,
    ) -> Result<String> {
        if matches!(endpoint.kind, NamespaceKind::Group) {
            bail!("{provider} endpoints use kind 'user' or 'org'");
        }
        Ok(format!(
            "{}/repos/{}/{repo_name}/hooks",
            self.site.api_base(),
            endpoint.namespace
        ))
    }

    fn repo_pulls_url(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        provider: &str,
    ) -> Result<String> {
        if matches!(endpoint.kind, NamespaceKind::Group) {
            bail!("{provider} endpoints use kind 'user' or 'org'");
        }
        Ok(format!(
            "{}/repos/{}/{repo_name}/pulls",
            self.site.api_base(),
            endpoint.namespace
        ))
    }

    fn gitlab_hooks_url(&self, endpoint: &EndpointConfig, repo_name: &str) -> String {
        format!("{}/hooks", self.gitlab_project_url(endpoint, repo_name))
    }

    fn gitlab_merge_requests_url(&self, endpoint: &EndpointConfig, repo_name: &str) -> String {
        format!(
            "{}/merge_requests",
            self.gitlab_project_url(endpoint, repo_name)
        )
    }

    fn gitlab_protected_branch_url(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        branch: &str,
    ) -> String {
        format!(
            "{}/protected_branches/{}",
            self.gitlab_project_url(endpoint, repo_name),
            urlencoding(branch)
        )
    }

    fn gitlab_project_url(&self, endpoint: &EndpointConfig, repo_name: &str) -> String {
        let project = format!("{}/{repo_name}", endpoint.namespace);
        format!(
            "{}/projects/{}",
            self.site.api_base(),
            urlencoding(&project)
        )
    }

    fn find_existing_hook(&self, hooks_url: &str, target_url: &str) -> Result<Option<RepoHook>> {
        let hooks: Vec<RepoHook> = self.paged_get(hooks_url)?;
        Ok(hooks.into_iter().find(|hook| {
            hook.url()
                .is_some_and(|hook_url| webhook_urls_match(hook_url, target_url))
        }))
    }

    fn upsert_hook(
        &self,
        hooks_url: &str,
        target_url: &str,
        body: &serde_json::Value,
        put_on_update: bool,
    ) -> Result<WebhookInstallOutcome> {
        let Some(hook) = self.find_existing_hook(hooks_url, target_url)? else {
            self.post_json::<serde_json::Value>(hooks_url, body)?;
            return Ok(WebhookInstallOutcome::Created);
        };

        let update_url = format!("{hooks_url}/{}", hook.id);
        if put_on_update {
            self.put_json::<serde_json::Value>(&update_url, body)?;
        } else {
            self.patch_json::<serde_json::Value>(&update_url, body)?;
        }
        Ok(WebhookInstallOutcome::Existing)
    }

    fn delete_matching_hook(&self, hooks_url: &str, target_url: &str) -> Result<bool> {
        let Some(hook) = self.find_existing_hook(hooks_url, target_url)? else {
            return Ok(false);
        };
        let delete_url = format!("{hooks_url}/{}", hook.id);
        self.delete(&delete_url)?;
        Ok(true)
    }

    fn paged_get<T>(&self, first_url: &str) -> Result<Vec<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut output = Vec::new();
        let mut next_url = Some(first_url.to_string());

        while let Some(url) = next_url.take() {
            let response = self.get(&url)?;
            next_url = next_link(response.headers());
            let mut page: Vec<T> = response
                .json()
                .with_context(|| format!("invalid JSON from {url}"))?;
            output.append(&mut page);
        }

        Ok(output)
    }

    fn paged_remote_repos<T>(&self, url: &str) -> Result<Vec<RemoteRepo>>
    where
        T: for<'de> Deserialize<'de> + Into<RemoteRepo>,
    {
        Ok(self
            .paged_get::<T>(url)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    fn gitea_paged_get<T>(&self, base_url: &str) -> Result<Vec<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        const LIMIT: usize = 50;
        let mut output = Vec::new();
        for page in 1.. {
            let separator = if base_url.contains('?') { '&' } else { '?' };
            let url = format!("{base_url}{separator}page={page}&limit={LIMIT}");
            let mut items: Vec<T> = self
                .get(&url)?
                .json()
                .with_context(|| format!("invalid JSON from {url}"))?;
            let count = items.len();
            output.append(&mut items);
            if count < LIMIT {
                break;
            }
        }
        Ok(output)
    }

    fn get_json<T>(&self, url: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.get(url)?
            .json()
            .with_context(|| format!("invalid JSON from {url}"))
    }

    json_method!(post_json, "POST", post);
    json_method!(put_json, "PUT", put);
    json_method!(patch_json, "PATCH", patch);

    fn send_json<T>(
        &self,
        method: &str,
        request: reqwest::blocking::RequestBuilder,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.request_headers(request)?
            .json(body)
            .send()
            .with_context(|| format!("{method} {url} failed"))
            .and_then(|response| check_response(method, url, response))?
            .json()
            .with_context(|| format!("invalid JSON from {url}"))
    }

    fn get(&self, url: &str) -> Result<Response> {
        self.send("GET", self.http.get(url), url)
    }

    fn delete(&self, url: &str) -> Result<Response> {
        self.send("DELETE", self.http.delete(url), url)
    }

    fn send(
        &self,
        method: &str,
        request: reqwest::blocking::RequestBuilder,
        url: &str,
    ) -> Result<Response> {
        self.request_headers(request)?
            .send()
            .with_context(|| format!("{method} {url} failed"))
            .and_then(|response| check_response(method, url, response))
    }

    fn request_headers(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("refray/0.1"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        match self.site.provider {
            ProviderKind::Github => {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {}", self.token))
                        .context("PAT contains invalid header characters")?,
                );
                headers.insert(
                    "X-GitHub-Api-Version",
                    HeaderValue::from_static("2022-11-28"),
                );
            }
            ProviderKind::Gitlab => {
                headers.insert(
                    "PRIVATE-TOKEN",
                    HeaderValue::from_str(&self.token)
                        .context("PAT contains invalid header characters")?,
                );
            }
            ProviderKind::Gitea | ProviderKind::Forgejo => {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("token {}", self.token))
                        .context("PAT contains invalid header characters")?,
                );
            }
        }
        Ok(request.headers(headers))
    }
}

fn check_response(method: &str, url: &str, response: Response) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().unwrap_or_default();
    bail!("{method} {url} returned {status}: {body}");
}

fn is_not_found_error(error: &anyhow::Error) -> bool {
    error.to_string().contains("404 Not Found")
}

fn is_conflict_error(error: &anyhow::Error) -> bool {
    error.to_string().contains("409 Conflict")
}

fn is_already_taken_error(error: &anyhow::Error) -> bool {
    let text = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    text.contains("has already been taken")
}

fn webhook_urls_match(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (normalize_webhook_url(left), normalize_webhook_url(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn normalize_webhook_url(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    let scheme = url.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let port = url.port_or_known_default()?;
    let username = url.username();
    let password = url.password().unwrap_or_default();
    let path = url.path().trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    let query = url.query().unwrap_or_default();
    Some(format!(
        "{scheme}://{username}:{password}@{host}:{port}{path}?{query}"
    ))
}

fn next_link(headers: &HeaderMap) -> Option<String> {
    let header = headers.get("link")?.to_str().ok()?;
    for part in header.split(',') {
        let mut sections = part.trim().split(';');
        let url = sections.next()?.trim();
        let rel = sections.any(|section| section.trim() == "rel=\"next\"");
        if rel {
            return url
                .strip_prefix('<')
                .and_then(|value| value.strip_suffix('>'))
                .map(ToString::to_string);
        }
    }
    None
}

fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn default_git_username(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Github => "x-access-token",
        ProviderKind::Gitlab | ProviderKind::Gitea | ProviderKind::Forgejo => "oauth2",
    }
}

#[derive(Deserialize)]
struct GithubRepo {
    name: String,
    clone_url: String,
    private: bool,
    description: Option<String>,
    owner: GithubOwner,
}

#[derive(Deserialize)]
struct GithubOwner {
    login: String,
}

impl From<GithubRepo> for RemoteRepo {
    fn from(value: GithubRepo) -> Self {
        Self {
            name: value.name,
            clone_url: value.clone_url,
            private: value.private,
            description: value.description,
        }
    }
}

#[derive(Deserialize)]
struct GitlabProject {
    name: String,
    path: Option<String>,
    path_with_namespace: Option<String>,
    namespace: Option<GitlabNamespace>,
    http_url_to_repo: String,
    visibility: String,
    description: Option<String>,
    marked_for_deletion_at: Option<String>,
    marked_for_deletion_on: Option<String>,
    #[serde(default)]
    pending_delete: bool,
}

impl GitlabProject {
    fn project_path(&self) -> &str {
        self.path.as_deref().unwrap_or(&self.name)
    }

    fn is_in_namespace(&self, namespace: &str) -> bool {
        self.namespace
            .as_ref()
            .and_then(GitlabNamespace::full_path)
            .is_some_and(|path| path.eq_ignore_ascii_case(namespace))
            || self
                .path_with_namespace
                .as_deref()
                .and_then(|path| path.rsplit_once('/').map(|(namespace, _)| namespace))
                .is_some_and(|path| path.eq_ignore_ascii_case(namespace))
    }

    fn same_path(&self, other: &Self) -> bool {
        match (&self.path_with_namespace, &other.path_with_namespace) {
            (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
            _ => self
                .project_path()
                .eq_ignore_ascii_case(other.project_path()),
        }
    }

    fn is_deletion_scheduled(&self) -> bool {
        self.pending_delete
            || self
                .marked_for_deletion_at
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            || self
                .marked_for_deletion_on
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            || is_gitlab_deletion_scheduled_path(&self.name)
            || self
                .path
                .as_deref()
                .is_some_and(is_gitlab_deletion_scheduled_path)
            || self
                .path_with_namespace
                .as_deref()
                .and_then(|path| path.rsplit('/').next())
                .is_some_and(is_gitlab_deletion_scheduled_path)
    }
}

fn is_gitlab_deletion_scheduled_path(path: &str) -> bool {
    let Some((name, project_id)) = path.rsplit_once("-deletion_scheduled-") else {
        return false;
    };
    !name.is_empty()
        && !project_id.is_empty()
        && project_id.bytes().all(|byte| byte.is_ascii_digit())
}

#[derive(Deserialize)]
struct GitlabNamespace {
    path: Option<String>,
    full_path: Option<String>,
}

impl GitlabNamespace {
    fn full_path(&self) -> Option<&str> {
        self.full_path.as_deref().or(self.path.as_deref())
    }
}

impl From<GitlabProject> for RemoteRepo {
    fn from(value: GitlabProject) -> Self {
        Self {
            name: value.path.unwrap_or(value.name),
            clone_url: value.http_url_to_repo,
            private: value.visibility != "public",
            description: value.description,
        }
    }
}

#[derive(Deserialize)]
struct GitlabGroup {
    id: u64,
}

#[derive(Deserialize)]
struct GiteaRepo {
    name: String,
    clone_url: String,
    private: bool,
    description: Option<String>,
    owner: GiteaOwner,
}

#[derive(Deserialize)]
struct GiteaOwner {
    login: String,
}

impl From<GiteaRepo> for RemoteRepo {
    fn from(value: GiteaRepo) -> Self {
        Self {
            name: value.name,
            clone_url: value.clone_url,
            private: value.private,
            description: value.description,
        }
    }
}

#[derive(Deserialize)]
struct RepoHook {
    id: u64,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    config: HashMap<String, String>,
}

#[derive(Deserialize)]
struct ProviderPullRequest {
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    iid: Option<u64>,
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    web_url: Option<String>,
    #[serde(default)]
    head: Option<PullRequestHead>,
    #[serde(default)]
    source_branch: Option<String>,
}

impl ProviderPullRequest {
    fn url(&self) -> Option<String> {
        self.html_url.clone().or_else(|| self.web_url.clone())
    }

    fn head_ref(&self) -> Option<&str> {
        self.head
            .as_ref()
            .map(|head| head.reference.as_str())
            .or(self.source_branch.as_deref())
    }
}

#[derive(Deserialize)]
struct PullRequestHead {
    #[serde(rename = "ref")]
    reference: String,
}

impl RepoHook {
    fn url(&self) -> Option<&str> {
        self.config
            .get("url")
            .map(String::as_str)
            .or(self.url.as_deref())
    }
}

pub fn repos_by_name(repos: Vec<EndpointRepo>) -> HashMap<String, Vec<EndpointRepo>> {
    let mut output: HashMap<String, Vec<EndpointRepo>> = HashMap::new();
    for repo in repos {
        output.entry(repo.repo.name.clone()).or_default().push(repo);
    }
    output
}

#[cfg(test)]
#[path = "../tests/unit/provider.rs"]
mod tests;
