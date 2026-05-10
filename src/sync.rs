use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use console::style;
use regex::Regex;

use crate::config::{
    Config, ConflictResolutionStrategy, DEFAULT_JOBS, EndpointConfig, MirrorConfig, NamespaceKind,
    ProviderKind, RepoNameFilter, SyncVisibility, Visibility, default_work_dir, validate_config,
};
use crate::git::{
    BranchConflict, BranchDeletion, BranchUpdate, GitMirror, Redactor, RefBackup, RemoteSpec,
    is_disabled_repository_error, is_missing_repository_error, ls_remote_refs, safe_remote_name,
};
use crate::logging;
use crate::provider::{
    EndpointRepo, ProviderClient, PullRequestRequest, RemoteRepo, list_mirror_repos, repos_by_name,
};
use crate::webhook;

mod output;
mod state;

use self::output::{
    print_branch_decisions, print_branch_deletions, print_failure, print_failure_summary,
    print_tag_decisions, short_sha,
};
#[cfg(test)]
use self::state::{FailedRepo, failure_state_path};
use self::state::{
    FailureState, RefState, RemoteRefState, SyncFailure, load_failure_state, load_ref_state,
    save_failure_state, save_ref_state,
};

const CONFLICT_BRANCH_ROOT: &str = "refray/conflicts/";

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub group: Option<String>,
    pub dry_run: bool,
    pub create_missing_override: Option<bool>,
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
        bail!("jobs must be at least 1");
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
        .with_context(|| "invalid repository filter regex")?;
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

pub fn sync_webhook_repo(
    config: &Config,
    group: &str,
    repo_name: &str,
    work_dir: Option<PathBuf>,
    jobs: usize,
) -> Result<()> {
    validate_config(config)?;
    if jobs == 0 {
        bail!("jobs must be at least 1");
    }
    let work_dir = work_dir.unwrap_or_else(default_work_dir);
    fs::create_dir_all(&work_dir)
        .with_context(|| format!("failed to create {}", work_dir.display()))?;
    let mirror = config
        .mirrors
        .iter()
        .find(|mirror| mirror.name == group)
        .with_context(|| format!("no mirror group matched '{group}'"))?;
    let repo_filter = mirror.repo_filter()?;
    if !repo_filter.matches(repo_name) {
        crate::logln!(
            "  {} {} does not match configured repository filters",
            style("skip").yellow().bold(),
            style(repo_name).cyan()
        );
        return Ok(());
    }

    let tokens = config
        .sites
        .iter()
        .map(|site| site.token())
        .collect::<Result<Vec<_>>>()?;
    let redactor = Redactor::new(tokens);
    let mut ref_state = load_ref_state(&work_dir)?;
    crate::logln!();
    crate::logln!(
        "{} {}",
        style("Mirror group").cyan().bold(),
        style(&mirror.name).bold()
    );
    let mut repos = targeted_endpoint_repos(config, mirror, repo_name)?;
    let context = RepoSyncContext {
        config,
        mirror,
        work_dir: &work_dir,
        redactor,
        dry_run: false,
        jobs,
    };
    let outcome = sync_assumed_repo(
        &context,
        repo_name,
        &mut repos,
        mirror.create_missing,
        &ref_state,
    )?;
    if !outcome.created_repos.is_empty() {
        webhook::ensure_configured_webhooks(
            config,
            mirror,
            &outcome.created_repos,
            &work_dir,
            jobs,
        )?;
    }
    if let Some(update) = outcome.state_update {
        match update {
            RepoStateUpdate::Set(refs) => ref_state.set_repo(&mirror.name, repo_name, refs),
            RepoStateUpdate::Remove => ref_state.remove_repo(&mirror.name, repo_name),
        }
        save_ref_state(&work_dir, &ref_state)?;
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
    let repo_filter = mirror.repo_filter()?;

    let all_endpoint_repos =
        list_mirror_repos(context.config, mirror, &repo_filter, context.options.jobs)?;
    if !context.options.dry_run {
        webhook::ensure_configured_webhooks(
            context.config,
            mirror,
            &all_endpoint_repos,
            context.work_dir,
            context.options.jobs,
        )?;
    }

    let mut repos = repos_by_name(all_endpoint_repos);
    let retry_repo_names = context
        .retry_failed_repos
        .and_then(|repos| repos.get(&mirror.name));
    let all_repo_names = sync_candidate_repo_names(&repos, context.ref_state, mirror, &repo_filter);
    let all_repo_count = all_repo_names.len();
    let repo_names = all_repo_names
        .into_iter()
        .filter(|name| {
            context
                .repo_pattern
                .is_none_or(|pattern| pattern.is_match(name))
                && retry_repo_names.is_none_or(|repos| repos.contains(name.as_str()))
        })
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
    let jobs = context.options.jobs;
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
                        jobs,
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
                    if let Some(update) = success.outcome.state_update {
                        match update {
                            RepoStateUpdate::Set(refs) => {
                                context
                                    .ref_state
                                    .set_repo(&mirror.name, &success.repo_name, refs);
                            }
                            RepoStateUpdate::Remove => {
                                context
                                    .ref_state
                                    .remove_repo(&mirror.name, &success.repo_name);
                            }
                        }
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
        let repos = list_mirror_repos(context.config, mirror, &repo_filter, jobs)?;
        webhook::ensure_configured_webhooks(
            context.config,
            mirror,
            &repos,
            context.work_dir,
            jobs,
        )?;
    }

    Ok(failures)
}

fn sync_candidate_repo_names(
    repos: &HashMap<String, Vec<EndpointRepo>>,
    ref_state: &RefState,
    mirror: &MirrorConfig,
    repo_filter: &RepoNameFilter,
) -> BTreeSet<String> {
    let mut names = repos.keys().cloned().collect::<BTreeSet<_>>();
    if mirror.sync_visibility == SyncVisibility::All {
        names.extend(
            ref_state
                .repo_names(&mirror.name)
                .into_iter()
                .filter(|name| repo_filter.matches(name)),
        );
    }
    names
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
    context: &RepoSyncContext<'_>,
    repo_name: &str,
    existing: &mut Vec<EndpointRepo>,
    create_missing: bool,
) -> Result<Vec<EndpointRepo>> {
    let present = existing
        .iter()
        .map(|repo| repo.endpoint.clone())
        .collect::<BTreeSet<_>>();
    let template = existing.first().map(|repo| repo.repo.clone());
    let missing = context
        .mirror
        .endpoints
        .iter()
        .filter(|endpoint| !present.contains(*endpoint))
        .cloned()
        .collect::<Vec<_>>();
    let create_visibility = visibility_for_created_repo(context.mirror, template.as_ref());

    if !create_missing || context.dry_run {
        for endpoint in &missing {
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
        }
        return Ok(Vec::new());
    }

    let description = template.and_then(|repo| repo.description);
    let expected_private = matches!(create_visibility, Visibility::Private);
    let create_jobs = missing.into_iter().enumerate().collect::<Vec<_>>();
    let created = crate::parallel::map(create_jobs, context.jobs, |(index, endpoint)| {
        let site = context.config.site(&endpoint.site).unwrap();
        if site.provider == ProviderKind::Gitlab && !is_supported_gitlab_project_path(repo_name) {
            log_invalid_gitlab_project_name_skip(repo_name, &endpoint);
            return Ok(None);
        }

        crate::logln!(
            "  {} {} {}",
            style("create").green().bold(),
            style(repo_name).cyan(),
            style(format!("on {}", endpoint.label())).dim()
        );

        let client = ProviderClient::new(site)?;
        let created = match client.create_repo(
            &endpoint,
            repo_name,
            &create_visibility,
            description.as_deref(),
        ) {
            Ok(created) => created,
            Err(error)
                if site.provider == ProviderKind::Gitlab
                    && is_gitlab_invalid_project_name_error(&error) =>
            {
                log_invalid_gitlab_project_name_skip(repo_name, &endpoint);
                return Ok(None);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create {} on {}", repo_name, endpoint.label())
                });
            }
        };
        if created.private != expected_private {
            crate::logln!(
                "  {} created {} on {}, but provider reported a different visibility than requested",
                style("warn").yellow().bold(),
                style(repo_name).cyan(),
                style(endpoint.label()).dim()
            );
        }
        Ok(Some((
            index,
            EndpointRepo {
                endpoint,
                repo: created,
            },
        )))
    })?;
    let mut created = created.into_iter().flatten().collect::<Vec<_>>();
    created.sort_by_key(|(index, _)| *index);
    let created = created
        .into_iter()
        .map(|(_, repo)| repo)
        .collect::<Vec<_>>();
    existing.extend(created.clone());

    Ok(created)
}

