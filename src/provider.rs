use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use gitlab::api::{self, Pagination, Query};
use octocrab::{Octocrab, Page};
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use tokio::runtime::Runtime;
use url::Url;

use crate::config::{EndpointConfig, NamespaceKind, ProviderKind, SiteConfig, Visibility};

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

pub struct ProviderClient<'a> {
    site: &'a SiteConfig,
    token: String,
    http: Client,
    runtime: Runtime,
    github: Option<Octocrab>,
    gitlab: Option<GitlabApiClient>,
}

macro_rules! dispatch_provider {
    ($provider:expr, github => $github:expr, gitlab => $gitlab:expr, gitea_like => $gitea:expr $(,)?) => {
        match $provider {
            ProviderKind::Github => $github,
            ProviderKind::Gitlab => $gitlab,
            ProviderKind::Gitea | ProviderKind::Forgejo => $gitea,
        }
    };
}

macro_rules! json_method {
    ($name:ident, $method:ident, $verb:literal) => {
        fn $name<T>(&self, url: &str, body: &serde_json::Value) -> Result<T>
        where
            T: for<'de> Deserialize<'de>,
        {
            self.request_headers(self.http.$method(url))?
                .json(body)
                .send()
                .with_context(|| format!("{} {} failed", $verb, url))
                .and_then(|response| check_response($verb, url, response))?
                .json()
                .with_context(|| format!("invalid JSON from {url}"))
        }
    };
}

impl<'a> ProviderClient<'a> {
    pub fn new(site: &'a SiteConfig) -> Result<Self> {
        let token = site.token()?;
        let http = Client::builder().build()?;
        let runtime = Runtime::new().context("failed to create async API runtime")?;
        let github = if matches!(site.provider, ProviderKind::Github) {
            let _runtime_context = runtime.enter();
            Some(
                Octocrab::builder()
                    .personal_token(token.clone())
                    .base_uri(site.api_base())
                    .context("invalid GitHub API base URL")?
                    .build()
                    .context("failed to create GitHub API client")?,
            )
        } else {
            None
        };
        let gitlab = if matches!(site.provider, ProviderKind::Gitlab) {
            Some(GitlabApiClient::new(
                site.api_base(),
                token.clone(),
                http.clone(),
            )?)
        } else {
            None
        };
        Ok(Self {
            site,
            token,
            http,
            runtime,
            github,
            gitlab,
        })
    }

