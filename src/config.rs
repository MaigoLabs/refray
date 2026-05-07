use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub sites: Vec<SiteConfig>,
    #[serde(default)]
    pub mirrors: Vec<MirrorConfig>,
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
    #[serde(default = "default_true")]
    pub create_missing: bool,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub allow_force: bool,
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

fn default_true() -> bool {
    true
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

    pub fn remove_site(&mut self, name: &str) -> Result<()> {
        if !self.sites.iter().any(|site| site.name == name) {
            bail!("site '{name}' does not exist");
        }
        for mirror in &self.mirrors {
            if mirror
                .endpoints
                .iter()
                .any(|endpoint| endpoint.site == name)
            {
                bail!("site '{name}' is still used by mirror '{}'", mirror.name);
            }
        }
        self.sites.retain(|site| site.name != name);
        Ok(())
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
        match &self.token {
            TokenConfig::Value(value) => Ok(value.clone()),
            TokenConfig::Env(name) => {
                env::var(name).with_context(|| format!("environment variable {name} is not set"))
            }
        }
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

impl EndpointConfig {
    pub fn label(&self) -> String {
        format!("{}:{}:{:?}", self.site, self.namespace, self.kind)
    }
}

pub fn default_config_path() -> PathBuf {
    ProjectDirs::from("dev", "git-sync", "git-sync")
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("git-sync.toml"))
}

pub fn default_work_dir() -> PathBuf {
    ProjectDirs::from("dev", "git-sync", "git-sync")
        .map(|dirs| dirs.cache_dir().join("mirrors"))
        .unwrap_or_else(|| PathBuf::from(".git-sync-cache"))
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
    if config.sites.is_empty() {
        bail!("no sites configured");
    }
    if config.mirrors.is_empty() {
        bail!("no mirror groups configured");
    }
    for mirror in &config.mirrors {
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
mod tests {
    use super::*;

    #[test]
    fn parses_token_forms() {
        let config: Config = toml::from_str(
            r#"
            [[sites]]
            name = "github"
            provider = "github"
            base_url = "https://github.com"
            token = { env = "GITHUB_TOKEN" }

            [[mirrors]]
            name = "personal"
            create_missing = true
            visibility = "private"
            allow_force = false

            [[mirrors.endpoints]]
            site = "github"
            kind = "user"
            namespace = "alice"

            [[mirrors.endpoints]]
            site = "github"
            kind = "org"
            namespace = "example"
            "#,
        )
        .unwrap();

        assert_eq!(config.sites.len(), 1);
        assert_eq!(config.mirrors[0].endpoints.len(), 2);
    }

    #[test]
    fn validation_rejects_unknown_sites_and_single_endpoint_groups() {
        let config = Config {
            sites: vec![site("github", ProviderKind::Github)],
            mirrors: vec![MirrorConfig {
                name: "broken".to_string(),
                endpoints: vec![EndpointConfig {
                    site: "github".to_string(),
                    kind: NamespaceKind::User,
                    namespace: "alice".to_string(),
                }],
                create_missing: true,
                visibility: Visibility::Private,
                allow_force: false,
            }],
        };
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("at least two endpoints"));

        let config = Config {
            sites: vec![site("github", ProviderKind::Github)],
            mirrors: vec![MirrorConfig {
                name: "broken".to_string(),
                endpoints: vec![
                    EndpointConfig {
                        site: "github".to_string(),
                        kind: NamespaceKind::User,
                        namespace: "alice".to_string(),
                    },
                    EndpointConfig {
                        site: "missing".to_string(),
                        kind: NamespaceKind::User,
                        namespace: "alice".to_string(),
                    },
                ],
                create_missing: true,
                visibility: Visibility::Private,
                allow_force: false,
            }],
        };
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("unknown site 'missing'"));
    }

    #[test]
    fn removing_referenced_site_is_rejected() {
        let mut config = Config {
            sites: vec![
                site("github", ProviderKind::Github),
                site("gitea", ProviderKind::Gitea),
            ],
            mirrors: vec![MirrorConfig {
                name: "personal".to_string(),
                endpoints: vec![
                    EndpointConfig {
                        site: "github".to_string(),
                        kind: NamespaceKind::User,
                        namespace: "alice".to_string(),
                    },
                    EndpointConfig {
                        site: "gitea".to_string(),
                        kind: NamespaceKind::User,
                        namespace: "alice".to_string(),
                    },
                ],
                create_missing: true,
                visibility: Visibility::Private,
                allow_force: false,
            }],
        };

        let err = config.remove_site("github").unwrap_err().to_string();
        assert!(err.contains("still used by mirror 'personal'"));
        assert!(config.site("github").is_some());
    }

    #[test]
    fn api_base_defaults_match_providers() {
        assert_eq!(
            site("github", ProviderKind::Github).api_base(),
            "https://api.github.com"
        );
        assert_eq!(
            SiteConfig {
                base_url: "https://github.example.test/".to_string(),
                ..site("github-enterprise", ProviderKind::Github)
            }
            .api_base(),
            "https://github.example.test/api/v3"
        );
        assert_eq!(
            SiteConfig {
                base_url: "https://gitlab.example.test".to_string(),
                ..site("gitlab", ProviderKind::Gitlab)
            }
            .api_base(),
            "https://gitlab.example.test/api/v4"
        );
        assert_eq!(
            SiteConfig {
                base_url: "https://gitea.example.test".to_string(),
                ..site("gitea", ProviderKind::Gitea)
            }
            .api_base(),
            "https://gitea.example.test/api/v1"
        );
        assert_eq!(
            SiteConfig {
                base_url: "https://forgejo.example.test".to_string(),
                ..site("forgejo", ProviderKind::Forgejo)
            }
            .api_base(),
            "https://forgejo.example.test/api/v1"
        );
    }

    fn site(name: &str, provider: ProviderKind) -> SiteConfig {
        SiteConfig {
            name: name.to_string(),
            provider,
            base_url: "https://github.com".to_string(),
            api_url: None,
            token: TokenConfig::Value("token".to_string()),
            git_username: None,
        }
    }
}
