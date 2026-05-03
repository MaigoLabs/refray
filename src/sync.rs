use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{Config, EndpointConfig, MirrorConfig, default_work_dir, validate_config};
use crate::git::{GitMirror, Redactor, RemoteSpec, safe_remote_name};
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

    for mirror in mirrors {
        sync_group(config, mirror, &options, &work_dir, redactor.clone())?;
    }

    Ok(())
}

fn sync_group(
    config: &Config,
    mirror: &MirrorConfig,
    options: &SyncOptions,
    work_dir: &Path,
    redactor: Redactor,
) -> Result<()> {
    println!("syncing mirror group {}", mirror.name);
    let create_missing = options
        .create_missing_override
        .unwrap_or(mirror.create_missing);
    let allow_force = options.force_override.unwrap_or(mirror.allow_force);

    let mut all_endpoint_repos = Vec::new();
    for endpoint in &mirror.endpoints {
        let site = config.site(&endpoint.site).unwrap();
        let client = ProviderClient::new(site)?;
        println!("listing {}", endpoint.label());
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
        println!("mirror group {} has no repositories", mirror.name);
        return Ok(());
    }

    for repo_name in repo_names {
        let mut existing = repos.remove(&repo_name).unwrap_or_default();
        ensure_missing_repos(
            config,
            mirror,
            &repo_name,
            &mut existing,
            create_missing,
            options.dry_run,
        )?;
        if existing.len() < 2 {
            println!(
                "skipping {}: fewer than two endpoints have this repository",
                repo_name
            );
            continue;
        }
        let context = RepoSyncContext {
            config,
            mirror,
            work_dir,
            redactor: redactor.clone(),
            dry_run: options.dry_run,
            allow_force,
        };
        sync_repo(&context, &repo_name, &existing)?;
    }

    Ok(())
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
                "{} is missing on {}; creation disabled",
                repo_name,
                endpoint.label()
            );
            continue;
        }

        println!("creating {} on {}", repo_name, endpoint.label());
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
                "created {} on {}, but provider reported a different visibility than requested",
                repo_name,
                endpoint.label()
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

fn sync_repo(context: &RepoSyncContext<'_>, repo_name: &str, repos: &[EndpointRepo]) -> Result<()> {
    println!("syncing repo {}", repo_name);
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

    let path = context
        .work_dir
        .join(safe_remote_name(&context.mirror.name))
        .join(format!("{}.git", safe_remote_name(repo_name)));
    let mirror_repo = GitMirror::open(path, context.redactor.clone(), context.dry_run)?;
    mirror_repo.configure_remotes(&remotes)?;
    for remote in &remotes {
        mirror_repo.fetch_remote(remote)?;
    }

    let (branches, conflicts) = mirror_repo.branch_decisions(&remotes, context.allow_force)?;
    for conflict in conflicts {
        let details = conflict
            .tips
            .iter()
            .map(|(remote, sha)| format!("{remote}@{}", short_sha(sha)))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "conflict in {}/{}: branch {} diverged across {}. Skipping that branch.",
            context.mirror.name, repo_name, conflict.branch, details
        );
    }

    let (tags, tag_conflicts) = mirror_repo.tag_decisions(&remotes)?;
    for conflict in tag_conflicts {
        let details = conflict
            .tips
            .iter()
            .map(|(remote, sha)| format!("{remote}@{}", short_sha(sha)))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "conflict in {}/{}: tag {} differs across {}. Skipping that tag.",
            context.mirror.name, repo_name, conflict.tag, details
        );
    }

    if branches.is_empty() && tags.is_empty() {
        println!("{} has no branches or tags to push", repo_name);
        return Ok(());
    }
    if !branches.is_empty() {
        let branch_summary = branches
            .iter()
            .map(|branch| {
                format!(
                    "{}@{} from {}",
                    branch.branch,
                    short_sha(&branch.sha),
                    branch.source_remotes.join("+")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!("resolved branches for {}: {}", repo_name, branch_summary);
        mirror_repo.push_branches(&remotes, &branches, context.allow_force)?;
    }
    if !tags.is_empty() {
        let tag_summary = tags
            .iter()
            .map(|tag| {
                format!(
                    "{}@{} from {}",
                    tag.tag,
                    short_sha(&tag.sha),
                    tag.source_remotes.join("+")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!("resolved tags for {}: {}", repo_name, tag_summary);
        mirror_repo.push_tags(&remotes, &tags)?;
    }
    Ok(())
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}