fn visibility_for_created_repo(mirror: &MirrorConfig, template: Option<&RemoteRepo>) -> Visibility {
    template
        .map(|repo| {
            if repo.private {
                Visibility::Private
            } else {
                Visibility::Public
            }
        })
        .unwrap_or_else(|| mirror.visibility.clone())
}

fn is_supported_gitlab_project_path(name: &str) -> bool {
    if name.is_empty()
        || matches!(name.chars().next(), Some('-' | '_' | '.'))
        || matches!(name.chars().last(), Some('-' | '_' | '.'))
    {
        return false;
    }

    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".git") || lower.ends_with(".atom") {
        return false;
    }

    name.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

fn log_invalid_gitlab_project_name_skip(repo_name: &str, endpoint: &EndpointConfig) {
    crate::logln!(
        "  {} {} {}",
        style("skip").yellow().bold(),
        style(repo_name).cyan(),
        style(format!(
            "on {}: invalid GitLab project name/path",
            endpoint.label()
        ))
        .dim()
    );
}

fn is_gitlab_invalid_project_name_error(error: &anyhow::Error) -> bool {
    let text = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    text.contains("400 bad request")
        && (text.contains("project_namespace.path")
            || text.contains("can only include non-accented letters")
            || text.contains("must not start with")
            || text.contains("must start with a letter"))
}

struct RepoSyncContext<'a> {
    config: &'a Config,
    mirror: &'a MirrorConfig,
    work_dir: &'a Path,
    redactor: Redactor,
    dry_run: bool,
    jobs: usize,
}

#[derive(Default)]
struct RepoSyncOutcome {
    state_update: Option<RepoStateUpdate>,
    created_repos: Vec<EndpointRepo>,
}

enum RepoStateUpdate {
    Set(BTreeMap<String, RemoteRefState>),
    Remove,
}

fn mirror_repo_path(context: &RepoSyncContext<'_>, repo_name: &str) -> PathBuf {
    context
        .work_dir
        .join(safe_remote_name(&context.mirror.name))
        .join(format!("{}.git", safe_remote_name(repo_name)))
}

fn targeted_endpoint_repos(
    config: &Config,
    mirror: &MirrorConfig,
    repo_name: &str,
) -> Result<Vec<EndpointRepo>> {
    mirror
        .endpoints
        .iter()
        .map(|endpoint| {
            let site = config.site(&endpoint.site).unwrap();
            Ok(EndpointRepo {
                endpoint: endpoint.clone(),
                repo: RemoteRepo {
                    name: repo_name.to_string(),
                    clone_url: endpoint_clone_url(site, endpoint, repo_name)?,
                    private: matches!(mirror.visibility, Visibility::Private),
                    description: None,
                },
            })
        })
        .collect()
}

fn endpoint_clone_url(
    site: &crate::config::SiteConfig,
    endpoint: &EndpointConfig,
    repo_name: &str,
) -> Result<String> {
    let mut url = url::Url::parse(&site.base_url)
        .with_context(|| format!("invalid base URL for site '{}'", site.name))?;
    let base_path = url.path().trim_end_matches('/');
    let repo_path = format!("{}/{}.git", endpoint.namespace.trim_matches('/'), repo_name);
    if base_path.is_empty() {
        url.set_path(&repo_path);
    } else {
        url.set_path(&format!("{base_path}/{repo_path}"));
    }
    Ok(url.to_string())
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
    let previous_repo_refs = ref_state.repo(&context.mirror.name, repo_name);
    if repos.is_empty() {
        return handle_repo_deletion(
            context,
            repo_name,
            repos,
            previous_repo_refs,
            &BTreeMap::new(),
        )
        .map(|outcome| outcome.unwrap_or_default());
    }

    let initial_remotes = remote_specs(context, repos)?;
    let Some(initial_ref_state) = check_remote_refs(context, repo_name, &initial_remotes)? else {
        return Ok(RepoSyncOutcome::default());
    };
    if let Some(outcome) = handle_repo_deletion(
        context,
        repo_name,
        repos,
        previous_repo_refs,
        &initial_ref_state,
    )? {
        return Ok(outcome);
    }
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

    let path = mirror_repo_path(context, repo_name);
    let mirror_repo = GitMirror::open(path, context.redactor.clone(), context.dry_run)?;

    mirror_repo.configure_remotes(&initial_remotes)?;
    let cached_ref_state = cached_ref_state(&mirror_repo, &initial_remotes)?;
    backup_branches_deleted_everywhere(
        context,
        &mirror_repo,
        repo_name,
        detailed_repo_ref_state(previous_repo_refs).or(cached_ref_state.as_ref()),
        &initial_ref_state,
    )?;
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

    let created_repos = ensure_missing_repos(context, repo_name, repos, create_missing)?;

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
        repo_name,
        &remotes,
        repos,
        detailed_repo_ref_state(previous_repo_refs).or(cached_ref_state.as_ref()),
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
            state_update: Some(RepoStateUpdate::Set(refs)),
            created_repos,
        });
    }
    Ok(RepoSyncOutcome {
        created_repos,
        ..RepoSyncOutcome::default()
    })
}

