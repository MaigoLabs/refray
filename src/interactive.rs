use std::fmt::Display;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use console::{Term, style};
use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};
use reqwest::blocking::Client;
use url::Url;

#[cfg(test)]
use anyhow::bail;
#[cfg(test)]
use std::io::{BufRead, Write};

use crate::config::{
    Config, EndpointConfig, MirrorConfig, NamespaceKind, ProviderKind, SiteConfig, TokenConfig,
    Visibility,
};
use crate::provider::ProviderClient;

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

pub fn run_config_wizard(path: &Path) -> Result<()> {
    let existing_config = path.exists();
    let mut config = Config::load_or_default(path)?;
    let theme = ColorfulTheme::default();

    println!();
    println!("{}", style("git-sync configuration wizard").cyan().bold());
    let description = if existing_config {
        "Review, add, or delete sync groups."
    } else {
        "Enter profile or organization URLs, then git-sync will build the mirror group."
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
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WizardAction {
    AddSyncGroup,
    DeleteSyncGroup,
    Done,
}

fn add_sync_group_styled(config: &mut Config, theme: &ColorfulTheme) -> Result<()> {
    let mut endpoints = Vec::new();

    let first = prompt_target_styled(theme, "Profile/org URL")?;
    endpoints.push(ensure_credentials_styled(config, first, theme)?);

    let second = prompt_target_styled(theme, "Profile/org URL to sync with")?;
    endpoints.push(ensure_credentials_styled(config, second, theme)?);

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
        let next = prompt_target_styled(theme, "Additional profile/org URL")?;
        endpoints.push(ensure_credentials_styled(config, next, theme)?);
    }

    config.upsert_mirror(MirrorConfig {
        name: next_mirror_name(config),
        endpoints,
        create_missing: true,
        visibility: Visibility::Private,
        allow_force: false,
    });

    Ok(())
}

fn prompt_wizard_action_styled(theme: &ColorfulTheme) -> Result<WizardAction> {
    let options = ["Add another sync group", "Delete an existing group", "Done"];
    let index = Select::with_theme(theme)
        .with_prompt("What would you like to do?")
        .items(options)
        .default(0)
        .interact()?;

    Ok(match index {
        0 => WizardAction::AddSyncGroup,
        1 => WizardAction::DeleteSyncGroup,
        _ => WizardAction::Done,
    })
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

fn prompt_target_styled(theme: &ColorfulTheme, prompt: &str) -> Result<ProfileTarget> {
    let url = Input::<String>::with_theme(theme)
        .with_prompt(prompt)
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
    let options = ["GitHub", "GitLab", "Gitea"];
    let index = Select::with_theme(theme)
        .with_prompt(format!("Provider for {base_url}"))
        .items(options)
        .default(0)
        .interact()?;
    Ok(match index {
        0 => ProviderKind::Github,
        1 => ProviderKind::Gitlab,
        _ => ProviderKind::Gitea,
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
    mirror
        .endpoints
        .iter()
        .map(|endpoint| {
            config
                .site(&endpoint.site)
                .map(|site| endpoint_url(site, endpoint))
                .unwrap_or_else(|| format!("{}:{}", endpoint.site, endpoint.namespace))
        })
        .collect::<Vec<_>>()
        .join(" <-> ")
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
            "Create a personal access token with API permissions.".to_string(),
            format!("Open: {url}"),
            "Select api, create the token, then paste it here.".to_string(),
        ],
        ProviderKind::Gitea => vec![
            "Create a personal access token with repository permissions.".to_string(),
            format!("Open: {url}"),
            "Generate a new token, allow repository access, then paste it here.".to_string(),
        ],
    }
}

#[cfg(test)]
pub fn run_config_wizard_with_io<R, W>(
    mut config: Config,
    reader: &mut R,
    writer: &mut W,
) -> Result<Config>
where
    R: BufRead,
    W: Write,
{
    writeln!(writer, "git-sync configuration wizard")?;
    if config.mirrors.is_empty() {
        add_sync_group(reader, writer, &mut config)?;
        write_sync_groups(&config, writer)?;
    } else {
        write_sync_groups(&config, writer)?;
    }

    loop {
        match prompt_wizard_action(reader, writer)? {
            WizardAction::AddSyncGroup => {
                add_sync_group(reader, writer, &mut config)?;
                write_sync_groups(&config, writer)?;
            }
            WizardAction::DeleteSyncGroup => {
                if delete_sync_group(reader, writer, &mut config)? {
                    write_sync_groups(&config, writer)?;
                }
            }
            WizardAction::Done => break,
        }
    }
    Ok(config)
}

#[cfg(test)]
fn add_sync_group<R, W>(reader: &mut R, writer: &mut W, config: &mut Config) -> Result<()>
where
    R: BufRead,
    W: Write,
{
    let mut endpoints = Vec::new();
    let first = prompt_target(reader, writer, "Profile/org URL")?;
    endpoints.push(ensure_credentials(config, first, reader, writer)?);
    let second = prompt_target(reader, writer, "Profile/org URL to sync with")?;
    endpoints.push(ensure_credentials(config, second, reader, writer)?);

    while prompt_bool(
        reader,
        writer,
        "Add a third endpoint for 3-way sync?",
        false,
    )? {
        let next = prompt_target(reader, writer, "Additional profile/org URL")?;
        endpoints.push(ensure_credentials(config, next, reader, writer)?);
    }

    config.upsert_mirror(MirrorConfig {
        name: next_mirror_name(config),
        endpoints,
        create_missing: true,
        visibility: Visibility::Private,
        allow_force: false,
    });
    Ok(())
}

#[cfg(test)]
fn prompt_wizard_action<R, W>(reader: &mut R, writer: &mut W) -> Result<WizardAction>
where
    R: BufRead,
    W: Write,
{
    loop {
        writeln!(writer, "What would you like to do?")?;
        writeln!(writer, "  1. Add another sync group")?;
        writeln!(writer, "  2. Delete an existing group")?;
        writeln!(writer, "  3. Done")?;
        write!(writer, "Choose an option: ")?;
        writer.flush()?;
        let value = read_line(reader)?.trim().to_ascii_lowercase();
        match value.as_str() {
            "1" | "add" | "add another sync group" => return Ok(WizardAction::AddSyncGroup),
            "2" | "delete" | "delete an existing group" => {
                return Ok(WizardAction::DeleteSyncGroup);
            }
            "3" | "done" | "finish" => return Ok(WizardAction::Done),
            _ => writeln!(writer, "Enter 1, 2, or 3.")?,
        }
    }
}

#[cfg(test)]
fn delete_sync_group<R, W>(reader: &mut R, writer: &mut W, config: &mut Config) -> Result<bool>
where
    R: BufRead,
    W: Write,
{
    if config.mirrors.is_empty() {
        writeln!(writer, "No sync groups to delete.")?;
        return Ok(false);
    }

    loop {
        writeln!(writer, "Delete sync group")?;
        for (index, option) in sync_group_summaries(config).iter().enumerate() {
            writeln!(writer, "  {}. {}", index + 1, option)?;
        }
        writeln!(writer, "  {}. Back", config.mirrors.len() + 1)?;
        write!(writer, "Choose a sync group: ")?;
        writer.flush()?;
        let value = read_line(reader)?.trim().to_ascii_lowercase();
        if value == "b" || value == "back" {
            return Ok(false);
        }
        match value.parse::<usize>() {
            Ok(index) if (1..=config.mirrors.len()).contains(&index) => {
                let name = config.mirrors[index - 1].name.clone();
                config.remove_mirror(&name)?;
                writeln!(writer, "deleted sync group {index}")?;
                return Ok(true);
            }
            Ok(index) if index == config.mirrors.len() + 1 => return Ok(false),
            _ => writeln!(writer, "Enter a sync group number, or choose Back.")?,
        }
    }
}

#[cfg(test)]
fn prompt_target<R, W>(reader: &mut R, writer: &mut W, prompt: &str) -> Result<ProfileTarget>
where
    R: BufRead,
    W: Write,
{
    let url = prompt_required(reader, writer, prompt)?;
    let parsed = parse_profile_url(&url)?;
    let provider = known_provider_from_host(&parsed.host).unwrap_or_else(|| {
        prompt_provider(reader, writer, &parsed.base_url).expect("provider prompt failed")
    });
    Ok(ProfileTarget {
        base_url: parsed.base_url,
        provider,
        namespace: parsed.namespace,
        kind: None,
    })
}

#[cfg(test)]
fn ensure_credentials<R, W>(
    config: &mut Config,
    target: ProfileTarget,
    reader: &mut R,
    writer: &mut W,
) -> Result<EndpointConfig>
where
    R: BufRead,
    W: Write,
{
    if let Some(site) = config.sites.iter().find(|site| {
        site.provider == target.provider
            && trim_url_end(&site.base_url) == trim_url_end(&target.base_url)
    }) {
        let kind = target.kind.clone().unwrap_or_else(|| {
            prompt_namespace_kind(reader, writer, &target.namespace).expect("kind prompt failed")
        });
        let endpoint = target_endpoint(&target, kind, site.name.clone());
        writeln!(
            writer,
            "Using existing credentials for {}",
            target_display(&target)
        )?;
        return Ok(endpoint);
    }

    for line in pat_instruction_lines(&target.provider, &target.base_url) {
        writeln!(writer, "{line}")?;
    }
    let token = prompt_required(reader, writer, "PAT token")?;
    let site = SiteConfig {
        name: default_site_name(config, &target.base_url, &target.provider),
        provider: target.provider.clone(),
        base_url: target.base_url.clone(),
        api_url: None,
        token: TokenConfig::Value(token),
        git_username: None,
    };
    let site_name = site.name.clone();
    config.upsert_site(site);
    let kind = target.kind.clone().unwrap_or_else(|| {
        prompt_namespace_kind(reader, writer, &target.namespace).expect("kind prompt failed")
    });
    Ok(target_endpoint(&target, kind, site_name))
}

#[cfg(test)]
fn prompt_provider<R, W>(reader: &mut R, writer: &mut W, base_url: &str) -> Result<ProviderKind>
where
    R: BufRead,
    W: Write,
{
    loop {
        let value = prompt_required(reader, writer, &format!("Provider for {base_url}"))?;
        match value.to_ascii_lowercase().as_str() {
            "github" => return Ok(ProviderKind::Github),
            "gitlab" => return Ok(ProviderKind::Gitlab),
            "gitea" => return Ok(ProviderKind::Gitea),
            _ => writeln!(writer, "Provider must be github, gitlab, or gitea.")?,
        }
    }
}

#[cfg(test)]
fn prompt_namespace_kind<R, W>(
    reader: &mut R,
    writer: &mut W,
    namespace: &str,
) -> Result<NamespaceKind>
where
    R: BufRead,
    W: Write,
{
    loop {
        let value = prompt_with_default(reader, writer, &format!("What is {namespace}?"), "user")?;
        match value.to_ascii_lowercase().as_str() {
            "user" => return Ok(NamespaceKind::User),
            "org" | "organization" => return Ok(NamespaceKind::Org),
            "group" => return Ok(NamespaceKind::Group),
            _ => writeln!(writer, "Namespace kind must be user, org, or group.")?,
        }
    }
}

#[cfg(test)]
fn write_sync_groups<W>(config: &Config, writer: &mut W) -> Result<()>
where
    W: Write,
{
    writeln!(writer, "Sync groups")?;
    if config.mirrors.is_empty() {
        writeln!(writer, "No sync groups configured.")?;
        return Ok(());
    }
    for (index, mirror) in config.mirrors.iter().enumerate() {
        writeln!(
            writer,
            "{}. {}",
            index + 1,
            sync_group_summary(config, mirror)
        )?;
    }
    Ok(())
}

#[cfg(test)]
fn prompt_bool<R, W>(reader: &mut R, writer: &mut W, label: &str, default: bool) -> Result<bool>
where
    R: BufRead,
    W: Write,
{
    let default_label = if default { "Y/n" } else { "y/N" };
    loop {
        write!(writer, "{label} [{default_label}]: ")?;
        writer.flush()?;
        let value = read_line(reader)?.trim().to_ascii_lowercase();
        match value.as_str() {
            "" => return Ok(default),
            "y" | "yes" | "true" => return Ok(true),
            "n" | "no" | "false" => return Ok(false),
            _ => writeln!(writer, "Enter yes or no.")?,
        }
    }
}

#[cfg(test)]
fn prompt_required<R, W>(reader: &mut R, writer: &mut W, label: &str) -> Result<String>
where
    R: BufRead,
    W: Write,
{
    loop {
        write!(writer, "{label}: ")?;
        writer.flush()?;
        let value = read_line(reader)?.trim().to_string();
        if !value.is_empty() {
            return Ok(value);
        }
        writeln!(writer, "A value is required.")?;
    }
}

#[cfg(test)]
fn prompt_with_default<R, W>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    default: &str,
) -> Result<String>
where
    R: BufRead,
    W: Write,
{
    write!(writer, "{label} [{default}]: ")?;
    writer.flush()?;
    let value = read_line(reader)?.trim().to_string();
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value)
    }
}

