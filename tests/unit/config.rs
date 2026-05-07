use super::*;

#[test]
fn parses_token_forms() {
    let config: Config = toml::from_str(
        r#"
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
        webhook: None,
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
