use std::fmt::Display;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use console::{Term, style};
use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};
use regex::Regex;
use reqwest::blocking::Client;
use tiny_http::{Request, Response, Server, StatusCode};
use url::Url;

use crate::config::{
    Config, ConflictResolutionStrategy, EndpointConfig, MirrorConfig, NamespaceKind, ProviderKind,
    SiteConfig, SyncVisibility, TokenConfig, Visibility, WebhookConfig,
};
use crate::provider::ProviderClient;
use crate::webhook::check_webhook_url_reachable;

const DEFAULT_WEBHOOK_LISTEN: &str = "127.0.0.1:8787";

#[derive(Clone, Debug)]
struct ProfileTarget {
    base_url: String,
    provider: ProviderKind,
    namespace: String,
    kind: Option<NamespaceKind>,
}

#[derive(Clone, Debug)]
struct ParsedProfileUrl {
    base_url: String,
    host: String,
    namespace: String,
}

#[derive(Clone, Debug, Default)]
struct RepoFilterInput {
    whitelist: Vec<String>,
    blacklist: Vec<String>,
}

pub fn run_config_wizard(path: &Path) -> Result<ConfigWizardOutcome> {
    let existing_config = path.exists();
    let mut config = Config::load_or_default(path)?;
    let theme = ColorfulTheme::default();

    println!();
    println!("{}", style("refray configuration wizard").cyan().bold());
    let description = if existing_config {
        "Review, add, edit, or delete sync groups."
    } else {
        "Enter profile or organization URLs, then refray will build the mirror group."
    };
    println!("{}", style(description).dim());
    println!();

    if existing_config {
        print_sync_groups(&config);
    } else {
        add_sync_group_styled(&mut config, &theme)?;
        print_sync_groups(&config);
    }

    loop {
        match prompt_wizard_action_styled(&theme)? {
            WizardAction::AddSyncGroup => {
                add_sync_group_styled(&mut config, &theme)?;
                print_sync_groups(&config);
            }
            WizardAction::EditSyncGroup => {
                if edit_sync_group_styled(&mut config, &theme)? {
                    print_sync_groups(&config);
                }
            }
            WizardAction::DeleteSyncGroup => {
                if delete_sync_group_styled(&mut config, &theme)? {
                    print_sync_groups(&config);
                }
            }
            WizardAction::Done => break,
        }
    }

    config.save(path)?;
    println!(
        "{} {}",
        style("saved").green().bold(),
        style(path.display()).cyan()
    );
    let run_full_sync_now = prompt_run_full_sync_now_styled(&config, &theme)?;
    Ok(ConfigWizardOutcome {
        config,
        run_full_sync_now,
    })
}

