use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use regex::Regex;
use serde::{Deserialize, Serialize};

const APP_NAME: &str = "refray";
pub const DEFAULT_JOBS: usize = 4;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_jobs", skip_serializing_if = "is_default_jobs")]
    pub jobs: usize,
    #[serde(default)]
    pub sites: Vec<SiteConfig>,
    #[serde(default)]
    pub mirrors: Vec<MirrorConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SiteConfig {
    pub name: String,
    pub provider: ProviderKind,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    pub token: TokenConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_username: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Github,
    Gitlab,
    Gitea,
    Forgejo,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenConfig {
    Value(String),
    Env(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MirrorConfig {
    pub name: String,
    pub endpoints: Vec<EndpointConfig>,
    #[serde(default)]
    pub sync_visibility: SyncVisibility,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_whitelist: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_blacklist: Vec<String>,
    #[serde(default = "default_true")]
    pub create_missing: bool,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub conflict_resolution: ConflictResolutionStrategy,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolutionStrategy {
    #[default]
    Fail,
    AutoRebase,
    PullRequest,
    AutoRebasePullRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WebhookConfig {
    #[serde(default = "default_true")]
    pub install: bool,
    pub url: String,
    pub secret: TokenConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_sync_interval_minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachability_check_interval_minutes: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            jobs: DEFAULT_JOBS,
            sites: Vec::new(),
            mirrors: Vec::new(),
            webhook: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EndpointConfig {
    pub site: String,
    pub kind: NamespaceKind,
    pub namespace: String,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceKind {
    User,
    Org,
    Group,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Private,
    Public,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncVisibility {
    #[default]
    All,
    Private,
    Public,
}

#[derive(Clone, Debug)]
pub struct RepoNameFilter {
    whitelist: Vec<Regex>,
    blacklist: Vec<Regex>,
}

impl SyncVisibility {
    pub fn matches_private(&self, private: bool) -> bool {
        match self {
            SyncVisibility::All => true,
            SyncVisibility::Private => private,
            SyncVisibility::Public => !private,
        }
    }
}

impl MirrorConfig {
    pub fn repo_filter(&self) -> Result<RepoNameFilter> {
        Ok(RepoNameFilter {
            whitelist: compile_repo_patterns(&self.name, "repo_whitelist", &self.repo_whitelist)?,
            blacklist: compile_repo_patterns(&self.name, "repo_blacklist", &self.repo_blacklist)?,
        })
    }
}

impl RepoNameFilter {
    pub fn matches(&self, repo_name: &str) -> bool {
        let whitelisted = self.whitelist.is_empty()
            || self
                .whitelist
                .iter()
                .any(|pattern| pattern.is_match(repo_name));
        let blacklisted = self
            .blacklist
            .iter()
            .any(|pattern| pattern.is_match(repo_name));
        whitelisted && !blacklisted
    }
}

fn compile_repo_patterns(mirror: &str, field: &str, patterns: &[String]) -> Result<Vec<Regex>> {
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(pattern)
                .with_context(|| format!("mirror '{mirror}' has invalid {field} regex '{pattern}'"))
        })
        .collect()
}

fn default_true() -> bool {
    true
}

fn default_jobs() -> usize {
    DEFAULT_JOBS
}

fn is_default_jobs(jobs: &usize) -> bool {
    *jobs == DEFAULT_JOBS
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let contents = toml::to_string_pretty(self)?;
        let mut file = fs::File::create(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        protect_file(path)?;
        Ok(())
    }

    pub fn site(&self, name: &str) -> Option<&SiteConfig> {
        self.sites.iter().find(|site| site.name == name)
    }

    pub fn upsert_site(&mut self, site: SiteConfig) {
        if let Some(existing) = self
            .sites
            .iter_mut()
            .find(|existing| existing.name == site.name)
        {
            *existing = site;
        } else {
            self.sites.push(site);
        }
    }

    pub fn upsert_mirror(&mut self, mirror: MirrorConfig) {
        if let Some(existing) = self
            .mirrors
            .iter_mut()
            .find(|existing| existing.name == mirror.name)
        {
            *existing = mirror;
        } else {
            self.mirrors.push(mirror);
        }
    }

    pub fn remove_mirror(&mut self, name: &str) -> Result<()> {
        let old_len = self.mirrors.len();
        self.mirrors.retain(|mirror| mirror.name != name);
        if self.mirrors.len() == old_len {
            bail!("mirror '{name}' does not exist");
        }
        Ok(())
    }
}

impl SiteConfig {
    pub fn token(&self) -> Result<String> {
        self.token.value("site token")
    }

    pub fn api_base(&self) -> String {
        if let Some(api_url) = &self.api_url {
            return trim_end(api_url).to_string();
        }

        match self.provider {
            ProviderKind::Github => {
                if self.base_url.trim_end_matches('/') == "https://github.com" {
                    "https://api.github.com".to_string()
                } else {
                    format!("{}/api/v3", trim_end(&self.base_url))
                }
            }
            ProviderKind::Gitlab => format!("{}/api/v4", trim_end(&self.base_url)),
            ProviderKind::Gitea => format!("{}/api/v1", trim_end(&self.base_url)),
            ProviderKind::Forgejo => format!("{}/api/v1", trim_end(&self.base_url)),
        }
    }
}

impl WebhookConfig {
    pub fn secret(&self) -> Result<String> {
        self.secret.value("webhook secret")
    }
}

impl TokenConfig {
    pub fn value(&self, label: &str) -> Result<String> {
        match self {
            TokenConfig::Value(value) => Ok(value.clone()),
            TokenConfig::Env(name) => env::var(name)
                .with_context(|| format!("environment variable {name} for {label} is not set")),
        }
    }
}

impl EndpointConfig {
    pub fn label(&self) -> String {
        format!("{}:{}:{:?}", self.site, self.namespace, self.kind)
    }
}

pub fn default_config_path() -> PathBuf {
    ProjectDirs::from("dev", APP_NAME, APP_NAME)
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("refray.toml"))
}

pub fn default_work_dir() -> PathBuf {
    ProjectDirs::from("dev", APP_NAME, APP_NAME)
        .map(|dirs| dirs.cache_dir().join("mirrors"))
        .unwrap_or_else(|| PathBuf::from(".refray-cache"))
}

fn trim_end(value: &str) -> &str {
    value.trim_end_matches('/')
}

#[cfg(unix)]
fn protect_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = fs::Permissions::from_mode(0o600);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn protect_file(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn validate_config(config: &Config) -> Result<()> {
    if config.jobs == 0 {
        bail!("jobs must be at least 1");
    }
    if config.sites.is_empty() {
        bail!("no sites configured");
    }
    if config.mirrors.is_empty() {
        bail!("no mirror groups configured");
    }
    for mirror in &config.mirrors {
        mirror.repo_filter()?;
        if mirror.endpoints.len() < 2 {
            bail!(
                "mirror '{}' must contain at least two endpoints",
                mirror.name
            );
        }
        for endpoint in &mirror.endpoints {
            config.site(&endpoint.site).ok_or_else(|| {
                anyhow!(
                    "mirror '{}' references unknown site '{}'",
                    mirror.name,
                    endpoint.site
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/config.rs"]
mod tests;
