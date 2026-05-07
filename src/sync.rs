use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use anyhow::{Context, Result, bail};
use console::style;
use regex::Regex;

use crate::config::{Config, EndpointConfig, MirrorConfig, default_work_dir, validate_config};
use crate::git::{
    BranchDeletion, GitMirror, Redactor, RemoteRefSnapshot, RemoteSpec,
    is_disabled_repository_error, ls_remote_refs, safe_remote_name,
};
use crate::logging;
use crate::provider::{EndpointRepo, ProviderClient, repos_by_name};
use crate::webhook;

mod output;
mod state;

use self::output::{
    print_branch_decisions, print_branch_deletions, print_failure, print_failure_summary,
    print_tag_decisions, short_sha,
};
use self::state::{
    FailureState, RefState, RemoteRefState, SyncFailure, load_failure_state, load_ref_state,
    save_failure_state, save_ref_state,
};
#[cfg(test)]
use self::state::{FailedRepo, failure_state_path};

pub const DEFAULT_JOBS: usize = 4;

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub group: Option<String>,
    pub dry_run: bool,
    pub create_missing_override: Option<bool>,
    pub force_override: Option<bool>,
    pub repo_pattern: Option<String>,
    pub retry_failed: bool,
    pub work_dir: Option<PathBuf>,
    pub jobs: usize,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            group: None,
            dry_run: false,
            create_missing_override: None,
            force_override: None,
            repo_pattern: None,
            retry_failed: false,
            work_dir: None,
            jobs: DEFAULT_JOBS,
        }
    }
}

pub fn sync_all(config: &Config, options: SyncOptions) -> Result<()> {
    validate_config(config)?;
    if options.jobs == 0 {
        bail!("--jobs must be at least 1");
    }
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
    let repo_pattern = options
        .repo_pattern
        .as_deref()
        .map(Regex::new)
        .transpose()
        .with_context(|| "invalid --repo-pattern regex")?;
    let retry_failed_repos = if options.retry_failed {
        Some(load_failure_state(&work_dir)?.repos_by_group())
    } else {
        None
    };
    let mut ref_state = load_ref_state(&work_dir)?;
    let mut failures = Vec::new();

    for mirror in mirrors {
        let mut group_context = GroupSyncContext {
            config,
            options: &options,
            work_dir: &work_dir,
            redactor: redactor.clone(),
            repo_pattern: repo_pattern.as_ref(),
            retry_failed_repos: retry_failed_repos.as_ref(),
            ref_state: &mut ref_state,
        };
        match sync_group(&mut group_context, mirror) {
            Ok(mut group_failures) => failures.append(&mut group_failures),
            Err(error) => {
                let scope = format!("mirror group {}", mirror.name);
                print_failure(&scope, &error);
                failures.push(SyncFailure::group(scope, error));
            }
        }
    }

    if !options.dry_run {
        save_failure_state(&work_dir, &FailureState::from_failures(&failures))?;
        save_ref_state(&work_dir, &ref_state)?;
    }

    if !failures.is_empty() {
        print_failure_summary(&failures);
        bail!("sync completed with {} failure(s)", failures.len());
    }

    Ok(())
}

struct GroupSyncContext<'a> {
    config: &'a Config,
    options: &'a SyncOptions,
    work_dir: &'a Path,
    redactor: Redactor,
    repo_pattern: Option<&'a Regex>,
    retry_failed_repos: Option<&'a BTreeMap<String, BTreeSet<String>>>,
    ref_state: &'a mut RefState,
}