#[derive(Clone, Debug)]
pub struct ConfigWizardOutcome {
    pub config: Config,
    pub run_full_sync_now: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WizardAction {
    AddSyncGroup,
    EditSyncGroup,
    DeleteSyncGroup,
    Done,
}

fn add_sync_group_styled(config: &mut Config, theme: &ColorfulTheme) -> Result<()> {
    let endpoints = prompt_sync_group_endpoints_styled(config, theme, &[])?;
    let sync_visibility = prompt_sync_visibility_styled(theme, None)?;
    let repo_filters = prompt_repo_filters_styled(theme, None)?;
    let conflict_resolution = prompt_conflict_resolution_styled(theme, None)?;
    config.upsert_mirror(MirrorConfig {
        name: next_mirror_name(config),
        endpoints,
        sync_visibility,
        repo_whitelist: repo_filters.whitelist,
        repo_blacklist: repo_filters.blacklist,
        create_missing: true,
        visibility: Visibility::Private,
        allow_force: false,
        conflict_resolution,
    });
    prompt_webhook_setup_styled(config, theme)?;

    Ok(())
}

fn prompt_sync_group_endpoints_styled(
    config: &mut Config,
    theme: &ColorfulTheme,
    existing: &[EndpointConfig],
) -> Result<Vec<EndpointConfig>> {
    let mut endpoints = Vec::new();

    let first = prompt_target_styled(
        theme,
        "Profile/org URL",
        endpoint_profile_url(config, existing.first()),
    )?;
    endpoints.push(ensure_credentials_styled(config, first, theme)?);

    let second = prompt_target_styled(
        theme,
        "Profile/org URL to sync with",
        endpoint_profile_url(config, existing.get(1)),
    )?;
    endpoints.push(ensure_credentials_styled(config, second, theme)?);

    for (index, endpoint) in existing.iter().enumerate().skip(2) {
        let Some(default_url) = endpoint_profile_url(config, Some(endpoint)) else {
            continue;
        };
        if !Confirm::with_theme(theme)
            .with_prompt(format!("Keep endpoint {}?", index + 1))
            .default(true)
            .interact()?
        {
            continue;
        }
        let next = prompt_target_styled(theme, "Additional profile/org URL", Some(default_url))?;
        endpoints.push(ensure_credentials_styled(config, next, theme)?);
    }

    loop {
        let prompt = if endpoints.len() == 2 {
            "Add a third endpoint for 3-way sync?"
        } else {
            "Add another endpoint to this sync group?"
        };
        if !Confirm::with_theme(theme)
            .with_prompt(prompt)
            .default(false)
            .interact()?
        {
            break;
        }
        let next = prompt_target_styled(theme, "Additional profile/org URL", None)?;
        endpoints.push(ensure_credentials_styled(config, next, theme)?);
    }

    Ok(endpoints)
}

fn prompt_webhook_setup_styled(config: &mut Config, theme: &ColorfulTheme) -> Result<()> {
    if config
        .webhook
        .as_ref()
        .is_some_and(|webhook| webhook.install)
    {
        println!(
            "{} {}",
            style("Webhooks").green().bold(),
            style("already enabled").dim()
        );
        return Ok(());
    }
    println!();
    println!(
        "{} {}",
        style("Webhooks").cyan().bold(),
        style(
            "strongly recommended; they sync immediately after pushes and greatly reduce conflicts"
        )
        .dim()
    );
    if !Confirm::with_theme(theme)
        .with_prompt("Install webhooks for configured repositories?")
        .default(true)
        .interact()?
    {
        return Ok(());
    }
    print_webhook_url_instructions();
    let demo_server = match DemoWebhookServer::start(DEFAULT_WEBHOOK_LISTEN) {
        Ok(server) => {
            println!(
                "  {} Temporary test listener running on {}",
                style("-").cyan(),
                style(server.listen()).bold()
            );
            Some(server)
        }
        Err(error) => {
            println!(
                "  {} Could not start temporary test listener on {}: {error:#}",
                style("-").yellow(),
                style(DEFAULT_WEBHOOK_LISTEN).bold()
            );
            println!(
                "  {} If refray serve is already running there, you can continue.",
                style("-").yellow()
            );
            None
        }
    };
    let url = Input::<String>::with_theme(theme)
        .with_prompt("Webhook URL reachable by GitHub/GitLab/Gitea")
        .validate_with(|value: &String| validate_url(value))
        .interact_text()?;
    match check_webhook_url_reachable(&url) {
        Ok(()) => println!(
            "{} {}",
            style("reachable").green().bold(),
            style(&url).cyan()
        ),
        Err(error) => {
            println!(
                "{} {}: {error:#}",
                style("not reachable from here").yellow().bold(),
                style(&url).cyan()
            );
            if !Confirm::with_theme(theme)
                .with_prompt("Save this webhook URL anyway?")
                .default(false)
                .interact()?
            {
                return Ok(());
            }
        }
    }
    drop(demo_server);
    let full_sync_interval_minutes = if Confirm::with_theme(theme)
        .with_prompt("Run periodic full sync while the webhook server is running?")
        .default(true)
        .interact()?
    {
        Some(
            Input::<u64>::with_theme(theme)
                .with_prompt("Full sync interval in minutes")
                .default(60)
                .interact_text()?,
        )
    } else {
        None
    };
    config.webhook = Some(WebhookConfig {
        install: true,
        url,
        secret: TokenConfig::Value(generate_webhook_secret()),
        full_sync_interval_minutes,
        reachability_check_interval_minutes: Some(15),
    });
    Ok(())
}

fn print_webhook_url_instructions() {
    println!();
    println!(
        "{} {}",
        style("Webhook URL").cyan().bold(),
        style("must be reachable from every Git provider").dim()
    );
    println!(
        "  {} Start the receiver with: {}",
        style("-").cyan(),
        style(format!("refray serve --listen {DEFAULT_WEBHOOK_LISTEN}")).bold()
    );
    println!(
        "  {} The receiver accepts: {} and {}",
        style("-").cyan(),
        style("POST /").bold(),
        style("POST /webhook").bold()
    );
    println!(
        "  {} If running locally, expose it with a tunnel, for example: {}",
        style("-").cyan(),
        style(format!(
            "cloudflared tunnel --url http://{DEFAULT_WEBHOOK_LISTEN}"
        ))
        .bold()
    );
    println!(
        "  {} Then enter the public URL, usually ending in {}",
        style("-").cyan(),
        style("/webhook").bold()
    );
    println!(
        "  {} The wizard starts a temporary listener on {} so you can test the tunnel now.",
        style("-").cyan(),
        style(DEFAULT_WEBHOOK_LISTEN).bold()
    );
}

struct DemoWebhookServer {
    listen: String,
    stop: mpsc::Sender<()>,
    handle: Option<thread::JoinHandle<()>>,
}

impl DemoWebhookServer {
    fn start(listen: &str) -> Result<Self> {
        let server = Server::http(listen)
            .map_err(|error| anyhow!("failed to listen on {listen}: {error}"))?;
        let listen = server.server_addr().to_string();
        let (stop, stop_receiver) = mpsc::channel();
        let handle = thread::spawn(move || run_demo_webhook_server(server, stop_receiver));
        Ok(Self {
            listen,
            stop,
            handle: Some(handle),
        })
    }

