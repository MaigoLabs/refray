use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use console::style;

use crate::config::{Config, EndpointConfig, MirrorConfig, default_work_dir, validate_config};
use crate::git::{GitMirror, Redactor, RemoteSpec, is_disabled_repository_error, safe_remote_name};
use crate::provider::{EndpointRepo, ProviderClient, repos_by_name};

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    pub group: Option<String>,
    pub dry_run: bool,
    pub create_missing_override: Option<bool>,
    pub force_override: Option<bool>,
    pub work_dir: Option<PathBuf>,
}

pub fn sync_all(config: &Config, options: SyncOptions) -> Result<()> {
    validate_config(config)?;
    let work_dir = options.work_dir.clone().unwrap_or_else(default_work_dir);
    fs::create_dir_all(&work_dir)
        .with_context(|| format!("failed to create {}", work_dir.display()))?;

    let mirrors = config
        .mirrors
        .iter()
        .filter(|mirror| {
            options
                .group
                .as_ref()
                .is_none_or(|name| mirror.name == *name)
        })
        .collect::<Vec<_>>();
    if mirrors.is_empty() {
        bail!("no mirror group matched");
    }

    let tokens = config
        .sites
        .iter()
        .map(|site| site.token())
        .collect::<Result<Vec<_>>>()?;
    let redactor = Redactor::new(tokens);
    let mut failures = Vec::new();

    for mirror in mirrors {
        match sync_group(config, mirror, &options, &work_dir, redactor.clone()) {
            Ok(mut group_failures) => failures.append(&mut group_failures),
            Err(error) => {
                let scope = format!("mirror group {}", mirror.name);
                print_failure(&scope, &error);
                failures.push(SyncFailure::new(scope, error));
            }
        }
    }

    if !failures.is_empty() {
        print_failure_summary(&failures);
        bail!("sync completed with {} failure(s)", failures.len());
    }

    Ok(())
}

#[derive(Debug)]
struct SyncFailure {
    scope: String,
    error: String,
}

impl SyncFailure {
    fn new(scope: String, error: anyhow::Error) -> Self {
        Self {
            scope,
            error: format_error(&error),
        }
    }
}

fn print_failure(scope: &str, error: &anyhow::Error) {
    println!(
        "  {} {} {}",
        style("fail").red().bold(),
        style(scope).cyan(),
        style(error_headline(error)).dim()
    );
}

fn print_failure_summary(failures: &[SyncFailure]) {
    println!();
    println!(
        "{} {}",
        style("Failures").red().bold(),
        style(format!("({})", failures.len())).dim()
    );
    for (index, failure) in failures.iter().enumerate() {
        println!("  {}. {}", index + 1, style(&failure.scope).cyan().bold());
        for line in failure.error.lines() {
            println!("     {line}");
        }
    }
}

fn error_headline(error: &anyhow::Error) -> String {
    format_error(error)
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("unknown error")
        .to_string()
}

