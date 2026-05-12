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
    writeln!(writer, "refray configuration wizard")?;
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
            WizardAction::EditSyncGroup => {
                if edit_sync_group(reader, writer, &mut config)? {
                    write_sync_groups(&config, writer)?;
                }
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

pub fn prompt_run_full_sync_now_with_io<R, W>(
    config: &Config,
    reader: &mut R,
    writer: &mut W,
) -> Result<bool>
where
    R: BufRead,
    W: Write,
{
    if config.mirrors.is_empty() {
        return Ok(false);
    }

    prompt_bool(reader, writer, "Run full sync now?", false)
}

fn add_sync_group<R, W>(reader: &mut R, writer: &mut W, config: &mut Config) -> Result<()>
where
    R: BufRead,
    W: Write,
{
    let endpoints = prompt_sync_group_endpoints(reader, writer, config, &[])?;
    let sync_visibility = prompt_sync_visibility(reader, writer, None)?;
    let repo_filters = prompt_repo_filters(reader, writer, None)?;
    write_deletion_backup_notice(writer)?;
    let create_missing = prompt_create_missing(reader, writer, None)?;
    let delete_missing = prompt_delete_missing(reader, writer, None)?;
    let conflict_resolution = prompt_conflict_resolution(reader, writer, None)?;
    config.upsert_mirror(MirrorConfig {
        name: next_mirror_name(config),
        endpoints,
        sync_visibility,
        repo_whitelist: repo_filters.whitelist,
        repo_blacklist: repo_filters.blacklist,
        create_missing,
        delete_missing,
        visibility: Visibility::Private,
        conflict_resolution,
        allow_temporary_gitlab_force_push: true,
    });
    prompt_webhook_setup(reader, writer, config)?;
    Ok(())
}

fn prompt_sync_group_endpoints<R, W>(
    reader: &mut R,
    writer: &mut W,
    config: &mut Config,
    existing: &[EndpointConfig],
) -> Result<Vec<EndpointConfig>>
where
    R: BufRead,
    W: Write,
{
    let mut endpoints = Vec::new();
    let first = prompt_target(
        reader,
        writer,
        "Profile/org URL",
        endpoint_profile_url(config, existing.first()),
    )?;
    endpoints.push(ensure_credentials(config, first, reader, writer)?);
    let second = prompt_target(
        reader,
        writer,
        "Profile/org URL to sync with",
        endpoint_profile_url(config, existing.get(1)),
    )?;
    endpoints.push(ensure_credentials(config, second, reader, writer)?);

    for (index, endpoint) in existing.iter().enumerate().skip(2) {
        let Some(default_url) = endpoint_profile_url(config, Some(endpoint)) else {
            continue;
        };
        if !prompt_bool(
            reader,
            writer,
            &format!("Keep endpoint {}?", index + 1),
            true,
        )? {
            continue;
        }
        let next = prompt_target(
            reader,
            writer,
            "Additional profile/org URL",
            Some(default_url),
        )?;
        endpoints.push(ensure_credentials(config, next, reader, writer)?);
    }

    loop {
        let prompt = if endpoints.len() == 2 {
            "Add a third endpoint for 3-way sync?"
        } else {
            "Add another endpoint to this sync group?"
        };
        if !prompt_bool(reader, writer, prompt, false)? {
            break;
        }
        let next = prompt_target(reader, writer, "Additional profile/org URL", None)?;
        endpoints.push(ensure_credentials(config, next, reader, writer)?);
    }

    Ok(endpoints)
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
    write_webhook_url_instructions(writer)?;
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

fn write_webhook_url_instructions<W>(writer: &mut W) -> Result<()>
where
    W: Write,
{
    writeln!(
        writer,
        "Webhook URL must be reachable from every Git provider."
    )?;
    writeln!(
        writer,
        "Start the receiver with: refray serve --listen 127.0.0.1:8787"
    )?;
    writeln!(writer, "The receiver accepts: POST / and POST /webhook")?;
    writeln!(
        writer,
        "If running locally, expose it with a tunnel, for example: cloudflared tunnel --url http://127.0.0.1:8787"
    )?;
    writeln!(
        writer,
        "Then enter the public URL, usually ending in /webhook."
    )?;
    writeln!(
        writer,
        "During the real wizard, refray starts a temporary listener on 127.0.0.1:8787 so you can test the tunnel now."
    )?;
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
        writeln!(writer, "  2. Edit an existing group")?;
        writeln!(writer, "  3. Delete an existing group")?;
        writeln!(writer, "  4. Done")?;
        write!(writer, "Choose an option: ")?;
        writer.flush()?;
        let value = read_line(reader)?.trim().to_ascii_lowercase();
        match value.as_str() {
            "1" | "add" | "add another sync group" => return Ok(WizardAction::AddSyncGroup),
            "2" | "edit" | "edit an existing group" => return Ok(WizardAction::EditSyncGroup),
            "3" | "delete" | "delete an existing group" => {
                return Ok(WizardAction::DeleteSyncGroup);
            }
            "4" | "done" | "finish" => return Ok(WizardAction::Done),
            _ => writeln!(writer, "Enter 1, 2, 3, or 4.")?,
        }
    }
}

fn edit_sync_group<R, W>(reader: &mut R, writer: &mut W, config: &mut Config) -> Result<bool>
where
    R: BufRead,
    W: Write,
{
    if config.mirrors.is_empty() {
        writeln!(writer, "No sync groups to edit.")?;
        return Ok(false);
    }

    loop {
        writeln!(writer, "Edit sync group")?;
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
                let existing = config.mirrors[index - 1].endpoints.clone();
                let existing_sync_visibility = config.mirrors[index - 1].sync_visibility.clone();
                let existing_repo_filters = RepoFilterInput {
                    whitelist: config.mirrors[index - 1].repo_whitelist.clone(),
                    blacklist: config.mirrors[index - 1].repo_blacklist.clone(),
                };
                let existing_create_missing = config.mirrors[index - 1].create_missing;
                let existing_delete_missing = config.mirrors[index - 1].delete_missing;
                let existing_conflict_resolution =
                    config.mirrors[index - 1].conflict_resolution.clone();
                let endpoints = prompt_sync_group_endpoints(reader, writer, config, &existing)?;
                let sync_visibility =
                    prompt_sync_visibility(reader, writer, Some(&existing_sync_visibility))?;
                let repo_filters =
                    prompt_repo_filters(reader, writer, Some(&existing_repo_filters))?;
                write_deletion_backup_notice(writer)?;
                let create_missing =
                    prompt_create_missing(reader, writer, Some(existing_create_missing))?;
                let delete_missing =
                    prompt_delete_missing(reader, writer, Some(existing_delete_missing))?;
                let conflict_resolution = prompt_conflict_resolution(
                    reader,
                    writer,
                    Some(&existing_conflict_resolution),
                )?;
                config.mirrors[index - 1].endpoints = endpoints;
                config.mirrors[index - 1].sync_visibility = sync_visibility;
                config.mirrors[index - 1].repo_whitelist = repo_filters.whitelist;
                config.mirrors[index - 1].repo_blacklist = repo_filters.blacklist;
                config.mirrors[index - 1].create_missing = create_missing;
                config.mirrors[index - 1].delete_missing = delete_missing;
                config.mirrors[index - 1].conflict_resolution = conflict_resolution;
                prompt_webhook_setup(reader, writer, config)?;
                writeln!(writer, "updated sync group {index}")?;
                return Ok(true);
            }
            Ok(index) if index == config.mirrors.len() + 1 => return Ok(false),
            _ => writeln!(writer, "Enter a sync group number, or choose Back.")?,
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

fn prompt_target<R, W>(
    reader: &mut R,
    writer: &mut W,
    prompt: &str,
    default: Option<String>,
) -> Result<ProfileTarget>
where
    R: BufRead,
    W: Write,
{
    let url = match default {
        Some(default) => prompt_with_default(reader, writer, prompt, &default)?,
        None => prompt_required(reader, writer, prompt)?,
    };
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

fn prompt_conflict_resolution<R, W>(
    reader: &mut R,
    writer: &mut W,
    existing: Option<&ConflictResolutionStrategy>,
) -> Result<ConflictResolutionStrategy>
where
    R: BufRead,
    W: Write,
{
    let default = existing
        .map(conflict_resolution_value)
        .unwrap_or("auto-rebase + pull-request");
    loop {
        writeln!(writer, "How should refray resolve branch conflicts?")?;
        writeln!(writer, "  1. fail")?;
        writeln!(writer, "  2. auto-rebase and fail on file conflict")?;
        writeln!(writer, "  3. pull-request")?;
        writeln!(writer, "  4. auto-rebase + pull-request (recommended)")?;
        let value = prompt_with_default(reader, writer, "Conflict resolution", default)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "1" | "fail" => return Ok(ConflictResolutionStrategy::Fail),
            "2" | "auto-rebase" | "auto_rebase" | "rebase" => {
                return Ok(ConflictResolutionStrategy::AutoRebase);
            }
            "3" | "pull-request" | "pull_request" | "pr" => {
                return Ok(ConflictResolutionStrategy::PullRequest);
            }
            "4"
            | "auto-rebase + pull-request"
            | "auto-rebase+pull-request"
            | "auto_rebase_pull_request"
            | "auto-rebase-pull-request" => {
                return Ok(ConflictResolutionStrategy::AutoRebasePullRequest);
            }
            _ => writeln!(
                writer,
                "Enter 1, 2, 3, 4, fail, auto-rebase, pull-request, or auto-rebase + pull-request."
            )?,
        }
    }
}

fn prompt_sync_visibility<R, W>(
    reader: &mut R,
    writer: &mut W,
    existing: Option<&SyncVisibility>,
) -> Result<SyncVisibility>
where
    R: BufRead,
    W: Write,
{
    let default = existing.map(sync_visibility_value).unwrap_or("all");
    loop {
        writeln!(writer, "Which repositories should this sync group include?")?;
        writeln!(writer, "  1. all")?;
        writeln!(writer, "  2. private only")?;
        writeln!(writer, "  3. public only")?;
        let value = prompt_with_default(reader, writer, "Sync visibility", default)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "1" | "all" => return Ok(SyncVisibility::All),
            "2" | "private" | "private only" | "private-only" => {
                return Ok(SyncVisibility::Private);
            }
            "3" | "public" | "public only" | "public-only" => {
                return Ok(SyncVisibility::Public);
            }
            _ => writeln!(writer, "Enter 1, 2, 3, all, private, or public.")?,
        }
    }
}

fn prompt_repo_filters<R, W>(
    reader: &mut R,
    writer: &mut W,
    existing: Option<&RepoFilterInput>,
) -> Result<RepoFilterInput>
where
    R: BufRead,
    W: Write,
{
    let existing = existing.cloned().unwrap_or_default();
    let has_existing = existing.whitelist.is_some() || existing.blacklist.is_some();
    if !prompt_bool(
        reader,
        writer,
        "Configure repository name whitelist/blacklist?",
        has_existing,
    )? {
        return Ok(RepoFilterInput::default());
    }

    Ok(RepoFilterInput {
        whitelist: prompt_repo_pattern(
            reader,
            writer,
            "Whitelist regex (empty means all repo names)",
            &existing.whitelist,
        )?,
        blacklist: prompt_repo_pattern(reader, writer, "Blacklist regex", &existing.blacklist)?,
    })
}

fn prompt_repo_pattern<R, W>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    existing: &Option<String>,
) -> Result<Option<String>>
where
    R: BufRead,
    W: Write,
{
    let value = match existing {
        Some(existing) => prompt_with_default(reader, writer, label, existing)?,
        None => prompt_optional(reader, writer, label)?,
    };
    if let Err(error) = validate_repo_pattern(&value) {
        bail!(error);
    }
    Ok(parse_repo_pattern(&value))
}

