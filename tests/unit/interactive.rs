use super::*;
use std::io::Cursor;

#[test]
fn wizard_builds_sync_group_from_profile_urls() {
    let input = [
        "https://github.com/hykilpikonna",
        "gh-token",
        "",
        "https://gitea.example.test/azalea",
        "gt-token",
        "",
        "n",
        "",
        "",
        "",
        "",
        "",
        "n",
        "4",
    ]
    .join("\n")
        + "\n";
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    let config = run_config_wizard_with_io(Config::default(), &mut reader, &mut output).unwrap();

    assert_eq!(config.sites.len(), 2);
    assert_eq!(config.sites[0].name, "github");
    assert_eq!(config.sites[0].provider, ProviderKind::Github);
    assert_eq!(config.sites[0].base_url, "https://github.com");
    assert_eq!(
        config.sites[0].token,
        TokenConfig::Value("gh-token".to_string())
    );
    assert_eq!(config.sites[1].name, "gitea-example-test");
    assert_eq!(config.sites[1].provider, ProviderKind::Gitea);
    assert_eq!(config.sites[1].base_url, "https://gitea.example.test");

    assert_eq!(config.mirrors.len(), 1);
    assert_eq!(config.mirrors[0].name, "sync-1");
    assert_eq!(config.mirrors[0].endpoints.len(), 2);
    assert_eq!(config.mirrors[0].endpoints[0].site, "github");
    assert_eq!(config.mirrors[0].endpoints[0].kind, NamespaceKind::User);
    assert_eq!(config.mirrors[0].endpoints[0].namespace, "hykilpikonna");
    assert_eq!(config.mirrors[0].endpoints[1].site, "gitea-example-test");
    assert_eq!(config.mirrors[0].endpoints[1].namespace, "azalea");
    assert_eq!(config.mirrors[0].sync_visibility, SyncVisibility::All);
    assert!(config.mirrors[0].create_missing);
    assert!(config.mirrors[0].delete_missing);
    assert_eq!(config.mirrors[0].visibility, Visibility::Private);
    assert_eq!(
        config.mirrors[0].conflict_resolution,
        ConflictResolutionStrategy::AutoRebasePullRequest
    );

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("1. github.com/hykilpikonna <-> gitea.example.test/azalea"));
    assert!(output.contains("Deletion backups: refray keeps a local backup"));
    assert!(output.contains("Create repositories that are missing from an endpoint?"));
    assert!(output.contains("delete it everywhere?"));
    assert!(output.contains("Add another sync group"));
    assert!(output.contains("Edit an existing group"));
    assert!(output.contains("Delete an existing group"));
    assert!(output.contains("Done"));
}

#[test]
fn wizard_can_build_three_way_sync() {
    let input = [
        "https://github.com/alice",
        "gh-token",
        "",
        "https://gitlab.com/alice",
        "gl-token",
        "",
        "y",
        "https://gitea.example.test/alice",
        "gt-token",
        "",
        "n",
        "",
        "",
        "",
        "",
        "",
        "n",
        "4",
    ]
    .join("\n")
        + "\n";
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    let config = run_config_wizard_with_io(Config::default(), &mut reader, &mut output).unwrap();

    assert_eq!(config.mirrors.len(), 1);
    assert_eq!(config.mirrors[0].endpoints.len(), 3);
    assert_eq!(config.sites.len(), 3);
}

#[test]
fn wizard_can_disable_missing_repo_creation_and_repo_delete_propagation() {
    let input = [
        "https://github.com/alice",
        "gh-token",
        "",
        "https://gitea.example.test/alice",
        "gt-token",
        "",
        "n",
        "",
        "",
        "n",
        "n",
        "",
        "n",
        "4",
    ]
    .join("\n")
        + "\n";
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    let config = run_config_wizard_with_io(Config::default(), &mut reader, &mut output).unwrap();

    assert!(!config.mirrors[0].create_missing);
    assert!(!config.mirrors[0].delete_missing);
}

#[test]
fn wizard_can_enable_webhooks() {
    let input = [
        "https://github.com/alice",
        "gh-token",
        "",
        "https://gitea.example.test/alice",
        "gt-token",
        "",
        "n",
        "",
        "",
        "",
        "",
        "",
        "y",
        "https://mirror.example.test/webhook",
        "y",
        "30",
        "4",
    ]
    .join("\n")
        + "\n";
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    let config = run_config_wizard_with_io(Config::default(), &mut reader, &mut output).unwrap();

    let webhook = config.webhook.unwrap();
    assert!(webhook.install);
    assert_eq!(webhook.url, "https://mirror.example.test/webhook");
    assert_eq!(webhook.full_sync_interval_minutes, Some(30));
    assert_eq!(webhook.reachability_check_interval_minutes, Some(15));
    assert_eq!(
        webhook.secret,
        TokenConfig::Value("test-webhook-secret".to_string())
    );

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("refray serve --listen 127.0.0.1:8787"));
    assert!(output.contains("cloudflared tunnel --url http://127.0.0.1:8787"));
    assert!(output.contains("POST / and POST /webhook"));
    assert!(output.contains("temporary listener on 127.0.0.1:8787"));
}