fn format_error(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

fn sync_group(
    config: &Config,
    mirror: &MirrorConfig,
    options: &SyncOptions,
    work_dir: &Path,
    redactor: Redactor,
) -> Result<Vec<SyncFailure>> {
    println!();
    println!(
        "{} {}",
        style("Mirror group").cyan().bold(),
        style(&mirror.name).bold()
    );
    let create_missing = options
        .create_missing_override
        .unwrap_or(mirror.create_missing);
    let allow_force = options.force_override.unwrap_or(mirror.allow_force);

    let mut all_endpoint_repos = Vec::new();
    for endpoint in &mirror.endpoints {
        let site = config.site(&endpoint.site).unwrap();
        let client = ProviderClient::new(site)?;
        println!(
            "  {} {}",
            style("list").cyan().bold(),
            style(endpoint.label()).dim()
        );
        let repos = client
            .list_repos(endpoint)
            .with_context(|| format!("failed to list repos for {}", endpoint.label()))?;
        for repo in repos {
            all_endpoint_repos.push(EndpointRepo {
                endpoint: endpoint.clone(),
                repo,
            });
        }
    }

    let mut repos = repos_by_name(all_endpoint_repos);
    let repo_names = repos.keys().cloned().collect::<BTreeSet<_>>();
    if repo_names.is_empty() {
        println!(
            "  {} mirror group has no repositories",
            style("skip").yellow().bold()
        );
        return Ok(Vec::new());
    }

    let mut failures = Vec::new();
    for repo_name in repo_names {
        let mut existing = repos.remove(&repo_name).unwrap_or_default();
        let context = RepoSyncContext {
            config,
            mirror,
            work_dir,
            redactor: redactor.clone(),
            dry_run: options.dry_run,
            allow_force,
        };
        if let Err(error) = sync_repo(&context, &repo_name, &mut existing, create_missing)
            .with_context(|| format!("failed to sync repo {repo_name}"))
        {
            let scope = format!("{}/{}", mirror.name, repo_name);
            print_failure(&scope, &error);
            failures.push(SyncFailure::new(scope, error));
        }
    }

    Ok(failures)
}

fn ensure_missing_repos(
    config: &Config,
    mirror: &MirrorConfig,
    repo_name: &str,
    existing: &mut Vec<EndpointRepo>,
    create_missing: bool,
    dry_run: bool,
) -> Result<()> {
    let present = existing
        .iter()
        .map(|repo| repo.endpoint.clone())
        .collect::<BTreeSet<_>>();
    let template = existing.first().map(|repo| repo.repo.clone());

    for endpoint in &mirror.endpoints {
        if present.contains(endpoint) {
            continue;
        }
        if !create_missing {
            println!(
                "  {} {} missing on {} ({})",
                style("skip").yellow().bold(),
                style(repo_name).cyan(),
                style(endpoint.label()).dim(),
                style("creation disabled").dim()
            );
            continue;
        }

        println!(
            "  {} {} {}",
            style("create").green().bold(),
            style(repo_name).cyan(),
            style(format!("on {}", endpoint.label())).dim()
        );
        if dry_run {
            continue;
        }

        let site = config.site(&endpoint.site).unwrap();
        let client = ProviderClient::new(site)?;
        let created = client
            .create_repo(
                endpoint,
                repo_name,
                &mirror.visibility,
                template
                    .as_ref()
                    .and_then(|repo| repo.description.as_deref()),
            )
            .with_context(|| format!("failed to create {} on {}", repo_name, endpoint.label()))?;
        if created.private != matches!(mirror.visibility, crate::config::Visibility::Private) {
            println!(
                "  {} created {} on {}, but provider reported a different visibility than requested",
                style("warn").yellow().bold(),
                style(repo_name).cyan(),
                style(endpoint.label()).dim()
            );
        }
        existing.push(EndpointRepo {
            endpoint: endpoint.clone(),
            repo: created,
        });
    }

    Ok(())
}

struct RepoSyncContext<'a> {
    config: &'a Config,
    mirror: &'a MirrorConfig,
    work_dir: &'a Path,
    redactor: Redactor,
    dry_run: bool,
    allow_force: bool,
}

fn sync_repo(
    context: &RepoSyncContext<'_>,
    repo_name: &str,
    repos: &mut Vec<EndpointRepo>,
    create_missing: bool,
) -> Result<()> {
    println!();
    println!(
        "{} {}",
        style("Repo").magenta().bold(),
        style(repo_name).bold()
    );
    if repos.is_empty() {
        println!(
            "  {} {}",
            style("skip").yellow().bold(),
            style("repository not found on any endpoint").dim()
        );
        return Ok(());
    }

    let path = context
        .work_dir
        .join(safe_remote_name(&context.mirror.name))
        .join(format!("{}.git", safe_remote_name(repo_name)));
    let mirror_repo = GitMirror::open(path, context.redactor.clone(), context.dry_run)?;

    let initial_remotes = remote_specs(context, repos)?;
    mirror_repo.configure_remotes(&initial_remotes)?;
    for remote in &initial_remotes {
        if let Err(error) = mirror_repo.fetch_remote(remote) {
            if is_disabled_repository_error(&error) {
                println!(
                    "  {} {} {}",
                    style("skip").yellow().bold(),
                    style(repo_name).cyan(),
                    style(format!("provider blocked access on {}", remote.display)).dim()
                );
                return Ok(());
            }
            return Err(error).with_context(|| format!("failed to fetch {}", remote.display));
        }
    }

    ensure_missing_repos(
        context.config,
        context.mirror,
        repo_name,
        repos,
        create_missing,
        context.dry_run,
    )?;

    if repos.len() < 2 {
        println!(
            "  {} {} {}",
            style("skip").yellow().bold(),
            style(repo_name).cyan(),
            style("fewer than two endpoints have this repository").dim()
        );
        return Ok(());
    }

    let remotes = remote_specs(context, repos)?;
    mirror_repo.configure_remotes(&remotes)?;
    let initial_remote_names = initial_remotes
        .iter()
        .map(|remote| remote.name.clone())
        .collect::<BTreeSet<_>>();
    for remote in remotes
        .iter()
        .filter(|remote| !initial_remote_names.contains(&remote.name))
    {
        if let Err(error) = mirror_repo.fetch_remote(remote) {
            if is_disabled_repository_error(&error) {
                println!(
                    "  {} {} {}",
                    style("skip").yellow().bold(),
                    style(repo_name).cyan(),
                    style(format!("provider blocked access on {}", remote.display)).dim()
                );
                return Ok(());
            }
            return Err(error).with_context(|| format!("failed to fetch {}", remote.display));
        }
    }

    push_repo_refs(context, &mirror_repo, &remotes)
}

