use super::*;
use crate::config::{
    EndpointConfig, MirrorConfig, NamespaceKind, SiteConfig, TokenConfig, Visibility,
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
            create_missing: true,
            visibility: Visibility::Private,
            allow_force: false,
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
