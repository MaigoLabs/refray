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
        remote_key("github"),
        remote_ref_state("abc", &[("main", "111")]),
    );
    refs.insert(
        remote_key("gitea"),
        remote_ref_state("def", &[("main", "111")]),
    );
    let mut state = RefState::default();
    state.set_repo("sync-1", "repo-a", refs.clone());

    save_ref_state(temp.path(), &state).unwrap();
    let loaded = load_ref_state(temp.path()).unwrap();

    assert!(loaded.repo_matches("sync-1", "repo-a", &refs));

    let mut changed_hash = refs.clone();
    changed_hash.insert(
        remote_key("github"),
        remote_ref_state("changed", &[("main", "111")]),
    );
    assert!(!loaded.repo_matches("sync-1", "repo-a", &changed_hash));

    let mut missing_remote = refs;
    missing_remote.remove(&remote_key("gitea"));
    assert!(!loaded.repo_matches("sync-1", "repo-a", &missing_remote));
}

#[test]
fn branch_deletion_decisions_propagate_previous_synced_branch_deletion() {
    let remotes = test_remotes();
    let mut previous = BTreeMap::new();
    previous.insert(
        "github".to_string(),
        remote_ref_state("a", &[("main", "111")]),
    );
    previous.insert(
        "gitea".to_string(),
        remote_ref_state("b", &[("main", "111")]),
    );
    let mut current = BTreeMap::new();
    current.insert("github".to_string(), remote_ref_state("c", &[]));
    current.insert(
        "gitea".to_string(),
        remote_ref_state("d", &[("main", "111")]),
    );

    let (deletions, conflicts, blocked) =
        branch_deletion_decisions(&remotes, Some(&previous), &current);

    assert!(conflicts.is_empty());
    assert!(blocked.contains("main"));
    assert_eq!(deletions.len(), 1);
    assert_eq!(deletions[0].branch, "main");
    assert_eq!(deletions[0].deleted_remotes, vec!["github".to_string()]);
    assert_eq!(deletions[0].target_remotes, vec!["gitea".to_string()]);
}

#[test]
fn branch_deletion_decisions_conflict_when_branch_changed_elsewhere() {
    let remotes = test_remotes();
    let mut previous = BTreeMap::new();
    previous.insert(
        "github".to_string(),
        remote_ref_state("a", &[("main", "111")]),
    );
    previous.insert(
        "gitea".to_string(),
        remote_ref_state("b", &[("main", "111")]),
    );
    let mut current = BTreeMap::new();
    current.insert("github".to_string(), remote_ref_state("c", &[]));
    current.insert(
        "gitea".to_string(),
        remote_ref_state("d", &[("main", "222")]),
    );

    let (deletions, conflicts, blocked) =
        branch_deletion_decisions(&remotes, Some(&previous), &current);

    assert!(deletions.is_empty());
    assert!(blocked.contains("main"));
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].branch, "main");
    assert_eq!(conflicts[0].deleted_remotes, vec!["github".to_string()]);
    assert_eq!(conflicts[0].changed_remotes, vec!["gitea".to_string()]);
}

#[test]
fn branch_deletion_decisions_ignore_internal_conflict_branches() {
    let remotes = test_remotes();
    let conflict_branch = conflict_pr_branch("main", "gitea", "abc123");
    let mut previous = BTreeMap::new();
    previous.insert(
        "github".to_string(),
        remote_ref_state("a", &[(conflict_branch.as_str(), "111")]),
    );
    previous.insert(
        "gitea".to_string(),
        remote_ref_state("b", &[(conflict_branch.as_str(), "111")]),
    );
    let mut current = BTreeMap::new();
    current.insert("github".to_string(), remote_ref_state("c", &[]));
    current.insert(
        "gitea".to_string(),
        remote_ref_state("d", &[(conflict_branch.as_str(), "111")]),
    );

    let (deletions, conflicts, blocked) =
        branch_deletion_decisions(&remotes, Some(&previous), &current);

    assert!(deletions.is_empty());
    assert!(conflicts.is_empty());
    assert!(blocked.is_empty());
}

