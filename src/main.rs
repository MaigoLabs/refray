mod config;
mod git;
mod interactive;
mod logging;
mod provider;
mod sync;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::{
    Config, EndpointConfig, NamespaceKind, ProviderKind, SiteConfig, TokenConfig, Visibility,
    default_config_path,
};
use crate::sync::{DEFAULT_JOBS, SyncOptions, sync_all};

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
    #[command(subcommand)]
    Config(ConfigCommand),
    Sync(SyncCommand),
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    Init,
    Wizard,
    #[command(subcommand)]
    Site(SiteCommand),
    #[command(subcommand)]
    Mirror(MirrorCommand),
    Show,
}

#[derive(Subcommand, Debug)]
enum SiteCommand {
    Add(SiteAddCommand),
    Remove(NameCommand),
    List,
}

#[derive(Subcommand, Debug)]
enum MirrorCommand {
    Add(MirrorAddCommand),
    Remove(NameCommand),
    List,
}

#[derive(Args, Debug)]
struct NameCommand {
    name: String,
}

#[derive(Args, Debug)]
struct SiteAddCommand {
    #[arg(long)]
    name: String,
    #[arg(long)]
    provider: ProviderArg,
    #[arg(long, value_name = "URL")]
    base_url: String,
    #[arg(long, value_name = "URL")]
    api_url: Option<String>,
    #[arg(long, conflicts_with = "token_env")]
    token: Option<String>,
    #[arg(long, value_name = "ENV", conflicts_with = "token")]
    token_env: Option<String>,
    #[arg(long)]
    git_username: Option<String>,
}

#[derive(Args, Debug)]
struct MirrorAddCommand {
    #[arg(long)]
    name: String,
    #[arg(long = "endpoint", required = true, action = clap::ArgAction::Append, value_name = "SITE:KIND:NAMESPACE")]
    endpoints: Vec<String>,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    create_missing: bool,
    #[arg(long, default_value_t = VisibilityArg::Private)]
    visibility: VisibilityArg,
    #[arg(long, default_value_t = false)]
    allow_force: bool,
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

#[derive(Clone, Debug, ValueEnum)]
enum ProviderArg {
    Github,
    Gitlab,
    Gitea,
    Forgejo,
}

#[derive(Clone, Debug, ValueEnum)]
enum VisibilityArg {
    Private,
    Public,
}

impl std::fmt::Display for VisibilityArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Private => write!(f, "private"),
            Self::Public => write!(f, "public"),
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);

    match cli.command {
        Command::Config(command) => handle_config(command, config_path),
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
    }
}

fn handle_config(command: ConfigCommand, path: PathBuf) -> Result<()> {
    match command {
        ConfigCommand::Init => {
            if path.exists() {
                bail!("config already exists at {}", path.display());
            }
            let config = Config::default();
            config.save(&path)?;
            println!("created {}", path.display());
            Ok(())
        }
        ConfigCommand::Wizard => interactive::run_config_wizard(&path),
        ConfigCommand::Site(command) => handle_site(command, path),
        ConfigCommand::Mirror(command) => handle_mirror(command, path),
        ConfigCommand::Show => {
            let config = Config::load(&path)?;
            println!("{}", toml::to_string_pretty(&config)?);
            Ok(())
        }
    }
}

fn handle_site(command: SiteCommand, path: PathBuf) -> Result<()> {
    let mut config = Config::load_or_default(&path)?;
    match command {
        SiteCommand::Add(args) => {
            let token = match (args.token, args.token_env) {
                (Some(value), None) => TokenConfig::Value(value),
                (None, Some(env)) => TokenConfig::Env(env),
                (None, None) => bail!("pass either --token or --token-env"),
                (Some(_), Some(_)) => unreachable!("clap enforces token conflicts"),
            };
            config.upsert_site(SiteConfig {
                name: args.name,
                provider: args.provider.into(),
                base_url: args.base_url,
                api_url: args.api_url,
                token,
                git_username: args.git_username,
            });
            config.save(&path)?;
            println!("updated {}", path.display());
            Ok(())
        }
        SiteCommand::Remove(args) => {
            config.remove_site(&args.name)?;
            config.save(&path)?;
            println!("removed site {}", args.name);
            Ok(())
        }
        SiteCommand::List => {
            for site in &config.sites {
                println!("{}\t{:?}\t{}", site.name, site.provider, site.base_url);
            }
            Ok(())
        }
    }
}