fn write_deletion_backup_notice<W>(writer: &mut W) -> Result<()>
where
    W: Write,
{
    writeln!(
        writer,
        "Deletion backups: refray keeps a local backup before propagating repository or branch deletes."
    )?;
    Ok(())
}

fn prompt_create_missing<R, W>(
    reader: &mut R,
    writer: &mut W,
    existing: Option<bool>,
) -> Result<bool>
where
    R: BufRead,
    W: Write,
{
    prompt_bool(
        reader,
        writer,
        "Create repositories that are missing from an endpoint?",
        existing.unwrap_or(true),
    )
}

fn prompt_delete_missing<R, W>(
    reader: &mut R,
    writer: &mut W,
    existing: Option<bool>,
) -> Result<bool>
where
    R: BufRead,
    W: Write,
{
    prompt_bool(
        reader,
        writer,
        "When a previously synced repository is deleted from one endpoint, delete it everywhere?",
        existing.unwrap_or(true),
    )
}

fn sync_visibility_value(sync_visibility: &SyncVisibility) -> &'static str {
    match sync_visibility {
        SyncVisibility::All => "all",
        SyncVisibility::Private => "private",
        SyncVisibility::Public => "public",
    }
}

fn conflict_resolution_value(strategy: &ConflictResolutionStrategy) -> &'static str {
    match strategy {
        ConflictResolutionStrategy::Fail => "fail",
        ConflictResolutionStrategy::AutoRebase => "auto-rebase",
        ConflictResolutionStrategy::PullRequest => "pull-request",
        ConflictResolutionStrategy::AutoRebasePullRequest => "auto-rebase + pull-request",
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

fn prompt_optional<R, W>(reader: &mut R, writer: &mut W, label: &str) -> Result<String>
where
    R: BufRead,
    W: Write,
{
    write!(writer, "{label}: ")?;
    writer.flush()?;
    Ok(read_line(reader)?.trim().to_string())
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