    pub fn list_repos(&self, endpoint: &EndpointConfig) -> Result<Vec<RemoteRepo>> {
        dispatch_provider!(
            self.site.provider,
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
        dispatch_provider!(
            self.site.provider,
            github => self.github_create_repo(endpoint, name, visibility, description),
            gitlab => self.gitlab_create_repo(endpoint, name, visibility, description),
            gitea_like => self.gitea_create_repo(endpoint, name, visibility, description),
        )
    }

    pub fn install_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        url: &str,
        secret: &str,
    ) -> Result<()> {
        dispatch_provider!(
            self.site.provider,
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
        dispatch_provider!(
            self.site.provider,
            github => self.github_uninstall_webhook(endpoint, repo_name, url),
            gitlab => self.gitlab_uninstall_webhook(endpoint, repo_name, url),
            gitea_like => self.gitea_uninstall_webhook(endpoint, repo_name, url),
        )
    }

    pub fn validate_token(&self) -> Result<()> {
        dispatch_provider!(
            self.site.provider,
            github => self.github_get_json::<serde_json::Value>("/user").map(|_| ()),
            gitlab => self.gitlab_query::<serde_json::Value, _>(
                gitlab::api::users::CurrentUser::builder().build()?,
            )
            .map(|_| ()),
            gitea_like => {
                let url = format!("{}/user", self.site.api_base());
                self.get(&url).map(|_| ())
            },
        )
    }

    pub fn detect_namespace_kind(&self, namespace: &str) -> Result<Option<NamespaceKind>> {
        dispatch_provider!(
            self.site.provider,
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
            .unwrap_or_else(|| match self.site.provider {
                ProviderKind::Github => "x-access-token".to_string(),
                ProviderKind::Gitlab | ProviderKind::Gitea | ProviderKind::Forgejo => {
                    "oauth2".to_string()
                }
            });
        url.set_username(&username)
            .map_err(|_| anyhow!("failed to set username on clone URL"))?;
        url.set_password(Some(&self.token))
            .map_err(|_| anyhow!("failed to set token on clone URL"))?;
        Ok(url.to_string())
    }

    fn github(&self) -> Result<&Octocrab> {
        self.github
            .as_ref()
            .ok_or_else(|| anyhow!("GitHub API client is not configured"))
    }

    fn github_paged<T>(&self, route: &str) -> Result<Vec<T>>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let github = self.github()?;
        let page = self
            .runtime
            .block_on(github.get::<Page<T>, _, _>(route, None::<&()>))
            .with_context(|| format!("GitHub GET {route} failed"))?;
        self.runtime
            .block_on(github.all_pages(page))
            .with_context(|| format!("GitHub pagination for {route} failed"))
    }

    fn github_get_json<T>(&self, route: &str) -> Result<T>
    where
        T: DeserializeOwned + Send + 'static,
    {
        self.runtime
            .block_on(self.github()?.get(route, None::<&()>))
            .with_context(|| format!("GitHub GET {route} failed"))
    }

    fn github_post<T>(&self, route: &str, body: &serde_json::Value) -> Result<T>
    where
        T: DeserializeOwned + Send + 'static,
    {
        self.runtime
            .block_on(self.github()?.post(route, Some(body)))
            .with_context(|| format!("GitHub POST {route} failed"))
    }

    fn github_patch<T>(&self, route: &str, body: &serde_json::Value) -> Result<T>
    where
        T: DeserializeOwned + Send + 'static,
    {
        self.runtime
            .block_on(self.github()?.patch(route, Some(body)))
            .with_context(|| format!("GitHub PATCH {route} failed"))
    }

    fn github_delete(&self, route: &str) -> Result<()> {
        let github = self.github()?;
        self.runtime.block_on(async {
            let response = github._delete(route, None::<&()>).await?;
            if response.status().is_success() {
                Ok(())
            } else {
                let status = response.status();
                let body = github.body_to_string(response).await.unwrap_or_default();
                bail!("GitHub DELETE {route} returned {status}: {body}")
            }
        })
    }

    fn github_find_existing_hook(
        &self,
        hooks_route: &str,
        target_url: &str,
    ) -> Result<Option<RepoHook>> {
        let hooks: Vec<RepoHook> = self.github_paged(hooks_route)?;
        Ok(hooks
            .into_iter()
            .find(|hook| hook.url() == Some(target_url)))
    }

    fn gitlab(&self) -> Result<&GitlabApiClient> {
        self.gitlab
            .as_ref()
            .ok_or_else(|| anyhow!("GitLab API client is not configured"))
    }

    fn gitlab_query<T, E>(&self, endpoint: E) -> Result<T>
    where
        T: DeserializeOwned,
        E: api::Endpoint,
    {
        endpoint
            .query(self.gitlab()?)
            .map_err(|error| anyhow!("GitLab API request failed: {error}"))
    }

    fn gitlab_paged<T, E>(&self, endpoint: E) -> Result<Vec<T>>
    where
        T: DeserializeOwned + 'static,
        E: api::Endpoint + api::Pageable,
    {
        api::paged(endpoint, Pagination::All)
            .query(self.gitlab()?)
            .map_err(|error| anyhow!("GitLab paged API request failed: {error}"))
    }

    fn gitlab_ignore<E>(&self, endpoint: E) -> Result<()>
    where
        E: api::Endpoint,
    {
        api::ignore(endpoint)
            .query(self.gitlab()?)
            .map_err(|error| anyhow!("GitLab API request failed: {error}"))
    }

    fn gitlab_find_existing_hook(
        &self,
        project: &str,
        target_url: &str,
    ) -> Result<Option<RepoHook>> {
        let endpoint = gitlab::api::projects::hooks::Hooks::builder()
            .project(project)
            .build()?;
        let hooks: Vec<RepoHook> = self.gitlab_paged(endpoint)?;
        Ok(hooks
            .into_iter()
            .find(|hook| hook.url() == Some(target_url)))
    }

    fn github_list_repos(&self, endpoint: &EndpointConfig) -> Result<Vec<RemoteRepo>> {
        match endpoint.kind {
            NamespaceKind::User => {
                let repos: Vec<GithubRepo> = self
                    .github_paged::<GithubRepo>(
                        "/user/repos?affiliation=owner&visibility=all&per_page=100",
                    )?
                    .into_iter()
                    .filter(|repo: &GithubRepo| {
                        repo.owner.login.eq_ignore_ascii_case(&endpoint.namespace)
                    })
                    .collect();
                Ok(repos.into_iter().map(Into::into).collect())
            }
            NamespaceKind::Org => {
                let route = format!("/orgs/{}/repos?type=all&per_page=100", endpoint.namespace);
                let repos: Vec<GithubRepo> = self.github_paged(&route)?;
                Ok(repos.into_iter().map(Into::into).collect())
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
        let route = match endpoint.kind {
            NamespaceKind::User => "/user/repos".to_string(),
            NamespaceKind::Org => format!("/orgs/{}/repos", endpoint.namespace),
            NamespaceKind::Group => bail!("GitHub endpoints use kind 'user' or 'org'"),
        };
        let body = json!({
            "name": name,
            "private": matches!(visibility, Visibility::Private),
            "description": description.unwrap_or(""),
        });
        self.github_post::<GithubRepo>(&route, &body)
            .map(Into::into)
    }

    fn github_detect_namespace_kind(&self, namespace: &str) -> Result<Option<NamespaceKind>> {
        let value: serde_json::Value = self.github_get_json(&format!("/users/{namespace}"))?;
        Ok(match value.get("type").and_then(|value| value.as_str()) {
            Some("Organization") => Some(NamespaceKind::Org),
            Some("User") => Some(NamespaceKind::User),
            _ => None,
        })
    }

    fn github_install_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        url: &str,
        secret: &str,
    ) -> Result<()> {
        if matches!(endpoint.kind, NamespaceKind::Group) {
            bail!("GitHub endpoints use kind 'user' or 'org'");
        }
        let hooks_route = format!("/repos/{}/{}/hooks", endpoint.namespace, repo.name);
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
        if let Some(hook) = self.github_find_existing_hook(&hooks_route, url)? {
            let update_route = format!("{hooks_route}/{}", hook.id);
            self.github_patch::<serde_json::Value>(&update_route, &body)?;
        } else {
            self.github_post::<serde_json::Value>(&hooks_route, &body)?;
        }
        Ok(())
    }

    fn github_uninstall_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        url: &str,
    ) -> Result<bool> {
        if matches!(endpoint.kind, NamespaceKind::Group) {
            bail!("GitHub endpoints use kind 'user' or 'org'");
        }
        let hooks_route = format!("/repos/{}/{}/hooks", endpoint.namespace, repo_name);
        let Some(hook) = self.github_find_existing_hook(&hooks_route, url)? else {
            return Ok(false);
        };
        self.github_delete(&format!("{hooks_route}/{}", hook.id))?;
        Ok(true)
    }

    fn gitlab_list_repos(&self, endpoint: &EndpointConfig) -> Result<Vec<RemoteRepo>> {
        match endpoint.kind {
            NamespaceKind::User => {
                let endpoint = gitlab::api::users::UserProjects::builder()
                    .user(endpoint.namespace.as_str())
                    .simple(true)
                    .owned(true)
                    .build()?;
                let repos: Vec<GitlabProject> = self.gitlab_paged(endpoint)?;
                Ok(repos.into_iter().map(Into::into).collect())
            }
            NamespaceKind::Org | NamespaceKind::Group => {
                let endpoint = gitlab::api::groups::projects::GroupProjects::builder()
                    .group(endpoint.namespace.as_str())
                    .simple(true)
                    .include_subgroups(false)
                    .build()?;
                let repos: Vec<GitlabProject> = self.gitlab_paged(endpoint)?;
                Ok(repos.into_iter().map(Into::into).collect())
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
        let mut builder = gitlab::api::projects::CreateProject::builder();
        builder
            .name(name)
            .path(name)
            .visibility(gitlab_visibility(visibility))
            .description(description.unwrap_or(""));
        if matches!(endpoint.kind, NamespaceKind::Org | NamespaceKind::Group) {
            let group = self.gitlab_group(&endpoint.namespace)?;
            builder.namespace_id(group.id);
        }
        self.gitlab_query::<GitlabProject, _>(builder.build()?)
            .map(Into::into)
    }

    fn gitlab_group(&self, namespace: &str) -> Result<GitlabGroup> {
        self.gitlab_query(
            gitlab::api::groups::Group::builder()
                .group(namespace)
                .build()?,
        )
    }

    fn gitlab_detect_namespace_kind(&self, namespace: &str) -> Result<Option<NamespaceKind>> {
        let group = gitlab::api::groups::Group::builder()
            .group(namespace)
            .build()?;
        if self.gitlab_query::<serde_json::Value, _>(group).is_ok() {
            return Ok(Some(NamespaceKind::Group));
        }

        let username = namespace.rsplit('/').next().unwrap_or(namespace);
        let users = gitlab::api::users::Users::builder()
            .username(username)
            .build()?;
        let users: Vec<serde_json::Value> = self.gitlab_query(users)?;
        Ok((!users.is_empty()).then_some(NamespaceKind::User))
    }

    fn gitlab_install_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        url: &str,
        secret: &str,
    ) -> Result<()> {
        let project = format!("{}/{}", endpoint.namespace, repo.name);
        if let Some(hook) = self.gitlab_find_existing_hook(&project, url)? {
            let endpoint = gitlab::api::projects::hooks::EditHook::builder()
                .project(project)
                .hook_id(hook.id)
                .url(url)
                .push_events(true)
                .tag_push_events(true)
                .token(secret)
                .enable_ssl_verification(true)
                .build()?;
            self.gitlab_ignore(endpoint)?;
        } else {
            let endpoint = gitlab::api::projects::hooks::CreateHook::builder()
                .project(project)
                .url(url)
                .push_events(true)
                .tag_push_events(true)
                .token(secret)
                .enable_ssl_verification(true)
                .build()?;
            self.gitlab_ignore(endpoint)?;
        }
        Ok(())
    }

    fn gitlab_uninstall_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        url: &str,
    ) -> Result<bool> {
        let project = format!("{}/{}", endpoint.namespace, repo_name);
        let Some(hook) = self.gitlab_find_existing_hook(&project, url)? else {
            return Ok(false);
        };
        let endpoint = gitlab::api::projects::hooks::DeleteHook::builder()
            .project(project)
            .hook_id(hook.id)
            .build()?;
        self.gitlab_ignore(endpoint)?;
        Ok(true)
    }

    fn gitea_list_repos(&self, endpoint: &EndpointConfig) -> Result<Vec<RemoteRepo>> {
        match endpoint.kind {
            NamespaceKind::User => {
                let url = format!("{}/user/repos?limit=50", self.site.api_base());
                let repos: Vec<GiteaRepo> = self
                    .paged_get(&url)?
                    .into_iter()
                    .filter(|repo: &GiteaRepo| {
                        repo.owner.login.eq_ignore_ascii_case(&endpoint.namespace)
                    })
                    .collect();
                Ok(repos.into_iter().map(Into::into).collect())
            }
            NamespaceKind::Org => {
                let url = format!(
                    "{}/orgs/{}/repos?limit=50",
                    self.site.api_base(),
                    endpoint.namespace
                );
                let repos: Vec<GiteaRepo> = self.paged_get(&url)?;
                Ok(repos.into_iter().map(Into::into).collect())
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
        self.post_json::<GiteaRepo>(&url, &body).map(Into::into)
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

    fn gitea_install_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        url: &str,
        secret: &str,
    ) -> Result<()> {
        if matches!(endpoint.kind, NamespaceKind::Group) {
            bail!("Gitea endpoints use kind 'user' or 'org'");
        }
        let hooks_url = format!(
            "{}/repos/{}/{}/hooks",
            self.site.api_base(),
            endpoint.namespace,
            repo.name
        );
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
        if let Some(hook) = self.find_existing_hook(&hooks_url, url)? {
            let update_url = format!("{hooks_url}/{}", hook.id);
            self.patch_json::<serde_json::Value>(&update_url, &body)?;
        } else {
            self.post_json::<serde_json::Value>(&hooks_url, &body)?;
        }
        Ok(())
    }

    fn gitea_uninstall_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo_name: &str,
        url: &str,
    ) -> Result<bool> {
        if matches!(endpoint.kind, NamespaceKind::Group) {
            bail!("Gitea endpoints use kind 'user' or 'org'");
        }
        let hooks_url = format!(
            "{}/repos/{}/{}/hooks",
            self.site.api_base(),
            endpoint.namespace,
            repo_name
        );
        self.delete_matching_hook(&hooks_url, url)
    }

    fn find_existing_hook(&self, hooks_url: &str, target_url: &str) -> Result<Option<RepoHook>> {
        let hooks: Vec<RepoHook> = self.paged_get(hooks_url)?;
        Ok(hooks
            .into_iter()
            .find(|hook| hook.url() == Some(target_url)))
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

    json_method!(post_json, post, "POST");
    json_method!(patch_json, patch, "PATCH");

    fn get(&self, url: &str) -> Result<Response> {
        self.request_headers(self.http.get(url))?
            .send()
            .with_context(|| format!("GET {url} failed"))
            .and_then(|response| check_response("GET", url, response))
    }

    fn delete(&self, url: &str) -> Result<Response> {
        self.request_headers(self.http.delete(url))?
            .send()
            .with_context(|| format!("DELETE {url} failed"))
            .and_then(|response| check_response("DELETE", url, response))
    }

    fn request_headers(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("git-sync/0.1"));
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

struct GitlabApiClient {
    rest_url: Url,
    token: String,
    http: Client,
}

impl GitlabApiClient {
    fn new(api_base: String, token: String, http: Client) -> Result<Self> {
        let rest_url = Url::parse(&format!("{}/", api_base.trim_end_matches('/')))
            .context("invalid GitLab API base URL")?;
        Ok(Self {
            rest_url,
            token,
            http,
        })
    }
}

impl api::RestClient for GitlabApiClient {
    type Error = GitlabClientError;

    fn rest_endpoint(
        &self,
        endpoint: &str,
    ) -> std::result::Result<Url, api::ApiError<Self::Error>> {
        Ok(self.rest_url.join(endpoint)?)
    }
}

impl api::Client for GitlabApiClient {
    fn rest(
        &self,
        request: http::request::Builder,
        body: Vec<u8>,
    ) -> std::result::Result<http::Response<Bytes>, api::ApiError<Self::Error>> {
        self.rest_request(request, body)
            .map_err(api::ApiError::client)
    }
}

impl GitlabApiClient {
    fn rest_request(
        &self,
        request: http::request::Builder,
        body: Vec<u8>,
    ) -> std::result::Result<http::Response<Bytes>, GitlabClientError> {
        let request = request
            .header(
                "PRIVATE-TOKEN",
                HeaderValue::from_str(&self.token)
                    .map_err(GitlabClientError::InvalidHeaderValue)?,
            )
            .body(body)
            .map_err(GitlabClientError::Http)?;
        let (parts, body) = request.into_parts();
        let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
            .map_err(GitlabClientError::InvalidMethod)?;
        let mut builder = self.http.request(method, parts.uri.to_string());
        for (name, value) in &parts.headers {
            builder = builder.header(name.as_str(), value.as_bytes());
        }
        let response = builder
            .body(body)
            .send()
            .map_err(GitlabClientError::Reqwest)?;
        let mut output = http::Response::builder().status(response.status());
        for (name, value) in response.headers() {
            output = output.header(name.as_str(), value.as_bytes());
        }
        output
            .body(response.bytes().map_err(GitlabClientError::Reqwest)?)
            .map_err(GitlabClientError::Http)
    }
}

#[derive(Debug)]
enum GitlabClientError {
    Reqwest(reqwest::Error),
    Http(http::Error),
    InvalidHeaderValue(reqwest::header::InvalidHeaderValue),
    InvalidMethod(http::method::InvalidMethod),
}

impl fmt::Display for GitlabClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reqwest(error) => write!(formatter, "communication with GitLab: {error}"),
            Self::Http(error) => write!(formatter, "HTTP request error: {error}"),
            Self::InvalidHeaderValue(error) => {
                write!(formatter, "invalid GitLab token header: {error}")
            }
            Self::InvalidMethod(error) => write!(formatter, "invalid GitLab HTTP method: {error}"),
        }
    }
}

impl Error for GitlabClientError {}

fn gitlab_visibility(visibility: &Visibility) -> gitlab::api::common::VisibilityLevel {
    match visibility {
        Visibility::Private => gitlab::api::common::VisibilityLevel::Private,
        Visibility::Public => gitlab::api::common::VisibilityLevel::Public,
    }
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
    http_url_to_repo: String,
    visibility: String,
    description: Option<String>,
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

impl RepoHook {
    fn url(&self) -> Option<&str> {
        self.url
            .as_deref()
            .or_else(|| self.config.get("url").map(String::as_str))
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
mod tests {
    use super::*;
    use crate::config::TokenConfig;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn extracts_next_link() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "link",
            HeaderValue::from_static("<https://example.test?page=2>; rel=\"next\", <https://example.test?page=5>; rel=\"last\""),
        );
        assert_eq!(next_link(&headers).unwrap(), "https://example.test?page=2");
    }

    #[test]
    fn authenticated_clone_urls_use_provider_defaults() {
        let github_site = site(ProviderKind::Github, None);
        let github = ProviderClient::new(&github_site).unwrap();
        assert_eq!(
            github
                .authenticated_clone_url("https://github.com/alice/repo.git")
                .unwrap(),
            "https://x-access-token:secret@github.com/alice/repo.git"
        );

        let gitlab_site = site(ProviderKind::Gitlab, None);
        let gitlab = ProviderClient::new(&gitlab_site).unwrap();
        assert_eq!(
            gitlab
                .authenticated_clone_url("https://gitlab.example.test/alice/repo.git")
                .unwrap(),
            "https://oauth2:secret@gitlab.example.test/alice/repo.git"
        );

        let forgejo_site = site(ProviderKind::Forgejo, None);
        let forgejo = ProviderClient::new(&forgejo_site).unwrap();
        assert_eq!(
            forgejo
                .authenticated_clone_url("https://forgejo.example.test/alice/repo.git")
                .unwrap(),
            "https://oauth2:secret@forgejo.example.test/alice/repo.git"
        );
    }

    #[test]
    fn authenticated_clone_urls_can_override_git_username() {
        let gitea_site = site(ProviderKind::Gitea, Some("mirror-user".to_string()));
        let client = ProviderClient::new(&gitea_site).unwrap();

        assert_eq!(
            client
                .authenticated_clone_url("https://gitea.example.test/alice/repo.git")
                .unwrap(),
            "https://mirror-user:secret@gitea.example.test/alice/repo.git"
        );
    }

    #[test]
    fn validate_token_checks_user_endpoint_with_provider_auth_header() {
        let (api_url, handle) = one_request_server("200 OK", "{}", |request| {
            assert!(request.starts_with("GET /user "), "request was {request}");
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer secret"),
                "request was {request}"
            );
        });
        let site = SiteConfig {
            api_url: Some(api_url),
            ..site(ProviderKind::Github, None)
        };

        ProviderClient::new(&site)
            .unwrap()
            .validate_token()
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn validate_token_reports_provider_rejection() {
        let (api_url, handle) = one_request_server("401 Unauthorized", "bad token", |request| {
            assert!(request.starts_with("GET /user"), "request was {request}");
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("private-token: secret"),
                "request was {request}"
            );
        });
        let site = SiteConfig {
            api_url: Some(api_url),
            ..site(ProviderKind::Gitlab, None)
        };

        let err = ProviderClient::new(&site)
            .unwrap()
            .validate_token()
            .unwrap_err()
            .to_string();
        assert!(err.contains("401") || err.contains("Unauthorized"));
        handle.join().unwrap();
    }

    #[test]
    fn detect_namespace_kind_uses_authenticated_github_api() {
        let (api_url, handle) =
            one_request_server("200 OK", r#"{"type":"Organization"}"#, |request| {
                assert!(
                    request.starts_with("GET /users/acme "),
                    "request was {request}"
                );
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer secret"),
                    "request was {request}"
                );
            });
        let site = SiteConfig {
            api_url: Some(api_url),
            ..site(ProviderKind::Github, None)
        };

