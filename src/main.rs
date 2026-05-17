mod config;
mod git;
mod interactive;
mod logging;
mod parallel;
mod provider;
mod state;
mod sync;
mod webhook;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::config::{Config, default_config_path};
use crate::sync::{SyncOptions, sync_all};
use crate::webhook::{
    ServeOptions, WebhookInstallOptions, WebhookUninstallOptions, WebhookUpdateOptions,
    install_webhooks, serve, uninstall_webhooks, update_webhooks,
};

#[derive(Parser, Debug)]
#[command(name = "refray")]
#[command(version = concat!(env!("CARGO_PKG_VERSION"), " (built ", env!("REFRAY_BUILD_TIME"), ")"))]
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
    /// Do not create repositories that are missing from an endpoint.
    #[arg(long)]
    no_create: bool,
    /// Sync only repositories that failed during the previous non-dry-run sync.
    #[arg(long)]
    retry_failed: bool,
}

#[derive(Args, Debug)]
struct ServeCommand {
    #[arg(long, default_value = "127.0.0.1:8787", value_name = "HOST:PORT")]
    listen: String,
}

#[derive(Subcommand, Debug)]
enum WebhookCommand {
    Install(WebhookInstallCommand),
    Uninstall(WebhookUninstallCommand),
    Update(WebhookUpdateCommand),
}

#[derive(Args, Debug)]
struct WebhookInstallCommand {
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct WebhookUninstallCommand {
    #[arg(value_name = "URL", value_parser = parse_webhook_url)]
    url: Option<String>,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct WebhookUpdateCommand {
    #[arg(value_name = "URL")]
    url: String,
    #[arg(long)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);

    match cli.command {
        Command::Config => {
            let outcome = interactive::run_config_wizard(&config_path)?;
            if outcome.run_full_sync_now {
                sync_all(
                    &outcome.config,
                    SyncOptions {
                        jobs: outcome.config.jobs,
                        ..SyncOptions::default()
                    },
                )
            } else {
                Ok(())
            }
        }
        Command::Sync(command) => {
            let config = load_config(&config_path)?;
            sync_all(
                &config,
                SyncOptions {
                    group: command.group,
                    dry_run: command.dry_run,
                    create_missing_override: command.no_create.then_some(false),
                    retry_failed: command.retry_failed,
                    jobs: config.jobs,
                    ..SyncOptions::default()
                },
            )
        }
        Command::Serve(command) => {
            let config = load_config(&config_path)?;
            let full_sync_interval_minutes = config
                .webhook
                .as_ref()
                .and_then(|webhook| webhook.full_sync_interval_minutes);
            let reachability_url = config.webhook.as_ref().map(|webhook| webhook.url.clone());
            let reachability_check_interval_minutes = config
                .webhook
                .as_ref()
                .and_then(|webhook| webhook.reachability_check_interval_minutes);
            let secret = resolve_config_webhook_secret(&config)?;
            let workers = config.jobs;
            serve(
                config,
                ServeOptions {
                    listen: command.listen,
                    secret,
                    workers,
                    work_dir: None,
                    full_sync_interval_minutes,
                    reachability_url,
                    reachability_check_interval_minutes,
                },
            )
        }
        Command::Webhook(WebhookCommand::Install(command)) => {
            let config = load_config(&config_path)?;
            let secret = resolve_config_webhook_secret(&config)?;
            let url = resolve_config_webhook_url(&config)?;
            install_webhooks(
                &config,
                WebhookInstallOptions {
                    url,
                    secret,
                    dry_run: command.dry_run,
                    work_dir: None,
                    jobs: config.jobs,
                },
            )
        }
        Command::Webhook(WebhookCommand::Uninstall(command)) => {
            let config = load_config(&config_path)?;
            let url = resolve_uninstall_webhook_url(&config, command.url)?;
            uninstall_webhooks(
                &config,
                WebhookUninstallOptions {
                    url,
                    dry_run: command.dry_run,
                    work_dir: None,
                    jobs: config.jobs,
                },
            )
        }
        Command::Webhook(WebhookCommand::Update(command)) => {
            let mut config = load_config(&config_path)?;
            let old_url = config
                .webhook
                .as_ref()
                .map(|webhook| webhook.url.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!("configure [webhook] before running webhook update")
                })?;
            let secret = resolve_config_webhook_secret(&config)?;
            update_webhooks(
                &config,
                WebhookUpdateOptions {
                    old_url,
                    new_url: command.url.clone(),
                    secret,
                    dry_run: command.dry_run,
                    work_dir: None,
                    jobs: config.jobs,
                },
            )?;
            if !command.dry_run {
                set_config_webhook_url(&mut config, command.url);
                config.save(&config_path)?;
            }
            Ok(())
        }
    }
}

fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        anyhow::bail!(
            "config not found at {}. Run `refray config` first to create one.",
            path.display()
        );
    }
    Config::load(path).with_context(|| format!("failed to load config at {}", path.display()))
}

fn resolve_config_webhook_secret(config: &Config) -> Result<String> {
    config
        .webhook
        .as_ref()
        .map(|webhook| webhook.secret())
        .transpose()?
        .ok_or_else(|| {
            anyhow::anyhow!("configure [webhook].secret before running serve or webhook commands")
        })
}

fn resolve_config_webhook_url(config: &Config) -> Result<String> {
    config
        .webhook
        .as_ref()
        .map(|webhook| webhook.url.clone())
        .ok_or_else(|| anyhow::anyhow!("configure [webhook].url before running webhook commands"))
}

fn resolve_uninstall_webhook_url(config: &Config, url: Option<String>) -> Result<String> {
    match url {
        Some(url) => Ok(url),
        None => resolve_config_webhook_url(config),
    }
}

fn parse_webhook_url(value: &str) -> std::result::Result<String, String> {
    if value.trim().is_empty() {
        return Err("A value is required".to_string());
    }
    let url = url::Url::parse(value).map_err(|error| format!("Invalid URL: {error}"))?;
    match url.scheme() {
        "http" | "https" => Ok(value.to_string()),
        _ => Err("URL must start with http:// or https://".to_string()),
    }
}

fn set_config_webhook_url(config: &mut Config, url: String) {
    let Some(webhook) = &mut config.webhook else {
        unreachable!("caller verifies webhook config exists before saving update")
    };
    webhook.url = url;
}

#[cfg(test)]
#[path = "../tests/unit/cli.rs"]
mod tests;