fn sync_group(
    context: &mut GroupSyncContext<'_>,
    mirror: &MirrorConfig,
) -> Result<Vec<SyncFailure>> {
    crate::logln!();
    crate::logln!(
        "{} {}",
        style("Mirror group").cyan().bold(),
        style(&mirror.name).bold()
    );
    let create_missing = context
        .options
        .create_missing_override
        .unwrap_or(mirror.create_missing);
    let allow_force = context.options.force_override.unwrap_or(mirror.allow_force);

    let all_endpoint_repos = list_group_repos(context.config, mirror)?;
    if !context.options.dry_run {
        webhook::ensure_configured_webhooks(
            context.config,
            mirror,
            &all_endpoint_repos,
            context.work_dir,
        )?;
    }

    let mut repos = repos_by_name(all_endpoint_repos);
    let all_repo_count = repos.len();
    let retry_repo_names = context
        .retry_failed_repos
        .and_then(|repos| repos.get(&mirror.name));
    let repo_names = repos
        .keys()
        .filter(|name| {
            context
                .repo_pattern
                .is_none_or(|pattern| pattern.is_match(name))
                && retry_repo_names.is_none_or(|repos| repos.contains(name.as_str()))
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    if repo_names.is_empty() {
        if let Some(retry_repo_names) = retry_repo_names {
            crate::logln!(
                "  {} no previously failed repositories were found in this group ({} saved)",
                style("skip").yellow().bold(),
                retry_repo_names.len()
            );
        } else if context.retry_failed_repos.is_some() {
            crate::logln!(
                "  {} no previous failures for this group",
                style("skip").yellow().bold()
            );
        } else if let Some(pattern) = context.repo_pattern {
            crate::logln!(
                "  {} no repositories match {} ({} discovered)",
                style("skip").yellow().bold(),
                style(pattern.as_str()).cyan(),
                all_repo_count
            );
        } else {
            crate::logln!(
                "  {} mirror group has no repositories",
                style("skip").yellow().bold()
            );
        }
        return Ok(Vec::new());
    }
    if let Some(pattern) = context.repo_pattern {
        crate::logln!(
            "  {} {} of {} repositories match {}",
            style("filter").cyan().bold(),
            repo_names.len(),
            all_repo_count,
            style(pattern.as_str()).cyan()
        );
    }
    if let Some(retry_repo_names) = retry_repo_names {
        crate::logln!(
            "  {} retrying {} of {} previously failed repositories",
            style("retry").cyan().bold(),
            repo_names.len(),
            retry_repo_names.len()
        );
    }

    let repo_log_width = repo_log_width(&repo_names);
    let repo_jobs = repo_names
        .into_iter()
        .map(|repo_name| {
            let existing = repos.remove(&repo_name).unwrap_or_default();
            RepoSyncJob {
                repo_name,
                existing,
            }
        })
        .collect::<VecDeque<_>>();
    let worker_count = context.options.jobs.min(repo_jobs.len()).max(1);
    if worker_count > 1 {
        crate::logln!(
            "  {} syncing repositories with {} workers",
            style("jobs").cyan().bold(),
            worker_count
        );
    }

    let base_ref_state = context.ref_state.clone();
    let queue = Arc::new(Mutex::new(repo_jobs));
    let (sender, receiver) = mpsc::channel();
    let use_status_area = worker_count > 1;
    let _status_guard = use_status_area.then(|| logging::start_status_area(worker_count));
    let failures = thread::scope(|scope| {
        for worker_id in 0..worker_count {
            let queue = Arc::clone(&queue);
            let sender = sender.clone();
            let redactor = context.redactor.clone();
            let config = context.config;
            let work_dir = context.work_dir;
            let dry_run = context.options.dry_run;
            let ref_state = &base_ref_state;

            scope.spawn(move || {
                while let Some(mut job) = pop_repo_job(&queue) {
                    let _repo_log_guard = use_status_area.then(|| {
                        logging::start_repo_log(job.repo_name.clone(), worker_id, repo_log_width)
                    });
                    let repo_context = RepoSyncContext {
                        config,
                        mirror,
                        work_dir,
                        redactor: redactor.clone(),
                        dry_run,
                        allow_force,
                    };
                    let result = sync_repo(
                        &repo_context,
                        &job.repo_name,
                        &mut job.existing,
                        create_missing,
                        ref_state,
                    )
                    .with_context(|| format!("failed to sync repo {}", job.repo_name))
                    .map(|outcome| RepoWorkerSuccess {
                        repo_name: job.repo_name.clone(),
                        outcome,
                    })
                    .map_err(|error| RepoWorkerFailure {
                        repo_name: job.repo_name,
                        error,
                    });
                    logging::finish_repo_log();
                    if sender.send(result).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);

        let mut failures = Vec::new();
        for result in receiver {
            match result {
                Ok(success) => {
                    if let Some(refs) = success.outcome.ref_update {
                        context
                            .ref_state
                            .set_repo(&mirror.name, &success.repo_name, refs);
                    }
                }
                Err(failure) => {
                    let scope = format!("{}/{}", mirror.name, failure.repo_name);
                    print_failure(&scope, &failure.error);
                    failures.push(SyncFailure::repo(
                        mirror.name.clone(),
                        failure.repo_name,
                        failure.error,
                    ));
                }
            }
        }
        failures
    });

    if create_missing && !context.options.dry_run {
        let repos = list_group_repos(context.config, mirror)?;
        webhook::ensure_configured_webhooks(context.config, mirror, &repos, context.work_dir)?;
    }

    Ok(failures)
}

fn list_group_repos(config: &Config, mirror: &MirrorConfig) -> Result<Vec<EndpointRepo>> {
    let mut all_endpoint_repos = Vec::new();
    for endpoint in &mirror.endpoints {
        let site = config.site(&endpoint.site).unwrap();
        let client = ProviderClient::new(site)?;
        crate::logln!(
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
    Ok(all_endpoint_repos)
}

fn pop_repo_job(queue: &Arc<Mutex<VecDeque<RepoSyncJob>>>) -> Option<RepoSyncJob> {
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .pop_front()
}

fn repo_log_width(repo_names: &BTreeSet<String>) -> usize {
    repo_names
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 32)
}

struct RepoSyncJob {
    repo_name: String,
    existing: Vec<EndpointRepo>,
}

struct RepoWorkerSuccess {
    repo_name: String,
    outcome: RepoSyncOutcome,
}

struct RepoWorkerFailure {
    repo_name: String,
    error: anyhow::Error,
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
            crate::logln!(
                "  {} {} missing on {} ({})",
                style("skip").yellow().bold(),
                style(repo_name).cyan(),
                style(endpoint.label()).dim(),
                style("creation disabled").dim()
            );
            continue;
        }

        crate::logln!(
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
            crate::logln!(
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

#[derive(Default)]
struct RepoSyncOutcome {
    ref_update: Option<BTreeMap<String, RemoteRefState>>,
}

fn sync_repo(
    context: &RepoSyncContext<'_>,
    repo_name: &str,
    repos: &mut Vec<EndpointRepo>,
    create_missing: bool,
    ref_state: &RefState,
) -> Result<RepoSyncOutcome> {
    crate::logln!();
    crate::logln!(
        "{} {}",
        style("Repo").magenta().bold(),
        style(repo_name).bold()
    );
    if repos.is_empty() {
        crate::logln!(
            "  {} {}",
            style("skip").yellow().bold(),
            style("repository not found on any endpoint").dim()
        );
        return Ok(RepoSyncOutcome::default());
    }

    let initial_remotes = remote_specs(context, repos)?;
    let Some(initial_ref_state) = check_remote_refs(context, repo_name, &initial_remotes)? else {
        return Ok(RepoSyncOutcome::default());
    };
    let all_endpoints_present = all_configured_endpoints_present(context.mirror, repos);
    if !context.dry_run
        && all_endpoints_present
        && ref_state.repo_matches(&context.mirror.name, repo_name, &initial_ref_state)
    {
        crate::logln!(
            "  {} refs unchanged since last successful sync",
            style("up-to-date").green().bold()
        );
        return Ok(RepoSyncOutcome::default());
    }

    let path = context
        .work_dir
        .join(safe_remote_name(&context.mirror.name))
        .join(format!("{}.git", safe_remote_name(repo_name)));
    let mirror_repo = GitMirror::open(path, context.redactor.clone(), context.dry_run)?;

    mirror_repo.configure_remotes(&initial_remotes)?;
    let cached_ref_state = cached_ref_state(&mirror_repo, &initial_remotes)?;
    if !context.dry_run
        && all_endpoints_present
        && cached_refs_match(&mirror_repo, &initial_remotes, &initial_ref_state)?
    {
        crate::logln!(
            "  {} refs unchanged from local mirror cache",
            style("up-to-date").green().bold()
        );
        return Ok(RepoSyncOutcome {
            ref_update: Some(initial_ref_state),
        });
    }

    for remote in &initial_remotes {
        if let Err(error) = mirror_repo.fetch_remote(remote) {
            if is_disabled_repository_error(&error) {
                crate::logln!(
                    "  {} {} {}",
                    style("skip").yellow().bold(),
                    style(repo_name).cyan(),
                    style(format!("provider blocked access on {}", remote.display)).dim()
                );
                return Ok(RepoSyncOutcome::default());
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
        crate::logln!(
            "  {} {} {}",
            style("skip").yellow().bold(),
            style(repo_name).cyan(),
            style("fewer than two endpoints have this repository").dim()
        );
        return Ok(RepoSyncOutcome::default());
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
                crate::logln!(
                    "  {} {} {}",
                    style("skip").yellow().bold(),
                    style(repo_name).cyan(),
                    style(format!("provider blocked access on {}", remote.display)).dim()
                );
                return Ok(RepoSyncOutcome::default());
            }
            return Err(error).with_context(|| format!("failed to fetch {}", remote.display));
        }
    }

    let result = push_repo_refs(
        context,
        &mirror_repo,
        &remotes,
        detailed_repo_ref_state(ref_state.repo(&context.mirror.name, repo_name))
            .or(cached_ref_state.as_ref()),
        &initial_ref_state,
    )?;
    if !context.dry_run && !result.had_conflicts {
        let refs = if result.pushed {
            let Some(refs) = check_remote_refs(context, repo_name, &remotes)? else {
                return Ok(RepoSyncOutcome::default());
            };
            refs
        } else {
            initial_ref_state
        };
        return Ok(RepoSyncOutcome {
            ref_update: Some(refs),
        });
    }
    Ok(RepoSyncOutcome::default())
}

fn all_configured_endpoints_present(mirror: &MirrorConfig, repos: &[EndpointRepo]) -> bool {
    let present = repos
        .iter()
        .map(|repo| repo.endpoint.clone())
        .collect::<BTreeSet<_>>();
    mirror
        .endpoints
        .iter()
        .all(|endpoint| present.contains(endpoint))
}

fn cached_refs_match(
    mirror_repo: &GitMirror,
    remotes: &[RemoteSpec],
    expected_refs: &BTreeMap<String, RemoteRefState>,
) -> Result<bool> {
    for remote in remotes {
        let Some(expected) = expected_refs.get(&remote.name) else {
            return Ok(false);
        };
        if !mirror_repo.cached_remote_refs_match(remote, &RemoteRefSnapshot::from(expected))? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn cached_ref_state(
    mirror_repo: &GitMirror,
    remotes: &[RemoteSpec],
) -> Result<Option<BTreeMap<String, RemoteRefState>>> {
    let mut refs = BTreeMap::new();
    for remote in remotes {
        let Some(snapshot) = mirror_repo.cached_remote_ref_snapshot(remote)? else {
            return Ok(None);
        };
        refs.insert(remote.name.clone(), snapshot.into());
    }
    Ok(Some(refs))
}

fn detailed_repo_ref_state(
    refs: Option<&BTreeMap<String, RemoteRefState>>,
) -> Option<&BTreeMap<String, RemoteRefState>> {
    refs.filter(|refs| {
        refs.values()
            .any(|remote| !remote.branches.is_empty() || !remote.tags.is_empty())
    })
}

fn check_remote_refs(
    context: &RepoSyncContext<'_>,
    repo_name: &str,
    remotes: &[RemoteSpec],
) -> Result<Option<BTreeMap<String, RemoteRefState>>> {
    let mut refs = BTreeMap::new();
    for remote in remotes {
        crate::logln!(
            "  {} {}",
            style("check refs").cyan().bold(),
            style(&remote.display).dim()
        );
        let snapshot = match ls_remote_refs(remote, &context.redactor) {
            Ok(snapshot) => snapshot,
            Err(error) if is_disabled_repository_error(&error) => {
                crate::logln!(
                    "  {} {} {}",
                    style("skip").yellow().bold(),
                    style(repo_name).cyan(),
                    style(format!("provider blocked access on {}", remote.display)).dim()
                );
                return Ok(None);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to check refs for {}", remote.display));
            }
        };
        refs.insert(remote.name.clone(), snapshot.into());
    }
    Ok(Some(refs))
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
    previous_refs: Option<&BTreeMap<String, RemoteRefState>>,
    current_refs: &BTreeMap<String, RemoteRefState>,
) -> Result<RepoRefSyncResult> {
    let (branch_deletions, deletion_conflicts, blocked_branches) =
        branch_deletion_decisions(remotes, previous_refs, current_refs);
    let had_deletion_conflicts = !deletion_conflicts.is_empty();
    for conflict in deletion_conflicts {
        crate::logln!(
            "  {} branch {} was deleted on {} but changed on {} ({})",
            style("conflict").yellow().bold(),
            style(conflict.branch).cyan(),
            conflict.deleted_remotes.join("+"),
            conflict.changed_remotes.join("+"),
            style("skipped").dim()
        );
    }

    let (branches, conflicts) = mirror_repo.branch_decisions(remotes, context.allow_force)?;
    let branches_to_push = branches
        .into_iter()
        .filter(|branch| !blocked_branches.contains(&branch.branch))
        .filter(|branch| !branch.target_remotes.is_empty())
        .collect::<Vec<_>>();
    let mut had_branch_conflicts = false;
    for conflict in conflicts {
        if blocked_branches.contains(&conflict.branch) {
            continue;
        }
        had_branch_conflicts = true;
        let details = conflict
            .tips
            .iter()
            .map(|(remote, sha)| format!("{remote}@{}", short_sha(sha)))
            .collect::<Vec<_>>()
            .join(", ");
        crate::logln!(
            "  {} branch {} diverged across {} ({})",
            style("conflict").yellow().bold(),
            style(conflict.branch).cyan(),
            details,
            style("skipped").dim()
        );
    }

    let (tags, tag_conflicts) = mirror_repo.tag_decisions(remotes)?;
    let had_tag_conflicts = !tag_conflicts.is_empty();
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
        crate::logln!(
            "  {} tag {} differs across {} ({})",
            style("conflict").yellow().bold(),
            style(conflict.tag).cyan(),
            details,
            style("skipped").dim()
        );
    }

    if branches_to_push.is_empty() && tags_to_push.is_empty() {
        if !branch_deletions.is_empty() {
            print_branch_deletions(&branch_deletions);
            mirror_repo.delete_branches(remotes, &branch_deletions)?;
            return Ok(RepoRefSyncResult {
                pushed: true,
                had_conflicts: had_deletion_conflicts,
            });
        }
        if had_branch_conflicts || had_tag_conflicts || had_deletion_conflicts {
            return Ok(RepoRefSyncResult {
                pushed: false,
                had_conflicts: true,
            });
        }
        crate::logln!(
            "  {} branches and tags already match all endpoints",
            style("up-to-date").green().bold()
        );
        return Ok(RepoRefSyncResult {
            pushed: false,
            had_conflicts: had_branch_conflicts || had_tag_conflicts || had_deletion_conflicts,
        });
    }
    if !branch_deletions.is_empty() {
        print_branch_deletions(&branch_deletions);
        mirror_repo.delete_branches(remotes, &branch_deletions)?;
    }
    if !branches_to_push.is_empty() {
        print_branch_decisions(&branches_to_push);
        mirror_repo.push_branches(remotes, &branches_to_push, context.allow_force)?;
    }
    if !tags_to_push.is_empty() {
        print_tag_decisions(&tags_to_push);
        mirror_repo.push_tags(remotes, &tags_to_push)?;
    }
    Ok(RepoRefSyncResult {
        pushed: true,
        had_conflicts: had_branch_conflicts || had_tag_conflicts || had_deletion_conflicts,
    })
}

struct BranchDeletionConflict {
    branch: String,
    deleted_remotes: Vec<String>,
    changed_remotes: Vec<String>,
}

fn branch_deletion_decisions(
    remotes: &[RemoteSpec],
    previous_refs: Option<&BTreeMap<String, RemoteRefState>>,
    current_refs: &BTreeMap<String, RemoteRefState>,
) -> (
    Vec<BranchDeletion>,
    Vec<BranchDeletionConflict>,
    BTreeSet<String>,
) {
    let Some(previous_refs) = previous_refs else {
        return (Vec::new(), Vec::new(), BTreeSet::new());
    };
    let remote_names = remotes
        .iter()
        .map(|remote| remote.name.clone())
        .collect::<Vec<_>>();
    let mut branches = BTreeSet::new();
    for refs in previous_refs.values() {
        branches.extend(refs.branches.keys().cloned());
    }

    let mut deletions = Vec::new();
    let mut conflicts = Vec::new();
    let mut blocked = BTreeSet::new();
    for branch in branches {
        let previous_remotes = remote_names
            .iter()
            .filter(|remote| {
                previous_refs
                    .get(*remote)
                    .is_some_and(|refs| refs.branches.contains_key(&branch))
            })
            .cloned()
            .collect::<Vec<_>>();
        if previous_remotes.len() != remote_names.len() {
            continue;
        }

        let mut deleted_remotes = Vec::new();
        let mut target_remotes = Vec::new();
        let mut changed_remotes = Vec::new();
        for remote in &remote_names {
            let current = current_refs
                .get(remote)
                .and_then(|refs| refs.branches.get(&branch));
            let previous = previous_refs
                .get(remote)
                .and_then(|refs| refs.branches.get(&branch));
            match (previous, current) {
                (Some(_), None) => deleted_remotes.push(remote.clone()),
                (Some(previous), Some(current)) if previous == current => {
                    target_remotes.push(remote.clone());
                }
                (Some(_), Some(_)) => {
                    target_remotes.push(remote.clone());
                    changed_remotes.push(remote.clone());
                }
                _ => {}
            }
        }

        if deleted_remotes.is_empty() {
            continue;
        }
        blocked.insert(branch.clone());
        if target_remotes.is_empty() {
            continue;
        }
        if changed_remotes.is_empty() {
            deletions.push(BranchDeletion {
                branch,
                deleted_remotes,
                target_remotes,
            });
        } else {
            conflicts.push(BranchDeletionConflict {
                branch,
                deleted_remotes,
                changed_remotes,
            });
        }
    }

    (deletions, conflicts, blocked)
}

struct RepoRefSyncResult {
    pushed: bool,
    had_conflicts: bool,
}

#[cfg(test)]
mod tests;