#[test]
fn branches_deleted_everywhere_are_backed_up_before_prune() {
    let mut previous = BTreeMap::new();
    previous.insert(
        "github".to_string(),
        remote_ref_state("a", &[("main", "111")]),
    );
    previous.insert(
        "gitea".to_string(),
        remote_ref_state("b", &[("main", "111")]),
    );

    let backups = branches_deleted_everywhere_backups(&previous, &BTreeMap::new(), "stamp");

    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0].sha, "111");
    assert!(
        backups[0]
            .refname
            .starts_with("refs/refray-backups/branches/")
    );
}

#[test]
fn repo_deletion_decision_propagates_previous_synced_repo_deletion() {
    let mirror = test_mirror();
    let mut previous = BTreeMap::new();
    previous.insert(
        remote_key("github"),
        remote_ref_state("a", &[("main", "111")]),
    );
    previous.insert(
        remote_key("gitea"),
        remote_ref_state("b", &[("main", "111")]),
    );
    let mut current = BTreeMap::new();
    current.insert(
        remote_key("gitea"),
        remote_ref_state("b", &[("main", "111")]),
    );

    let decision = repo_deletion_decision(
        &mirror,
        &[endpoint_repo("gitea")],
        Some(&previous),
        &current,
    );

    assert_eq!(
        decision,
        RepoDeletionDecision::Propagate {
            deleted_remotes: vec![remote_key("github")],
            target_remotes: vec![remote_key("gitea")],
        }
    );
}

#[test]
fn repo_deletion_decision_is_disabled_by_mirror_policy() {
    let mut mirror = test_mirror();
    mirror.delete_missing = false;
    let mut previous = BTreeMap::new();
    previous.insert(
        remote_key("github"),
        remote_ref_state("a", &[("main", "111")]),
    );
    previous.insert(
        remote_key("gitea"),
        remote_ref_state("b", &[("main", "111")]),
    );
    let mut current = BTreeMap::new();
    current.insert(
        remote_key("gitea"),
        remote_ref_state("b", &[("main", "111")]),
    );

    let decision = repo_deletion_decision(
        &mirror,
        &[endpoint_repo("gitea")],
        Some(&previous),
        &current,
    );

    assert_eq!(decision, RepoDeletionDecision::None);
}

#[test]
fn repo_deletion_decision_conflicts_when_remaining_repo_changed() {
    let mirror = test_mirror();
    let mut previous = BTreeMap::new();
    previous.insert(
        remote_key("github"),
        remote_ref_state("a", &[("main", "111")]),
    );
    previous.insert(
        remote_key("gitea"),
        remote_ref_state("b", &[("main", "111")]),
    );
    let mut current = BTreeMap::new();
    current.insert(
        remote_key("gitea"),
        remote_ref_state("changed", &[("main", "222")]),
    );

    let decision = repo_deletion_decision(
        &mirror,
        &[endpoint_repo("gitea")],
        Some(&previous),
        &current,
    );

    assert_eq!(
        decision,
        RepoDeletionDecision::Conflict {
            deleted_remotes: vec![remote_key("github")],
            changed_remotes: vec![remote_key("gitea")],
        }
    );
}

#[test]
fn repo_deletion_decision_removes_state_when_deleted_everywhere() {
    let mirror = test_mirror();
    let mut previous = BTreeMap::new();
    previous.insert(
        remote_key("github"),
        remote_ref_state("a", &[("main", "111")]),
    );
    previous.insert(
        remote_key("gitea"),
        remote_ref_state("b", &[("main", "111")]),
    );

    let decision = repo_deletion_decision(&mirror, &[], Some(&previous), &BTreeMap::new());

    assert_eq!(
        decision,
        RepoDeletionDecision::DeletedEverywhere {
            deleted_remotes: vec![remote_key("github"), remote_key("gitea")],
        }
    );
}