#[cfg(test)]
fn read_line<R>(reader: &mut R) -> Result<String>
where
    R: BufRead,
{
    let mut value = String::new();
    let bytes = reader.read_line(&mut value)?;
    if bytes == 0 {
        bail!("unexpected end of input while reading interactive configuration");
    }
    Ok(value)
}

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
        ProviderKind::Gitea => {
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
    }
}

fn token_creation_url(provider: &ProviderKind, base_url: &str) -> String {
    let base = ensure_url_scheme(base_url)
        .trim_end_matches('/')
        .to_string();
    match provider {
        ProviderKind::Github => format!("{base}/settings/tokens"),
        ProviderKind::Gitlab => {
            format!("{base}/-/user_settings/personal_access_tokens?name=git-sync&scopes=api")
        }
        ProviderKind::Gitea => format!("{base}/user/settings/applications"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn wizard_builds_sync_group_from_profile_urls() {
        let input = [
            "https://github.com/hykilpikonna",
            "gh-token",
            "",
            "https://gitea.example.test/azalea",
            "gt-token",
            "",
            "n",
            "3",
        ]
        .join("\n")
            + "\n";
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();

        let config =
            run_config_wizard_with_io(Config::default(), &mut reader, &mut output).unwrap();

        assert_eq!(config.sites.len(), 2);
        assert_eq!(config.sites[0].name, "github");
        assert_eq!(config.sites[0].provider, ProviderKind::Github);
        assert_eq!(config.sites[0].base_url, "https://github.com");
        assert_eq!(
            config.sites[0].token,
            TokenConfig::Value("gh-token".to_string())
        );
        assert_eq!(config.sites[1].name, "gitea-example-test");
        assert_eq!(config.sites[1].provider, ProviderKind::Gitea);
        assert_eq!(config.sites[1].base_url, "https://gitea.example.test");

        assert_eq!(config.mirrors.len(), 1);
        assert_eq!(config.mirrors[0].name, "sync-1");
        assert_eq!(config.mirrors[0].endpoints.len(), 2);
        assert_eq!(config.mirrors[0].endpoints[0].site, "github");
        assert_eq!(config.mirrors[0].endpoints[0].kind, NamespaceKind::User);
        assert_eq!(config.mirrors[0].endpoints[0].namespace, "hykilpikonna");
        assert_eq!(config.mirrors[0].endpoints[1].site, "gitea-example-test");
        assert_eq!(config.mirrors[0].endpoints[1].namespace, "azalea");
        assert!(config.mirrors[0].create_missing);
        assert_eq!(config.mirrors[0].visibility, Visibility::Private);
        assert!(!config.mirrors[0].allow_force);

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("1. github.com/hykilpikonna <-> gitea.example.test/azalea"));
        assert!(output.contains("Add another sync group"));
        assert!(output.contains("Delete an existing group"));
        assert!(output.contains("Done"));
    }

    #[test]
    fn wizard_can_build_three_way_sync() {
        let input = [
            "https://github.com/alice",
            "gh-token",
            "",
            "https://gitlab.com/alice",
            "gl-token",
            "",
            "y",
            "https://gitea.example.test/alice",
            "gt-token",
            "",
            "n",
            "3",
        ]
        .join("\n")
            + "\n";
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();

        let config =
            run_config_wizard_with_io(Config::default(), &mut reader, &mut output).unwrap();

        assert_eq!(config.mirrors.len(), 1);
        assert_eq!(config.mirrors[0].endpoints.len(), 3);
        assert_eq!(config.sites.len(), 3);
    }

    #[test]
    fn wizard_reuses_existing_credentials_for_same_instance() {
        let config = Config {
            sites: vec![SiteConfig {
                name: "github".to_string(),
                provider: ProviderKind::Github,
                base_url: "https://github.com".to_string(),
                api_url: None,
                token: TokenConfig::Value("existing".to_string()),
                git_username: None,
            }],
            mirrors: Vec::new(),
        };
        let input = [
            "https://github.com/alice",
            "",
            "https://github.com/bob",
            "",
            "n",
            "3",
        ]
        .join("\n")
            + "\n";
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();

        let updated = run_config_wizard_with_io(config, &mut reader, &mut output).unwrap();

        assert_eq!(updated.sites.len(), 1);
        assert_eq!(updated.mirrors[0].endpoints[0].site, "github");
        assert_eq!(updated.mirrors[0].endpoints[1].site, "github");
    }

    #[test]
    fn wizard_starts_existing_config_at_sync_group_menu() {
        let config = Config {
            sites: vec![
                SiteConfig {
                    name: "github".to_string(),
                    provider: ProviderKind::Github,
                    base_url: "https://github.com".to_string(),
                    api_url: None,
                    token: TokenConfig::Value("existing-gh".to_string()),
                    git_username: None,
                },
                SiteConfig {
                    name: "gitea".to_string(),
                    provider: ProviderKind::Gitea,
                    base_url: "https://gitea.example.test".to_string(),
                    api_url: None,
                    token: TokenConfig::Value("existing-gt".to_string()),
                    git_username: None,
                },
            ],
            mirrors: vec![MirrorConfig {
                name: "sync-1".to_string(),
                endpoints: vec![
                    EndpointConfig {
                        site: "github".to_string(),
                        kind: NamespaceKind::User,
                        namespace: "alice".to_string(),
                    },
                    EndpointConfig {
                        site: "gitea".to_string(),
                        kind: NamespaceKind::User,
                        namespace: "alice".to_string(),
                    },
                ],
                create_missing: true,
                visibility: Visibility::Private,
                allow_force: false,
            }],
        };
        let mut reader = Cursor::new(b"3\n".as_slice());
        let mut output = Vec::new();

        let updated = run_config_wizard_with_io(config, &mut reader, &mut output).unwrap();

        assert_eq!(updated.mirrors.len(), 1);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("1. github.com/alice <-> gitea.example.test/alice"));
        assert!(output.contains("What would you like to do?"));
        assert!(!output.contains("Profile/org URL:"));
    }

    #[test]
    fn wizard_deletes_existing_sync_group_from_menu() {
        let config = Config {
            sites: vec![
                SiteConfig {
                    name: "github".to_string(),
                    provider: ProviderKind::Github,
                    base_url: "https://github.com".to_string(),
                    api_url: None,
                    token: TokenConfig::Value("existing-gh".to_string()),
                    git_username: None,
                },
                SiteConfig {
                    name: "gitea".to_string(),
                    provider: ProviderKind::Gitea,
                    base_url: "https://gitea.example.test".to_string(),
                    api_url: None,
                    token: TokenConfig::Value("existing-gt".to_string()),
                    git_username: None,
                },
            ],
            mirrors: vec![MirrorConfig {
                name: "sync-1".to_string(),
                endpoints: vec![
                    EndpointConfig {
                        site: "github".to_string(),
                        kind: NamespaceKind::User,
                        namespace: "alice".to_string(),
                    },
                    EndpointConfig {
                        site: "gitea".to_string(),
                        kind: NamespaceKind::User,
                        namespace: "alice".to_string(),
                    },
                ],
                create_missing: true,
                visibility: Visibility::Private,
                allow_force: false,
            }],
        };
        let input = ["2", "1", "3"].join("\n") + "\n";
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();

        let updated = run_config_wizard_with_io(config, &mut reader, &mut output).unwrap();

        assert!(updated.mirrors.is_empty());
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Delete sync group"));
        assert!(output.contains("2. Back"));
        assert!(output.contains("deleted sync group 1"));
        assert!(output.contains("No sync groups configured."));
    }

    #[test]
    fn wizard_can_go_back_from_delete_menu() {
        let config = Config {
            sites: vec![
                SiteConfig {
                    name: "github".to_string(),
                    provider: ProviderKind::Github,
                    base_url: "https://github.com".to_string(),
                    api_url: None,
                    token: TokenConfig::Value("existing-gh".to_string()),
                    git_username: None,
                },
                SiteConfig {
                    name: "gitea".to_string(),
                    provider: ProviderKind::Gitea,
                    base_url: "https://gitea.example.test".to_string(),
                    api_url: None,
                    token: TokenConfig::Value("existing-gt".to_string()),
                    git_username: None,
                },
            ],
            mirrors: vec![MirrorConfig {
                name: "sync-1".to_string(),
                endpoints: vec![
                    EndpointConfig {
                        site: "github".to_string(),
                        kind: NamespaceKind::User,
                        namespace: "alice".to_string(),
                    },
                    EndpointConfig {
                        site: "gitea".to_string(),
                        kind: NamespaceKind::User,
                        namespace: "alice".to_string(),
                    },
                ],
                create_missing: true,
                visibility: Visibility::Private,
                allow_force: false,
            }],
        };
        let input = ["2", "2", "3"].join("\n") + "\n";
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();

        let updated = run_config_wizard_with_io(config, &mut reader, &mut output).unwrap();

        assert_eq!(updated.mirrors.len(), 1);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("2. Back"));
        assert!(!output.contains("deleted sync group"));
    }

    #[test]
    fn wizard_reports_eof_instead_of_looping() {
        let mut reader = Cursor::new(b"".as_slice());
        let mut output = Vec::new();

        let err = run_config_wizard_with_io(Config::default(), &mut reader, &mut output)
            .unwrap_err()
            .to_string();

        assert!(err.contains("unexpected end of input"));
    }

    #[test]
    fn profile_urls_are_parsed_into_base_and_namespace() {
        let parsed = parse_profile_url("github.com/alice").unwrap();
        assert_eq!(parsed.base_url, "https://github.com");
        assert_eq!(parsed.host, "github.com");
        assert_eq!(parsed.namespace, "alice");

        let parsed = parse_profile_url("https://gitlab.example.test:8443/groups/team").unwrap();
        assert_eq!(parsed.base_url, "https://gitlab.example.test:8443");
        assert_eq!(parsed.namespace, "groups/team");
    }

    #[test]
    fn site_names_are_derived_from_urls_and_made_unique() {
        let mut config = Config::default();
        assert_eq!(
            default_site_name(&config, "https://github.com", &ProviderKind::Github),
            "github"
        );
        assert_eq!(
            default_site_name(
                &config,
                "https://git.my-company.com:3000",
                &ProviderKind::Gitea
            ),
            "git-my-company"
        );

        config.upsert_site(SiteConfig {
            name: "github".to_string(),
            provider: ProviderKind::Github,
            base_url: "https://github.com".to_string(),
            api_url: None,
            token: TokenConfig::Value("token".to_string()),
            git_username: None,
        });
        assert_eq!(
            default_site_name(&config, "https://github.com", &ProviderKind::Github),
            "github-2"
        );
    }

    #[test]
    fn token_creation_urls_are_provider_specific() {
        assert_eq!(
            token_creation_url(&ProviderKind::Github, "https://github.com/"),
            "https://github.com/settings/tokens"
        );
        assert_eq!(
            token_creation_url(&ProviderKind::Gitlab, "https://gitlab.example.test"),
            "https://gitlab.example.test/-/user_settings/personal_access_tokens?name=git-sync&scopes=api"
        );
        assert_eq!(
            token_creation_url(&ProviderKind::Gitea, "gitea.example.test"),
            "https://gitea.example.test/user/settings/applications"
        );
    }
}
