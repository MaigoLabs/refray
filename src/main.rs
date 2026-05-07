mod config;
mod git;
mod interactive;
mod logging;
mod provider;
mod sync;
mod webhook;

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::config::{Config, default_config_path};
use crate::sync::{DEFAULT_JOBS, SyncOptions, sync_all};
use crate::webhook::{
    ServeOptions, WebhookInstallOptions, WebhookUninstallOptions, install_webhooks, serve,
    uninstall_webhooks,
};

#[derive(Parser, Debug)]
#[command(name = "git-sync")]
#[command(about = "Mirror repositories between Git hosting providers")]
struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the interactive configuration wizard
    Config,
    /// Sync configured mirror groups once
    Sync(SyncCommand),
    /// Run the webhook receiver
    Serve(ServeCommand),
    /// Install or uninstall repository webhooks
    #[command(subcommand)]
    Webhook(WebhookCommand),
}

#[derive(Args, Debug)]
struct SyncCommand {
    #[arg(long, value_name = "NAME")]
    group: Option<String>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    no_create: bool,
    #[arg(long)]
    force: bool,
    #[arg(long, value_name = "REGEX")]
    repo_pattern: Option<String>,
    #[arg(long)]
    retry_failed: bool,
    #[arg(long, value_name = "PATH")]
    work_dir: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_JOBS, value_name = "N")]
    jobs: usize,
}

#[derive(Args, Debug)]
struct ServeCommand {
    #[arg(long, default_value = "127.0.0.1:8787", value_name = "HOST:PORT")]
    listen: String,
    #[arg(long, conflicts_with = "secret_env")]
    secret: Option<String>,
    #[arg(long, value_name = "ENV", conflicts_with = "secret")]
    secret_env: Option<String>,
    #[arg(long, default_value_t = DEFAULT_JOBS, value_name = "N")]
    jobs: usize,
    #[arg(long, value_name = "PATH")]
    work_dir: Option<PathBuf>,
    #[arg(long, value_name = "MINUTES")]
    full_sync_interval_minutes: Option<u64>,
}

#[derive(Subcommand, Debug)]
enum WebhookCommand {
    Install(WebhookInstallCommand),
    Uninstall(WebhookUninstallCommand),
}

#[derive(Args, Debug)]
struct WebhookInstallCommand {
    #[arg(long, value_name = "URL")]
    url: Option<String>,
    #[arg(long, conflicts_with = "secret_env")]
    secret: Option<String>,
    #[arg(long, value_name = "ENV", conflicts_with = "secret")]
    secret_env: Option<String>,
    #[arg(long, value_name = "NAME")]
    group: Option<String>,
    #[arg(long, value_name = "REGEX")]
    repo_pattern: Option<String>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, value_name = "PATH")]
    work_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct WebhookUninstallCommand {
    #[arg(long, value_name = "NAME")]
    group: Option<String>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, value_name = "PATH")]
    work_dir: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);

    match cli.command {
        Command::Config => interactive::run_config_wizard(&config_path),
        Command::Sync(command) => {
            let config = Config::load(&config_path)
                .with_context(|| format!("failed to load config at {}", config_path.display()))?;
            sync_all(
                &config,
                SyncOptions {
                    group: command.group,
                    dry_run: command.dry_run,
                    create_missing_override: command.no_create.then_some(false),
                    force_override: command.force.then_some(true),
                    repo_pattern: command.repo_pattern,
                    retry_failed: command.retry_failed,
                    work_dir: command.work_dir,
                    jobs: command.jobs,
                },
            )
        }
        Command::Serve(command) => {
            let config = Config::load(&config_path)
                .with_context(|| format!("failed to load config at {}", config_path.display()))?;
            let full_sync_interval_minutes = command.full_sync_interval_minutes.or_else(|| {
                config
                    .webhook
                    .as_ref()
                    .and_then(|webhook| webhook.full_sync_interval_minutes)
            });
            let reachability_url = config.webhook.as_ref().map(|webhook| webhook.url.clone());
            let reachability_check_interval_minutes = config
                .webhook
                .as_ref()
                .and_then(|webhook| webhook.reachability_check_interval_minutes);
            let secret = resolve_webhook_secret(&config, command.secret, command.secret_env)?;
            serve(
                config,
                ServeOptions {
                    listen: command.listen,
                    secret,
                    workers: command.jobs,
                    work_dir: command.work_dir,
                    full_sync_interval_minutes,
                    reachability_url,
                    reachability_check_interval_minutes,
                },
            )
        }
        Command::Webhook(WebhookCommand::Install(command)) => {
            let config = Config::load(&config_path)
                .with_context(|| format!("failed to load config at {}", config_path.display()))?;
            let secret = resolve_webhook_secret(&config, command.secret, command.secret_env)?;
            let url = resolve_webhook_url(&config, command.url)?;
            install_webhooks(
                &config,
                WebhookInstallOptions {
                    url,
                    secret,
                    group: command.group,
                    repo_pattern: command.repo_pattern,
                    dry_run: command.dry_run,
                    work_dir: command.work_dir,
                },
            )
        }
        Command::Webhook(WebhookCommand::Uninstall(command)) => {
            let config = Config::load(&config_path)
                .with_context(|| format!("failed to load config at {}", config_path.display()))?;
            uninstall_webhooks(
                &config,
                WebhookUninstallOptions {
                    group: command.group,
                    dry_run: command.dry_run,
                    work_dir: command.work_dir,
                },
            )
        }
    }
}

fn resolve_webhook_secret(
    config: &Config,
    value: Option<String>,
    env_name: Option<String>,
) -> Result<String> {
    match (value, env_name) {
        (Some(value), None) => Ok(value),
        (None, Some(env_name)) => env::var(&env_name)
            .with_context(|| format!("environment variable {env_name} is not set")),
        (None, None) => config
            .webhook
            .as_ref()
            .map(|webhook| webhook.secret())
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("pass either --secret or --secret-env")),
        (Some(_), Some(_)) => unreachable!("clap enforces secret conflicts"),
    }
}

fn resolve_webhook_url(config: &Config, value: Option<String>) -> Result<String> {
    value
        .or_else(|| config.webhook.as_ref().map(|webhook| webhook.url.clone()))
        .ok_or_else(|| anyhow::anyhow!("pass --url or configure [webhook].url"))
}

#[cfg(test)]
mod tests {
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
        ])
        .unwrap();

        let Command::Webhook(WebhookCommand::Uninstall(args)) = cli.command else {
            panic!("parsed unexpected command");
        };
        assert_eq!(args.group, Some("sync-1".to_string()));
        assert!(args.dry_run);
    }
}