fn handle_mirror(command: MirrorCommand, path: PathBuf) -> Result<()> {
    let mut config = Config::load_or_default(&path)?;
    match command {
        MirrorCommand::Add(args) => {
            if args.endpoints.len() < 2 {
                bail!("mirror groups need at least two --endpoint values");
            }
            let endpoints = args
                .endpoints
                .iter()
                .map(|value| parse_endpoint(value))
                .collect::<Result<Vec<_>>>()?;
            for endpoint in &endpoints {
                config
                    .site(&endpoint.site)
                    .with_context(|| format!("unknown site '{}'", endpoint.site))?;
            }
            config.upsert_mirror(config::MirrorConfig {
                name: args.name,
                endpoints,
                create_missing: args.create_missing,
                visibility: args.visibility.into(),
                allow_force: args.allow_force,
            });
            config.save(&path)?;
            println!("updated {}", path.display());
            Ok(())
        }
        MirrorCommand::Remove(args) => {
            config.remove_mirror(&args.name)?;
            config.save(&path)?;
            println!("removed mirror {}", args.name);
            Ok(())
        }
        MirrorCommand::List => {
            for mirror in &config.mirrors {
                let endpoints = mirror
                    .endpoints
                    .iter()
                    .map(|endpoint| {
                        format!(
                            "{}:{:?}:{}",
                            endpoint.site, endpoint.kind, endpoint.namespace
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("{}\t{}", mirror.name, endpoints);
            }
            Ok(())
        }
    }
}

fn parse_endpoint(value: &str) -> Result<EndpointConfig> {
    let parts = value.splitn(3, ':').collect::<Vec<_>>();
    if parts.len() != 3 {
        bail!("endpoint must be SITE:KIND:NAMESPACE, got '{value}'");
    }

    let kind = match parts[1].to_ascii_lowercase().as_str() {
        "user" => NamespaceKind::User,
        "org" | "organization" => NamespaceKind::Org,
        "group" => NamespaceKind::Group,
        other => bail!("unsupported namespace kind '{other}'"),
    };

    Ok(EndpointConfig {
        site: parts[0].to_string(),
        kind,
        namespace: parts[2].to_string(),
    })
}

impl From<ProviderArg> for ProviderKind {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::Github => Self::Github,
            ProviderArg::Gitlab => Self::Gitlab,
            ProviderArg::Gitea => Self::Gitea,
            ProviderArg::Forgejo => Self::Forgejo,
        }
    }
}

impl From<VisibilityArg> for Visibility {
    fn from(value: VisibilityArg) -> Self {
        match value {
            VisibilityArg::Private => Self::Private,
            VisibilityArg::Public => Self::Public,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_accepts_repeated_mirror_endpoints() {
        let cli = Cli::try_parse_from([
            "git-sync",
            "config",
            "mirror",
            "add",
            "--name",
            "personal",
            "--endpoint",
            "github:user:hykilpikonna",
            "--endpoint",
            "gitea:user:azalea",
        ])
        .unwrap();

        let Command::Config(ConfigCommand::Mirror(MirrorCommand::Add(args))) = cli.command else {
            panic!("parsed unexpected command");
        };
        assert_eq!(args.name, "personal");
        assert_eq!(
            args.endpoints,
            vec![
                "github:user:hykilpikonna".to_string(),
                "gitea:user:azalea".to_string()
            ]
        );
    }

    #[test]
    fn cli_accepts_config_wizard() {
        let cli = Cli::try_parse_from(["git-sync", "config", "wizard"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Config(ConfigCommand::Wizard)
        ));
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
    fn cli_accepts_new_provider_kinds() {
        for (name, expected) in [("forgejo", ProviderKind::Forgejo)] {
            let cli = Cli::try_parse_from([
                "git-sync",
                "config",
                "site",
                "add",
                "--name",
                name,
                "--provider",
                name,
                "--base-url",
                "https://example.test",
                "--token",
                "token",
            ])
            .unwrap();

            let Command::Config(ConfigCommand::Site(SiteCommand::Add(args))) = cli.command else {
                panic!("parsed unexpected command");
            };
            assert_eq!(ProviderKind::from(args.provider), expected);
        }
    }

    #[test]
    fn endpoint_parser_supports_aliases_and_rejects_bad_kinds() {
        let endpoint = parse_endpoint("github:organization:MewoLab").unwrap();
        assert_eq!(endpoint.site, "github");
        assert_eq!(endpoint.kind, NamespaceKind::Org);
        assert_eq!(endpoint.namespace, "MewoLab");

        let endpoint = parse_endpoint("gitlab:group:parent/child").unwrap();
        assert_eq!(endpoint.kind, NamespaceKind::Group);

        let err = parse_endpoint("github:team:alice").unwrap_err().to_string();
        assert!(err.contains("unsupported namespace kind 'team'"));

        let err = parse_endpoint("github:user").unwrap_err().to_string();
        assert!(err.contains("SITE:KIND:NAMESPACE"));
    }

    #[test]
    fn site_add_requires_one_token_source() {
        let missing = Cli::try_parse_from([
            "git-sync",
            "config",
            "site",
            "add",
            "--name",
            "github",
            "--provider",
            "github",
            "--base-url",
            "https://github.com",
        ])
        .unwrap();

        let Command::Config(ConfigCommand::Site(SiteCommand::Add(args))) = missing.command else {
            panic!("parsed unexpected command");
        };
        let temp = tempfile::TempDir::new().unwrap();
        let err = handle_site(SiteCommand::Add(args), temp.path().join("config.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("pass either --token or --token-env"));

        let conflict = Cli::try_parse_from([
            "git-sync",
            "config",
            "site",
            "add",
            "--name",
            "github",
            "--provider",
            "github",
            "--base-url",
            "https://github.com",
            "--token",
            "a",
            "--token-env",
            "GITHUB_TOKEN",
        ]);
        assert!(conflict.is_err());
    }
}
