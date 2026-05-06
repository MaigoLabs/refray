use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use console::style;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::{Config, EndpointConfig, MirrorConfig, default_work_dir, validate_config};
use crate::git::{
    GitMirror, Redactor, RemoteRefSnapshot, RemoteSpec, is_disabled_repository_error,
    ls_remote_refs, safe_remote_name,
};
use crate::provider::{EndpointRepo, ProviderClient, repos_by_name};

const FAILURE_STATE_FILE: &str = "failed-repos.toml";
const REF_STATE_FILE: &str = "ref-state.toml";

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    pub group: Option<String>,
    pub dry_run: bool,
    pub create_missing_override: Option<bool>,
    pub force_override: Option<bool>,
    pub repo_pattern: Option<String>,
    pub retry_failed: bool,
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

#[derive(Debug)]
struct SyncFailure {
    scope: String,
    error: String,
    retry: Option<FailedRepo>,
}

impl SyncFailure {
    fn group(scope: String, error: anyhow::Error) -> Self {
        Self {
            scope,
            error: format_error(&error),
            retry: None,
        }
    }

    fn repo(group: String, repo: String, error: anyhow::Error) -> Self {
        Self {
            scope: format!("{group}/{repo}"),
            error: format_error(&error),
            retry: Some(FailedRepo { group, repo }),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct FailedRepo {
    group: String,
    repo: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct FailureState {
    #[serde(default)]
    repos: Vec<FailedRepo>,
}

impl FailureState {
    fn from_failures(failures: &[SyncFailure]) -> Self {
        let repos = failures
            .iter()
            .filter_map(|failure| failure.retry.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Self { repos }
    }

    fn repos_by_group(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut output = BTreeMap::<String, BTreeSet<String>>::new();
        for failure in &self.repos {
            output
                .entry(failure.group.clone())
                .or_default()
                .insert(failure.repo.clone());
        }
        output
    }
}

fn load_failure_state(work_dir: &Path) -> Result<FailureState> {
    let path = failure_state_path(work_dir);
    if !path.exists() {
        return Ok(FailureState::default());
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

fn save_failure_state(work_dir: &Path, state: &FailureState) -> Result<()> {
    let path = failure_state_path(work_dir);
    if state.repos.is_empty() {
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let contents = toml::to_string_pretty(state)?;
    fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn failure_state_path(work_dir: &Path) -> PathBuf {
    work_dir.join(FAILURE_STATE_FILE)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct RemoteRefState {
    hash: String,
    refs: usize,
}

impl From<RemoteRefSnapshot> for RemoteRefState {
    fn from(value: RemoteRefSnapshot) -> Self {
        Self {
            hash: value.hash,
            refs: value.refs,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RefState {
    #[serde(default)]
    repos: BTreeMap<String, BTreeMap<String, BTreeMap<String, RemoteRefState>>>,
}

impl RefState {
    fn repo_matches(
        &self,
        group: &str,
        repo: &str,
        refs: &BTreeMap<String, RemoteRefState>,
    ) -> bool {
        self.repos.get(group).and_then(|repos| repos.get(repo)) == Some(refs)
    }

    fn set_repo(&mut self, group: &str, repo: &str, refs: BTreeMap<String, RemoteRefState>) {
        self.repos
            .entry(group.to_string())
            .or_default()
            .insert(repo.to_string(), refs);
    }
}

fn load_ref_state(work_dir: &Path) -> Result<RefState> {
    let path = ref_state_path(work_dir);
    if !path.exists() {
        return Ok(RefState::default());
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

fn save_ref_state(work_dir: &Path, state: &RefState) -> Result<()> {
    let path = ref_state_path(work_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let contents = toml::to_string_pretty(state)?;
    fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn ref_state_path(work_dir: &Path) -> PathBuf {
    work_dir.join(REF_STATE_FILE)
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
    println!();
    println!(
        "{} {}",
        style("Mirror group").cyan().bold(),
        style(&mirror.name).bold()
    );
    let create_missing = context
        .options
        .create_missing_override
        .unwrap_or(mirror.create_missing);
    let allow_force = context.options.force_override.unwrap_or(mirror.allow_force);

    let mut all_endpoint_repos = Vec::new();
    for endpoint in &mirror.endpoints {
        let site = context.config.site(&endpoint.site).unwrap();
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
            println!(
                "  {} no previously failed repositories were found in this group ({} saved)",
                style("skip").yellow().bold(),
                retry_repo_names.len()
            );
        } else if context.retry_failed_repos.is_some() {
            println!(
                "  {} no previous failures for this group",
                style("skip").yellow().bold()
            );
        } else if let Some(pattern) = context.repo_pattern {
            println!(
                "  {} no repositories match {} ({} discovered)",
                style("skip").yellow().bold(),
                style(pattern.as_str()).cyan(),
                all_repo_count
            );
        } else {
            println!(
                "  {} mirror group has no repositories",
                style("skip").yellow().bold()
            );
        }
        return Ok(Vec::new());
    }
    if let Some(pattern) = context.repo_pattern {
        println!(
            "  {} {} of {} repositories match {}",
            style("filter").cyan().bold(),
            repo_names.len(),
            all_repo_count,
            style(pattern.as_str()).cyan()
        );
    }
    if let Some(retry_repo_names) = retry_repo_names {
        println!(
            "  {} retrying {} of {} previously failed repositories",
            style("retry").cyan().bold(),
            repo_names.len(),
            retry_repo_names.len()
        );
    }

    let mut failures = Vec::new();
    for repo_name in repo_names {
        let mut existing = repos.remove(&repo_name).unwrap_or_default();
        let repo_context = RepoSyncContext {
            config: context.config,
            mirror,
            work_dir: context.work_dir,
            redactor: context.redactor.clone(),
            dry_run: context.options.dry_run,
            allow_force,
        };
        if let Err(error) = sync_repo(
            &repo_context,
            &repo_name,
            &mut existing,
            create_missing,
            &mut *context.ref_state,
        )
        .with_context(|| format!("failed to sync repo {repo_name}"))
        {
            let scope = format!("{}/{}", mirror.name, repo_name);
            print_failure(&scope, &error);
            failures.push(SyncFailure::repo(mirror.name.clone(), repo_name, error));
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
    ref_state: &mut RefState,
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

    let initial_remotes = remote_specs(context, repos)?;
    let Some(initial_ref_state) = check_remote_refs(context, repo_name, &initial_remotes)? else {
        return Ok(());
    };
    if !context.dry_run
        && all_configured_endpoints_present(context.mirror, repos)
        && ref_state.repo_matches(&context.mirror.name, repo_name, &initial_ref_state)
    {
        println!(
            "  {} refs unchanged since last successful sync",
            style("up-to-date").green().bold()
        );
        return Ok(());
    }

    let path = context
        .work_dir
        .join(safe_remote_name(&context.mirror.name))
        .join(format!("{}.git", safe_remote_name(repo_name)));
    let mirror_repo = GitMirror::open(path, context.redactor.clone(), context.dry_run)?;

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

    let result = push_repo_refs(context, &mirror_repo, &remotes)?;
    if !context.dry_run && !result.had_conflicts {
        let refs = if result.pushed {
            let Some(refs) = check_remote_refs(context, repo_name, &remotes)? else {
                return Ok(());
            };
            refs
        } else {
            initial_ref_state
        };
        ref_state.set_repo(&context.mirror.name, repo_name, refs);
    }
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

fn check_remote_refs(
    context: &RepoSyncContext<'_>,
    repo_name: &str,
    remotes: &[RemoteSpec],
) -> Result<Option<BTreeMap<String, RemoteRefState>>> {
    let mut refs = BTreeMap::new();
    for remote in remotes {
        println!(
            "  {} {}",
            style("check refs").cyan().bold(),
            style(&remote.display).dim()
        );
        let snapshot = match ls_remote_refs(remote, &context.redactor) {
            Ok(snapshot) => snapshot,
            Err(error) if is_disabled_repository_error(&error) => {
                println!(
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
) -> Result<RepoRefSyncResult> {
    let (branches, conflicts) = mirror_repo.branch_decisions(remotes, context.allow_force)?;
    let had_branch_conflicts = !conflicts.is_empty();
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
        return Ok(RepoRefSyncResult {
            pushed: false,
            had_conflicts: had_branch_conflicts || had_tag_conflicts,
        });
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
        had_conflicts: had_branch_conflicts || had_tag_conflicts,
    })
}

struct RepoRefSyncResult {
    pushed: bool,
    had_conflicts: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_state_persists_repo_failures_by_group() {
        let temp = tempfile::TempDir::new().unwrap();
        let failures = vec![
            SyncFailure::repo(
                "sync-1".to_string(),
                "repo-a".to_string(),
                anyhow::anyhow!("a"),
            ),
            SyncFailure::repo(
                "sync-1".to_string(),
                "repo-a".to_string(),
                anyhow::anyhow!("a again"),
            ),
            SyncFailure::repo(
                "sync-2".to_string(),
                "repo-b".to_string(),
                anyhow::anyhow!("b"),
            ),
            SyncFailure::group(
                "mirror group sync-3".to_string(),
                anyhow::anyhow!("list failed"),
            ),
        ];
        let state = FailureState::from_failures(&failures);

        save_failure_state(temp.path(), &state).unwrap();
        let loaded = load_failure_state(temp.path()).unwrap();
        let by_group = loaded.repos_by_group();

        assert_eq!(by_group["sync-1"].len(), 1);
        assert!(by_group["sync-1"].contains("repo-a"));
        assert_eq!(by_group["sync-2"].len(), 1);
        assert!(by_group["sync-2"].contains("repo-b"));
        assert!(!by_group.contains_key("sync-3"));
    }

    #[test]
    fn empty_failure_state_removes_retry_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let state = FailureState {
            repos: vec![FailedRepo {
                group: "sync-1".to_string(),
                repo: "repo-a".to_string(),
            }],
        };
        save_failure_state(temp.path(), &state).unwrap();
        assert!(failure_state_path(temp.path()).exists());

        save_failure_state(temp.path(), &FailureState::default()).unwrap();

        assert!(!failure_state_path(temp.path()).exists());
    }

    #[test]
    fn ref_state_persists_and_requires_exact_remote_ref_match() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut refs = BTreeMap::new();
        refs.insert(
            "github_alice".to_string(),
            RemoteRefState {
                hash: "abc".to_string(),
                refs: 2,
            },
        );
        refs.insert(
            "gitea_alice".to_string(),
            RemoteRefState {
                hash: "def".to_string(),
                refs: 2,
            },
        );
        let mut state = RefState::default();
        state.set_repo("sync-1", "repo-a", refs.clone());

        save_ref_state(temp.path(), &state).unwrap();
        let loaded = load_ref_state(temp.path()).unwrap();

        assert!(loaded.repo_matches("sync-1", "repo-a", &refs));

        let mut changed_hash = refs.clone();
        changed_hash.insert(
            "github_alice".to_string(),
            RemoteRefState {
                hash: "changed".to_string(),
                refs: 2,
            },
        );
        assert!(!loaded.repo_matches("sync-1", "repo-a", &changed_hash));

        let mut missing_remote = refs;
        missing_remote.remove("gitea_alice");
        assert!(!loaded.repo_matches("sync-1", "repo-a", &missing_remote));
    }
}