    fn listen(&self) -> &str {
        &self.listen
    }
}

impl Drop for DemoWebhookServer {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_demo_webhook_server(server: Server, stop: mpsc::Receiver<()>) {
    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        match server.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(request)) => respond_demo_webhook_request(request),
            Ok(None) => {}
            Err(_) => break,
        }
    }
}

fn respond_demo_webhook_request(request: Request) {
    let path = request.url().split('?').next().unwrap_or(request.url());
    let (status, body) = if path == "/" || path == "/webhook" {
        (
            StatusCode(200),
            "refray webhook setup listener\nThis temporary server only confirms that your public URL reaches this machine.\nAfter saving config, run refray serve for real webhooks.\n",
        )
    } else {
        (StatusCode(404), "not found\n")
    };
    let _ = request.respond(Response::from_string(body).with_status_code(status));
}

fn prompt_wizard_action_styled(theme: &ColorfulTheme) -> Result<WizardAction> {
    let options = [
        "Add another sync group",
        "Edit an existing group",
        "Delete an existing group",
        "Done",
    ];
    let index = Select::with_theme(theme)
        .with_prompt("What would you like to do?")
        .items(options)
        .default(0)
        .interact()?;

    Ok(match index {
        0 => WizardAction::AddSyncGroup,
        1 => WizardAction::EditSyncGroup,
        2 => WizardAction::DeleteSyncGroup,
        _ => WizardAction::Done,
    })
}

fn prompt_run_full_sync_now_styled(config: &Config, theme: &ColorfulTheme) -> Result<bool> {
    if config.mirrors.is_empty() {
        return Ok(false);
    }

    println!();
    Confirm::with_theme(theme)
        .with_prompt("Run full sync now?")
        .default(false)
        .interact()
        .map_err(Into::into)
}

fn edit_sync_group_styled(config: &mut Config, theme: &ColorfulTheme) -> Result<bool> {
    if config.mirrors.is_empty() {
        println!("{}", style("No sync groups to edit.").yellow());
        return Ok(false);
    }

    let mut options = numbered_sync_group_options(config);
    options.push("Back".to_string());
    let index = Select::with_theme(theme)
        .with_prompt("Edit sync group")
        .items(&options)
        .default(0)
        .interact()?;
    if index == config.mirrors.len() {
        return Ok(false);
    }

    println!(
        "{} {}",
        style("Editing").cyan().bold(),
        style(format!("sync group {}", index + 1)).cyan()
    );
    let existing = config.mirrors[index].endpoints.clone();
    let existing_sync_visibility = config.mirrors[index].sync_visibility.clone();
    let existing_repo_whitelist = config.mirrors[index].repo_whitelist.clone();
    let existing_repo_blacklist = config.mirrors[index].repo_blacklist.clone();
    let existing_conflict_resolution = config.mirrors[index].conflict_resolution.clone();
    let endpoints = prompt_sync_group_endpoints_styled(config, theme, &existing)?;
    let sync_visibility = prompt_sync_visibility_styled(theme, Some(&existing_sync_visibility))?;
    let existing_repo_filters = RepoFilterInput {
        whitelist: existing_repo_whitelist,
        blacklist: existing_repo_blacklist,
    };
    let repo_filters = prompt_repo_filters_styled(theme, Some(&existing_repo_filters))?;
    let conflict_resolution =
        prompt_conflict_resolution_styled(theme, Some(&existing_conflict_resolution))?;
    config.mirrors[index].endpoints = endpoints;
    config.mirrors[index].sync_visibility = sync_visibility;
    config.mirrors[index].repo_whitelist = repo_filters.whitelist;
    config.mirrors[index].repo_blacklist = repo_filters.blacklist;
    config.mirrors[index].conflict_resolution = conflict_resolution;
    prompt_webhook_setup_styled(config, theme)?;
    println!(
        "{} {}",
        style("updated").green().bold(),
        style(format!("sync group {}", index + 1)).cyan()
    );
    Ok(true)
}

fn delete_sync_group_styled(config: &mut Config, theme: &ColorfulTheme) -> Result<bool> {
    if config.mirrors.is_empty() {
        println!("{}", style("No sync groups to delete.").yellow());
        return Ok(false);
    }

    let mut options = numbered_sync_group_options(config);
    options.push("Back".to_string());
    let index = Select::with_theme(theme)
        .with_prompt("Delete sync group")
        .items(&options)
        .default(0)
        .interact()?;
    if index == config.mirrors.len() {
        return Ok(false);
    }
    let name = config.mirrors[index].name.clone();
    config.remove_mirror(&name)?;
    println!(
        "{} {}",
        style("deleted").red().bold(),
        style(format!("sync group {}", index + 1)).cyan()
    );
    Ok(true)
}

