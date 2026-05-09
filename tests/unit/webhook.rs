use super::*;
use crate::config::SyncVisibility;
use crate::config::{
    ConflictResolutionStrategy, EndpointConfig, MirrorConfig, NamespaceKind, SiteConfig,
    TokenConfig, Visibility, WebhookConfig,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

#[test]
fn verifies_github_hmac_signature() {
    let body = br#"{"repository":{"name":"repo"}}"#;
    let mut headers = HashMap::new();
    headers.insert("x-github-event".to_string(), "push".to_string());
    headers.insert(
        "x-hub-signature-256".to_string(),
        format!("sha256={}", hmac_sha256_hex(b"secret", body)),
    );

    assert!(verify_signature(
        Some(&ProviderKind::Github),
        &headers,
        body,
        "secret"
    ));
    assert!(!verify_signature(
        Some(&ProviderKind::Github),
        &headers,
        body,
        "wrong"
    ));
}

#[test]
fn parses_github_push_payload() {
    let mut headers = HashMap::new();
    headers.insert("x-github-event".to_string(), "push".to_string());
    let value: Value = serde_json::from_str(
        r#"{"repository":{"name":"repo","full_name":"alice/repo","owner":{"login":"alice"}}}"#,
    )
    .unwrap();

    let event = parse_event(Some(ProviderKind::Github), &headers, &value).unwrap();

    assert_eq!(event.repo, "repo");
    assert_eq!(event.namespace.as_deref(), Some("alice"));
}

#[test]
fn parses_forgejo_push_payload() {
    let mut headers = HashMap::new();
    headers.insert("x-forgejo-event".to_string(), "push".to_string());
    let value: Value = serde_json::from_str(
        r#"{"repository":{"name":"repo","full_name":"azalea/repo","owner":{"username":"azalea"}}}"#,
    )
    .unwrap();

    let provider = detect_provider(&headers);
    let event = parse_event(provider.clone(), &headers, &value).unwrap();

    assert_eq!(provider, Some(ProviderKind::Forgejo));
    assert_eq!(event.repo, "repo");
    assert_eq!(event.namespace.as_deref(), Some("azalea"));
}

#[test]
fn verifies_forgejo_hmac_signature() {
    let body = br#"{"repository":{"name":"repo"}}"#;
    let mut headers = HashMap::new();
    headers.insert("x-forgejo-event".to_string(), "push".to_string());
    headers.insert(
        "x-forgejo-signature".to_string(),
        format!("sha256={}", hmac_sha256_hex(b"secret", body)),
    );

    assert!(verify_signature(
        Some(&ProviderKind::Forgejo),
        &headers,
        body,
        "secret"
    ));
}

#[test]
fn parses_gitlab_push_payload() {
    let mut headers = HashMap::new();
    headers.insert("x-gitlab-event".to_string(), "Push Hook".to_string());
    let value: Value = serde_json::from_str(
        r#"{"project":{"path":"repo","path_with_namespace":"parent/alice/repo"}}"#,
    )
    .unwrap();

    let event = parse_event(Some(ProviderKind::Gitlab), &headers, &value).unwrap();

    assert_eq!(event.repo, "repo");
    assert_eq!(event.namespace.as_deref(), Some("parent/alice"));
}

#[test]
fn matches_jobs_by_provider_and_namespace() {
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![
            site("github", ProviderKind::Github),
            site("gitea", ProviderKind::Gitea),
        ],
        mirrors: vec![MirrorConfig {
            name: "sync-1".to_string(),
            endpoints: vec![
                endpoint("github", NamespaceKind::User, "alice"),
                endpoint("gitea", NamespaceKind::User, "azalea"),
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
    let event = WebhookEvent {
        provider: Some(ProviderKind::Github),
        repo: "repo".to_string(),
        namespace: Some("alice".to_string()),
    };

    let jobs = matching_jobs(&config, &event);

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].group, "sync-1");
    assert_eq!(jobs[0].repo, "repo");
}

#[test]
fn matching_jobs_respects_repo_name_filters() {
    let mut mirror = MirrorConfig {
        name: "sync-1".to_string(),
        endpoints: vec![endpoint("github", NamespaceKind::User, "alice")],
        sync_visibility: SyncVisibility::All,
        repo_whitelist: vec!["^important-".to_string()],
        repo_blacklist: vec!["-archive$".to_string()],
        create_missing: true,
        visibility: Visibility::Private,
        conflict_resolution: ConflictResolutionStrategy::Fail,
    };
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![site("github", ProviderKind::Github)],
        mirrors: vec![mirror.clone()],
        webhook: None,
    };

    assert_eq!(
        matching_jobs(&config, &webhook_event("important-api")).len(),
        1
    );
    assert!(matching_jobs(&config, &webhook_event("important-archive")).is_empty());
    assert!(matching_jobs(&config, &webhook_event("random")).is_empty());

    mirror.repo_whitelist.clear();
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![site("github", ProviderKind::Github)],
        mirrors: vec![mirror],
        webhook: None,
    };
    assert_eq!(matching_jobs(&config, &webhook_event("random")).len(), 1);
}

