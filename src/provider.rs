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
        match self.site.provider {
            ProviderKind::Github => self.github_list_repos(endpoint),
            ProviderKind::Gitlab => self.gitlab_list_repos(endpoint),
            ProviderKind::Gitea => self.gitea_list_repos(endpoint),
        }
    }

    pub fn create_repo(
        &self,
        endpoint: &EndpointConfig,
        name: &str,
        visibility: &Visibility,
        description: Option<&str>,
    ) -> Result<RemoteRepo> {
        match self.site.provider {
            ProviderKind::Github => {
                self.github_create_repo(endpoint, name, visibility, description)
            }
            ProviderKind::Gitlab => {
                self.gitlab_create_repo(endpoint, name, visibility, description)
            }
            ProviderKind::Gitea => self.gitea_create_repo(endpoint, name, visibility, description),
        }
    }

    pub fn validate_token(&self) -> Result<()> {
        let url = format!("{}/user", self.site.api_base());
        self.get(&url).map(|_| ())
    }

    pub fn detect_namespace_kind(&self, namespace: &str) -> Result<Option<NamespaceKind>> {
        match self.site.provider {
            ProviderKind::Github => self.github_detect_namespace_kind(namespace),
            ProviderKind::Gitlab => self.gitlab_detect_namespace_kind(namespace),
            ProviderKind::Gitea => self.gitea_detect_namespace_kind(namespace),
        }
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
                ProviderKind::Gitlab | ProviderKind::Gitea => "oauth2".to_string(),
            });
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
                let repos: Vec<GithubRepo> = self
                    .paged_get(&url)?
                    .into_iter()
                    .filter(|repo: &GithubRepo| {
                        repo.owner.login.eq_ignore_ascii_case(&endpoint.namespace)
                    })
                    .collect();
                Ok(repos.into_iter().map(Into::into).collect())
            }
            NamespaceKind::Org => {
                let url = format!(
                    "{}/orgs/{}/repos?type=all&per_page=100",
                    self.site.api_base(),
                    endpoint.namespace
                );
                let repos: Vec<GithubRepo> = self.paged_get(&url)?;
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

    fn gitlab_list_repos(&self, endpoint: &EndpointConfig) -> Result<Vec<RemoteRepo>> {
        match endpoint.kind {
            NamespaceKind::User => {
                let url = format!(
                    "{}/users/{}/projects?simple=true&per_page=100&owned=true",
                    self.site.api_base(),
                    endpoint.namespace
                );
                let repos: Vec<GitlabProject> = self.paged_get(&url)?;
                Ok(repos.into_iter().map(Into::into).collect())
            }
            NamespaceKind::Org | NamespaceKind::Group => {
                let encoded = urlencoding(&endpoint.namespace);
                let url = format!(
                    "{}/groups/{}/projects?simple=true&include_subgroups=false&per_page=100",
                    self.site.api_base(),
                    encoded
                );
                let repos: Vec<GitlabProject> = self.paged_get(&url)?;
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
            NamespaceKind::Group => bail!("Gitea endpoints use kind 'user' or 'org'"),
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
            NamespaceKind::Group => bail!("Gitea endpoints use kind 'user' or 'org'"),
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

    fn get_json<T>(&self, url: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.get(url)?
            .json()
            .with_context(|| format!("invalid JSON from {url}"))
    }

    fn post_json<T>(&self, url: &str, body: &serde_json::Value) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.request_headers(self.http.post(url))?
            .json(body)
            .send()
            .with_context(|| format!("POST {url} failed"))
            .and_then(|response| check_response("POST", url, response))?
            .json()
            .with_context(|| format!("invalid JSON from {url}"))
    }

    fn get(&self, url: &str) -> Result<Response> {
        self.request_headers(self.http.get(url))?
            .send()
            .with_context(|| format!("GET {url} failed"))
            .and_then(|response| check_response("GET", url, response))
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
            ProviderKind::Gitea => {
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
    fn group_paths_are_url_encoded_for_gitlab() {
        assert_eq!(urlencoding("parent/child group"), "parent%2Fchild+group");
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
            assert!(request.starts_with("GET /user "), "request was {request}");
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
        assert!(err.contains("401 Unauthorized"));
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
}