fn remote_specs(context: &RepoSyncContext<'_>, repos: &[EndpointRepo]) -> Result<Vec<RemoteSpec>> {
    let endpoint_map = context
        .mirror
        .endpoints
        .iter()
        .map(|endpoint| (endpoint.clone(), endpoint))
        .collect::<HashMap<EndpointConfig, &EndpointConfig>>();
    let mut remotes = Vec::new();

    for endpoint_repo in repos {
        if !endpoint_map.contains_key(&endpoint_repo.endpoint) {
            continue;
        }
        let site = context.config.site(&endpoint_repo.endpoint.site).unwrap();
        let client = ProviderClient::new(site)?;
        let remote_name = safe_remote_name(&format!(
            "{}_{}",
            endpoint_repo.endpoint.site, endpoint_repo.endpoint.namespace
        ));
        remotes.push(RemoteSpec {
            name: remote_name,
            url: client.authenticated_clone_url(&endpoint_repo.repo.clone_url)?,
            display: endpoint_repo.endpoint.label(),
        });
    }

    Ok(remotes)
}

fn push_repo_refs(
    context: &RepoSyncContext<'_>,
    mirror_repo: &GitMirror,
    remotes: &[RemoteSpec],
) -> Result<()> {
    let (branches, conflicts) = mirror_repo.branch_decisions(remotes, context.allow_force)?;
    let branches_to_push = branches
        .into_iter()
        .filter(|branch| !branch.target_remotes.is_empty())
        .collect::<Vec<_>>();
    for conflict in conflicts {
        let details = conflict
            .tips
            .iter()
            .map(|(remote, sha)| format!("{remote}@{}", short_sha(sha)))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  {} branch {} diverged across {} ({})",
            style("conflict").yellow().bold(),
            style(conflict.branch).cyan(),
            details,
            style("skipped").dim()
        );
    }

    let (tags, tag_conflicts) = mirror_repo.tag_decisions(remotes)?;
    let tags_to_push = tags
        .into_iter()
        .filter(|tag| !tag.target_remotes.is_empty())
        .collect::<Vec<_>>();
    for conflict in tag_conflicts {
        let details = conflict
            .tips
            .iter()
            .map(|(remote, sha)| format!("{remote}@{}", short_sha(sha)))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  {} tag {} differs across {} ({})",
            style("conflict").yellow().bold(),
            style(conflict.tag).cyan(),
            details,
            style("skipped").dim()
        );
    }

    if branches_to_push.is_empty() && tags_to_push.is_empty() {
        println!(
            "  {} branches and tags already match all endpoints",
            style("up-to-date").green().bold()
        );
        return Ok(());
    }
    if !branches_to_push.is_empty() {
        print_branch_decisions(&branches_to_push);
        mirror_repo.push_branches(remotes, &branches_to_push, context.allow_force)?;
    }
    if !tags_to_push.is_empty() {
        print_tag_decisions(&tags_to_push);
        mirror_repo.push_tags(remotes, &tags_to_push)?;
    }
    Ok(())
}

fn print_branch_decisions(branches: &[crate::git::BranchDecision]) {
    println!(
        "  {} {}",
        style("branches").cyan().bold(),
        style(format!("({})", branches.len())).dim()
    );
    for branch in branches {
        println!(
            "    {} {} {}",
            style(&branch.branch).cyan(),
            style(format!("@{}", short_sha(&branch.sha))).dim(),
            style(format!(
                "{} -> {}",
                branch.source_remotes.join("+"),
                branch.target_remotes.join("+")
            ))
            .dim()
        );
    }
}

fn print_tag_decisions(tags: &[crate::git::TagDecision]) {
    println!(
        "  {} {}",
        style("tags").cyan().bold(),
        style(format!("({})", tags.len())).dim()
    );
    for tag in tags {
        println!(
            "    {} {} {}",
            style(&tag.tag).cyan(),
            style(format!("@{}", short_sha(&tag.sha))).dim(),
            style(format!(
                "{} -> {}",
                tag.source_remotes.join("+"),
                tag.target_remotes.join("+")
            ))
            .dim()
        );
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}