#[test]
fn repo_deletion_decision_removes_partial_state_when_deleted_everywhere() {
    let mirror = test_mirror();
    let mut previous = BTreeMap::new();
    previous.insert(
        remote_key("gitea"),
        remote_ref_state("b", &[("main", "111")]),
    );

    let decision = repo_deletion_decision(&mirror, &[], Some(&previous), &BTreeMap::new());

    assert_eq!(
        decision,
        RepoDeletionDecision::DeletedEverywhere {
            deleted_remotes: vec![remote_key("github"), remote_key("gitea")],
        }
    );
}

#[test]
fn repo_deletion_decision_ignores_repos_not_previously_synced_everywhere() {
    let mirror = test_mirror();
    let mut previous = BTreeMap::new();
    previous.insert(
        remote_key("gitea"),
        remote_ref_state("b", &[("main", "111")]),
    );
    let mut current = BTreeMap::new();
    current.insert(
        remote_key("gitea"),
        remote_ref_state("b", &[("main", "111")]),
    );

    let decision = repo_deletion_decision(
        &mirror,
        &[endpoint_repo("gitea")],
        Some(&previous),
        &current,
    );

    assert_eq!(decision, RepoDeletionDecision::None);
}

#[test]
fn filtered_sync_visibility_does_not_treat_state_only_repos_as_deleted() {
    let mut mirror = test_mirror();
    mirror.sync_visibility = crate::config::SyncVisibility::Public;
    let mut ref_state = RefState::default();
    ref_state.set_repo(
        &mirror.name,
        "private-repo",
        BTreeMap::from([(remote_key("github"), remote_ref_state("a", &[]))]),
    );

    let repo_filter = mirror.repo_filter().unwrap();
    let names = sync_candidate_repo_names(&HashMap::new(), &ref_state, &mirror, &repo_filter);

    assert!(names.is_empty());
}

#[test]
fn all_visibility_keeps_state_only_repos_for_deletion_detection() {
    let mirror = test_mirror();
    let mut ref_state = RefState::default();
    ref_state.set_repo(
        &mirror.name,
        "deleted-repo",
        BTreeMap::from([(remote_key("github"), remote_ref_state("a", &[]))]),
    );

    let repo_filter = mirror.repo_filter().unwrap();
    let names = sync_candidate_repo_names(&HashMap::new(), &ref_state, &mirror, &repo_filter);

    assert_eq!(names, BTreeSet::from(["deleted-repo".to_string()]));
}

#[test]
fn repo_name_filters_do_not_treat_state_only_repos_as_deleted() {
    let mut mirror = test_mirror();
    mirror.repo_whitelist = Some("^public-".to_string());
    let repo_filter = mirror.repo_filter().unwrap();
    let mut ref_state = RefState::default();
    ref_state.set_repo(
        &mirror.name,
        "private-repo",
        BTreeMap::from([(remote_key("github"), remote_ref_state("a", &[]))]),
    );

    let names = sync_candidate_repo_names(&HashMap::new(), &ref_state, &mirror, &repo_filter);

    assert!(names.is_empty());
}

#[test]
fn conflict_branch_prefixes_are_reversible_not_slug_collisions() {
    let slash_branch = conflict_pr_branch_prefix("release/foo");
    let dash_branch = conflict_pr_branch_prefix("release-foo");

    assert_ne!(slash_branch, dash_branch);
    assert!(slash_branch.starts_with(CONFLICT_BRANCH_ROOT));
    assert!(dash_branch.starts_with(CONFLICT_BRANCH_ROOT));
    assert!(conflict_pr_branch("main", "gitea", "abc123").starts_with(CONFLICT_BRANCH_ROOT));
    assert_eq!(
        conflict_pr_base_branch(&format!("{slash_branch}from-gitea-abc123")),
        Some("release/foo".to_string())
    );
    assert_eq!(
        conflict_pr_base_branch(&format!("{dash_branch}from-gitea-abc123")),
        Some("release-foo".to_string())
    );
}

