use super::*;
use anyhow::bail;
use std::io::{BufRead, Write};

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
    prompt_webhook_setup(reader, writer, config)?;
    Ok(())
}

fn prompt_webhook_setup<R, W>(reader: &mut R, writer: &mut W, config: &mut Config) -> Result<()>
where
    R: BufRead,
    W: Write,
{
    if config
        .webhook
        .as_ref()
        .is_some_and(|webhook| webhook.install)
    {
        writeln!(writer, "Webhooks already enabled.")?;
        return Ok(());
    }
    writeln!(
        writer,
        "Install webhooks? Strongly recommended because immediate sync greatly reduces conflicts."
    )?;
    if !prompt_bool(reader, writer, "Install webhook?", true)? {
        return Ok(());
    }
    let url = prompt_required(reader, writer, "Webhook URL reachable by providers")?;
    if let Err(error) = validate_url(&url) {
        bail!(error);
    }
    let full_sync_interval_minutes = if prompt_bool(
        reader,
        writer,
        "Run periodic full sync while serve is running?",
        true,
    )? {
        Some(
            prompt_with_default(reader, writer, "Full sync interval in minutes", "60")?
                .parse::<u64>()
                .context("full sync interval must be a number")?,
        )
    } else {
        None
    };
    config.webhook = Some(WebhookConfig {
        install: true,
        url,
        secret: TokenConfig::Value("test-webhook-secret".to_string()),
        full_sync_interval_minutes,
        reachability_check_interval_minutes: Some(15),
    });
    Ok(())
}

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
            "forgejo" => return Ok(ProviderKind::Forgejo),
            _ => writeln!(
                writer,
                "Provider must be github, gitlab, gitea, or forgejo."
            )?,
        }
    }
}

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
