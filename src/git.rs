use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct RemoteSpec {
    pub name: String,
    pub url: String,
    pub display: String,
}

#[derive(Clone, Debug)]
pub struct BranchDecision {
    pub branch: String,
    pub sha: String,
    pub source_remotes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct BranchConflict {
    pub branch: String,
    pub tips: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct TagDecision {
    pub tag: String,
    pub sha: String,
    pub source_remotes: Vec<String>,
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
                println!("dry-run: git init --bare {}", path.display());
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
        println!("fetching {}", remote.display);
        self.run(["fetch", "--prune", &remote.name, &branch_refspec])?;
        self.run(["fetch", "--prune", &remote.name, &tag_refspec])
    }

    pub fn branch_decisions(
        &self,
        remotes: &[RemoteSpec],
        allow_force: bool,
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

        for (branch, tips) in by_branch {
            let unique = tips
                .iter()
                .map(|(_, sha)| sha.clone())
                .collect::<BTreeSet<_>>();
            if unique.len() == 1 {
                decisions.push(BranchDecision {
                    branch,
                    sha: unique.into_iter().next().unwrap(),
                    source_remotes: tips.into_iter().map(|(remote, _)| remote).collect(),
                });
                continue;
            }

            if let Some(winner) = self.fast_forward_winner(unique.iter())? {
                let source_remotes = tips
                    .into_iter()
                    .filter_map(|(remote, sha)| (sha == winner).then_some(remote))
                    .collect();
                decisions.push(BranchDecision {
                    branch,
                    sha: winner,
                    source_remotes,
                });
            } else if allow_force {
                let winner = self.newest_commit(unique.iter())?;
                let source_remotes = tips
                    .into_iter()
                    .filter_map(|(remote, sha)| (sha == winner).then_some(remote))
                    .collect();
                decisions.push(BranchDecision {
                    branch,
                    sha: winner,
                    source_remotes,
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

        for (tag, tips) in by_tag {
            let unique = tips
                .iter()
                .map(|(_, sha)| sha.clone())
                .collect::<BTreeSet<_>>();
            if unique.len() == 1 {
                decisions.push(TagDecision {
                    tag,
                    sha: unique.into_iter().next().unwrap(),
                    source_remotes: tips.into_iter().map(|(remote, _)| remote).collect(),
                });
            } else {
                conflicts.push(TagConflict { tag, tips });
            }
        }

        Ok((decisions, conflicts))
    }

    pub fn push_branches(
        &self,
        remotes: &[RemoteSpec],
        branches: &[BranchDecision],
        force: bool,
    ) -> Result<()> {
        for remote in remotes {
            for branch in branches {
                let refspec = if force {
                    format!("+{}:refs/heads/{}", branch.sha, branch.branch)
                } else {
                    format!("{}:refs/heads/{}", branch.sha, branch.branch)
                };
                println!("pushing {} to {}", branch.branch, remote.display);
                self.run(["push", &remote.name, &refspec])?;
            }
        }
        Ok(())
    }

    pub fn push_tags(&self, remotes: &[RemoteSpec], tags: &[TagDecision]) -> Result<()> {
        for remote in remotes {
            for tag in tags {
                let refspec = format!("{}:refs/tags/{}", tag.sha, tag.tag);
                println!("pushing tag {} to {}", tag.tag, remote.display);
                self.run(["push", &remote.name, &refspec])?;
            }
        }
        Ok(())
    }

    fn remote_url(&self, name: &str) -> Result<Option<String>> {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.path)
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

    fn newest_commit<'a>(&self, shas: impl Iterator<Item = &'a String>) -> Result<String> {
        let mut newest: Option<(i64, String)> = None;
        for sha in shas {
            let timestamp = self
                .output(["show", "-s", "--format=%ct", sha])?
                .trim()
                .parse::<i64>()?;
            match &newest {
                Some((old, _)) if *old >= timestamp => {}
                _ => newest = Some((timestamp, sha.clone())),
            }
        }
        newest
            .map(|(_, sha)| sha)
            .context("no commits found while choosing force winner")
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let status = Command::new("git")
            .arg("--git-dir")
            .arg(&self.path)
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
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.path)
            .args(args)
            .output()
            .with_context(|| "failed to run git")?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            bail!(
                "git failed: {}",
                self.redactor
                    .redact(&String::from_utf8_lossy(&output.stderr))
            );
        }
    }
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
        println!("dry-run: {} {}", program, redactor.redact(&args.join(" ")));
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
        bail!("{program} failed\nstdout: {stdout}\nstderr: {stderr}");
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
mod tests {
    use super::*;
    use std::io::Write;

    use tempfile::TempDir;

    #[test]
    fn remote_names_are_git_friendly() {
        assert_eq!(
            safe_remote_name("github:alice/project"),
            "github_alice_project"
        );
    }

    #[test]
    fn redacts_all_secrets() {
        let redactor = Redactor::new(vec!["secret".to_string()]);
        assert_eq!(
            redactor.redact("https://secret@example.test"),
            "https://<redacted>@example.test"
        );
    }

    #[test]
    fn branch_decisions_choose_fast_forward_tip() {
        let fixture = GitFixture::new();
        let base = fixture.commit("base", "base", 1_700_000_000);
        fixture.push_head(&fixture.remote_a, "main");
        fixture.push_head(&fixture.remote_b, "main");
        let newer = fixture.commit("newer", "newer", 1_700_000_100);
        fixture.push_head(&fixture.remote_a, "main");

        let mirror = fixture.mirror();
        fixture.fetch_all(&mirror);
        let (decisions, conflicts) = mirror.branch_decisions(&fixture.remotes(), false).unwrap();

        assert!(conflicts.is_empty());
        let main = find_branch(&decisions, "main");
        assert_eq!(main.sha, newer);
        assert_eq!(main.source_remotes, vec!["a".to_string()]);
        assert_ne!(main.sha, base);
    }

    #[test]
    fn branch_decisions_report_divergent_tips_without_force() {
        let fixture = GitFixture::new();
        let base = fixture.commit("base", "base", 1_700_000_000);
        fixture.push_head(&fixture.remote_a, "main");
        fixture.push_head(&fixture.remote_b, "main");

        let a_tip = fixture.commit("a", "a", 1_700_000_100);
        fixture.push_head(&fixture.remote_a, "main");
        fixture.reset_hard(&base);
        let b_tip = fixture.commit("b", "b", 1_700_000_200);
        fixture.push_head(&fixture.remote_b, "main");

        let mirror = fixture.mirror();
        fixture.fetch_all(&mirror);
        let (decisions, conflicts) = mirror.branch_decisions(&fixture.remotes(), false).unwrap();

        assert!(decisions.is_empty());
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].branch, "main");
        assert!(conflicts[0].tips.iter().any(|(_, sha)| sha == &a_tip));
        assert!(conflicts[0].tips.iter().any(|(_, sha)| sha == &b_tip));
    }

    #[test]
    fn branch_decisions_force_selects_newest_divergent_tip() {
        let fixture = GitFixture::new();
        let base = fixture.commit("base", "base", 1_700_000_000);
        fixture.push_head(&fixture.remote_a, "main");
        fixture.push_head(&fixture.remote_b, "main");

        let older = fixture.commit("older", "older", 1_700_000_100);
        fixture.push_head(&fixture.remote_a, "main");
        fixture.reset_hard(&base);
        let newer = fixture.commit("newer", "newer", 1_700_000_200);
        fixture.push_head(&fixture.remote_b, "main");

        let mirror = fixture.mirror();
        fixture.fetch_all(&mirror);
        let (decisions, conflicts) = mirror.branch_decisions(&fixture.remotes(), true).unwrap();

        assert!(conflicts.is_empty());
        let main = find_branch(&decisions, "main");
        assert_eq!(main.sha, newer);
        assert_ne!(main.sha, older);
        assert_eq!(main.source_remotes, vec!["b".to_string()]);
    }

    #[test]
    fn push_branches_creates_missing_branch_on_other_remotes() {
        let fixture = GitFixture::new();
        let expected = fixture.commit("base", "base", 1_700_000_000);
        fixture.push_head(&fixture.remote_a, "main");

        let mirror = fixture.mirror();
        fixture.fetch_all(&mirror);
        let (decisions, conflicts) = mirror.branch_decisions(&fixture.remotes(), false).unwrap();
        assert!(conflicts.is_empty());
        mirror
            .push_branches(&fixture.remotes(), &decisions, false)
            .unwrap();

        assert_eq!(
            fixture.remote_ref(&fixture.remote_b, "refs/heads/main"),
            expected
        );
    }

    #[test]
    fn tag_decisions_mirror_matching_or_missing_tags_and_skip_divergent_tags() {
        let fixture = GitFixture::new();
        let base = fixture.commit("base", "base", 1_700_000_000);
        fixture.tag("v1");
        fixture.push_head(&fixture.remote_a, "main");
        fixture.push_head(&fixture.remote_b, "main");
        fixture.push_tag(&fixture.remote_a, "v1");
        fixture.push_tag(&fixture.remote_b, "v1");

        let a_tip = fixture.commit("a", "a", 1_700_000_100);
        fixture.tag("release");
        fixture.push_head(&fixture.remote_a, "main");
        fixture.push_tag(&fixture.remote_a, "release");

        fixture.delete_tag("release");
        fixture.reset_hard(&base);
        let b_tip = fixture.commit("b", "b", 1_700_000_200);
        fixture.tag("release");
        fixture.push_head(&fixture.remote_b, "main");
        fixture.push_tag(&fixture.remote_b, "release");

        fixture.delete_tag("missing-on-b");
        fixture.reset_hard(&a_tip);
        fixture.tag("missing-on-b");
        fixture.push_tag(&fixture.remote_a, "missing-on-b");

        let mirror = fixture.mirror();
        fixture.fetch_all(&mirror);
        let (tags, conflicts) = mirror.tag_decisions(&fixture.remotes()).unwrap();

        assert_eq!(find_tag(&tags, "v1").sha, base);
        assert_eq!(find_tag(&tags, "missing-on-b").sha, a_tip);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].tag, "release");
        assert!(conflicts[0].tips.iter().any(|(_, sha)| sha == &a_tip));
        assert!(conflicts[0].tips.iter().any(|(_, sha)| sha == &b_tip));

