use super::*;

#[test]
fn cli_config_opens_wizard() {
    let cli = Cli::try_parse_from(["git-sync", "config"]).unwrap();

    assert!(matches!(cli.command, Command::Config));
}

#[test]
fn cli_rejects_removed_config_subcommands() {
    for args in [
        ["git-sync", "config", "wizard"].as_slice(),
        ["git-sync", "config", "init"].as_slice(),
        ["git-sync", "config", "show"].as_slice(),
        ["git-sync", "config", "site", "list"].as_slice(),
        ["git-sync", "config", "mirror", "list"].as_slice(),
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn cli_accepts_sync_repo_pattern() {
    let cli = Cli::try_parse_from([
        "git-sync",
        "sync",
        "--repo-pattern",
        "^(foo|bar)-",
        "--dry-run",
    ])
    .unwrap();

    let Command::Sync(args) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert_eq!(args.repo_pattern, Some("^(foo|bar)-".to_string()));
    assert!(args.dry_run);
}

#[test]
fn cli_accepts_sync_retry_failed() {
    let cli = Cli::try_parse_from(["git-sync", "sync", "--retry-failed"]).unwrap();

    let Command::Sync(args) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert!(args.retry_failed);
}

#[test]
fn cli_accepts_sync_jobs() {
    let cli = Cli::try_parse_from(["git-sync", "sync", "--jobs", "8"]).unwrap();

    let Command::Sync(args) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert_eq!(args.jobs, 8);
}

#[test]
fn cli_accepts_webhook_serve() {
    let cli = Cli::try_parse_from([
        "git-sync",
        "serve",
        "--listen",
        "127.0.0.1:9000",
        "--secret-env",
        "WEBHOOK_SECRET",
        "--jobs",
        "2",
        "--full-sync-interval-minutes",
        "30",
    ])
    .unwrap();

    let Command::Serve(args) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert_eq!(args.listen, "127.0.0.1:9000");
    assert_eq!(args.secret_env, Some("WEBHOOK_SECRET".to_string()));
    assert_eq!(args.jobs, 2);
    assert_eq!(args.full_sync_interval_minutes, Some(30));
}

#[test]
fn cli_accepts_webhook_install() {
    let cli = Cli::try_parse_from([
        "git-sync",
        "webhook",
        "install",
        "--url",
        "https://mirror.example.test/webhook",
        "--secret",
        "secret",
        "--group",
        "sync-1",
        "--repo-pattern",
        "^repo$",
        "--jobs",
        "6",
    ])
    .unwrap();

    let Command::Webhook(WebhookCommand::Install(args)) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert_eq!(
        args.url,
        Some("https://mirror.example.test/webhook".to_string())
    );
    assert_eq!(args.secret, Some("secret".to_string()));
    assert_eq!(args.group, Some("sync-1".to_string()));
    assert_eq!(args.repo_pattern, Some("^repo$".to_string()));
    assert_eq!(args.jobs, 6);
}

#[test]
fn cli_accepts_webhook_uninstall() {
    let cli = Cli::try_parse_from([
        "git-sync",
        "webhook",
        "uninstall",
        "--group",
        "sync-1",
        "--dry-run",
        "--jobs",
        "3",
    ])
    .unwrap();

    let Command::Webhook(WebhookCommand::Uninstall(args)) = cli.command else {
        panic!("parsed unexpected command");
    };
    assert_eq!(args.group, Some("sync-1".to_string()));
    assert!(args.dry_run);
    assert_eq!(args.jobs, 3);
}
