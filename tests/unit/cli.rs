use super::*;

#[test]
fn cli_config_opens_wizard() {
    let cli = Cli::try_parse_from(["refray", "config"]).unwrap();

    assert!(matches!(cli.command, Command::Config));
}

#[test]
fn cli_prints_version() {
    let error = Cli::try_parse_from(["refray", "--version"]).unwrap_err();

    assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
    let output = error.to_string();
    assert!(output.contains(env!("CARGO_PKG_VERSION")));
    assert!(output.contains(env!("REFRAY_BUILD_TIME")));
}

#[test]
fn cli_rejects_removed_config_subcommands() {
    for args in [
        ["refray", "config", "wizard"].as_slice(),
        ["refray", "config", "init"].as_slice(),
        ["refray", "config", "show"].as_slice(),
        ["refray", "config", "site", "list"].as_slice(),
        ["refray", "config", "mirror", "list"].as_slice(),
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn cli_rejects_removed_sync_args() {
    for args in [
        ["refray", "sync", "--repo-pattern", "^(foo|bar)-"].as_slice(),
        ["refray", "sync", "--work-dir", "/tmp/refray"].as_slice(),
        ["refray", "sync", "--force"].as_slice(),
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn cli_accepts_sync_retry_failed() {
    let cli = Cli::try_parse_from(["refray", "sync", "--retry-failed"]).unwrap();

    let Command::Sync(args) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert!(args.retry_failed);
}

#[test]
fn cli_accepts_webhook_serve() {
    let cli = Cli::try_parse_from(["refray", "serve", "--listen", "127.0.0.1:9000"]).unwrap();

    let Command::Serve(args) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert_eq!(args.listen, "127.0.0.1:9000");
}

#[test]
fn cli_rejects_removed_jobs_args() {
    for args in [
        ["refray", "sync", "--jobs", "8"].as_slice(),
        ["refray", "serve", "--jobs", "2"].as_slice(),
        ["refray", "webhook", "install", "--jobs", "6"].as_slice(),
        ["refray", "webhook", "uninstall", "--jobs", "3"].as_slice(),
        [
            "refray",
            "webhook",
            "update",
            "https://new.example.test/webhook",
            "--jobs",
            "5",
        ]
        .as_slice(),
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn cli_rejects_removed_serve_args() {
    for args in [
        ["refray", "serve", "--secret", "secret"].as_slice(),
        ["refray", "serve", "--secret-env", "WEBHOOK_SECRET"].as_slice(),
        ["refray", "serve", "--full-sync-interval-minutes", "30"].as_slice(),
        ["refray", "serve", "--work-dir", "/tmp/refray"].as_slice(),
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn cli_accepts_webhook_install() {
    let cli = Cli::try_parse_from(["refray", "webhook", "install"]).unwrap();

    let Command::Webhook(WebhookCommand::Install(args)) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert!(!args.dry_run);
}

#[test]
fn cli_rejects_removed_webhook_install_args() {
    for args in [
        ["refray", "webhook", "install", "repo-one"].as_slice(),
        [
            "refray",
            "webhook",
            "install",
            "--url",
            "https://mirror.example.test/webhook",
        ]
        .as_slice(),
        ["refray", "webhook", "install", "--group", "sync-1"].as_slice(),
        ["refray", "webhook", "install", "--work-dir", "/tmp/refray"].as_slice(),
        ["refray", "webhook", "install", "--secret", "secret"].as_slice(),
        [
            "refray",
            "webhook",
            "install",
            "--secret-env",
            "WEBHOOK_SECRET",
        ]
        .as_slice(),
        ["refray", "webhook", "install", "--repo-pattern", "^repo$"].as_slice(),
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn cli_accepts_webhook_uninstall() {
    let cli = Cli::try_parse_from(["refray", "webhook", "uninstall", "--dry-run"]).unwrap();

    let Command::Webhook(WebhookCommand::Uninstall(args)) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert_eq!(args.url, None);
    assert!(args.dry_run);
}

#[test]
fn cli_accepts_webhook_uninstall_url() {
    let cli = Cli::try_parse_from([
        "refray",
        "webhook",
        "uninstall",
        "https://old.example.test/webhook",
    ])
    .unwrap();

    let Command::Webhook(WebhookCommand::Uninstall(args)) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert_eq!(
        args.url,
        Some("https://old.example.test/webhook".to_string())
    );
}

#[test]
fn cli_rejects_removed_webhook_uninstall_args() {
    for args in [
        [
            "refray",
            "webhook",
            "uninstall",
            "--url",
            "https://mirror.example.test/webhook",
        ]
        .as_slice(),
        ["refray", "webhook", "uninstall", "--group", "sync-1"].as_slice(),
        [
            "refray",
            "webhook",
            "uninstall",
            "--work-dir",
            "/tmp/refray",
        ]
        .as_slice(),
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn cli_rejects_invalid_webhook_uninstall_url() {
    for args in [
        ["refray", "webhook", "uninstall", "repo-one"].as_slice(),
        [
            "refray",
            "webhook",
            "uninstall",
            "ftp://example.test/webhook",
        ]
        .as_slice(),
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn webhook_uninstall_url_uses_arg_before_config() {
    let config = Config {
        jobs: crate::config::DEFAULT_JOBS,
        webhook: None,
        ..Config::default()
    };

    let url = resolve_uninstall_webhook_url(
        &config,
        Some("https://old.example.test/webhook".to_string()),
    )
    .unwrap();

    assert_eq!(url, "https://old.example.test/webhook");
}

#[test]
fn cli_accepts_webhook_update() {
    let cli = Cli::try_parse_from([
        "refray",
        "webhook",
        "update",
        "https://new.example.test/webhook",
    ])
    .unwrap();

    let Command::Webhook(WebhookCommand::Update(args)) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert_eq!(args.url, "https://new.example.test/webhook");
}

#[test]
fn cli_rejects_removed_webhook_update_secret_args() {
    for args in [
        [
            "refray",
            "webhook",
            "update",
            "--url",
            "https://new.example.test/webhook",
        ]
        .as_slice(),
        [
            "refray",
            "webhook",
            "update",
            "https://new.example.test/webhook",
            "--secret",
            "secret",
        ]
        .as_slice(),
        [
            "refray",
            "webhook",
            "update",
            "https://new.example.test/webhook",
            "--secret-env",
            "WEBHOOK_SECRET",
        ]
        .as_slice(),
        [
            "refray",
            "webhook",
            "update",
            "https://new.example.test/webhook",
            "--work-dir",
            "/tmp/refray",
        ]
        .as_slice(),
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn cli_rejects_scoped_webhook_update() {
    let result = Cli::try_parse_from([
        "refray",
        "webhook",
        "update",
        "repo-one",
        "https://new.example.test/webhook",
    ]);

    assert!(result.is_err());
}

#[test]
fn missing_config_error_asks_user_to_run_config() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");

    let error = load_config(&path).unwrap_err().to_string();

    assert!(error.contains("config not found at"));
    assert!(error.contains("Run `refray config` first"));
}
