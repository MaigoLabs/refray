use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use console::style;

#[derive(Clone, Debug)]
pub struct RemoteSpec {
    pub name: String,
    pub url: String,
    pub display: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRefSnapshot {
    pub hash: String,
    pub refs: usize,
    pub branches: BTreeMap<String, String>,
    pub tags: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct BranchDecision {
    pub branch: String,
    pub sha: String,
    pub source_remotes: Vec<String>,
    pub target_remotes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct BranchConflict {
    pub branch: String,
    pub tips: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct BranchDeletion {
    pub branch: String,
    pub deleted_remotes: Vec<String>,
    pub target_remotes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct BranchUpdate {
    pub branch: String,
    pub sha: String,
    pub target_remote: String,
    pub force: bool,
}

#[derive(Clone, Debug)]
pub struct BranchRebaseDecision {
    pub branch: String,
    pub sha: String,
    pub updates: Vec<BranchUpdate>,
}

#[derive(Clone, Debug)]
pub struct TagDecision {
    pub tag: String,
    pub sha: String,
    pub source_remotes: Vec<String>,
    pub target_remotes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct TagConflict {
    pub tag: String,
    pub tips: Vec<(String, String)>,
}

pub struct GitMirror {
    path: PathBuf,
    redactor: Redactor,
    dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl GitMirror {
    pub fn open(path: PathBuf, redactor: Redactor, dry_run: bool) -> Result<Self> {
        if !path.exists() {
            if dry_run {
                crate::logln!(
                    "  {} git init --bare {}",
                    style("dry-run").yellow().bold(),
                    style(path.display()).dim()
                );
                return Ok(Self {
                    path,
                    redactor,
                    dry_run,
                });
            }
            fs::create_dir_all(&path)?;
            run_plain("git", ["init", "--bare"], Some(&path), &redactor, dry_run)
                .with_context(|| format!("failed to initialize {}", path.display()))?;
        }
        Ok(Self {
            path,
            redactor,
            dry_run,
        })
    }

    pub fn configure_remotes(&self, remotes: &[RemoteSpec]) -> Result<()> {
        for remote in remotes {
            let existing = self.remote_url(&remote.name)?;
            match existing {
                Some(_) => self.run(["remote", "set-url", &remote.name, &remote.url])?,
                None => self.run(["remote", "add", &remote.name, &remote.url])?,
            }
        }
        Ok(())
    }

    pub fn fetch_remote(&self, remote: &RemoteSpec) -> Result<()> {
        let branch_refspec = format!("+refs/heads/*:refs/remotes/{}/*", remote.name);
        let tag_refspec = format!("+refs/tags/*:refs/remote-tags/{}/*", remote.name);
        crate::logln!(
            "  {} {}",
            style("fetch").cyan().bold(),
            style(&remote.display).dim()
        );
        self.run(["fetch", "--prune", &remote.name, &branch_refspec])?;
        self.run(["fetch", "--prune", &remote.name, &tag_refspec])
    }

    #[cfg(test)]
    pub fn cached_remote_refs_match(
        &self,
        remote: &RemoteSpec,
        expected: &RemoteRefSnapshot,
    ) -> Result<bool> {
        Ok(self
            .cached_remote_ref_snapshot(remote)?
            .is_some_and(|snapshot| &snapshot == expected))
    }

    pub fn cached_remote_ref_snapshot(
        &self,
        remote: &RemoteSpec,
    ) -> Result<Option<RemoteRefSnapshot>> {
        if !self.path.exists() || self.dry_run {
            return Ok(None);
        }
        let branches = self.remote_branches(&remote.name)?;
        let tags = self.remote_tags(&remote.name)?;
        let mut refs = Vec::with_capacity(branches.len() + tags.len());
        for (branch, sha) in branches {
            refs.push(format!("{sha}\trefs/heads/{branch}"));
        }
        for (tag, sha) in tags {
            refs.push(format!("{sha}\trefs/tags/{tag}"));
        }
        Ok(Some(snapshot_from_refs(refs)))
    }

    pub fn branch_decisions(
        &self,
        remotes: &[RemoteSpec],
    ) -> Result<(Vec<BranchDecision>, Vec<BranchConflict>)> {
        let mut by_branch: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for remote in remotes {
            for (branch, sha) in self.remote_branches(&remote.name)? {
                by_branch
                    .entry(branch)
                    .or_default()
                    .push((remote.name.clone(), sha));
            }
        }

        let mut decisions = Vec::new();
        let mut conflicts = Vec::new();
        let all_remote_names = remotes
            .iter()
            .map(|remote| remote.name.clone())
            .collect::<Vec<_>>();

        for (branch, tips) in by_branch {
            let unique = tips
                .iter()
                .map(|(_, sha)| sha.clone())
                .collect::<BTreeSet<_>>();
            if unique.len() == 1 {
                let source_remotes = tips
                    .into_iter()
                    .map(|(remote, _)| remote)
                    .collect::<Vec<_>>();
                let target_remotes = missing_remotes(&all_remote_names, &source_remotes);
                decisions.push(BranchDecision {
                    branch,
                    sha: unique.into_iter().next().unwrap(),
                    source_remotes,
                    target_remotes,
                });
                continue;
            }

            if let Some(winner) = self.fast_forward_winner(unique.iter())? {
                let source_remotes = tips
                    .iter()
                    .filter_map(|(remote, sha)| (sha == &winner).then_some(remote))
                    .cloned()
                    .collect::<Vec<_>>();
                let target_remotes = missing_remotes(&all_remote_names, &source_remotes);
                decisions.push(BranchDecision {
                    branch,
                    sha: winner,
                    source_remotes,
                    target_remotes,
                });
            } else {
                conflicts.push(BranchConflict { branch, tips });
            }
        }

        Ok((decisions, conflicts))
    }

    pub fn tag_decisions(
        &self,
        remotes: &[RemoteSpec],
    ) -> Result<(Vec<TagDecision>, Vec<TagConflict>)> {
        let mut by_tag: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for remote in remotes {
            for (tag, sha) in self.remote_tags(&remote.name)? {
                by_tag
                    .entry(tag)
                    .or_default()
                    .push((remote.name.clone(), sha));
            }
        }

        let mut decisions = Vec::new();
        let mut conflicts = Vec::new();
        let all_remote_names = remotes
            .iter()
            .map(|remote| remote.name.clone())
            .collect::<Vec<_>>();

        for (tag, tips) in by_tag {
            let unique = tips
                .iter()
                .map(|(_, sha)| sha.clone())
                .collect::<BTreeSet<_>>();
            if unique.len() == 1 {
                let source_remotes = tips
                    .into_iter()
                    .map(|(remote, _)| remote)
                    .collect::<Vec<_>>();
                let target_remotes = missing_remotes(&all_remote_names, &source_remotes);
                decisions.push(TagDecision {
                    tag,
                    sha: unique.into_iter().next().unwrap(),
                    source_remotes,
                    target_remotes,
                });
            } else {
                conflicts.push(TagConflict { tag, tips });
            }
        }

        Ok((decisions, conflicts))
    }

    pub fn push_branches(&self, remotes: &[RemoteSpec], branches: &[BranchDecision]) -> Result<()> {
        for remote in remotes {
            for branch in branches {
                if !branch.target_remotes.contains(&remote.name) {
                    continue;
                }
                let refspec = format!("{}:refs/heads/{}", branch.sha, branch.branch);
                crate::logln!(
                    "  {} {} {} {}",
                    style("push").green().bold(),
                    style("branch").dim(),
                    style(&branch.branch).cyan(),
                    style(format!("-> {}", remote.display)).dim()
                );
                self.run(["push", &remote.name, &refspec])?;
            }
        }
        Ok(())
    }

    pub fn push_branch_updates(
        &self,
        remotes: &[RemoteSpec],
        updates: &[BranchUpdate],
    ) -> Result<()> {
        for update in updates {
            let remote = remotes
                .iter()
                .find(|remote| remote.name == update.target_remote)
                .with_context(|| format!("unknown remote '{}'", update.target_remote))?;
            self.push_branch_update(remote, update)?;
        }
        Ok(())
    }

    pub fn remote_branch_names_with_prefix(
        &self,
        remote: &str,
        prefix: &str,
    ) -> Result<Vec<String>> {
        Ok(self
            .remote_branches(remote)?
            .into_iter()
            .filter_map(|(branch, _)| branch.starts_with(prefix).then_some(branch))
            .collect())
    }

    pub fn auto_rebase_branch_conflict(
        &self,
        remotes: &[RemoteSpec],
        branch: &str,
        tips: &[(String, String)],
    ) -> Result<BranchRebaseDecision> {
        let mut ordered_tips = Vec::new();
        for remote in remotes {
            let Some((_, sha)) = tips.iter().find(|(name, _)| name == &remote.name) else {
                continue;
            };
            if !ordered_tips
                .iter()
                .any(|(_, existing): &(String, String)| existing == sha)
            {
                ordered_tips.push((remote.name.clone(), sha.clone()));
            }
        }
        if ordered_tips.len() < 2 {
            bail!("branch {branch} does not have enough unique tips to auto-rebase");
        }

        let mut truth = ordered_tips[0].1.clone();
        for (remote, sha) in ordered_tips.iter().skip(1) {
            let base = self.merge_base(&truth, sha)?;
            crate::logln!(
                "  {} branch {} rebasing {}@{} onto {}",
                style("auto-rebase").cyan().bold(),
                style(branch).cyan(),
                remote,
                short_sha(sha),
                short_sha(&truth)
            );
            truth = self
                .rebase_tip_onto(&truth, &base, sha)
                .with_context(|| format!("auto-rebase failed for branch {branch}"))?;
        }

        let mut updates = Vec::new();
        for (remote, sha) in tips {
            if sha == &truth {
                continue;
            }
            updates.push(BranchUpdate {
                branch: branch.to_string(),
                sha: truth.clone(),
                target_remote: remote.clone(),
                force: !self.is_ancestor(sha, &truth)?,
            });
        }

        Ok(BranchRebaseDecision {
            branch: branch.to_string(),
            sha: truth,
            updates,
        })
    }

    pub fn push_tags(&self, remotes: &[RemoteSpec], tags: &[TagDecision]) -> Result<()> {
        for remote in remotes {
            for tag in tags {
                if !tag.target_remotes.contains(&remote.name) {
                    continue;
                }
                let refspec = format!("{}:refs/tags/{}", tag.sha, tag.tag);
                crate::logln!(
                    "  {} {} {} {}",
                    style("push").green().bold(),
                    style("tag").dim(),
                    style(&tag.tag).cyan(),
                    style(format!("-> {}", remote.display)).dim()
                );
                self.run(["push", &remote.name, &refspec])?;
            }
        }
        Ok(())
    }

    pub fn delete_branches(
        &self,
        remotes: &[RemoteSpec],
        deletions: &[BranchDeletion],
    ) -> Result<()> {
        for remote in remotes {
            for deletion in deletions {
                if !deletion.target_remotes.contains(&remote.name) {
                    continue;
                }
                let refspec = format!(":refs/heads/{}", deletion.branch);
                crate::logln!(
                    "  {} {} {} {}",
                    style("delete").red().bold(),
                    style("branch").dim(),
                    style(&deletion.branch).cyan(),
                    style(format!("-> {}", remote.display)).dim()
                );
                self.run(["push", &remote.name, &refspec])?;
            }
        }
        Ok(())
    }

    fn push_branch_update(&self, remote: &RemoteSpec, update: &BranchUpdate) -> Result<()> {
        let refspec = if update.force {
            format!("+{}:refs/heads/{}", update.sha, update.branch)
        } else {
            format!("{}:refs/heads/{}", update.sha, update.branch)
        };
        let label = if update.force { "force-push" } else { "push" };
        crate::logln!(
            "  {} {} {} {}",
            style(label).green().bold(),
            style("branch").dim(),
            style(&update.branch).cyan(),
            style(format!("-> {}", remote.display)).dim()
        );
        self.run(["push", &remote.name, &refspec])
    }

    fn remote_url(&self, name: &str) -> Result<Option<String>> {
        let output = self
            .command()
            .args(["remote", "get-url", name])
            .output()
            .with_context(|| "failed to run git remote get-url")?;
        if output.status.success() {
            Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            ))
        } else {
            Ok(None)
        }
    }

    fn remote_branches(&self, remote: &str) -> Result<Vec<(String, String)>> {
        let prefix = format!("refs/remotes/{remote}/");
        let output = self.output(["for-each-ref", "--format=%(refname) %(objectname)", &prefix])?;
        let mut branches = Vec::new();
        for line in output.lines() {
            let Some((refname, sha)) = line.split_once(' ') else {
                continue;
            };
            let Some(branch) = refname.strip_prefix(&prefix) else {
                continue;
            };
            if branch == "HEAD" {
                continue;
            }
            branches.push((branch.to_string(), sha.to_string()));
        }
        Ok(branches)
    }

    fn remote_tags(&self, remote: &str) -> Result<Vec<(String, String)>> {
        let prefix = format!("refs/remote-tags/{remote}/");
        let output = self.output(["for-each-ref", "--format=%(refname) %(objectname)", &prefix])?;
        let mut tags = Vec::new();
        for line in output.lines() {
            let Some((refname, sha)) = line.split_once(' ') else {
                continue;
            };
            let Some(tag) = refname.strip_prefix(&prefix) else {
                continue;
            };
            tags.push((tag.to_string(), sha.to_string()));
        }
        Ok(tags)
    }

    fn fast_forward_winner<'a>(
        &self,
        shas: impl Iterator<Item = &'a String> + Clone,
    ) -> Result<Option<String>> {
        for candidate in shas.clone() {
            let mut is_descendant = true;
            for other in shas.clone() {
                if candidate == other {
                    continue;
                }
                if !self.is_ancestor(other, candidate)? {
                    is_descendant = false;
                    break;
                }
            }
            if is_descendant {
                return Ok(Some(candidate.clone()));
            }
        }
        Ok(None)
    }

    fn merge_base(&self, left: &str, right: &str) -> Result<String> {
        Ok(self.output(["merge-base", left, right])?.trim().to_string())
    }

    fn rebase_tip_onto(&self, onto: &str, base: &str, tip: &str) -> Result<String> {
        if self.dry_run {
            return Ok(format!("dry-run-rebased-{}", short_sha(tip)));
        }

        let worktree = tempfile::TempDir::new().context("failed to create temporary worktree")?;
        let worktree_path = worktree.path().to_path_buf();
        self.run([
            "worktree",
            "add",
            "--detach",
            worktree_path.to_str().unwrap(),
            tip,
        ])?;

        let rebase_result = self.worktree_git(&worktree_path, ["rebase", "--onto", onto, base]);
        if let Err(error) = rebase_result {
            let _ = self.worktree_git(&worktree_path, ["rebase", "--abort"]);
            let _ = self.run([
                "worktree",
                "remove",
                "--force",
                worktree_path.to_str().unwrap(),
            ]);
            return Err(error);
        }
        let rebased = self.worktree_git_output(&worktree_path, ["rev-parse", "HEAD"])?;
        self.run([
            "worktree",
            "remove",
            "--force",
            worktree_path.to_str().unwrap(),
        ])?;
        Ok(rebased.trim().to_string())
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let status = self
            .command()
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .status()
            .with_context(|| "failed to run git merge-base")?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!("git merge-base failed for {ancestor} and {descendant}"),
        }
    }

    fn run<const N: usize>(&self, args: [&str; N]) -> Result<()> {
        run_plain(
            "git",
            std::iter::once("--git-dir")
                .chain(std::iter::once(self.path.to_str().unwrap()))
                .chain(args),
            None,
            &self.redactor,
            self.dry_run,
        )
    }

    fn output<const N: usize>(&self, args: [&str; N]) -> Result<String> {
        if self.dry_run {
            return Ok(String::new());
        }
        let output = self
            .command()
            .args(args)
            .output()
            .with_context(|| "failed to run git")?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(GitCommandError::new(
                "git",
                "",
                self.redactor
                    .redact(&String::from_utf8_lossy(&output.stderr)),
            )
            .into())
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command.arg("--git-dir").arg(&self.path);
        command
    }

    fn worktree_git<const N: usize>(&self, worktree: &Path, args: [&str; N]) -> Result<()> {
        run_plain(
            "git",
            [
                "-C",
                worktree.to_str().unwrap(),
                "-c",
                "user.name=refray",
                "-c",
                "user.email=refray@example.invalid",
            ]
            .into_iter()
            .chain(args),
            None,
            &self.redactor,
            self.dry_run,
        )
    }

    fn worktree_git_output<const N: usize>(
        &self,
        worktree: &Path,
        args: [&str; N],
    ) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(args)
            .output()
            .with_context(|| "failed to run git")?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(GitCommandError::new(
                "git",
                self.redactor
                    .redact(&String::from_utf8_lossy(&output.stdout)),
                self.redactor
                    .redact(&String::from_utf8_lossy(&output.stderr)),
            )
            .into())
        }
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

pub fn ls_remote_refs(remote: &RemoteSpec, redactor: &Redactor) -> Result<RemoteRefSnapshot> {
    let output = Command::new("git")
        .args(["ls-remote", "--heads", "--tags", "--refs", &remote.url])
        .output()
        .with_context(|| "failed to run git ls-remote")?;
    if !output.status.success() {
        let stdout = redactor.redact(&String::from_utf8_lossy(&output.stdout));
        let stderr = redactor.redact(&String::from_utf8_lossy(&output.stderr));
        return Err(GitCommandError::new("git ls-remote", stdout, stderr).into());
    }

    let refs = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    Ok(snapshot_from_refs(refs))
}

fn snapshot_from_refs(mut refs: Vec<String>) -> RemoteRefSnapshot {
    refs.sort();
    let mut branches = BTreeMap::new();
    let mut tags = BTreeMap::new();
    for line in &refs {
        let Some((sha, reference)) = line.split_once('\t') else {
            continue;
        };
        if let Some(branch) = reference.strip_prefix("refs/heads/") {
            branches.insert(branch.to_string(), sha.to_string());
        } else if let Some(tag) = reference.strip_prefix("refs/tags/") {
            tags.insert(tag.to_string(), sha.to_string());
        }
    }

    RemoteRefSnapshot {
        hash: stable_ref_hash(&refs),
        refs: refs.len(),
        branches,
        tags,
    }
}

fn stable_ref_hash(refs: &[String]) -> String {
    // FNV-1a is enough here: this is a deterministic change detector, not a
    // security boundary.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for line in refs {
        for byte in line.bytes().chain(std::iter::once(b'\n')) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

#[derive(Debug)]
pub struct GitCommandError {
    program: String,
    stdout: String,
    stderr: String,
}

impl GitCommandError {
    fn new(
        program: impl Into<String>,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
    ) -> Self {
        Self {
            program: program.into(),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    pub fn stderr(&self) -> &str {
        &self.stderr
    }
}

impl fmt::Display for GitCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} failed\nstdout: {}\nstderr: {}",
            self.program, self.stdout, self.stderr
        )
    }
}

impl Error for GitCommandError {}

pub fn is_disabled_repository_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<GitCommandError>())
        .any(|error| is_disabled_repository_stderr(error.stderr()))
}