#[test]
fn wizard_reuses_existing_credentials_for_same_instance() {
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![SiteConfig {
            name: "github".to_string(),
            provider: ProviderKind::Github,
            base_url: "https://github.com".to_string(),
            api_url: None,
            token: TokenConfig::Value("existing".to_string()),
            git_username: None,
        }],
        mirrors: Vec::new(),
        webhook: None,
    };
    let input = [
        "https://github.com/alice",
        "",
        "https://github.com/bob",
        "",
        "n",
        "",
        "",
        "",
        "",
        "",
        "n",
        "4",
    ]
    .join("\n")
        + "\n";
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    let updated = run_config_wizard_with_io(config, &mut reader, &mut output).unwrap();

    assert_eq!(updated.sites.len(), 1);
    assert_eq!(updated.mirrors[0].endpoints[0].site, "github");
    assert_eq!(updated.mirrors[0].endpoints[1].site, "github");
}

#[test]
fn wizard_starts_existing_config_at_sync_group_menu() {
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![
            SiteConfig {
                name: "github".to_string(),
                provider: ProviderKind::Github,
                base_url: "https://github.com".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing-gh".to_string()),
                git_username: None,
            },
            SiteConfig {
                name: "gitea".to_string(),
                provider: ProviderKind::Gitea,
                base_url: "https://gitea.example.test".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing-gt".to_string()),
                git_username: None,
            },
        ],
        mirrors: vec![MirrorConfig {
            name: "sync-1".to_string(),
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
            sync_visibility: SyncVisibility::All,
            repo_whitelist: None,
            repo_blacklist: None,
            create_missing: true,
            delete_missing: true,
            visibility: Visibility::Private,
            conflict_resolution: ConflictResolutionStrategy::Fail,
            allow_temporary_gitlab_force_push: true,
        }],
        webhook: None,
    };
    let mut reader = Cursor::new(b"4\n".as_slice());
    let mut output = Vec::new();

    let updated = run_config_wizard_with_io(config, &mut reader, &mut output).unwrap();

    assert_eq!(updated.mirrors.len(), 1);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("1. github.com/alice <-> gitea.example.test/alice"));
    assert!(output.contains("What would you like to do?"));
    assert!(!output.contains("Profile/org URL:"));
}

#[test]
fn wizard_can_ask_to_run_full_sync_after_config() {
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: Vec::new(),
        mirrors: vec![MirrorConfig {
            name: "sync-1".to_string(),
            endpoints: Vec::new(),
            sync_visibility: SyncVisibility::All,
            repo_whitelist: None,
            repo_blacklist: None,
            create_missing: true,
            delete_missing: true,
            visibility: Visibility::Private,
            conflict_resolution: ConflictResolutionStrategy::Fail,
            allow_temporary_gitlab_force_push: true,
        }],
        webhook: None,
    };
    let mut reader = Cursor::new(b"y\n".as_slice());
    let mut output = Vec::new();

    let run_full_sync =
        prompt_run_full_sync_now_with_io(&config, &mut reader, &mut output).unwrap();

    assert!(run_full_sync);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Run full sync now? [y/N]:"));
}

#[test]
fn wizard_skips_full_sync_prompt_without_sync_groups() {
    let mut reader = Cursor::new(b"".as_slice());
    let mut output = Vec::new();

    let run_full_sync =
        prompt_run_full_sync_now_with_io(&Config::default(), &mut reader, &mut output).unwrap();

    assert!(!run_full_sync);
    assert!(output.is_empty());
}