fn sync_assumed_repo(
    context: &RepoSyncContext<'_>,
    repo_name: &str,
    repos: &mut [EndpointRepo],
    create_missing: bool,
    ref_state: &RefState,
) -> Result<RepoSyncOutcome> {
    crate::logln!();
    crate::logln!(
        "{} {}",
        style("Repo").magenta().bold(),
        style(repo_name).bold()
    );
    let previous_repo_refs = ref_state.repo(&context.mirror.name, repo_name);
    let all_remotes = remote_specs(context, repos)?;
    let Some(initial_ref_check) = check_assumed_remote_refs(context, repo_name, &all_remotes)?
    else {
        return Ok(RepoSyncOutcome::default());
    };
    if initial_ref_check.refs.is_empty() {
        crate::logln!(
            "  {} {}",
            style("skip").yellow().bold(),
            style("repository not found on any endpoint").dim()
        );
        return Ok(RepoSyncOutcome::default());
    }

    let existing_remote_names = initial_ref_check
        .refs
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut existing_repos = repos
        .iter()
        .filter(|repo| existing_remote_names.contains(&remote_name_for_endpoint_repo(repo)))
        .cloned()
        .collect::<Vec<_>>();
    let existing_remotes = all_remotes
        .iter()
        .filter(|remote| existing_remote_names.contains(&remote.name))
        .cloned()
        .collect::<Vec<_>>();

    let path = mirror_repo_path(context, repo_name);
    let mirror_repo = GitMirror::open(path, context.redactor.clone(), context.dry_run)?;
    mirror_repo.configure_remotes(&all_remotes)?;
    let cached_ref_state = cached_ref_state(&mirror_repo, &existing_remotes)?;
    backup_branches_deleted_everywhere(
        context,
        &mirror_repo,
        repo_name,
        detailed_repo_ref_state(previous_repo_refs).or(cached_ref_state.as_ref()),
        &initial_ref_check.refs,
    )?;

    for remote in &existing_remotes {
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
            if is_missing_repository_error(&error) {
                crate::logln!(
                    "  {} {} {}",
                    style("missing").yellow().bold(),
                    style(repo_name).cyan(),
                    style(format!("on {}", remote.display)).dim()
                );
                existing_repos.retain(|repo| remote_name_for_endpoint_repo(repo) != remote.name);
                continue;
            }
            return Err(error).with_context(|| format!("failed to fetch {}", remote.display));
        }
    }

    let created_repos =
        ensure_missing_repos(context, repo_name, &mut existing_repos, create_missing)?;
    if existing_repos.len() < 2 {
        crate::logln!(
            "  {} {} {}",
            style("skip").yellow().bold(),
            style(repo_name).cyan(),
            style("fewer than two endpoints have this repository").dim()
        );
        return Ok(RepoSyncOutcome {
            created_repos,
            ..RepoSyncOutcome::default()
        });
    }

    let remotes = remote_specs(context, &existing_repos)?;
    mirror_repo.configure_remotes(&remotes)?;
    let result = push_repo_refs(
        context,
        &mirror_repo,
        repo_name,
        &remotes,
        &existing_repos,
        detailed_repo_ref_state(previous_repo_refs).or(cached_ref_state.as_ref()),
        &initial_ref_check.refs,
    )?;
    if !context.dry_run && !result.had_conflicts {
        let refs = if result.pushed {
            let Some(refs) = check_remote_refs(context, repo_name, &remotes)? else {
                return Ok(RepoSyncOutcome {
                    created_repos,
                    ..RepoSyncOutcome::default()
                });
            };
            refs
        } else {
            initial_ref_check.refs
        };
        return Ok(RepoSyncOutcome {
            state_update: Some(RepoStateUpdate::Set(refs)),
            created_repos,
        });
    }
    Ok(RepoSyncOutcome {
        created_repos,
        ..RepoSyncOutcome::default()
    })
}

fn handle_repo_deletion(
    context: &RepoSyncContext<'_>,
    repo_name: &str,
    repos: &[EndpointRepo],
    previous_refs: Option<&BTreeMap<String, RemoteRefState>>,
    current_refs: &BTreeMap<String, RemoteRefState>,
) -> Result<Option<RepoSyncOutcome>> {
    match repo_deletion_decision(context.mirror, repos, previous_refs, current_refs) {
        RepoDeletionDecision::None => {
            if repos.is_empty() {
                crate::logln!(
                    "  {} {}",
                    style("skip").yellow().bold(),
                    style("repository not found on any endpoint").dim()
                );
                return Ok(Some(RepoSyncOutcome::default()));
            }
            Ok(None)
        }
        RepoDeletionDecision::DeletedEverywhere { deleted_remotes } => {
            crate::logln!(
                "  {} {} deleted on {}",
                style("deleted repo").red().bold(),
                style(repo_name).cyan(),
                deleted_remotes.join("+")
            );
            backup_deleted_repo(context, repo_name, repos, previous_refs, current_refs)?;
            Ok(Some(RepoSyncOutcome {
                state_update: (!context.dry_run).then_some(RepoStateUpdate::Remove),
                ..RepoSyncOutcome::default()
            }))
        }
        RepoDeletionDecision::Propagate {
            deleted_remotes,
            target_remotes,
        } => {
            crate::logln!(
                "  {} {} deleted on {} -> {}",
                style("deleted repo").red().bold(),
                style(repo_name).cyan(),
                deleted_remotes.join("+"),
                target_remotes.join("+")
            );
            backup_deleted_repo(context, repo_name, repos, previous_refs, current_refs)?;
            delete_repos(context, repo_name, repos, &target_remotes)?;
            Ok(Some(RepoSyncOutcome {
                state_update: (!context.dry_run).then_some(RepoStateUpdate::Remove),
                ..RepoSyncOutcome::default()
            }))
        }
        RepoDeletionDecision::Conflict {
            deleted_remotes,
            changed_remotes,
        } => {
            crate::logln!(
                "  {} repo {} was deleted on {} but changed on {} ({})",
                style("conflict").yellow().bold(),
                style(repo_name).cyan(),
                deleted_remotes.join("+"),
                changed_remotes.join("+"),
                style("skipped").dim()
            );
            fail_on_unresolved_conflict(context, "repo deletion conflict")?;
            Ok(Some(RepoSyncOutcome::default()))
        }
    }
}

