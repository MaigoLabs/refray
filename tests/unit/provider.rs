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
    let (api_url, handle) = one_request_server("200 OK", r#"{"type":"Organization"}"#, |request| {
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

#[test]
fn delete_repo_deletes_github_repo() {
    let (api_url, handle) = one_request_server("204 No Content", "", |request| {
        assert!(
            request.starts_with("DELETE /repos/alice/repo "),
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

    ProviderClient::new(&site)
        .unwrap()
        .delete_repo(
            &EndpointConfig {
                site: "github".to_string(),
                kind: NamespaceKind::User,
                namespace: "alice".to_string(),
            },
            "repo",
        )
        .unwrap();
    handle.join().unwrap();
}

#[test]
fn delete_repo_deletes_url_encoded_gitlab_project() {
    let (api_url, handle) = one_request_server("202 Accepted", "", |request| {
        assert!(
            request.starts_with("DELETE /projects/parent%2Falice%2Frepo "),
            "request was {request}"
        );
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

    ProviderClient::new(&site)
        .unwrap()
        .delete_repo(
            &EndpointConfig {
                site: "gitlab".to_string(),
                kind: NamespaceKind::Group,
                namespace: "parent/alice".to_string(),
            },
            "repo",
        )
        .unwrap();
    handle.join().unwrap();
}

#[test]
fn delete_repo_deletes_gitea_repo() {
    let (api_url, handle) = one_request_server("204 No Content", "", |request| {
        assert!(
            request.starts_with("DELETE /repos/alice/repo "),
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

    ProviderClient::new(&site)
        .unwrap()
        .delete_repo(
            &EndpointConfig {
                site: "gitea".to_string(),
                kind: NamespaceKind::User,
                namespace: "alice".to_string(),
            },
            "repo",
        )
        .unwrap();
    handle.join().unwrap();
}

#[test]
fn open_pull_request_posts_github_pull_when_missing() {
    let (api_url, handle) = request_server(
        vec![
            ("200 OK", "[]"),
            (
                "201 Created",
                r#"{"number":7,"html_url":"https://github.example.test/pull/7"}"#,
            ),
        ],
        |index, request| match index {
            0 => assert!(
                request
                    .starts_with("GET /repos/alice/repo/pulls?state=open&base=main&per_page=100 "),
                "request was {request}"
            ),
            1 => {
                assert!(
                    request.starts_with("POST /repos/alice/repo/pulls "),
                    "request was {request}"
                );
                assert!(request.contains("Resolve conflict"));
                assert!(request.contains("refray/conflicts/main/from-b-abc123"));
                assert!(request.contains("main"));
            }
            _ => unreachable!(),
        },
    );
    let site = SiteConfig {
        api_url: Some(api_url),
        ..site(ProviderKind::Github, None)
    };
    let client = ProviderClient::new(&site).unwrap();

    let pr = client
        .open_pull_request(
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
            &PullRequestRequest {
                title: "Resolve conflict".to_string(),
                body: "Body".to_string(),
                head_branch: "refray/conflicts/main/from-b-abc123".to_string(),
                base_branch: "main".to_string(),
            },
        )
        .unwrap();

    assert_eq!(pr.url.unwrap(), "https://github.example.test/pull/7");
    handle.join().unwrap();
}

#[test]
fn close_pull_requests_by_head_prefix_closes_matching_github_pulls() {
    let (api_url, handle) = request_server(
        vec![
            (
                "200 OK",
                r#"[{"number":7,"head":{"ref":"refray/conflicts/main/from-b-abc123"}},{"number":8,"head":{"ref":"feature"}}]"#,
            ),
            ("200 OK", r#"{"number":7}"#),
        ],
        |index, request| match index {
            0 => assert!(
                request
                    .starts_with("GET /repos/alice/repo/pulls?state=open&base=main&per_page=100 "),
                "request was {request}"
            ),
            1 => {
                assert!(
                    request.starts_with("PATCH /repos/alice/repo/pulls/7 "),
                    "request was {request}"
                );
                assert!(request.contains("closed"));
            }
            _ => unreachable!(),
        },
    );
    let site = SiteConfig {
        api_url: Some(api_url),
        ..site(ProviderKind::Github, None)
    };
    let client = ProviderClient::new(&site).unwrap();

    let closed = client
        .close_pull_requests_by_head_prefix(
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
            "main",
            "refray/conflicts/main/",
        )
        .unwrap();

    assert_eq!(closed, 1);
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