#[test]
fn wizard_edits_existing_sync_group_from_menu() {
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![
            SiteConfig {
                name: "github".to_string(),
                provider: ProviderKind::Github,
                base_url: "https://github.com".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing-gh".to_string()),
                git_username: None,
            },
            SiteConfig {
                name: "gitea".to_string(),
                provider: ProviderKind::Gitea,
                base_url: "https://gitea.example.test".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing-gt".to_string()),
                git_username: None,
            },
            SiteConfig {
                name: "gitlab".to_string(),
                provider: ProviderKind::Gitlab,
                base_url: "https://gitlab.com".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing-gl".to_string()),
                git_username: None,
            },
        ],
        mirrors: vec![MirrorConfig {
            name: "sync-1".to_string(),
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
            sync_visibility: SyncVisibility::Private,
            repo_whitelist: Some("^important-".to_string()),
            repo_blacklist: Some("-archive$".to_string()),
            create_missing: false,
            delete_missing: true,
            visibility: Visibility::Public,
            conflict_resolution: ConflictResolutionStrategy::Fail,
            allow_temporary_gitlab_force_push: true,
        }],
        webhook: None,
    };
    let input = [
        "2",
        "1",
        "https://github.com/bob",
        "",
        "https://gitlab.com/bob",
        "",
        "n",
        "public",
        "y",
        "^public-",
        "-skip$",
        "",
        "",
        "",
        "n",
        "4",
    ]
    .join("\n")
        + "\n";
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    let updated = run_config_wizard_with_io(config, &mut reader, &mut output).unwrap();

    assert_eq!(updated.mirrors.len(), 1);
    let mirror = &updated.mirrors[0];
    assert_eq!(mirror.name, "sync-1");
    assert_eq!(mirror.endpoints.len(), 2);
    assert_eq!(mirror.endpoints[0].site, "github");
    assert_eq!(mirror.endpoints[0].namespace, "bob");
    assert_eq!(mirror.endpoints[1].site, "gitlab");
    assert_eq!(mirror.endpoints[1].namespace, "bob");
    assert!(!mirror.create_missing);
    assert!(mirror.delete_missing);
    assert_eq!(mirror.sync_visibility, SyncVisibility::Public);
    assert_eq!(mirror.repo_whitelist, Some("^public-".to_string()));
    assert_eq!(mirror.repo_blacklist, Some("-skip$".to_string()));
    assert_eq!(mirror.visibility, Visibility::Public);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Edit sync group"));
    assert!(output.contains("updated sync group 1"));
    assert!(output.contains("github.com/bob <-> gitlab.com/bob"));
}

#[test]
fn wizard_prefills_existing_sync_group_when_editing() {
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![
            SiteConfig {
                name: "github".to_string(),
                provider: ProviderKind::Github,
                base_url: "https://github.com".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing-gh".to_string()),
                git_username: None,
            },
            SiteConfig {
                name: "gitea".to_string(),
                provider: ProviderKind::Gitea,
                base_url: "https://gitea.example.test".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing-gt".to_string()),
                git_username: None,
            },
        ],
        mirrors: vec![MirrorConfig {
            name: "sync-1".to_string(),
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
            sync_visibility: SyncVisibility::All,
            repo_whitelist: None,
            repo_blacklist: None,
            create_missing: true,
            delete_missing: true,
            visibility: Visibility::Private,
            conflict_resolution: ConflictResolutionStrategy::Fail,
            allow_temporary_gitlab_force_push: true,
        }],
        webhook: None,
    };
    let input = ["2", "1", "", "", "", "", "n", "", "", "", "", "", "n", "4"].join("\n") + "\n";
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    let updated = run_config_wizard_with_io(config, &mut reader, &mut output).unwrap();

    let mirror = &updated.mirrors[0];
    assert_eq!(mirror.endpoints.len(), 2);
    assert_eq!(mirror.endpoints[0].site, "github");
    assert_eq!(mirror.endpoints[0].namespace, "alice");
    assert_eq!(mirror.endpoints[1].site, "gitea");
    assert_eq!(mirror.endpoints[1].namespace, "alice");

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Profile/org URL [https://github.com/alice]:"));
    assert!(output.contains("Profile/org URL to sync with [https://gitea.example.test/alice]:"));
    assert!(output.contains("updated sync group 1"));
}

#[test]
fn wizard_deletes_existing_sync_group_from_menu() {
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![
            SiteConfig {
                name: "github".to_string(),
                provider: ProviderKind::Github,
                base_url: "https://github.com".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing-gh".to_string()),
                git_username: None,
            },
            SiteConfig {
                name: "gitea".to_string(),
                provider: ProviderKind::Gitea,
                base_url: "https://gitea.example.test".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing-gt".to_string()),
                git_username: None,
            },
        ],
        mirrors: vec![MirrorConfig {
            name: "sync-1".to_string(),
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
            sync_visibility: SyncVisibility::All,
            repo_whitelist: None,
            repo_blacklist: None,
            create_missing: true,
            delete_missing: true,
            visibility: Visibility::Private,
            conflict_resolution: ConflictResolutionStrategy::Fail,
            allow_temporary_gitlab_force_push: true,
        }],
        webhook: None,
    };
    let input = ["3", "1", "4"].join("\n") + "\n";
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    let updated = run_config_wizard_with_io(config, &mut reader, &mut output).unwrap();

    assert!(updated.mirrors.is_empty());
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Delete sync group"));
    assert!(output.contains("2. Back"));
    assert!(output.contains("deleted sync group 1"));
    assert!(output.contains("No sync groups configured."));
}