fn prompt_target_styled(
    theme: &ColorfulTheme,
    prompt: &str,
    default: Option<String>,
) -> Result<ProfileTarget> {
    let input = Input::<String>::with_theme(theme).with_prompt(prompt);
    let input = if let Some(default) = default {
        input.default(default)
    } else {
        input
    };
    let url = input
        .validate_with(|value: &String| validate_required(value))
        .interact_text()?;
    let parsed = parse_profile_url(&url)?;
    let provider = known_provider_from_host(&parsed.host)
        .or_else(|| detect_provider_from_instance(&parsed.base_url))
        .map(Ok)
        .unwrap_or_else(|| prompt_provider_styled(theme, &parsed.base_url))?;
    let kind = detect_namespace_kind_public(&provider, &parsed.base_url, &parsed.namespace);

    Ok(ProfileTarget {
        base_url: parsed.base_url,
        provider,
        namespace: parsed.namespace,
        kind,
    })
}

fn ensure_credentials_styled(
    config: &mut Config,
    target: ProfileTarget,
    theme: &ColorfulTheme,
) -> Result<EndpointConfig> {
    for site in matching_sites(config, &target) {
        let kind = match target.kind.clone().or_else(|| {
            detect_namespace_kind_with_site(site, &target)
                .ok()
                .flatten()
        }) {
            Some(kind) => kind,
            None => prompt_namespace_kind_styled(theme, &target.namespace)?,
        };
        let endpoint = target_endpoint(&target, kind, site.name.clone());
        if validate_site_for_endpoint(site, &endpoint).is_ok() {
            println!(
                "{} {}",
                style("Using existing credentials for").green(),
                style(endpoint_url(site, &endpoint)).cyan()
            );
            return Ok(endpoint);
        }
    }

    let mut transient = TransientCredentialOutput::new();
    transient.write_line(format_args!(
        "{} {}",
        style("No existing usable credentials for")
            .yellow()
            .for_stderr(),
        style(target_display(&target)).cyan().for_stderr()
    ))?;
    print_pat_instructions(&mut transient, &target.provider, &target.base_url)?;

    loop {
        let token = Password::with_theme(theme)
            .with_prompt("PAT token")
            .validate_with(|value: &String| validate_required(value))
            .interact_on(transient.term())?;
        transient.add_line();
        let site_name = default_site_name(config, &target.base_url, &target.provider);
        let site = SiteConfig {
            name: site_name,
            provider: target.provider.clone(),
            base_url: target.base_url.clone(),
            api_url: None,
            token: TokenConfig::Value(token),
            git_username: None,
        };

        let detected_kind = detect_namespace_kind_with_site(&site, &target)
            .ok()
            .flatten();
        let kind = match target.kind.clone().or(detected_kind) {
            Some(kind) => kind,
            None => {
                let kind = prompt_namespace_kind_styled(theme, &target.namespace)?;
                transient.add_line();
                kind
            }
        };
        let endpoint = target_endpoint(&target, kind, site.name.clone());

        transient.write_status_prefix(style("Checking PAT... ").dim().for_stderr())?;
        match validate_site_for_endpoint(&site, &endpoint) {
            Ok(()) => {
                transient.finish_status(style("valid").green().bold().for_stderr())?;
                transient.clear()?;
                let site_name = site.name.clone();
                config.upsert_site(site);
                return Ok(endpoint_with_site(&endpoint, site_name));
            }
            Err(error) => {
                transient.finish_status(style("failed").red().bold().for_stderr())?;
                eprintln!(
                    "{} {error:#}",
                    style("PAT validation error:").red().for_stderr()
                );
                if !Confirm::with_theme(theme)
                    .with_prompt("Try another PAT?")
                    .default(true)
                    .interact()?
                    && Confirm::with_theme(theme)
                        .with_prompt("Save this credential anyway?")
                        .default(false)
                        .interact()?
                {
                    let site_name = site.name.clone();
                    config.upsert_site(site);
                    return Ok(endpoint_with_site(&endpoint, site_name));
                }
                transient.reset();
            }
        }
    }
}

struct TransientCredentialOutput {
    term: Term,
    lines: usize,
    status_pending: bool,
}

impl TransientCredentialOutput {
    fn new() -> Self {
        Self {
            term: Term::stderr(),
            lines: 0,
            status_pending: false,
        }
    }

    fn term(&self) -> &Term {
        &self.term
    }

    fn write_line(&mut self, line: impl Display) -> Result<()> {
        self.term.write_line(&line.to_string())?;
        self.lines += 1;
        Ok(())
    }

    fn write_status_prefix(&mut self, prefix: impl Display) -> Result<()> {
        self.term.write_str(&prefix.to_string())?;
        self.term.flush()?;
        self.status_pending = true;
        Ok(())
    }

    fn finish_status(&mut self, status: impl Display) -> Result<()> {
        self.term.write_line(&status.to_string())?;
        if self.status_pending {
            self.lines += 1;
            self.status_pending = false;
        }
        Ok(())
    }

    fn add_line(&mut self) {
        self.lines += 1;
    }