#[test]
fn install_webhooks_respects_visibility_and_repo_name_filters() {
    let repos = r#"[
        {"name":"important-api","clone_url":"https://github.com/alice/important-api.git","private":false,"description":null,"owner":{"login":"alice"}},
        {"name":"important-private","clone_url":"https://github.com/alice/important-private.git","private":true,"description":null,"owner":{"login":"alice"}},
        {"name":"important-archive","clone_url":"https://github.com/alice/important-archive.git","private":false,"description":null,"owner":{"login":"alice"}},
        {"name":"random","clone_url":"https://github.com/alice/random.git","private":false,"description":null,"owner":{"login":"alice"}}
    ]"#;
    let (api_url, handle) = request_server(
        vec![
            ("200 OK", repos),
            ("200 OK", "[]"),
            ("200 OK", "[]"),
            ("201 Created", r#"{"id":1}"#),
        ],
        |index, request| match index {
            0 => assert!(
                request
                    .starts_with("GET /user/repos?affiliation=owner&visibility=all&per_page=100 "),
                "request was {request}"
            ),
            1 => assert!(
                request
                    .starts_with("GET /user/repos?affiliation=owner&visibility=all&per_page=100 "),
                "request was {request}"
            ),
            2 => assert!(
                request.starts_with("GET /repos/alice/important-api/hooks "),
                "request was {request}"
            ),
            3 => assert!(
                request.starts_with("POST /repos/alice/important-api/hooks "),
                "request was {request}"
            ),
            _ => unreachable!(),
        },
    );
    let temp = tempfile::TempDir::new().unwrap();
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![
            SiteConfig {
                api_url: Some(api_url.clone()),
                ..site("github", ProviderKind::Github)
            },
            SiteConfig {
                api_url: Some(api_url),
                ..site("github-peer", ProviderKind::Github)
            },
        ],
        mirrors: vec![filtered_mirror()],
        webhook: None,
    };

    install_webhooks(
        &config,
        WebhookInstallOptions {
            url: "https://mirror.example.test/webhook".to_string(),
            secret: "secret".to_string(),
            dry_run: false,
            work_dir: Some(temp.path().to_path_buf()),
            jobs: 1,
        },
    )
    .unwrap();

    handle.join().unwrap();
}