fn backup_deleted_repo(
    context: &RepoSyncContext<'_>,
    repo_name: &str,
    repos: &[EndpointRepo],
    previous_refs: Option<&BTreeMap<String, RemoteRefState>>,
    current_refs: &BTreeMap<String, RemoteRefState>,
) -> Result<()> {
    if context.dry_run {
        crate::logln!(
            "  {} {} {}",
            style("dry-run").yellow().bold(),
            style("would create local backup for deleted repo").dim(),
            style(repo_name).cyan()
        );
        return Ok(());
    }

    let path = mirror_repo_path(context, repo_name);
    if repos.is_empty() && !path.exists() {
        bail!(
            "cannot back up deleted repo {} because local mirror cache {} is missing",
            repo_name,
            path.display()
        );
    }

    let mirror_repo = GitMirror::open(path, context.redactor.clone(), false)?;
    if !repos.is_empty() {
        let remotes = remote_specs(context, repos)?;
        mirror_repo.configure_remotes(&remotes)?;
        for remote in &remotes {
            mirror_repo.fetch_remote(remote).with_context(|| {
                format!("failed to fetch {} for deletion backup", remote.display)
            })?;
        }
    }

    let stamp = backup_stamp()?;
    let refs_to_backup = if current_refs.is_empty() {
        previous_refs.unwrap_or(current_refs)
    } else {
        current_refs
    };
    let backups = repo_ref_backups(repo_name, refs_to_backup, &stamp);
    if backups.is_empty() {
        crate::logln!(
            "  {} {} has no refs to bundle before deletion",
            style("backup").yellow().bold(),
            style(repo_name).cyan()
        );
        return Ok(());
    }

    let refs = mirror_repo.backup_refs(&backups)?;
    let bundle_path = backup_dir(context, repo_name).join(format!("repo-{stamp}.bundle"));
    mirror_repo.create_bundle(&bundle_path, &refs)?;
    Ok(())
}

fn delete_repos(
    context: &RepoSyncContext<'_>,
    repo_name: &str,
    repos: &[EndpointRepo],
    target_remotes: &[String],
) -> Result<()> {
    let delete_jobs = repos
        .iter()
        .filter(|repo| target_remotes.contains(&remote_name_for_endpoint_repo(repo)))
        .cloned()
        .collect::<Vec<_>>();
    if context.dry_run {
        for repo in &delete_jobs {
            crate::logln!(
                "  {} {} {}",
                style("would delete").red().bold(),
                style(repo_name).cyan(),
                style(format!("from {}", repo.endpoint.label())).dim()
            );
        }
        return Ok(());
    }

    crate::parallel::map(delete_jobs, context.jobs, |repo| {
        crate::logln!(
            "  {} {} {}",
            style("delete").red().bold(),
            style(repo_name).cyan(),
            style(format!("from {}", repo.endpoint.label())).dim()
        );
        let site = context.config.site(&repo.endpoint.site).unwrap();
        let client = ProviderClient::new(site)?;
        client
            .delete_repo(&repo.endpoint, repo_name)
            .with_context(|| {
                format!(
                    "failed to delete {} from {}",
                    repo_name,
                    repo.endpoint.label()
                )
            })?;
        Ok(())
    })?;
    Ok(())
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
    enum RemoteRefCheck {
        Found(String, RemoteRefState),
        Blocked,
    }

    let ref_jobs = remotes.to_vec();
    let results = crate::parallel::map(ref_jobs, context.jobs, |remote| {
        crate::logln!(
            "  {} {}",
            style("check refs").cyan().bold(),
            style(&remote.display).dim()
        );
        match ls_remote_refs(&remote, &context.redactor) {
            Ok(snapshot) => Ok(RemoteRefCheck::Found(remote.name, snapshot.into())),
            Err(error) if is_disabled_repository_error(&error) => {
                crate::logln!(
                    "  {} {} {}",
                    style("skip").yellow().bold(),
                    style(repo_name).cyan(),
                    style(format!("provider blocked access on {}", remote.display)).dim()
                );
                Ok(RemoteRefCheck::Blocked)
            }
            Err(error) => {
                Err(error).with_context(|| format!("failed to check refs for {}", remote.display))
            }
        }
    })?;

    let mut refs = BTreeMap::new();
    for result in results {
        match result {
            RemoteRefCheck::Found(remote, refs_for_remote) => {
                refs.insert(remote, refs_for_remote);
            }
            RemoteRefCheck::Blocked => return Ok(None),
        }
    }
    Ok(Some(refs))
}

struct AssumedRemoteRefState {
    refs: BTreeMap<String, RemoteRefState>,
}