        let kind = ProviderClient::new(&site)
            .unwrap()
            .detect_namespace_kind("acme")
            .unwrap();
        assert_eq!(kind, Some(NamespaceKind::Org));
        handle.join().unwrap();
    }

    #[test]
    fn detect_namespace_kind_uses_authenticated_gitea_api() {
        let (api_url, handle) = one_request_server("200 OK", "{}", |request| {
            assert!(
                request.starts_with("GET /orgs/acme "),
                "request was {request}"
            );
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: token secret"),
                "request was {request}"
            );
        });
        let site = SiteConfig {
            api_url: Some(api_url),
            ..site(ProviderKind::Gitea, None)
        };

        let kind = ProviderClient::new(&site)
            .unwrap()
            .detect_namespace_kind("acme")
            .unwrap();
        assert_eq!(kind, Some(NamespaceKind::Org));
        handle.join().unwrap();
    }

    #[test]
    fn install_webhook_posts_github_hook_when_missing() {
        let (api_url, handle) = request_server(
            vec![("200 OK", "[]"), ("201 Created", r#"{"id":1}"#)],
            |index, request| match index {
                0 => assert!(
                    request.starts_with("GET /repos/alice/repo/hooks "),
                    "request was {request}"
                ),
                1 => {
                    assert!(
                        request.starts_with("POST /repos/alice/repo/hooks "),
                        "request was {request}"
                    );
                    assert!(request.contains("https://mirror.example.test/webhook"));
                    assert!(request.contains("secret"));
                    assert!(request.contains("push"));
                }
                _ => unreachable!(),
            },
        );
        let site = SiteConfig {
            api_url: Some(api_url),
            ..site(ProviderKind::Github, None)
        };
        let client = ProviderClient::new(&site).unwrap();

        client
            .install_webhook(
                &EndpointConfig {
                    site: "github".to_string(),
                    kind: NamespaceKind::User,
                    namespace: "alice".to_string(),
                },
                &RemoteRepo {
                    name: "repo".to_string(),
                    clone_url: "https://github.com/alice/repo.git".to_string(),
                    private: true,
                    description: None,
                },
                "https://mirror.example.test/webhook",
                "secret",
            )
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn uninstall_webhook_deletes_matching_github_hook() {
        let (api_url, handle) = request_server(
            vec![
                (
                    "200 OK",
                    r#"[{"id":42,"config":{"url":"https://mirror.example.test/webhook"}}]"#,
                ),
                ("204 No Content", ""),
            ],
            |index, request| match index {
                0 => assert!(
                    request.starts_with("GET /repos/alice/repo/hooks "),
                    "request was {request}"
                ),
                1 => assert!(
                    request.starts_with("DELETE /repos/alice/repo/hooks/42 "),
                    "request was {request}"
                ),
                _ => unreachable!(),
            },
        );
        let site = SiteConfig {
            api_url: Some(api_url),
            ..site(ProviderKind::Github, None)
        };
        let client = ProviderClient::new(&site).unwrap();

        let removed = client
            .uninstall_webhook(
                &EndpointConfig {
                    site: "github".to_string(),
                    kind: NamespaceKind::User,
                    namespace: "alice".to_string(),
                },
                "repo",
                "https://mirror.example.test/webhook",
            )
            .unwrap();

        assert!(removed);
        handle.join().unwrap();
    }

    fn site(provider: ProviderKind, git_username: Option<String>) -> SiteConfig {
        SiteConfig {
            name: "site".to_string(),
            provider,
            base_url: "https://example.test".to_string(),
            api_url: None,
            token: TokenConfig::Value("secret".to_string()),
            git_username,
        }
    }

    fn one_request_server<F>(
        status: &'static str,
        body: &'static str,
        assert_request: F,
    ) -> (String, thread::JoinHandle<()>)
    where
        F: FnOnce(&str) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let bytes = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            assert_request(&request);

            write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        (format!("http://{address}"), handle)
    }

    fn request_server<F>(
        responses: Vec<(&'static str, &'static str)>,
        mut assert_request: F,
    ) -> (String, thread::JoinHandle<()>)
    where
        F: FnMut(usize, &str) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for (index, (status, body)) in responses.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 4096];
                let bytes = stream.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
                assert_request(index, &request);

                write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (format!("http://{address}"), handle)
    }
}