#[test]
fn uninstall_webhooks_respects_visibility_and_repo_name_filters() {
    let repos = r#"[
        {"name":"important-api","clone_url":"https://github.com/alice/important-api.git","private":false,"description":null,"owner":{"login":"alice"}},
        {"name":"important-private","clone_url":"https://github.com/alice/important-private.git","private":true,"description":null,"owner":{"login":"alice"}},
        {"name":"important-archive","clone_url":"https://github.com/alice/important-archive.git","private":false,"description":null,"owner":{"login":"alice"}},
        {"name":"random","clone_url":"https://github.com/alice/random.git","private":false,"description":null,"owner":{"login":"alice"}}
    ]"#;
    let hooks = r#"[{"id":42,"url":"https://api.github.com/repos/alice/important-api/hooks/42","config":{"url":"https://mirror.example.test/webhook"}}]"#;
    let (api_url, handle) = request_server(
        vec![
            ("200 OK", repos),
            ("200 OK", repos),
            ("200 OK", hooks),
            ("204 No Content", ""),
        ],
        |index, request| match index {
            0 => assert!(
                request
                    .starts_with("GET /user/repos?affiliation=owner&visibility=all&per_page=100 "),
                "request was {request}"
            ),
            1 => assert!(
                request
                    .starts_with("GET /user/repos?affiliation=owner&visibility=all&per_page=100 "),
                "request was {request}"
            ),
            2 => assert!(
                request.starts_with("GET /repos/alice/important-api/hooks "),
                "request was {request}"
            ),
            3 => assert!(
                request.starts_with("DELETE /repos/alice/important-api/hooks/42 "),
                "request was {request}"
            ),
            _ => unreachable!(),
        },
    );
    let temp = tempfile::TempDir::new().unwrap();
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![
            SiteConfig {
                api_url: Some(api_url.clone()),
                ..site("github", ProviderKind::Github)
            },
            SiteConfig {
                api_url: Some(api_url),
                ..site("github-peer", ProviderKind::Github)
            },
        ],
        mirrors: vec![filtered_mirror()],
        webhook: None,
    };

    uninstall_webhooks(
        &config,
        WebhookUninstallOptions {
            url: "https://mirror.example.test/webhook".to_string(),
            dry_run: false,
            work_dir: Some(temp.path().to_path_buf()),
            jobs: 1,
        },
    )
    .unwrap();

    handle.join().unwrap();
}

#[test]
fn uninstall_webhooks_skips_blocked_provider_access() {
    let repos = r#"[
        {"name":"BiliExp-Task","clone_url":"https://github.com/alice/BiliExp-Task.git","private":false,"description":null,"owner":{"login":"alice"}}
    ]"#;
    let blocked = r#"{"message":"Repository access blocked","block":{"reason":"sensitive_data","created_at":"2021-05-18T16:29:54Z","html_url":"https://github.com/tos"}}"#;
    let (api_url, handle) = request_server(
        vec![
            ("200 OK", repos),
            ("200 OK", repos),
            ("403 Forbidden", blocked),
        ],
        |index, request| match index {
            0 => assert!(
                request
                    .starts_with("GET /user/repos?affiliation=owner&visibility=all&per_page=100 "),
                "request was {request}"
            ),
            1 => assert!(
                request
                    .starts_with("GET /user/repos?affiliation=owner&visibility=all&per_page=100 "),
                "request was {request}"
            ),
            2 => assert!(
                request.starts_with("GET /repos/alice/BiliExp-Task/hooks "),
                "request was {request}"
            ),
            _ => unreachable!(),
        },
    );
    let temp = tempfile::TempDir::new().unwrap();
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![
            SiteConfig {
                api_url: Some(api_url.clone()),
                ..site("github", ProviderKind::Github)
            },
            SiteConfig {
                api_url: Some(api_url),
                ..site("github-peer", ProviderKind::Github)
            },
        ],
        mirrors: vec![MirrorConfig {
            name: "sync-1".to_string(),
            endpoints: vec![
                endpoint("github", NamespaceKind::User, "alice"),
                endpoint("github-peer", NamespaceKind::User, "bob"),
            ],
            sync_visibility: SyncVisibility::Public,
            repo_whitelist: Vec::new(),
            repo_blacklist: Vec::new(),
            create_missing: true,
            visibility: Visibility::Private,
            conflict_resolution: ConflictResolutionStrategy::Fail,
        }],
        webhook: None,
    };

    uninstall_webhooks(
        &config,
        WebhookUninstallOptions {
            url: "https://mirror.example.test/webhook".to_string(),
            dry_run: false,
            work_dir: Some(temp.path().to_path_buf()),
            jobs: 1,
        },
    )
    .unwrap();

    handle.join().unwrap();
}