    fn clear(&self) -> Result<()> {
        if self.lines > 0 && self.term.is_term() {
            self.term.clear_last_lines(self.lines)?;
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.lines = 0;
        self.status_pending = false;
    }
}

fn validate_site_for_endpoint(site: &SiteConfig, endpoint: &EndpointConfig) -> Result<()> {
    let client = ProviderClient::new(site)?;
    client.validate_token()?;
    client
        .list_repos(endpoint)
        .with_context(|| "token was valid, but repository access check failed")?;
    Ok(())
}

fn detect_namespace_kind_with_site(
    site: &SiteConfig,
    target: &ProfileTarget,
) -> Result<Option<NamespaceKind>> {
    ProviderClient::new(site)?.detect_namespace_kind(&target.namespace)
}

fn matching_sites<'a>(config: &'a Config, target: &ProfileTarget) -> Vec<&'a SiteConfig> {
    config
        .sites
        .iter()
        .filter(|site| {
            site.provider == target.provider
                && trim_url_end(&site.base_url) == trim_url_end(&target.base_url)
        })
        .collect()
}

fn prompt_provider_styled(theme: &ColorfulTheme, base_url: &str) -> Result<ProviderKind> {
    let options = ["GitHub", "GitLab", "Gitea", "Forgejo"];
    let index = Select::with_theme(theme)
        .with_prompt(format!("Provider for {base_url}"))
        .items(options)
        .default(0)
        .interact()?;
    Ok(match index {
        0 => ProviderKind::Github,
        1 => ProviderKind::Gitlab,
        2 => ProviderKind::Gitea,
        _ => ProviderKind::Forgejo,
    })
}

fn prompt_namespace_kind_styled(theme: &ColorfulTheme, namespace: &str) -> Result<NamespaceKind> {
    let options = ["User", "Organization", "Group"];
    let index = Select::with_theme(theme)
        .with_prompt(format!("What is {namespace}?"))
        .items(options)
        .default(0)
        .interact()?;
    Ok(match index {
        0 => NamespaceKind::User,
        1 => NamespaceKind::Org,
        _ => NamespaceKind::Group,
    })
}

fn prompt_conflict_resolution_styled(
    theme: &ColorfulTheme,
    existing: Option<&ConflictResolutionStrategy>,
) -> Result<ConflictResolutionStrategy> {
    let options = [
        "Fail",
        "Auto-rebase; fail on file conflict",
        "Pull request",
        "Auto-rebase; pull request on file conflict (recommended)",
    ];
    let default = existing.map(conflict_resolution_index).unwrap_or(3);
    let index = Select::with_theme(theme)
        .with_prompt("How should refray resolve branch conflicts?")
        .items(options)
        .default(default)
        .interact()?;
    Ok(conflict_resolution_from_index(index))
}

fn prompt_sync_visibility_styled(
    theme: &ColorfulTheme,
    existing: Option<&SyncVisibility>,
) -> Result<SyncVisibility> {
    let options = [
        "All repositories",
        "Only private repositories",
        "Only public repositories",
    ];
    let default = existing.map(sync_visibility_index).unwrap_or(0);
    let index = Select::with_theme(theme)
        .with_prompt("Which repositories should this sync group include?")
        .items(options)
        .default(default)
        .interact()?;
    Ok(sync_visibility_from_index(index))
}

fn prompt_repo_filters_styled(
    theme: &ColorfulTheme,
    existing: Option<&RepoFilterInput>,
) -> Result<RepoFilterInput> {
    let existing = existing.cloned().unwrap_or_default();
    let has_existing = !existing.whitelist.is_empty() || !existing.blacklist.is_empty();
    if !Confirm::with_theme(theme)
        .with_prompt("Configure repository name whitelist/blacklist?")
        .default(has_existing)
        .interact()?
    {
        return Ok(RepoFilterInput::default());
    }

    Ok(RepoFilterInput {
        whitelist: prompt_repo_pattern_list_styled(
            theme,
            "Whitelist regexes (comma-separated, empty means all repo names)",
            &existing.whitelist,
        )?,
        blacklist: prompt_repo_pattern_list_styled(
            theme,
            "Blacklist regexes (comma-separated)",
            &existing.blacklist,
        )?,
    })
}

fn prompt_repo_pattern_list_styled(
    theme: &ColorfulTheme,
    prompt: &str,
    existing: &[String],
) -> Result<Vec<String>> {
    let input = Input::<String>::with_theme(theme)
        .with_prompt(prompt)
        .allow_empty(true)
        .validate_with(|value: &String| validate_repo_pattern_list(value));
    let input = if existing.is_empty() {
        input
    } else {
        input.default(existing.join(", "))
    };
    let value = input.interact_text()?;
    Ok(parse_repo_pattern_list(&value))
}

fn validate_repo_pattern_list(value: &str) -> std::result::Result<(), String> {
    for pattern in parse_repo_pattern_list(value) {
        Regex::new(&pattern).map_err(|error| format!("invalid regex '{pattern}': {error}"))?;
    }
    Ok(())
}

