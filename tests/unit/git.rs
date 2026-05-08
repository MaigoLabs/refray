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
fn detects_provider_disabled_repository_errors() {
    let error: anyhow::Error = GitCommandError::new(
        "git",
        "",
        "remote: Access to this repository has been disabled by GitHub staff.\nfatal: unable to access 'https://github.com/alice/repo.git/': The requested URL returned error: 403",
    )
    .into();

    assert!(is_disabled_repository_error(&error));

    let generic_forbidden: anyhow::Error = GitCommandError::new(
        "git",
        "",
        "fatal: unable to access 'https://github.com/alice/repo.git/': The requested URL returned error: 403",
    )
    .into();

    assert!(!is_disabled_repository_error(&generic_forbidden));
}

#[test]
fn ls_remote_snapshot_changes_when_remote_refs_change() {
    let fixture = GitFixture::new();
    fixture.commit("base", "base", 1_700_000_000);
    fixture.tag("v1");
    fixture.push_head(&fixture.remote_a, "main");
    fixture.push_tag(&fixture.remote_a, "v1");
    let remote = fixture.remotes().remove(0);
    let redactor = Redactor::new(Vec::new());

    let first = ls_remote_refs(&remote, &redactor).unwrap();
    let unchanged = ls_remote_refs(&remote, &redactor).unwrap();
    assert_eq!(first, unchanged);
    assert_eq!(first.refs, 2);

    fixture.commit("feature", "feature", 1_700_000_100);
    fixture.push_head(&fixture.remote_a, "feature");
    let changed = ls_remote_refs(&remote, &redactor).unwrap();

    assert_ne!(first.hash, changed.hash);
    assert_eq!(changed.refs, 3);
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
    assert_eq!(main.target_remotes, vec!["b".to_string()]);
    assert_ne!(main.sha, base);
}

#[test]
fn branch_decisions_do_not_target_remotes_that_already_match() {
    let fixture = GitFixture::new();
    fixture.commit("base", "base", 1_700_000_000);
    fixture.push_head(&fixture.remote_a, "main");
    fixture.push_head(&fixture.remote_b, "main");

    let mirror = fixture.mirror();
    fixture.fetch_all(&mirror);
    let (decisions, conflicts) = mirror.branch_decisions(&fixture.remotes(), false).unwrap();

    assert!(conflicts.is_empty());
    let main = find_branch(&decisions, "main");
    assert_eq!(main.source_remotes, vec!["a".to_string(), "b".to_string()]);
    assert!(main.target_remotes.is_empty());
}

#[test]
fn cached_remote_refs_match_ls_remote_snapshot_after_fetch() {
    let fixture = GitFixture::new();
    fixture.commit("base", "base", 1_700_000_000);
    fixture.tag("v1");
    fixture.push_head(&fixture.remote_a, "main");
    fixture.push_tag(&fixture.remote_a, "v1");

    let mirror = fixture.mirror();
    let remote = fixture.remotes().remove(0);
    assert!(
        !mirror
            .cached_remote_refs_match(
                &remote,
                &ls_remote_refs(&remote, &Redactor::new(Vec::new())).unwrap(),
            )
            .unwrap()
    );

    mirror.fetch_remote(&remote).unwrap();
    let snapshot = ls_remote_refs(&remote, &Redactor::new(Vec::new())).unwrap();
    assert!(mirror.cached_remote_refs_match(&remote, &snapshot).unwrap());

    fixture.commit("newer", "newer", 1_700_000_100);
    fixture.push_head(&fixture.remote_a, "main");
    let changed = ls_remote_refs(&remote, &Redactor::new(Vec::new())).unwrap();
    assert!(!mirror.cached_remote_refs_match(&remote, &changed).unwrap());
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
    assert_eq!(main.target_remotes, vec!["a".to_string()]);
}