fn check_assumed_remote_refs(
    context: &RepoSyncContext<'_>,
    repo_name: &str,
    remotes: &[RemoteSpec],
) -> Result<Option<AssumedRemoteRefState>> {
    enum RemoteRefCheck {
        Found(String, RemoteRefState),
        Missing(String),
        Blocked,
    }

    let ref_jobs = remotes.to_vec();
    let results = crate::parallel::map(ref_jobs, context.jobs, |remote| {
        crate::logln!(
            "  {} {}",
            style("probe refs").cyan().bold(),
            style(&remote.display).dim()
        );
        match ls_remote_refs(&remote, &context.redactor) {
            Ok(snapshot) => Ok(RemoteRefCheck::Found(remote.name, snapshot.into())),
            Err(error) if is_missing_repository_error(&error) => {
                crate::logln!(
                    "  {} {} {}",
                    style("missing").yellow().bold(),
                    style(repo_name).cyan(),
                    style(format!("on {}", remote.display)).dim()
                );
                Ok(RemoteRefCheck::Missing(remote.name))
            }
            Err(error) if is_disabled_repository_error(&error) => {
                crate::logln!(
                    "  {} {} {}",
                    style("skip").yellow().bold(),
                    style(repo_name).cyan(),
                    style(format!("provider blocked access on {}", remote.display)).dim()
                );
                Ok(RemoteRefCheck::Blocked)
            }
            Err(error) => {
                Err(error).with_context(|| format!("failed to check refs for {}", remote.display))
            }
        }
    })?;

    let mut refs = BTreeMap::new();
    for result in results {
        match result {
            RemoteRefCheck::Found(remote, refs_for_remote) => {
                refs.insert(remote, refs_for_remote);
            }
            RemoteRefCheck::Missing(remote) => {
                let _ = remote;
            }
            RemoteRefCheck::Blocked => return Ok(None),
        }
    }
    Ok(Some(AssumedRemoteRefState { refs }))
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
        remotes.push(RemoteSpec {
            name: remote_name_for_endpoint(&endpoint_repo.endpoint),
            url: client.authenticated_clone_url(&endpoint_repo.repo.clone_url)?,
            display: endpoint_repo.endpoint.label(),
        });
    }

    Ok(remotes)
}