fn parse_repo_pattern_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn sync_visibility_index(sync_visibility: &SyncVisibility) -> usize {
    match sync_visibility {
        SyncVisibility::All => 0,
        SyncVisibility::Private => 1,
        SyncVisibility::Public => 2,
    }
}

fn sync_visibility_from_index(index: usize) -> SyncVisibility {
    match index {
        1 => SyncVisibility::Private,
        2 => SyncVisibility::Public,
        _ => SyncVisibility::All,
    }
}

fn conflict_resolution_index(strategy: &ConflictResolutionStrategy) -> usize {
    match strategy {
        ConflictResolutionStrategy::Fail => 0,
        ConflictResolutionStrategy::AutoRebase => 1,
        ConflictResolutionStrategy::PullRequest => 2,
        ConflictResolutionStrategy::AutoRebasePullRequest => 3,
    }
}

fn conflict_resolution_from_index(index: usize) -> ConflictResolutionStrategy {
    match index {
        0 => ConflictResolutionStrategy::Fail,
        1 => ConflictResolutionStrategy::AutoRebase,
        2 => ConflictResolutionStrategy::PullRequest,
        _ => ConflictResolutionStrategy::AutoRebasePullRequest,
    }
}

fn print_sync_groups(config: &Config) {
    println!();
    println!("{}", style("Sync groups").cyan().bold());
    if config.mirrors.is_empty() {
        println!(
            "  {} {}",
            style("-").cyan(),
            style("No sync groups configured.").dim()
        );
        println!();
        return;
    }
    for (index, mirror) in config.mirrors.iter().enumerate() {
        println!("  {}. {}", index + 1, sync_group_summary(config, mirror));
    }
    println!();
}

fn numbered_sync_group_options(config: &Config) -> Vec<String> {
    config
        .mirrors
        .iter()
        .enumerate()
        .map(|(index, mirror)| format!("{}. {}", index + 1, sync_group_summary(config, mirror)))
        .collect()
}

#[cfg(test)]
fn sync_group_summaries(config: &Config) -> Vec<String> {
    config
        .mirrors
        .iter()
        .map(|mirror| sync_group_summary(config, mirror))
        .collect()
}

fn sync_group_summary(config: &Config, mirror: &MirrorConfig) -> String {
    let endpoints = mirror
        .endpoints
        .iter()
        .map(|endpoint| {
            config
                .site(&endpoint.site)
                .map(|site| endpoint_url(site, endpoint))
                .unwrap_or_else(|| format!("{}:{}", endpoint.site, endpoint.namespace))
        })
        .collect::<Vec<_>>()
        .join(" <-> ");
    format!(
        "{} ({}, {}, {})",
        endpoints,
        sync_visibility_label(&mirror.sync_visibility),
        repo_filter_label(mirror),
        conflict_resolution_label(&mirror.conflict_resolution)
    )
}

fn sync_visibility_label(sync_visibility: &SyncVisibility) -> &'static str {
    match sync_visibility {
        SyncVisibility::All => "sync: all",
        SyncVisibility::Private => "sync: private only",
        SyncVisibility::Public => "sync: public only",
    }
}

fn repo_filter_label(mirror: &MirrorConfig) -> String {
    match (mirror.repo_whitelist.len(), mirror.repo_blacklist.len()) {
        (0, 0) => "repos: all names".to_string(),
        (whitelist, 0) => format!("repos: whitelist {whitelist}"),
        (0, blacklist) => format!("repos: blacklist {blacklist}"),
        (whitelist, blacklist) => {
            format!("repos: whitelist {whitelist}, blacklist {blacklist}")
        }
    }
}

fn conflict_resolution_label(strategy: &ConflictResolutionStrategy) -> &'static str {
    match strategy {
        ConflictResolutionStrategy::Fail => "conflicts: fail",
        ConflictResolutionStrategy::AutoRebase => "conflicts: auto-rebase",
        ConflictResolutionStrategy::PullRequest => "conflicts: pull request",
        ConflictResolutionStrategy::AutoRebasePullRequest => {
            "conflicts: auto-rebase + pull request"
        }
    }
}

fn print_pat_instructions(
    output: &mut TransientCredentialOutput,
    provider: &ProviderKind,
    base_url: &str,
) -> Result<()> {
    output.write_line(style("PAT setup").cyan().bold().for_stderr())?;
    for line in pat_instruction_lines(provider, base_url) {
        output.write_line(format_args!("  {} {line}", style("-").cyan().for_stderr()))?;
    }
    Ok(())
}

fn pat_instruction_lines(provider: &ProviderKind, base_url: &str) -> Vec<String> {
    let url = token_creation_url(provider, base_url);
    match provider {
        ProviderKind::Github => vec![
            "Create a classic PAT with repo permissions.".to_string(),
            format!("Open: {url}"),
            "Generate new token (classic), select repo, generate, then paste the token here."
                .to_string(),
        ],
        ProviderKind::Gitlab => vec![
            "Create a personal access token with API and repository write permissions.".to_string(),
            format!("Open: {url}"),
            "Select api and write_repository, create the token, then paste it here.".to_string(),
        ],
        ProviderKind::Gitea => vec![
            "Create a personal access token with repository permissions.".to_string(),
            format!("Open: {url}"),
            "Generate a new token, allow repository access, then paste it here.".to_string(),
        ],
        ProviderKind::Forgejo => vec![
            "Create a personal access token with repository permissions.".to_string(),
            format!("Open: {url}"),
            "Generate a new token, allow repository access, then paste it here.".to_string(),
        ],
    }
}

