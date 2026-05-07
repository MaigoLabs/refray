use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::git::RemoteRefSnapshot;
use crate::state::{load_toml_or_default, remove_file_if_exists, save_toml};

use super::output::format_error;

const FAILURE_STATE_FILE: &str = "failed-repos.toml";
const REF_STATE_FILE: &str = "ref-state.toml";

#[derive(Debug)]
pub(super) struct SyncFailure {
    pub(super) scope: String,
    pub(super) error: String,
    retry: Option<FailedRepo>,
}

impl SyncFailure {
    pub(super) fn group(scope: String, error: anyhow::Error) -> Self {
        Self {
            scope,
            error: format_error(&error),
            retry: None,
        }
    }

    pub(super) fn repo(group: String, repo: String, error: anyhow::Error) -> Self {
        Self {
            scope: format!("{group}/{repo}"),
            error: format_error(&error),
            retry: Some(FailedRepo { group, repo }),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct FailedRepo {
    pub(super) group: String,
    pub(super) repo: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct FailureState {
    #[serde(default)]
    pub(super) repos: Vec<FailedRepo>,
}

impl FailureState {
    pub(super) fn from_failures(failures: &[SyncFailure]) -> Self {
        let repos = failures
            .iter()
            .filter_map(|failure| failure.retry.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Self { repos }
    }

    pub(super) fn repos_by_group(&self) -> BTreeMap<String, BTreeSet<String>> {
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

pub(super) fn load_failure_state(work_dir: &Path) -> Result<FailureState> {
    load_toml_or_default(&failure_state_path(work_dir))
}

pub(super) fn save_failure_state(work_dir: &Path, state: &FailureState) -> Result<()> {
    let path = failure_state_path(work_dir);
    if state.repos.is_empty() {
        remove_file_if_exists(&path)
    } else {
        save_toml(&path, state)
    }
}

pub(super) fn failure_state_path(work_dir: &Path) -> PathBuf {
    work_dir.join(FAILURE_STATE_FILE)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct RemoteRefState {
    pub(super) hash: String,
    pub(super) refs: usize,
    #[serde(default)]
    pub(super) branches: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) tags: BTreeMap<String, String>,
}

impl From<RemoteRefSnapshot> for RemoteRefState {
    fn from(value: RemoteRefSnapshot) -> Self {
        Self {
            hash: value.hash,
            refs: value.refs,
            branches: value.branches,
            tags: value.tags,
        }
    }
}

impl From<&RemoteRefState> for RemoteRefSnapshot {
    fn from(value: &RemoteRefState) -> Self {
        Self {
            hash: value.hash.clone(),
            refs: value.refs,
            branches: value.branches.clone(),
            tags: value.tags.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct RefState {
    #[serde(default)]
    repos: BTreeMap<String, BTreeMap<String, BTreeMap<String, RemoteRefState>>>,
}

impl RefState {
    pub(super) fn repo_matches(
        &self,
        group: &str,
        repo: &str,
        refs: &BTreeMap<String, RemoteRefState>,
    ) -> bool {
        self.repos.get(group).and_then(|repos| repos.get(repo)) == Some(refs)
    }

    pub(super) fn set_repo(
        &mut self,
        group: &str,
        repo: &str,
        refs: BTreeMap<String, RemoteRefState>,
    ) {
        self.repos
            .entry(group.to_string())
            .or_default()
            .insert(repo.to_string(), refs);
    }

    pub(super) fn repo(
        &self,
        group: &str,
        repo: &str,
    ) -> Option<&BTreeMap<String, RemoteRefState>> {
        self.repos.get(group).and_then(|repos| repos.get(repo))
    }
}

pub(super) fn load_ref_state(work_dir: &Path) -> Result<RefState> {
    load_toml_or_default(&ref_state_path(work_dir))
}

pub(super) fn save_ref_state(work_dir: &Path, state: &RefState) -> Result<()> {
    save_toml(&ref_state_path(work_dir), state)
}

fn ref_state_path(work_dir: &Path) -> PathBuf {
    work_dir.join(REF_STATE_FILE)
}