#[test]
fn configured_webhook_install_respects_visibility_and_repo_name_filters() {
    let (api_url, handle) = request_server(
        vec![("200 OK", "[]"), ("201 Created", r#"{"id":1}"#)],
        |index, request| match index {
            0 => assert!(
                request.starts_with("GET /repos/alice/important-api/hooks "),
                "request was {request}"
            ),
            1 => assert!(
                request.starts_with("POST /repos/alice/important-api/hooks "),
                "request was {request}"
            ),
            _ => unreachable!(),
        },
    );
    let temp = tempfile::TempDir::new().unwrap();
    let mirror = filtered_mirror();
    let endpoint = mirror.endpoints[0].clone();
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        sites: vec![
            SiteConfig {
                api_url: Some(api_url),
                ..site("github", ProviderKind::Github)
            },
            site("github-peer", ProviderKind::Github),
        ],
        mirrors: vec![mirror.clone()],
        webhook: Some(WebhookConfig {
            install: true,
            url: "https://mirror.example.test/webhook".to_string(),
            secret: TokenConfig::Value("secret".to_string()),
            full_sync_interval_minutes: None,
            reachability_check_interval_minutes: None,
        }),
    };
    let repos = vec![
        endpoint_repo(&endpoint, "important-api", false),
        endpoint_repo(&endpoint, "important-private", true),
        endpoint_repo(&endpoint, "important-archive", false),
        endpoint_repo(&endpoint, "random", false),
    ];

    ensure_configured_webhooks(&config, &mirror, &repos, temp.path(), 1).unwrap();

    handle.join().unwrap();
}

#[test]
fn webhook_state_persists_installations() {
    let temp = tempfile::TempDir::new().unwrap();
    let endpoint = endpoint("github", NamespaceKind::User, "alice");
    let key = webhook_installation_key("sync-1", &endpoint, "repo");
    let mut state = WebhookState::default();
    state.installations.insert(
        key.clone(),
        WebhookInstallation {
            group: "sync-1".to_string(),
            endpoint,
            repo: "repo".to_string(),
            url: "https://mirror.example.test/webhook".to_string(),
        },
    );

    save_webhook_state(temp.path(), &state).unwrap();
    let loaded = load_webhook_state(temp.path()).unwrap();

    assert_eq!(loaded.installations[&key].repo, "repo");
    assert_eq!(
        loaded.installations[&key].url,
        "https://mirror.example.test/webhook"
    );
}

#[test]
fn blocked_webhook_install_is_skipped_and_recorded() {
    let (api_url, handle) = one_request_server(
        "403 Forbidden",
        r#"{"message":"Repository access blocked","block":{"html_url":"https://github.com/tos"}}"#,
        |request| assert!(request.starts_with("GET /repos/alice/repo/hooks ")),
    );
    let site = SiteConfig {
        api_url: Some(api_url),
        ..site("github", ProviderKind::Github)
    };
    let state = Arc::new(Mutex::new(WebhookState::default()));

    install_webhook_task(
        WebhookInstallTask {
            site,
            group: "sync-1".to_string(),
            endpoint: endpoint("github", NamespaceKind::User, "alice"),
            repo: RemoteRepo {
                name: "repo".to_string(),
                clone_url: "https://github.com/alice/repo.git".to_string(),
                private: true,
                description: None,
            },
            url: "https://mirror.example.test/webhook".to_string(),
            secret: "secret".to_string(),
            dry_run: false,
        },
        &state,
    )
    .unwrap();

    let state = state.lock().unwrap();
    assert!(state.installations.is_empty());
    assert_eq!(state.skipped.len(), 1);
    assert_eq!(
        state.skipped.values().next().unwrap().reason,
        "provider blocked access"
    );
    handle.join().unwrap();
}

#[test]
fn archived_webhook_failure_is_non_actionable() {
    let error = anyhow::anyhow!(
        "POST https://api.github.com/repos/acme/repo/hooks returned 403 Forbidden: Repository was archived so is read-only."
    );

    assert_eq!(
        non_actionable_webhook_failure_reason(&error).as_deref(),
        Some("repository is archived/read-only")
    );
}

#[test]
fn duplicate_webhook_error_records_existing_installation() {
    let error = anyhow::anyhow!(
        "{}",
        r#"POST https://api.github.com/repos/alice/repo/hooks returned 422 Unprocessable Entity: {"message":"Validation Failed","errors":[{"resource":"Hook","code":"custom","message":"Hook already exists on this repository"}]}"#
    );
    assert!(is_duplicate_webhook_error(&error));

    let state = Arc::new(Mutex::new(WebhookState::default()));
    let task = WebhookInstallTask {
        site: site("github", ProviderKind::Github),
        group: "sync-1".to_string(),
        endpoint: endpoint("github", NamespaceKind::User, "alice"),
        repo: RemoteRepo {
            name: "repo".to_string(),
            clone_url: "https://github.com/alice/repo.git".to_string(),
            private: true,
            description: None,
        },
        url: "https://mirror.example.test/webhook".to_string(),
        secret: "secret".to_string(),
        dry_run: false,
    };
    let key = webhook_installation_key(&task.group, &task.endpoint, &task.repo.name);

    record_webhook_installation(&state, key.clone(), task);

    let state = state.lock().unwrap();
    assert!(state.skipped.is_empty());
    assert_eq!(state.installations[&key].repo, "repo");
}

#[test]
fn install_task_rechecks_cached_installation() {
    let (api_url, handle) = request_server(
        vec![("200 OK", "[]"), ("201 Created", r#"{"id":1}"#)],
        |index, request| match index {
            0 => assert!(request.starts_with("GET /repos/alice/repo/hooks ")),
            1 => assert!(request.starts_with("POST /repos/alice/repo/hooks ")),
            _ => unreachable!(),
        },
    );
    let endpoint = endpoint("github", NamespaceKind::User, "alice");
    let key = webhook_installation_key("sync-1", &endpoint, "repo");
    let state = Arc::new(Mutex::new(WebhookState::default()));
    state.lock().unwrap().installations.insert(
        key,
        WebhookInstallation {
            group: "sync-1".to_string(),
            endpoint: endpoint.clone(),
            repo: "repo".to_string(),
            url: "https://mirror.example.test/webhook".to_string(),
        },
    );

    install_webhook_task(
        WebhookInstallTask {
            site: SiteConfig {
                api_url: Some(api_url),
                ..site("github", ProviderKind::Github)
            },
            group: "sync-1".to_string(),
            endpoint,
            repo: RemoteRepo {
                name: "repo".to_string(),
                clone_url: "https://github.com/alice/repo.git".to_string(),
                private: true,
                description: None,
            },
            url: "https://mirror.example.test/webhook".to_string(),
            secret: "secret".to_string(),
            dry_run: false,
        },
        &state,
    )
    .unwrap();

    handle.join().unwrap();
}

#[test]
fn uninstall_task_keeps_state_when_hook_is_missing() {
    let (api_url, handle) = one_request_server("200 OK", "[]", |request| {
        assert!(request.starts_with("GET /repos/alice/repo/hooks "))
    });

    let key = uninstall_webhook_task(WebhookUninstallTask {
        group: "sync-1".to_string(),
        site: SiteConfig {
            api_url: Some(api_url),
            ..site("github", ProviderKind::Github)
        },
        endpoint: endpoint("github", NamespaceKind::User, "alice"),
        repo: RemoteRepo {
            name: "repo".to_string(),
            clone_url: "https://github.com/alice/repo.git".to_string(),
            private: true,
            description: None,
        },
        url: "https://mirror.example.test/webhook".to_string(),
        dry_run: false,
    })
    .unwrap();

    assert_eq!(key, None);
    handle.join().unwrap();
}

#[test]
fn blocked_webhook_uninstall_is_skipped() {
    let (api_url, handle) = one_request_server(
        "403 Forbidden",
        r#"{"message":"Repository access blocked","block":{"reason":"sensitive_data","created_at":"2021-05-18T16:29:54Z","html_url":"https://github.com/tos"}}"#,
        |request| assert!(request.starts_with("GET /repos/alice/repo/hooks ")),
    );

    let key = uninstall_webhook_task(WebhookUninstallTask {
        group: "sync-1".to_string(),
        site: SiteConfig {
            api_url: Some(api_url),
            ..site("github", ProviderKind::Github)
        },
        endpoint: endpoint("github", NamespaceKind::User, "alice"),
        repo: RemoteRepo {
            name: "repo".to_string(),
            clone_url: "https://github.com/alice/repo.git".to_string(),
            private: true,
            description: None,
        },
        url: "https://mirror.example.test/webhook".to_string(),
        dry_run: false,
    })
    .unwrap();

    assert_eq!(key, None);
    handle.join().unwrap();
}

#[test]
fn uninstall_state_cleanup_only_removes_matching_url() {
    let endpoint = endpoint("github", NamespaceKind::User, "alice");
    let key = webhook_installation_key("sync-1", &endpoint, "repo");
    let mut state = WebhookState::default();
    state.installations.insert(
        key.clone(),
        WebhookInstallation {
            group: "sync-1".to_string(),
            endpoint: endpoint.clone(),
            repo: "repo".to_string(),
            url: "https://current.example.test/webhook".to_string(),
        },
    );
    state.skipped.insert(
        key.clone(),
        SkippedWebhookInstallation {
            group: "sync-1".to_string(),
            endpoint,
            repo: "repo".to_string(),
            url: "https://current.example.test/webhook".to_string(),
            reason: "provider blocked access".to_string(),
        },
    );

    remove_webhook_state_keys(
        &mut state,
        vec![key.clone()],
        "https://old.example.test/webhook",
    );

    assert!(state.installations.contains_key(&key));
    assert!(state.skipped.contains_key(&key));

    remove_webhook_state_keys(
        &mut state,
        vec![key.clone()],
        "https://current.example.test/webhook",
    );

    assert!(!state.installations.contains_key(&key));
    assert!(!state.skipped.contains_key(&key));
}

fn site(name: &str, provider: ProviderKind) -> SiteConfig {
    SiteConfig {
        name: name.to_string(),
        provider,
        base_url: "https://example.test".to_string(),
        api_url: None,
        token: TokenConfig::Value("secret".to_string()),
        git_username: None,
    }
}

fn filtered_mirror() -> MirrorConfig {
    MirrorConfig {
        name: "sync-1".to_string(),
        endpoints: vec![
            endpoint("github", NamespaceKind::User, "alice"),
            endpoint("github-peer", NamespaceKind::User, "bob"),
        ],
        sync_visibility: SyncVisibility::Public,
        repo_whitelist: vec!["^important-".to_string()],
        repo_blacklist: vec!["-archive$".to_string()],
        create_missing: true,
        visibility: Visibility::Private,
        conflict_resolution: ConflictResolutionStrategy::Fail,
    }
}

fn endpoint_repo(endpoint: &EndpointConfig, name: &str, private: bool) -> EndpointRepo {
    EndpointRepo {
        endpoint: endpoint.clone(),
        repo: RemoteRepo {
            name: name.to_string(),
            clone_url: format!("https://github.com/alice/{name}.git"),
            private,
            description: None,
        },
    }
}

fn webhook_event(repo: &str) -> WebhookEvent {
    WebhookEvent {
        provider: Some(ProviderKind::Github),
        repo: repo.to_string(),
        namespace: Some("alice".to_string()),
    }
}

fn endpoint(site: &str, kind: NamespaceKind, namespace: &str) -> EndpointConfig {
    EndpointConfig {
        site: site.to_string(),
        kind,
        namespace: namespace.to_string(),
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