fn push_repo_refs(
    context: &RepoSyncContext<'_>,
    mirror_repo: &GitMirror,
    repo_name: &str,
    remotes: &[RemoteSpec],
    repos: &[EndpointRepo],
    previous_refs: Option<&BTreeMap<String, RemoteRefState>>,
    current_refs: &BTreeMap<String, RemoteRefState>,
) -> Result<RepoRefSyncResult> {
    let (branch_deletions, deletion_conflicts, blocked_branches) =
        branch_deletion_decisions(remotes, previous_refs, current_refs);
    let had_deletion_conflicts = !deletion_conflicts.is_empty();
    for conflict in &deletion_conflicts {
        crate::logln!(
            "  {} branch {} was deleted on {} but changed on {} ({})",
            style("conflict").yellow().bold(),
            style(&conflict.branch).cyan(),
            conflict.deleted_remotes.join("+"),
            conflict.changed_remotes.join("+"),
            style("skipped").dim()
        );
    }
    if had_deletion_conflicts {
        fail_on_unresolved_conflict(context, "branch deletion conflict")?;
    }

    let (branches, conflicts) = mirror_repo.branch_decisions(remotes)?;
    let branches_to_push = branches
        .into_iter()
        .filter(|branch| !is_internal_conflict_branch(&branch.branch))
        .filter(|branch| !blocked_branches.contains(&branch.branch))
        .filter(|branch| !branch.target_remotes.is_empty())
        .collect::<Vec<_>>();
    let mut unresolved_branch_conflicts = Vec::new();
    let mut rebased_branch_updates = Vec::new();
    for conflict in conflicts {
        if is_internal_conflict_branch(&conflict.branch) {
            continue;
        }
        if blocked_branches.contains(&conflict.branch) {
            continue;
        }
        match resolve_branch_conflict(context, mirror_repo, remotes, conflict)? {
            BranchConflictResolution::Rebased(updates) => rebased_branch_updates.extend(updates),
            BranchConflictResolution::PullRequest(conflict) => {
                unresolved_branch_conflicts.push(conflict)
            }
        }
    }
    let had_branch_conflicts = !unresolved_branch_conflicts.is_empty();
    let unresolved_branch_names = unresolved_branch_conflicts
        .iter()
        .map(|conflict| conflict.branch.clone())
        .collect::<BTreeSet<_>>();
    let unresolved_or_blocked_branches = unresolved_branch_names
        .union(&blocked_branches)
        .cloned()
        .collect::<BTreeSet<_>>();
    let stale_conflict_branches = conflict_pr_base_branches(current_refs)
        .difference(&unresolved_or_blocked_branches)
        .cloned()
        .collect::<BTreeSet<_>>();

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
    if had_tag_conflicts {
        fail_on_unresolved_conflict(context, "tag conflict")?;
    }

    let pushed_branch_names = branch_names(&branches_to_push);
    let rebased_branch_names = branch_names_from_updates(&rebased_branch_updates);
    let mut cleanup_branches = stale_conflict_branches;
    cleanup_branches.retain(|branch| {
        !pushed_branch_names.contains(branch) && !rebased_branch_names.contains(branch)
    });

    if branches_to_push.is_empty()
        && rebased_branch_updates.is_empty()
        && tags_to_push.is_empty()
        && unresolved_branch_conflicts.is_empty()
    {
        if !branch_deletions.is_empty() {
            print_branch_deletions(&branch_deletions);
            backup_deleted_branches(
                context,
                mirror_repo,
                repo_name,
                &branch_deletions,
                current_refs,
            )?;
            mirror_repo.delete_branches(remotes, &branch_deletions)?;
        }
        if !cleanup_branches.is_empty() {
            close_resolved_pull_requests(context, mirror_repo, remotes, repos, &cleanup_branches)?;
        }
        if !branch_deletions.is_empty() || !cleanup_branches.is_empty() {
            return Ok(RepoRefSyncResult {
                pushed: true,
                had_conflicts: had_deletion_conflicts,
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
        backup_deleted_branches(
            context,
            mirror_repo,
            repo_name,
            &branch_deletions,
            current_refs,
        )?;
        mirror_repo.delete_branches(remotes, &branch_deletions)?;
    }
    if !branches_to_push.is_empty() {
        print_branch_decisions(&branches_to_push);
        mirror_repo.push_branches(remotes, &branches_to_push)?;
        close_resolved_pull_requests(context, mirror_repo, remotes, repos, &pushed_branch_names)?;
    }
    if !rebased_branch_updates.is_empty() {
        mirror_repo.push_branch_updates(remotes, &rebased_branch_updates)?;
        close_resolved_pull_requests(context, mirror_repo, remotes, repos, &rebased_branch_names)?;
    }
    if !tags_to_push.is_empty() {
        print_tag_decisions(&tags_to_push);
        mirror_repo.push_tags(remotes, &tags_to_push)?;
    }
    if !unresolved_branch_conflicts.is_empty() {
        open_conflict_pull_requests(
            context,
            mirror_repo,
            remotes,
            repos,
            &unresolved_branch_conflicts,
        )?;
    }
    if !cleanup_branches.is_empty() {
        close_resolved_pull_requests(context, mirror_repo, remotes, repos, &cleanup_branches)?;
    }
    Ok(RepoRefSyncResult {
        pushed: !branches_to_push.is_empty()
            || !rebased_branch_updates.is_empty()
            || !tags_to_push.is_empty()
            || !branch_deletions.is_empty()
            || !cleanup_branches.is_empty(),
        had_conflicts: had_branch_conflicts || had_tag_conflicts || had_deletion_conflicts,
    })
}

fn backup_deleted_branches(
    context: &RepoSyncContext<'_>,
    mirror_repo: &GitMirror,
    repo_name: &str,
    deletions: &[BranchDeletion],
    current_refs: &BTreeMap<String, RemoteRefState>,
) -> Result<()> {
    if context.dry_run {
        crate::logln!(
            "  {} {} deleted branch backup{}",
            style("dry-run").yellow().bold(),
            style("would create").dim(),
            if deletions.len() == 1 { "" } else { "s" }
        );
        return Ok(());
    }

    let stamp = backup_stamp()?;
    let backups = branch_ref_backups(deletions, current_refs, &stamp);
    if backups.is_empty() {
        bail!("cannot back up branch deletion because no target branch refs were available");
    }
    let refs = mirror_repo.backup_refs(&backups)?;
    let bundle_path = backup_dir(context, repo_name).join(format!("branches-{stamp}.bundle"));
    mirror_repo.create_bundle(&bundle_path, &refs)?;
    Ok(())
}

fn backup_branches_deleted_everywhere(
    context: &RepoSyncContext<'_>,
    mirror_repo: &GitMirror,
    repo_name: &str,
    previous_refs: Option<&BTreeMap<String, RemoteRefState>>,
    current_refs: &BTreeMap<String, RemoteRefState>,
) -> Result<()> {
    let Some(previous_refs) = previous_refs else {
        return Ok(());
    };
    let stamp = backup_stamp()?;
    let backups = branches_deleted_everywhere_backups(previous_refs, current_refs, &stamp);
    if backups.is_empty() {
        return Ok(());
    }
    if context.dry_run {
        crate::logln!(
            "  {} {} branch backup{} for refs deleted everywhere",
            style("dry-run").yellow().bold(),
            style("would create").dim(),
            if backups.len() == 1 { "" } else { "s" }
        );
        return Ok(());
    }
    let refs = mirror_repo.backup_refs(&backups)?;
    let bundle_path = backup_dir(context, repo_name).join(format!("branches-{stamp}.bundle"));
    mirror_repo.create_bundle(&bundle_path, &refs)?;
    Ok(())
}

enum BranchConflictResolution {
    Rebased(Vec<BranchUpdate>),
    PullRequest(BranchConflict),
}

fn resolve_branch_conflict(
    context: &RepoSyncContext<'_>,
    mirror_repo: &GitMirror,
    remotes: &[RemoteSpec],
    conflict: BranchConflict,
) -> Result<BranchConflictResolution> {
    log_branch_conflict(&conflict, context.mirror.conflict_resolution.clone());
    match &context.mirror.conflict_resolution {
        ConflictResolutionStrategy::Fail => {
            bail!("branch {} diverged across endpoints", conflict.branch)
        }
        ConflictResolutionStrategy::PullRequest => {
            Ok(BranchConflictResolution::PullRequest(conflict))
        }
        ConflictResolutionStrategy::AutoRebase => {
            let decision = mirror_repo.auto_rebase_branch_conflict(
                remotes,
                &conflict.branch,
                &conflict.tips,
            )?;
            log_rebase_decision(&decision.branch, &decision.sha, &decision.updates);
            Ok(BranchConflictResolution::Rebased(decision.updates))
        }
        ConflictResolutionStrategy::AutoRebasePullRequest => {
            match mirror_repo.auto_rebase_branch_conflict(remotes, &conflict.branch, &conflict.tips)
            {
                Ok(decision) => {
                    log_rebase_decision(&decision.branch, &decision.sha, &decision.updates);
                    Ok(BranchConflictResolution::Rebased(decision.updates))
                }
                Err(error) => {
                    crate::logln!(
                        "  {} branch {} auto-rebase failed; opening pull requests ({})",
                        style("fallback").yellow().bold(),
                        style(&conflict.branch).cyan(),
                        style(error.to_string()).dim()
                    );
                    Ok(BranchConflictResolution::PullRequest(conflict))
                }
            }
        }
    }
}

fn fail_on_unresolved_conflict(context: &RepoSyncContext<'_>, label: &str) -> Result<()> {
    if matches!(
        &context.mirror.conflict_resolution,
        ConflictResolutionStrategy::Fail
    ) {
        bail!("{label} detected");
    }
    Ok(())
}

fn log_branch_conflict(conflict: &BranchConflict, strategy: ConflictResolutionStrategy) {
    let details = conflict
        .tips
        .iter()
        .map(|(remote, sha)| format!("{remote}@{}", short_sha(sha)))
        .collect::<Vec<_>>()
        .join(", ");
    let action = match strategy {
        ConflictResolutionStrategy::Fail => "failing",
        ConflictResolutionStrategy::AutoRebase => "auto-rebase",
        ConflictResolutionStrategy::PullRequest => "pull-request",
        ConflictResolutionStrategy::AutoRebasePullRequest => "auto-rebase/pull-request",
    };
    crate::logln!(
        "  {} branch {} diverged across {} ({})",
        style("conflict").yellow().bold(),
        style(&conflict.branch).cyan(),
        details,
        style(action).dim()
    );
}

fn log_rebase_decision(branch: &str, sha: &str, updates: &[BranchUpdate]) {
    crate::logln!(
        "  {} branch {} resolved at {} ({} push{})",
        style("auto-rebase").green().bold(),
        style(branch).cyan(),
        style(format!("@{}", short_sha(sha))).dim(),
        updates.len(),
        if updates.len() == 1 { "" } else { "es" }
    );
}

fn open_conflict_pull_requests(
    context: &RepoSyncContext<'_>,
    mirror_repo: &GitMirror,
    remotes: &[RemoteSpec],
    repos: &[EndpointRepo],
    conflicts: &[BranchConflict],
) -> Result<()> {
    let repos_by_remote = endpoint_repos_by_remote_name(context, repos)?;
    for conflict in conflicts {
        for (target_remote, target_sha) in &conflict.tips {
            if !remotes.iter().any(|remote| &remote.name == target_remote) {
                continue;
            }
            let Some(target_repo) = repos_by_remote.get(target_remote) else {
                continue;
            };
            for (source_remote, source_sha) in
                conflict.tips.iter().filter(|(_, sha)| sha != target_sha)
            {
                if mirror_repo.is_ancestor(source_sha, target_sha)? {
                    continue;
                }
                let head_branch = conflict_pr_branch(&conflict.branch, source_remote, source_sha);
                let update = BranchUpdate {
                    branch: head_branch.clone(),
                    sha: source_sha.clone(),
                    target_remote: target_remote.clone(),
                    force: false,
                };
                mirror_repo.push_branch_updates(remotes, &[update])?;
                let title = format!(
                    "Resolve refray conflict: {} from {}",
                    conflict.branch, source_remote
                );
                let body = format!(
                    "refray detected divergent branch tips and opened this pull request to merge {}@{} into {}@{}.",
                    source_remote,
                    short_sha(source_sha),
                    target_remote,
                    short_sha(target_sha)
                );
                crate::logln!(
                    "  {} branch {} {} -> {}",
                    style("pull request").cyan().bold(),
                    style(&conflict.branch).cyan(),
                    source_remote,
                    target_remote
                );
                if context.dry_run {
                    continue;
                }
                let site = context.config.site(&target_repo.endpoint.site).unwrap();
                let client = ProviderClient::new(site)?;
                let pr = client.open_pull_request(
                    &target_repo.endpoint,
                    &target_repo.repo,
                    &PullRequestRequest {
                        title,
                        body,
                        head_branch,
                        base_branch: conflict.branch.clone(),
                    },
                )?;
                if let Some(url) = pr.url {
                    crate::logln!(
                        "    {} {}",
                        style("opened").green().bold(),
                        style(url).dim()
                    );
                }
            }
        }
    }
    Ok(())
}

fn close_resolved_pull_requests(
    context: &RepoSyncContext<'_>,
    mirror_repo: &GitMirror,
    remotes: &[RemoteSpec],
    repos: &[EndpointRepo],
    branches: &BTreeSet<String>,
) -> Result<()> {
    if context.dry_run || branches.is_empty() {
        return Ok(());
    }
    let repos_by_remote = endpoint_repos_by_remote_name(context, repos)?;
    for remote in remotes {
        let Some(endpoint_repo) = repos_by_remote.get(&remote.name) else {
            continue;
        };
        let site = context.config.site(&endpoint_repo.endpoint.site).unwrap();
        let client = ProviderClient::new(site)?;
        for branch in branches {
            let prefix = conflict_pr_branch_prefix(branch);
            let closed = client.close_pull_requests_by_head_prefix(
                &endpoint_repo.endpoint,
                &endpoint_repo.repo,
                branch,
                &prefix,
            )?;
            if closed > 0 {
                crate::logln!(
                    "  {} {} stale pull request{} for branch {} on {}",
                    style("close").green().bold(),
                    closed,
                    if closed == 1 { "" } else { "s" },
                    style(branch).cyan(),
                    style(&remote.display).dim()
                );
            }
            delete_conflict_branches(mirror_repo, remotes, remote, &prefix)?;
        }
    }
    Ok(())
}

fn delete_conflict_branches(
    mirror_repo: &GitMirror,
    remotes: &[RemoteSpec],
    remote: &RemoteSpec,
    prefix: &str,
) -> Result<()> {
    let deletions = mirror_repo
        .remote_branch_names_with_prefix(&remote.name, prefix)?
        .into_iter()
        .map(|branch| BranchDeletion {
            branch,
            deleted_remotes: Vec::new(),
            target_remotes: vec![remote.name.clone()],
        })
        .collect::<Vec<_>>();
    if deletions.is_empty() {
        return Ok(());
    }
    mirror_repo.delete_branches(remotes, &deletions)
}

fn endpoint_repos_by_remote_name<'a>(
    context: &RepoSyncContext<'_>,
    repos: &'a [EndpointRepo],
) -> Result<HashMap<String, &'a EndpointRepo>> {
    let mut output = HashMap::new();
    for repo in repos {
        let remote_name = remote_name_for_endpoint_repo(repo);
        if context
            .mirror
            .endpoints
            .iter()
            .any(|endpoint| endpoint == &repo.endpoint)
        {
            output.insert(remote_name, repo);
        }
    }
    Ok(output)
}

fn remote_name_for_endpoint_repo(endpoint_repo: &EndpointRepo) -> String {
    remote_name_for_endpoint(&endpoint_repo.endpoint)
}

fn remote_name_for_endpoint(endpoint: &EndpointConfig) -> String {
    format!(
        "r{}_{}_{}",
        hex_component(&endpoint.site),
        namespace_kind_key(&endpoint.kind),
        hex_component(&endpoint.namespace)
    )
}

fn namespace_kind_key(kind: &NamespaceKind) -> &'static str {
    match kind {
        NamespaceKind::User => "user",
        NamespaceKind::Org => "org",
        NamespaceKind::Group => "group",
    }
}