#[test]
fn endpoint_remote_names_do_not_slug_collide() {
    let slash = EndpointConfig {
        site: "gitlab".to_string(),
        kind: crate::config::NamespaceKind::Group,
        namespace: "parent/child".to_string(),
    };
    let underscore = EndpointConfig {
        site: "gitlab".to_string(),
        kind: crate::config::NamespaceKind::Group,
        namespace: "parent_child".to_string(),
    };

    assert_ne!(
        remote_name_for_endpoint(&slash),
        remote_name_for_endpoint(&underscore)
    );
}

#[test]
fn created_repo_visibility_follows_existing_public_repo() {
    let mirror = test_mirror();
    let repo = crate::provider::RemoteRepo {
        name: "repo".to_string(),
        clone_url: "https://github.invalid/alice/repo.git".to_string(),
        private: false,
        description: None,
    };

    assert_eq!(
        visibility_for_created_repo(&mirror, Some(&repo)),
        crate::config::Visibility::Public
    );
}

#[test]
fn created_repo_visibility_follows_existing_private_repo() {
    let mut mirror = test_mirror();
    mirror.visibility = crate::config::Visibility::Public;
    let repo = crate::provider::RemoteRepo {
        name: "repo".to_string(),
        clone_url: "https://github.invalid/alice/repo.git".to_string(),
        private: true,
        description: None,
    };

    assert_eq!(
        visibility_for_created_repo(&mirror, Some(&repo)),
        crate::config::Visibility::Private
    );
}

#[test]
fn created_repo_visibility_falls_back_to_config_without_template() {
    let mut mirror = test_mirror();
    mirror.visibility = crate::config::Visibility::Public;

    assert_eq!(
        visibility_for_created_repo(&mirror, None),
        crate::config::Visibility::Public
    );
}

fn remote_ref_state(hash: &str, branches: &[(&str, &str)]) -> RemoteRefState {
    RemoteRefState {
        hash: hash.to_string(),
        refs: branches.len(),
        branches: branches
            .iter()
            .map(|(branch, sha)| ((*branch).to_string(), (*sha).to_string()))
            .collect(),
        tags: BTreeMap::new(),
    }
}

fn test_remotes() -> Vec<RemoteSpec> {
    vec![
        RemoteSpec {
            name: "github".to_string(),
            url: "https://github.invalid/alice/repo.git".to_string(),
            display: "github:alice:User".to_string(),
        },
        RemoteSpec {
            name: "gitea".to_string(),
            url: "https://gitea.invalid/alice/repo.git".to_string(),
            display: "gitea:alice:User".to_string(),
        },
    ]
}

fn test_mirror() -> MirrorConfig {
    MirrorConfig {
        name: "sync-1".to_string(),
        endpoints: vec![endpoint("github"), endpoint("gitea")],
        sync_visibility: crate::config::SyncVisibility::All,
        repo_whitelist: None,
        repo_blacklist: None,
        create_missing: true,
        delete_missing: true,
        visibility: crate::config::Visibility::Private,
        conflict_resolution: ConflictResolutionStrategy::Fail,
    }
}

fn endpoint(site: &str) -> EndpointConfig {
    EndpointConfig {
        site: site.to_string(),
        kind: crate::config::NamespaceKind::User,
        namespace: "alice".to_string(),
    }
}

fn remote_key(site: &str) -> String {
    remote_name_for_endpoint(&endpoint(site))
}

fn endpoint_repo(site: &str) -> EndpointRepo {
    EndpointRepo {
        endpoint: endpoint(site),
        repo: crate::provider::RemoteRepo {
            name: "repo".to_string(),
            clone_url: format!("https://{site}.invalid/alice/repo.git"),
            private: true,
            description: None,
        },
    }
}