#[test]
fn auto_rebase_branch_conflict_replays_later_tip_and_marks_force_targets() {
    let fixture = GitFixture::new();
    let base = fixture.commit("base", "base", 1_700_000_000);
    fixture.push_head(&fixture.remote_a, "main");
    fixture.push_head(&fixture.remote_b, "main");

    let a_tip = fixture.commit_file("a", "a.txt", "a\n", 1_700_000_100);
    fixture.push_head(&fixture.remote_a, "main");
    fixture.reset_hard(&base);
    let b_tip = fixture.commit_file("b", "b.txt", "b\n", 1_700_000_200);
    fixture.push_head(&fixture.remote_b, "main");

    let mirror = fixture.mirror();
    fixture.fetch_all(&mirror);
    let (_, conflicts) = mirror.branch_decisions(&fixture.remotes(), false).unwrap();

    let decision = mirror
        .auto_rebase_branch_conflict(&fixture.remotes(), "main", &conflicts[0].tips)
        .unwrap();

    assert_eq!(decision.branch, "main");
    assert_ne!(decision.sha, a_tip);
    assert_ne!(decision.sha, b_tip);
    assert_eq!(decision.updates.len(), 2);
    let a_update = decision
        .updates
        .iter()
        .find(|update| update.target_remote == "a")
        .unwrap();
    assert!(!a_update.force);
    let b_update = decision
        .updates
        .iter()
        .find(|update| update.target_remote == "b")
        .unwrap();
    assert!(b_update.force);

    mirror
        .push_branch_updates(&fixture.remotes(), &decision.updates)
        .unwrap();
    assert_eq!(
        fixture.remote_ref(&fixture.remote_a, "refs/heads/main"),
        decision.sha
    );
    assert_eq!(
        fixture.remote_ref(&fixture.remote_b, "refs/heads/main"),
        decision.sha
    );
}

#[test]
fn auto_rebase_branch_conflict_fails_on_file_conflict() {
    let fixture = GitFixture::new();
    fixture.commit_file("base", "file.txt", "base\n", 1_700_000_000);
    let base = fixture.head();
    fixture.push_head(&fixture.remote_a, "main");
    fixture.push_head(&fixture.remote_b, "main");

    fixture.commit_file("a", "file.txt", "a\n", 1_700_000_100);
    fixture.push_head(&fixture.remote_a, "main");
    fixture.reset_hard(&base);
    fixture.commit_file("b", "file.txt", "b\n", 1_700_000_200);
    fixture.push_head(&fixture.remote_b, "main");

    let mirror = fixture.mirror();
    fixture.fetch_all(&mirror);
    let (_, conflicts) = mirror.branch_decisions(&fixture.remotes(), false).unwrap();

    let error = mirror
        .auto_rebase_branch_conflict(&fixture.remotes(), "main", &conflicts[0].tips)
        .unwrap_err()
        .to_string();

    assert!(error.contains("auto-rebase failed"));
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
fn delete_branches_removes_branch_from_target_remotes() {
    let fixture = GitFixture::new();
    fixture.commit("base", "base", 1_700_000_000);
    fixture.push_head(&fixture.remote_a, "main");
    fixture.push_head(&fixture.remote_b, "main");

    let mirror = fixture.mirror();
    mirror
        .delete_branches(
            &fixture.remotes(),
            &[BranchDeletion {
                branch: "main".to_string(),
                deleted_remotes: vec!["a".to_string()],
                target_remotes: vec!["b".to_string()],
            }],
        )
        .unwrap();

    assert!(fixture.remote_ref_exists(&fixture.remote_a, "refs/heads/main"));
    assert!(!fixture.remote_ref_exists(&fixture.remote_b, "refs/heads/main"));
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
    assert!(find_tag(&tags, "v1").target_remotes.is_empty());
    assert_eq!(find_tag(&tags, "missing-on-b").sha, a_tip);
    assert_eq!(
        find_tag(&tags, "missing-on-b").target_remotes,
        vec!["b".to_string()]
    );
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
            GitMirror::open(self.mirror_path.clone(), Redactor::new(Vec::new()), false).unwrap();
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

    fn commit_file(
        &self,
        message: &str,
        file_name: &str,
        contents: &str,
        timestamp: i64,
    ) -> String {
        let path = self.work.join(file_name);
        fs::write(path, contents).unwrap();
        git(Some(&self.work), ["add", file_name]);

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

    fn remote_ref_exists(&self, remote: &Path, reference: &str) -> bool {
        git_command(
            None,
            [
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "--verify",
                reference,
            ],
        )
        .output()
        .unwrap()
        .status
        .success()
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