        mirror.push_tags(&fixture.remotes(), &tags).unwrap();
        assert_eq!(
            fixture.remote_ref(&fixture.remote_b, "refs/tags/missing-on-b"),
            a_tip
        );
    }

    fn find_branch<'a>(decisions: &'a [BranchDecision], name: &str) -> &'a BranchDecision {
        decisions
            .iter()
            .find(|decision| decision.branch == name)
            .unwrap_or_else(|| panic!("missing branch decision for {name}"))
    }

    fn find_tag<'a>(decisions: &'a [TagDecision], name: &str) -> &'a TagDecision {
        decisions
            .iter()
            .find(|decision| decision.tag == name)
            .unwrap_or_else(|| panic!("missing tag decision for {name}"))
    }

    struct GitFixture {
        _temp: TempDir,
        work: PathBuf,
        mirror_path: PathBuf,
        remote_a: PathBuf,
        remote_b: PathBuf,
    }

    impl GitFixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let work = temp.path().join("work");
            let mirror_path = temp.path().join("mirror.git");
            let remote_a = temp.path().join("a.git");
            let remote_b = temp.path().join("b.git");
            git(None, ["init", "--bare", remote_a.to_str().unwrap()]);
            git(None, ["init", "--bare", remote_b.to_str().unwrap()]);
            fs::create_dir_all(&work).unwrap();
            git(Some(&work), ["init"]);
            git(Some(&work), ["config", "user.email", "test@example.test"]);
            git(Some(&work), ["config", "user.name", "Test User"]);
            git(Some(&work), ["checkout", "-b", "main"]);

            Self {
                _temp: temp,
                work,
                mirror_path,
                remote_a,
                remote_b,
            }
        }

        fn mirror(&self) -> GitMirror {
            let mirror =
                GitMirror::open(self.mirror_path.clone(), Redactor::new(Vec::new()), false)
                    .unwrap();
            mirror.configure_remotes(&self.remotes()).unwrap();
            mirror
        }

        fn remotes(&self) -> Vec<RemoteSpec> {
            vec![
                RemoteSpec {
                    name: "a".to_string(),
                    url: self.remote_a.to_string_lossy().to_string(),
                    display: "remote a".to_string(),
                },
                RemoteSpec {
                    name: "b".to_string(),
                    url: self.remote_b.to_string_lossy().to_string(),
                    display: "remote b".to_string(),
                },
            ]
        }

        fn fetch_all(&self, mirror: &GitMirror) {
            for remote in self.remotes() {
                mirror.fetch_remote(&remote).unwrap();
            }
        }

        fn commit(&self, message: &str, contents: &str, timestamp: i64) -> String {
            let path = self.work.join("file.txt");
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(file, "{contents}").unwrap();
            git(Some(&self.work), ["add", "file.txt"]);

            let date = format!("@{timestamp} +0000");
            let output = Command::new("git")
                .current_dir(&self.work)
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date)
                .args(["commit", "-m", message])
                .output()
                .unwrap();
            assert_success(&output, "git commit");
            self.head()
        }

        fn head(&self) -> String {
            git_output(Some(&self.work), ["rev-parse", "HEAD"])
        }

        fn reset_hard(&self, sha: &str) {
            git(Some(&self.work), ["reset", "--hard", sha]);
        }

        fn push_head(&self, remote: &Path, branch: &str) {
            let refspec = format!("HEAD:refs/heads/{branch}");
            git(
                Some(&self.work),
                ["push", remote.to_str().unwrap(), &refspec],
            );
        }

        fn tag(&self, name: &str) {
            git(Some(&self.work), ["tag", name]);
        }

        fn delete_tag(&self, name: &str) {
            let _ = Command::new("git")
                .current_dir(&self.work)
                .args(["tag", "-d", name])
                .output()
                .unwrap();
        }

        fn push_tag(&self, remote: &Path, tag: &str) {
            let refspec = format!("refs/tags/{tag}:refs/tags/{tag}");
            git(
                Some(&self.work),
                ["push", remote.to_str().unwrap(), &refspec],
            );
        }

        fn remote_ref(&self, remote: &Path, reference: &str) -> String {
            git_output(
                None,
                [
                    "--git-dir",
                    remote.to_str().unwrap(),
                    "rev-parse",
                    reference,
                ],
            )
        }
    }

    fn git<const N: usize>(current_dir: Option<&Path>, args: [&str; N]) {
        let output = git_command(current_dir, args).output().unwrap();
        assert_success(&output, "git");
    }

    fn git_output<const N: usize>(current_dir: Option<&Path>, args: [&str; N]) -> String {
        let output = git_command(current_dir, args).output().unwrap();
        assert_success(&output, "git output");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn git_command<const N: usize>(current_dir: Option<&Path>, args: [&str; N]) -> Command {
        let mut command = Command::new("git");
        command.args(args);
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }
        command
    }

    fn assert_success(output: &std::process::Output, label: &str) {
        assert!(
            output.status.success(),
            "{label} failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
