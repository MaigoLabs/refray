use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use serde_json::json;
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

    pub fn install_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        url: &str,
        secret: &str,
    ) -> Result<()> {
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

    fn github_install_webhook(
        &self,
        endpoint: &EndpointConfig,
        repo: &RemoteRepo,
        url: &str,
        secret: &str,
    ) -> Result<()> {
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

    fn gitlab_list_repos(&self, endpoint: &EndpointConfig) -> Result<Vec<RemoteRepo>> {
        match endpoint.kind {
            NamespaceKind::User => {
                let url = format!(
                    "{}/users/{}/projects?simple=true&per_page=100&owned=true",
                    self.site.api_base(),
                    endpoint.namespace
                );
                self.paged_remote_repos::<GitlabProject>(&url)
            }
            NamespaceKind::Org | NamespaceKind::Group => {
                let encoded = urlencoding(&endpoint.namespace);
                let url = format!(
                    "{}/groups/{}/projects?simple=true&include_subgroups=false&per_page=100",
                    self.site.api_base(),
                    encoded
                );
                self.paged_remote_repos::<GitlabProject>(&url)
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
        self.post_json::<GitlabProject>(&url, &serde_json::Value::Object(body))
            .map(Into::into)
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
    ) -> Result<()> {
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

    fn gitea_list_repos(&self, endpoint: &EndpointConfig) -> Result<Vec<RemoteRepo>> {
        match endpoint.kind {
            NamespaceKind::User => {
                let url = format!("{}/user/repos?limit=50", self.site.api_base());
                owned_repos!(self, GiteaRepo, &url, &endpoint.namespace)
            }
            NamespaceKind::Org => {
                let url = format!(
                    "{}/orgs/{}/repos?limit=50",
                    self.site.api_base(),
                    endpoint.namespace
                );
                self.paged_remote_repos::<GiteaRepo>(&url)
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

    fn gitlab_hooks_url(&self, endpoint: &EndpointConfig, repo_name: &str) -> String {
        let project = format!("{}/{repo_name}", endpoint.namespace);
        format!(
            "{}/projects/{}/hooks",
            self.site.api_base(),
            urlencoding(&project)
        )
    }

    fn find_existing_hook(&self, hooks_url: &str, target_url: &str) -> Result<Option<RepoHook>> {
        let hooks: Vec<RepoHook> = self.paged_get(hooks_url)?;
        Ok(hooks
            .into_iter()
            .find(|hook| hook.url() == Some(target_url)))
    }

    fn upsert_hook(
        &self,
        hooks_url: &str,
        target_url: &str,
        body: &serde_json::Value,
        put_on_update: bool,
    ) -> Result<()> {
        let Some(hook) = self.find_existing_hook(hooks_url, target_url)? else {
            self.post_json::<serde_json::Value>(hooks_url, body)?;
            return Ok(());
        };

        let update_url = format!("{hooks_url}/{}", hook.id);
        if put_on_update {
            self.put_json::<serde_json::Value>(&update_url, body)?;
        } else {
            self.patch_json::<serde_json::Value>(&update_url, body)?;
        }
        Ok(())
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
#[path = "../tests/unit/provider.rs"]
mod tests;
