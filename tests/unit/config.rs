use super::*;

#[test]
fn parses_token_forms() {
    let config: Config = toml::from_str(
        r#"
        jobs = 8

        [webhook]
        install = true
        url = "https://mirror.example.test/webhook"
        secret = { env = "WEBHOOK_SECRET" }
        full_sync_interval_minutes = 60
        reachability_check_interval_minutes = 15

        [[sites]]
        name = "github"
        provider = "github"
        base_url = "https://github.com"
        token = { env = "GITHUB_TOKEN" }

        [[mirrors]]
        name = "personal"
        sync_visibility = "public"
        repo_whitelist = ["^important-", "-mirror$"]
        repo_blacklist = ["-archive$"]
        create_missing = true
        visibility = "private"
        conflict_resolution = "auto_rebase_pull_request"

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

    assert_eq!(config.jobs, 8);
    assert_eq!(config.sites.len(), 1);
    assert_eq!(config.mirrors[0].endpoints.len(), 2);
    assert_eq!(
        config.mirrors[0].conflict_resolution,
        ConflictResolutionStrategy::AutoRebasePullRequest
    );
    assert_eq!(config.mirrors[0].sync_visibility, SyncVisibility::Public);
    assert_eq!(
        config.mirrors[0].repo_whitelist,
        vec!["^important-".to_string(), "-mirror$".to_string()]
    );
    assert_eq!(
        config.mirrors[0].repo_blacklist,
        vec!["-archive$".to_string()]
    );
    let webhook = config.webhook.unwrap();
    assert!(webhook.install);
    assert_eq!(webhook.url, "https://mirror.example.test/webhook");
    assert_eq!(
        webhook.secret,
        TokenConfig::Env("WEBHOOK_SECRET".to_string())
    );
    assert_eq!(webhook.full_sync_interval_minutes, Some(60));
}

#[test]
fn config_defaults_jobs() {
    let config: Config = toml::from_str("").unwrap();

    assert_eq!(config.jobs, DEFAULT_JOBS);
}

#[test]
fn validation_rejects_unknown_sites_and_single_endpoint_groups() {
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![site("github", ProviderKind::Github)],
        mirrors: vec![MirrorConfig {
            name: "broken".to_string(),
            endpoints: vec![EndpointConfig {
                site: "github".to_string(),
                kind: NamespaceKind::User,
                namespace: "alice".to_string(),
            }],
            sync_visibility: SyncVisibility::All,
            repo_whitelist: Vec::new(),
            repo_blacklist: Vec::new(),
            create_missing: true,
            visibility: Visibility::Private,
            conflict_resolution: ConflictResolutionStrategy::Fail,
        }],
        webhook: None,
    };
    let err = validate_config(&config).unwrap_err().to_string();
    assert!(err.contains("at least two endpoints"));

    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
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
            sync_visibility: SyncVisibility::All,
            repo_whitelist: Vec::new(),
            repo_blacklist: Vec::new(),
            create_missing: true,
            visibility: Visibility::Private,
            conflict_resolution: ConflictResolutionStrategy::Fail,
        }],
        webhook: None,
    };
    let err = validate_config(&config).unwrap_err().to_string();
    assert!(err.contains("unknown site 'missing'"));
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

#[test]
fn sync_visibility_matches_repo_privacy() {
    assert!(SyncVisibility::All.matches_private(true));
    assert!(SyncVisibility::All.matches_private(false));
    assert!(SyncVisibility::Private.matches_private(true));
    assert!(!SyncVisibility::Private.matches_private(false));
    assert!(!SyncVisibility::Public.matches_private(true));
    assert!(SyncVisibility::Public.matches_private(false));
}

#[test]
fn repo_name_filter_applies_whitelist_then_blacklist() {
    let mut mirror = mirror_config();
    mirror.repo_whitelist = vec!["^important-".to_string(), "-mirror$".to_string()];
    mirror.repo_blacklist = vec!["-archive$".to_string()];
    let filter = mirror.repo_filter().unwrap();

    assert!(filter.matches("important-api"));
    assert!(filter.matches("user-mirror"));
    assert!(!filter.matches("important-archive"));
    assert!(!filter.matches("random"));
}

#[test]
fn validation_rejects_invalid_repo_filter_regex() {
    let mut config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![site("github", ProviderKind::Github)],
        mirrors: vec![mirror_config()],
        webhook: None,
    };
    config.mirrors[0].repo_whitelist = vec!["(".to_string()];

    let err = validate_config(&config).unwrap_err().to_string();

    assert!(err.contains("invalid repo_whitelist regex"));
}

#[test]
fn validation_rejects_zero_jobs() {
    let mut config = Config {
        jobs: 0,
        sites: vec![site("github", ProviderKind::Github)],
        mirrors: vec![mirror_config()],
        webhook: None,
    };

    let err = validate_config(&config).unwrap_err().to_string();

    assert!(err.contains("jobs must be at least 1"));
    config.jobs = DEFAULT_JOBS;
    validate_config(&config).unwrap();
}

fn mirror_config() -> MirrorConfig {
    MirrorConfig {
        name: "personal".to_string(),
        endpoints: vec![
            EndpointConfig {
                site: "github".to_string(),
                kind: NamespaceKind::User,
                namespace: "alice".to_string(),
            },
            EndpointConfig {
                site: "github".to_string(),
                kind: NamespaceKind::Org,
                namespace: "example".to_string(),
            },
        ],
        sync_visibility: SyncVisibility::All,
        repo_whitelist: Vec::new(),
        repo_blacklist: Vec::new(),
        create_missing: true,
        visibility: Visibility::Private,
        conflict_resolution: ConflictResolutionStrategy::Fail,
    }
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