fn missing_remotes(all_remote_names: &[String], source_remotes: &[String]) -> Vec<String> {
    all_remote_names
        .iter()
        .filter(|remote| !source_remotes.contains(remote))
        .cloned()
        .collect()
}

fn is_disabled_repository_stderr(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("access to this repository has been disabled")
        || stderr.contains("repository has been disabled")
        || stderr.contains("disabled by github staff")
        || stderr.contains("dmca takedown")
}

impl Redactor {
    pub fn new(secrets: Vec<String>) -> Self {
        let secrets = secrets
            .into_iter()
            .filter(|secret| !secret.is_empty())
            .collect();
        Self { secrets }
    }

    pub fn redact(&self, value: &str) -> String {
        let mut redacted = value.to_string();
        for secret in &self.secrets {
            redacted = redacted.replace(secret, "<redacted>");
        }
        redacted
    }
}

fn run_plain<I, S>(
    program: &str,
    args: I,
    current_dir: Option<&Path>,
    redactor: &Redactor,
    dry_run: bool,
) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_string())
        .collect::<Vec<_>>();
    if dry_run {
        crate::logln!(
            "  {} {} {}",
            style("dry-run").yellow().bold(),
            program,
            style(redactor.redact(&args.join(" "))).dim()
        );
        return Ok(());
    }

    let mut command = Command::new(program);
    command.args(&args);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    let output = command
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stdout = redactor.redact(&String::from_utf8_lossy(&output.stdout));
        let stderr = redactor.redact(&String::from_utf8_lossy(&output.stderr));
        Err(GitCommandError::new(program, stdout, stderr).into())
    }
}

pub fn safe_remote_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "../tests/unit/git.rs"]
mod tests;