#[cfg(test)]
#[path = "../tests/unit/interactive_test_io.rs"]
mod test_io;
#[cfg(test)]
use test_io::*;

fn parse_profile_url(value: &str) -> Result<ParsedProfileUrl> {
    let normalized = ensure_url_scheme(value);
    let parsed =
        Url::parse(&normalized).with_context(|| format!("invalid profile URL '{value}'"))?;
    let host = parsed
        .host_str()
        .context("profile URL must include a host")?
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    let namespace = parsed
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|path| !path.is_empty())
        .context("profile URL must include a user or organization path")?;

    let mut base_url = format!("{}://{}", parsed.scheme(), host);
    if let Some(port) = parsed.port() {
        base_url.push_str(&format!(":{port}"));
    }

    Ok(ParsedProfileUrl {
        base_url,
        host,
        namespace,
    })
}

fn known_provider_from_host(host: &str) -> Option<ProviderKind> {
    let host = host.trim_start_matches("www.").to_ascii_lowercase();
    if host == "github.com" || host.ends_with(".github.com") || host.contains("github") {
        Some(ProviderKind::Github)
    } else if host == "gitlab.com" || host.ends_with(".gitlab.com") || host.contains("gitlab") {
        Some(ProviderKind::Gitlab)
    } else if host == "codeberg.org" || host.contains("forgejo") {
        Some(ProviderKind::Forgejo)
    } else if host.contains("gitea") {
        Some(ProviderKind::Gitea)
    } else {
        None
    }
}

fn detect_provider_from_instance(base_url: &str) -> Option<ProviderKind> {
    let client = Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let base = trim_url_end(base_url);
    if client
        .get(format!("{base}/api/forgejo/v1/version"))
        .send()
        .ok()
        .is_some_and(|response| response.status().is_success())
    {
        return Some(ProviderKind::Forgejo);
    }
    if client
        .get(format!("{base}/api/v1/version"))
        .send()
        .ok()
        .is_some_and(|response| response.status().is_success())
    {
        return Some(ProviderKind::Gitea);
    }
    if client
        .get(format!("{base}/api/v4/version"))
        .send()
        .ok()
        .is_some_and(|response| response.status().is_success())
    {
        return Some(ProviderKind::Gitlab);
    }
    if client
        .get(format!("{base}/api/v3/meta"))
        .send()
        .ok()
        .is_some_and(|response| response.status().is_success())
    {
        return Some(ProviderKind::Github);
    }
    None
}

fn detect_namespace_kind_public(
    provider: &ProviderKind,
    base_url: &str,
    namespace: &str,
) -> Option<NamespaceKind> {
    let client = Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let site = SiteConfig {
        name: "detect".to_string(),
        provider: provider.clone(),
        base_url: base_url.to_string(),
        api_url: None,
        token: TokenConfig::Value(String::new()),
        git_username: None,
    };
    let api_base = site.api_base();
    match provider {
        ProviderKind::Github => {
            let url = format!("{api_base}/users/{namespace}");
            let value = client
                .get(url)
                .send()
                .ok()?
                .json::<serde_json::Value>()
                .ok()?;
            match value.get("type")?.as_str()? {
                "Organization" => Some(NamespaceKind::Org),
                "User" => Some(NamespaceKind::User),
                _ => None,
            }
        }
        ProviderKind::Gitlab => {
            let encoded = urlencoding(namespace);
            if client
                .get(format!("{api_base}/groups/{encoded}"))
                .send()
                .ok()
                .is_some_and(|response| response.status().is_success())
            {
                return Some(NamespaceKind::Group);
            }
            let encoded = urlencoding(namespace.rsplit('/').next().unwrap_or(namespace));
            let users = client
                .get(format!("{api_base}/users?username={encoded}"))
                .send()
                .ok()?
                .json::<serde_json::Value>()
                .ok()?;
            users
                .as_array()
                .is_some_and(|items| !items.is_empty())
                .then_some(NamespaceKind::User)
        }
        ProviderKind::Gitea | ProviderKind::Forgejo => {
            if client
                .get(format!("{api_base}/orgs/{namespace}"))
                .send()
                .ok()
                .is_some_and(|response| response.status().is_success())
            {
                return Some(NamespaceKind::Org);
            }
            client
                .get(format!("{api_base}/users/{namespace}"))
                .send()
                .ok()
                .is_some_and(|response| response.status().is_success())
                .then_some(NamespaceKind::User)
        }
    }
}