#[test]
fn wizard_can_go_back_from_delete_menu() {
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![
            SiteConfig {
                name: "github".to_string(),
                provider: ProviderKind::Github,
                base_url: "https://github.com".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing-gh".to_string()),
                git_username: None,
            },
            SiteConfig {
                name: "gitea".to_string(),
                provider: ProviderKind::Gitea,
                base_url: "https://gitea.example.test".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing-gt".to_string()),
                git_username: None,
            },
        ],
        mirrors: vec![MirrorConfig {
            name: "sync-1".to_string(),
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
            sync_visibility: SyncVisibility::All,
            repo_whitelist: None,
            repo_blacklist: None,
            create_missing: true,
            delete_missing: true,
            visibility: Visibility::Private,
            conflict_resolution: ConflictResolutionStrategy::Fail,
            allow_temporary_gitlab_force_push: true,
        }],
        webhook: None,
    };
    let input = ["3", "2", "4"].join("\n") + "\n";
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    let updated = run_config_wizard_with_io(config, &mut reader, &mut output).unwrap();

    assert_eq!(updated.mirrors.len(), 1);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("2. Back"));
    assert!(!output.contains("deleted sync group"));
}

#[test]
fn wizard_reports_eof_instead_of_looping() {
    let mut reader = Cursor::new(b"".as_slice());
    let mut output = Vec::new();

    let err = run_config_wizard_with_io(Config::default(), &mut reader, &mut output)
        .unwrap_err()
        .to_string();

    assert!(err.contains("unexpected end of input"));
}

#[test]
fn profile_urls_are_parsed_into_base_and_namespace() {
    let parsed = parse_profile_url("github.com/alice").unwrap();
    assert_eq!(parsed.base_url, "https://github.com");
    assert_eq!(parsed.host, "github.com");
    assert_eq!(parsed.namespace, "alice");

    let parsed = parse_profile_url("https://gitlab.example.test:8443/groups/team").unwrap();
    assert_eq!(parsed.base_url, "https://gitlab.example.test:8443");
    assert_eq!(parsed.namespace, "groups/team");
}

#[test]
fn site_names_are_derived_from_urls_and_made_unique() {
    let mut config = Config::default();
    assert_eq!(
        default_site_name(&config, "https://github.com", &ProviderKind::Github),
        "github"
    );
    assert_eq!(
        default_site_name(
            &config,
            "https://git.my-company.com:3000",
            &ProviderKind::Gitea
        ),
        "git-my-company"
    );

    config.upsert_site(SiteConfig {
        name: "github".to_string(),
        provider: ProviderKind::Github,
        base_url: "https://github.com".to_string(),
        api_url: None,
        token: TokenConfig::Value("token".to_string()),
        git_username: None,
    });
    assert_eq!(
        default_site_name(&config, "https://github.com", &ProviderKind::Github),
        "github-2"
    );
}

#[test]
fn token_creation_urls_are_provider_specific() {
    assert_eq!(
        token_creation_url(&ProviderKind::Github, "https://github.com/"),
        "https://github.com/settings/tokens"
    );
    assert_eq!(
        token_creation_url(&ProviderKind::Gitlab, "https://gitlab.example.test"),
        "https://gitlab.example.test/-/user_settings/personal_access_tokens?name=refray&scopes=api,write_repository"
    );
    assert_eq!(
        token_creation_url(&ProviderKind::Gitea, "gitea.example.test"),
        "https://gitea.example.test/user/settings/applications"
    );
    assert_eq!(
        token_creation_url(&ProviderKind::Forgejo, "forgejo.example.test"),
        "https://forgejo.example.test/user/settings/applications"
    );
}

#[test]
fn demo_webhook_server_answers_reachability_checks() {
    let server = DemoWebhookServer::start("127.0.0.1:0").unwrap();
    let response = reqwest::blocking::get(format!("http://{}/webhook", server.listen())).unwrap();

    assert!(response.status().is_success());
    assert!(
        response
            .text()
            .unwrap()
            .contains("refray webhook setup listener")
    );
}