fn branch_names(branches: &[crate::git::BranchDecision]) -> BTreeSet<String> {
    branches
        .iter()
        .map(|branch| branch.branch.clone())
        .collect()
}

fn branch_names_from_updates(updates: &[BranchUpdate]) -> BTreeSet<String> {
    updates.iter().map(|update| update.branch.clone()).collect()
}

fn conflict_pr_base_branches(refs: &BTreeMap<String, RemoteRefState>) -> BTreeSet<String> {
    refs.values()
        .flat_map(|remote| remote.branches.keys())
        .filter_map(|branch| conflict_pr_base_branch(branch))
        .collect()
}

fn conflict_pr_branch(branch: &str, source_remote: &str, source_sha: &str) -> String {
    format!(
        "{}from-{}-{}",
        conflict_pr_branch_prefix(branch),
        safe_ref_component(source_remote),
        short_sha(source_sha)
    )
}

fn conflict_pr_branch_prefix(branch: &str) -> String {
    format!("{}{}/", CONFLICT_BRANCH_ROOT, hex_component(branch))
}

fn is_internal_conflict_branch(branch: &str) -> bool {
    branch.starts_with(CONFLICT_BRANCH_ROOT)
}

fn conflict_pr_base_branch(branch: &str) -> Option<String> {
    let rest = branch.strip_prefix(CONFLICT_BRANCH_ROOT)?;
    let (encoded, _) = rest.split_once('/')?;
    decode_hex_component(encoded)
}