fn endpoint_with_site(endpoint: &EndpointConfig, site: String) -> EndpointConfig {
    EndpointConfig {
        site,
        kind: endpoint.kind.clone(),
        namespace: endpoint.namespace.clone(),
    }
}

fn target_endpoint(target: &ProfileTarget, kind: NamespaceKind, site: String) -> EndpointConfig {
    EndpointConfig {
        site,
        kind,
        namespace: target.namespace.clone(),
    }
}

fn target_display(target: &ProfileTarget) -> String {
    format!("{}/{}", trim_url_scheme(&target.base_url), target.namespace)
}

fn endpoint_url(site: &SiteConfig, endpoint: &EndpointConfig) -> String {
    format!("{}/{}", trim_url_scheme(&site.base_url), endpoint.namespace)
}

fn endpoint_profile_url(config: &Config, endpoint: Option<&EndpointConfig>) -> Option<String> {
    let endpoint = endpoint?;
    let site = config.site(&endpoint.site)?;
    Some(format!(
        "{}/{}",
        trim_url_end(&site.base_url),
        endpoint.namespace
    ))
}

fn trim_url_scheme(value: &str) -> String {
    value
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

fn next_mirror_name(config: &Config) -> String {
    for index in 1.. {
        let candidate = format!("sync-{index}");
        if config.mirrors.iter().all(|mirror| mirror.name != candidate) {
            return candidate;
        }
    }
    unreachable!("unbounded suffix search should always return")
}

fn default_base_url(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Github => "https://github.com",
        ProviderKind::Gitlab => "https://gitlab.com",
        ProviderKind::Gitea => "https://gitea.example.com",
        ProviderKind::Forgejo => "https://forgejo.example.com",
    }
}

fn default_site_name(config: &Config, base_url: &str, provider: &ProviderKind) -> String {
    let base = if trim_url_end(base_url) == default_base_url(provider) {
        provider_slug(provider).to_string()
    } else {
        site_name_from_url(base_url).unwrap_or_else(|| provider_slug(provider).to_string())
    };
    if config.site(&base).is_none() {
        return base;
    }

    for suffix in 2.. {
        let candidate = format!("{base}-{suffix}");
        if config.site(&candidate).is_none() {
            return candidate;
        }
    }
    unreachable!("unbounded suffix search should always return")
}

fn site_name_from_url(base_url: &str) -> Option<String> {
    let normalized_url = ensure_url_scheme(base_url);
    let parsed = Url::parse(&normalized_url).ok()?;
    let host = parsed
        .host_str()?
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    let mut labels = host.split('.').collect::<Vec<_>>();
    if matches!(
        labels.last(),
        Some(&"com" | &"org" | &"net" | &"io" | &"dev")
    ) {
        labels.pop();
    }
    let candidate = labels.join("-");
    let normalized = normalize_site_name(&candidate);
    (!normalized.is_empty()).then_some(normalized)
}

fn normalize_site_name(value: &str) -> String {
    let mut output = String::new();
    let mut previous_dash = false;
    for ch in value.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            output.push(ch);
            previous_dash = false;
        } else if !previous_dash {
            output.push('-');
            previous_dash = true;
        }
    }
    output.trim_matches('-').to_string()
}

fn provider_slug(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Github => "github",
        ProviderKind::Gitlab => "gitlab",
        ProviderKind::Gitea => "gitea",
        ProviderKind::Forgejo => "forgejo",
    }
}

fn token_creation_url(provider: &ProviderKind, base_url: &str) -> String {
    let base = ensure_url_scheme(base_url)
        .trim_end_matches('/')
        .to_string();
    match provider {
        ProviderKind::Github => format!("{base}/settings/tokens"),
        ProviderKind::Gitlab => {
            format!(
                "{base}/-/user_settings/personal_access_tokens?name=refray&scopes=api,write_repository"
            )
        }
        ProviderKind::Gitea => format!("{base}/user/settings/applications"),
        ProviderKind::Forgejo => format!("{base}/user/settings/applications"),
    }
}

fn ensure_url_scheme(value: &str) -> String {
    if value.contains("://") {
        value.to_string()
    } else {
        format!("https://{value}")
    }
}

fn trim_url_end(value: &str) -> &str {
    value.trim_end_matches('/')
}

fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn validate_required(value: &str) -> std::result::Result<(), String> {
    if value.trim().is_empty() {
        Err("A value is required".to_string())
    } else {
        Ok(())
    }
}

fn validate_url(value: &str) -> std::result::Result<(), String> {
    validate_required(value)?;
    let url = Url::parse(value).map_err(|error| format!("Invalid URL: {error}"))?;
    match url.scheme() {
        "http" | "https" => Ok(()),
        _ => Err("URL must start with http:// or https://".to_string()),
    }
}

fn generate_webhook_secret() -> String {
    let mut bytes = [0_u8; 32];
    if File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_err()
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = ((nanos >> ((index % 16) * 8)) & 0xff) as u8;
        }
    }
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[cfg(test)]
#[path = "../tests/unit/interactive.rs"]
mod tests;
