mod config;
mod git;
mod interactive;
mod logging;
mod provider;
mod state;
mod sync;
mod webhook;

use std::env;
use std::path::{Path, PathBuf};

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
    #[arg(long, default_value_t = DEFAULT_JOBS, value_name = "N")]
    jobs: usize,
}

#[derive(Args, Debug)]
struct WebhookUninstallCommand {
    #[arg(long, value_name = "NAME")]
    group: Option<String>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, value_name = "PATH")]
    work_dir: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_JOBS, value_name = "N")]
    jobs: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);

    match cli.command {
        Command::Config => interactive::run_config_wizard(&config_path),
        Command::Sync(command) => {
            let config = load_config(&config_path)?;
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
            let config = load_config(&config_path)?;
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
            let config = load_config(&config_path)?;
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
                    jobs: command.jobs,
                },
            )
        }
        Command::Webhook(WebhookCommand::Uninstall(command)) => {
            let config = load_config(&config_path)?;
            uninstall_webhooks(
                &config,
                WebhookUninstallOptions {
                    group: command.group,
                    dry_run: command.dry_run,
                    work_dir: command.work_dir,
                    jobs: command.jobs,
                },
            )
        }
    }
}

fn load_config(path: &Path) -> Result<Config> {
    Config::load(path).with_context(|| format!("failed to load config at {}", path.display()))
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
#[path = "../tests/unit/cli.rs"]
mod tests;