fn backup_dir(context: &RepoSyncContext<'_>, repo_name: &str) -> PathBuf {
    context
        .work_dir
        .join("backups")
        .join(safe_remote_name(&context.mirror.name))
        .join(safe_remote_name(repo_name))
}

fn backup_stamp() -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .with_context(|| "system clock is before UNIX_EPOCH")?;
    Ok(format!("{}-{:09}", now.as_secs(), now.subsec_nanos()))
}

fn branch_ref_backups(
    deletions: &[BranchDeletion],
    current_refs: &BTreeMap<String, RemoteRefState>,
    stamp: &str,
) -> Vec<RefBackup> {
    let mut backups = Vec::new();
    let mut seen = BTreeSet::new();
    for deletion in deletions {
        for remote in &deletion.target_remotes {
            let Some(sha) = current_refs
                .get(remote)
                .and_then(|refs| refs.branches.get(&deletion.branch))
            else {
                continue;
            };
            if !seen.insert((deletion.branch.clone(), sha.clone())) {
                continue;
            }
            backups.push(RefBackup {
                refname: format!(
                    "refs/refray-backups/branches/{}/{}/{}",
                    hex_component(&deletion.branch),
                    stamp,
                    hex_component(remote)
                ),
                sha: sha.clone(),
                description: format!(
                    "branch {} from {} before propagated deletion",
                    deletion.branch, remote
                ),
            });
        }
    }
    backups
}

fn branches_deleted_everywhere_backups(
    previous_refs: &BTreeMap<String, RemoteRefState>,
    current_refs: &BTreeMap<String, RemoteRefState>,
    stamp: &str,
) -> Vec<RefBackup> {
    let mut branches = BTreeSet::new();
    for refs in previous_refs.values() {
        branches.extend(
            refs.branches
                .keys()
                .filter(|branch| !is_internal_conflict_branch(branch))
                .cloned(),
        );
    }

    let mut backups = Vec::new();
    for branch in branches {
        if current_refs
            .values()
            .any(|refs| refs.branches.contains_key(&branch))
        {
            continue;
        }
        let mut seen_shas = BTreeSet::new();
        for (remote, refs) in previous_refs {
            let Some(sha) = refs.branches.get(&branch) else {
                continue;
            };
            if !seen_shas.insert(sha.clone()) {
                continue;
            }
            backups.push(RefBackup {
                refname: format!(
                    "refs/refray-backups/branches/{}/{}/deleted-everywhere-{}",
                    hex_component(&branch),
                    stamp,
                    hex_component(remote)
                ),
                sha: sha.clone(),
                description: format!(
                    "branch {branch} from {remote} before all endpoints pruned it"
                ),
            });
        }
    }
    backups
}

fn repo_ref_backups(
    repo_name: &str,
    refs_by_remote: &BTreeMap<String, RemoteRefState>,
    stamp: &str,
) -> Vec<RefBackup> {
    let mut backups = Vec::new();
    for (remote, refs) in refs_by_remote {
        for (branch, sha) in &refs.branches {
            backups.push(RefBackup {
                refname: format!(
                    "refs/refray-backups/repos/{}/{}/heads/{}",
                    stamp,
                    hex_component(remote),
                    hex_component(branch)
                ),
                sha: sha.clone(),
                description: format!("repo {repo_name} branch {branch} from {remote}"),
            });
        }
        for (tag, sha) in &refs.tags {
            backups.push(RefBackup {
                refname: format!(
                    "refs/refray-backups/repos/{}/{}/tags/{}",
                    stamp,
                    hex_component(remote),
                    hex_component(tag)
                ),
                sha: sha.clone(),
                description: format!("repo {repo_name} tag {tag} from {remote}"),
            });
        }
    }
    backups
}

fn hex_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value.bytes() {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_hex_component(value: &str) -> Option<String> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        bytes.push((high << 4) | low);
    }
    String::from_utf8(bytes).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn safe_ref_component(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            output.push(ch);
        } else {
            output.push('-');
        }
    }
    output.trim_matches('-').to_string()
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
        branches.extend(
            refs.branches
                .keys()
                .filter(|branch| !is_internal_conflict_branch(branch))
                .cloned(),
        );
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

#[derive(Debug, Eq, PartialEq)]
enum RepoDeletionDecision {
    None,
    DeletedEverywhere {
        deleted_remotes: Vec<String>,
    },
    Propagate {
        deleted_remotes: Vec<String>,
        target_remotes: Vec<String>,
    },
    Conflict {
        deleted_remotes: Vec<String>,
        changed_remotes: Vec<String>,
    },
}

fn repo_deletion_decision(
    mirror: &MirrorConfig,
    repos: &[EndpointRepo],
    previous_refs: Option<&BTreeMap<String, RemoteRefState>>,
    current_refs: &BTreeMap<String, RemoteRefState>,
) -> RepoDeletionDecision {
    if !mirror.delete_missing {
        return RepoDeletionDecision::None;
    }
    let Some(previous_refs) = previous_refs else {
        return RepoDeletionDecision::None;
    };
    let remote_names = mirror
        .endpoints
        .iter()
        .map(remote_name_for_endpoint)
        .collect::<Vec<_>>();
    if remote_names.is_empty() {
        return RepoDeletionDecision::None;
    }

    let present_remotes = repos
        .iter()
        .map(remote_name_for_endpoint_repo)
        .collect::<BTreeSet<_>>();
    if present_remotes.len() == remote_names.len() {
        return RepoDeletionDecision::None;
    }

    let deleted_remotes = remote_names
        .iter()
        .filter(|remote| !present_remotes.contains(remote.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let target_remotes = remote_names
        .iter()
        .filter(|remote| present_remotes.contains(remote.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if target_remotes.is_empty() {
        return RepoDeletionDecision::DeletedEverywhere { deleted_remotes };
    }

    if remote_names
        .iter()
        .any(|remote| !previous_refs.contains_key(remote))
    {
        return RepoDeletionDecision::None;
    }

    let changed_remotes = target_remotes
        .iter()
        .filter(|remote| current_refs.get(remote.as_str()) != previous_refs.get(remote.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if changed_remotes.is_empty() {
        RepoDeletionDecision::Propagate {
            deleted_remotes,
            target_remotes,
        }
    } else {
        RepoDeletionDecision::Conflict {
            deleted_remotes,
            changed_remotes,
        }
    }
}

struct RepoRefSyncResult {
    pushed: bool,
    had_conflicts: bool,
}

#[cfg(test)]
#[path = "../tests/unit/sync.rs"]
mod tests;
