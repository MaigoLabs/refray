use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};
use sha2::Sha256;
use tempfile::TempDir;
use url::Url;

type HmacSha256 = Hmac<Sha256>;

const REPO_PREFIX: &str = "refray-e2e-";
const CONFLICT_BRANCH_ROOT: &str = "refray/conflicts/";
const MAIN_BRANCH: &str = "main";
const WEBHOOK_SECRET: &str = "refray-e2e-secret";

#[test]
#[ignore = "destructive live-provider e2e test; run explicitly with --ignored"]
fn sequential_live_e2e_all_supported_features() -> Result<()> {
    let env = EnvFile::load(Path::new(".env"))?;
    let settings = E2eSettings::from_env(&env)?;
    settings.require_destructive_guard()?;

    let mut run = E2eRun::new(settings)?;
    run.preflight()?;
    run.clear_repositories()?;
    run.write_config(ConflictMode::AutoRebasePullRequest, None, true)?;

    eprintln!("e2e phase: core sync");
    run.creation_branch_tag_and_read_write_sync()?;
    eprintln!("e2e phase: dry-run/no-create");
    run.dry_run_and_no_create_do_not_write()?;
    eprintln!("e2e phase: retry failed");
    run.failed_sync_can_retry_only_failed_repo()?;
    eprintln!("e2e phase: auto rebase");
    run.auto_rebase_resolves_non_conflicting_divergence()?;
    eprintln!("e2e phase: pull-request conflicts");
    run.pull_request_strategy_pushes_conflict_branches()?;
    eprintln!("e2e phase: auto-rebase PR fallback");
    run.auto_rebase_pull_request_falls_back_to_conflict_branches()?;
    eprintln!("e2e phase: branch deletion");
    run.branch_deletion_propagates()?;
    eprintln!("e2e phase: repo deletion");
    run.repo_deletion_propagates()?;
    eprintln!("e2e phase: webhooks");
    run.webhook_commands_and_receiver_work()?;

    run.clear_e2e_repositories()?;
    Ok(())
}

struct EnvFile {
    values: HashMap<String, String>,
}

impl EnvFile {
    fn load(path: &Path) -> Result<Self> {
        let mut values = std::env::vars().collect::<HashMap<_, _>>();
        if path.exists() {
            let contents = fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            for (line_number, line) in contents.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let Some((name, value)) = line.split_once('=') else {
                    bail!("invalid .env line {}", line_number + 1);
                };
                values.insert(name.trim().to_string(), parse_env_value(value.trim()));
            }
        }
        Ok(Self { values })
    }

    fn required(&self, names: &[&str]) -> Result<String> {
        names
            .iter()
            .find_map(|name| self.values.get(*name).filter(|value| !value.is_empty()))
            .cloned()
            .ok_or_else(|| anyhow!("missing required env var; tried {}", names.join(", ")))
    }

    fn optional(&self, names: &[&str]) -> Option<String> {
        names
            .iter()
            .find_map(|name| self.values.get(*name).filter(|value| !value.is_empty()))
            .cloned()
    }

    fn flag(&self, names: &[&str]) -> bool {
        self.optional(names)
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
    }
}

fn parse_env_value(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let quoted = (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''));
        if quoted {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

struct E2eSettings {
    providers: Vec<ProviderAccount>,
    destructive_guard: bool,
    clear_all_repos: bool,
    allow_partial: bool,
}

impl E2eSettings {
    fn from_env(env: &EnvFile) -> Result<Self> {
        let mut providers = Vec::new();
        if !env.flag(&["REFRAY_E2E_SKIP_GITHUB"]) {
            providers.push(ProviderAccount::new(
                "github",
                ProviderKind::Github,
                "https://github.com".to_string(),
                env.required(&["REFRAY_E2E_GITHUB_USERNAME", "GH_USER"])?,
                env.required(&["REFRAY_E2E_GITHUB_TOKEN", "GH_TOKEN"])?,
            ));
        }
        if !env.flag(&["REFRAY_E2E_SKIP_GITLAB"]) {
            providers.push(ProviderAccount::new(
                "gitlab",
                ProviderKind::Gitlab,
                env.optional(&["REFRAY_E2E_GITLAB_BASE_URL", "GL_BASE_URL"])
                    .unwrap_or_else(|| "https://gitlab.com".to_string()),
                env.required(&["REFRAY_E2E_GITLAB_USERNAME", "GL_USER"])?,
                env.required(&["REFRAY_E2E_GITLAB_TOKEN", "GL_TOKEN"])?,
            ));
        }
        if !env.flag(&["REFRAY_E2E_SKIP_GITEA"]) {
            providers.push(ProviderAccount::new(
                "gitea",
                ProviderKind::Gitea,
                env.optional(&["REFRAY_E2E_GITEA_BASE_URL", "GT_BASE_URL", "GITEA_BASE_URL"])
                    .unwrap_or_else(|| "https://gitea.com".to_string()),
                env.required(&["REFRAY_E2E_GITEA_USERNAME", "GT_USER"])?,
                env.required(&["REFRAY_E2E_GITEA_TOKEN", "GT_TOKEN"])?,
            ));
        }
        if !env.flag(&["REFRAY_E2E_SKIP_FORGEJO"]) {
            providers.push(ProviderAccount::new(
                "forgejo",
                ProviderKind::Forgejo,
                env.optional(&[
                    "REFRAY_E2E_FORGEJO_BASE_URL",
                    "FO_BASE_URL",
                    "FORGEJO_BASE_URL",
                ])
                .unwrap_or_else(|| "https://codeberg.org".to_string()),
                env.required(&["REFRAY_E2E_FORGEJO_USERNAME", "FO_USER"])?,
                env.required(&["REFRAY_E2E_FORGEJO_TOKEN", "FO_TOKEN"])?,
            ));
        }
        Ok(Self {
            providers,
            destructive_guard: env.flag(&["REFRAY_E2E_ALLOW_DESTRUCTIVE"]),
            clear_all_repos: clear_all_repos_enabled(env)?,
            allow_partial: env.flag(&["REFRAY_E2E_ALLOW_PARTIAL"]),
        })
    }

    fn require_destructive_guard(&self) -> Result<()> {
        if self.destructive_guard {
            return Ok(());
        }
        bail!("refusing to run destructive e2e test without REFRAY_E2E_ALLOW_DESTRUCTIVE=1")
    }

    fn secrets(&self) -> Vec<String> {
        self.providers
            .iter()
            .map(|provider| provider.token.clone())
            .collect()
    }
}

fn clear_all_repos_enabled(env: &EnvFile) -> Result<bool> {
    let Some(value) = env.optional(&["REFRAY_E2E_CLEAR_ALL_REPOS"]) else {
        return Ok(false);
    };
    if value == "DELETE_ALL_OWNED_REPOS" {
        return Ok(true);
    }
    bail!(
        "REFRAY_E2E_CLEAR_ALL_REPOS is destructive; set it to DELETE_ALL_OWNED_REPOS to delete every owned repo on the configured accounts"
    )
}

struct E2eRun {
    temp: TempDir,
    config_path: PathBuf,
    cache_home: PathBuf,
    settings: E2eSettings,
    redactor: Redactor,
    run_id: String,
}

impl E2eRun {
    fn new(settings: E2eSettings) -> Result<Self> {
        let temp = tempfile::tempdir().context("failed to create e2e temp dir")?;
        let config_path = temp.path().join("config.toml");
        let cache_home = temp.path().join("cache");
        let redactor = Redactor::new(settings.secrets());
        let run_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_secs()
            .to_string();
        Ok(Self {
            temp,
            config_path,
            cache_home,
            settings,
            redactor,
            run_id,
        })
    }

    fn preflight(&mut self) -> Result<()> {
        let mut valid = Vec::new();
        let mut failures = Vec::new();
        for mut provider in self.settings.providers.drain(..) {
            match provider.validate_token() {
                Ok(authenticated_username) => {
                    if provider.username != authenticated_username {
                        eprintln!(
                            "using authenticated {} username '{}' instead of configured username '{}'",
                            provider.site_name, authenticated_username, provider.username
                        );
                        provider.username = authenticated_username;
                    }
                    valid.push(provider);
                }
                Err(error) => failures.push((provider.site_name.clone(), error)),
            }
        }
        if !failures.is_empty() && !self.settings.allow_partial {
            let details = failures
                .into_iter()
                .map(|(site, error)| format!("{site}: {error:#}"))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("provider preflight failed:\n{details}");
        }
        for (site, error) in failures {
            eprintln!("skipping {site} e2e provider after preflight failure: {error:#}");
        }
        if valid.len() < 2 {
            bail!(
                "need at least two valid providers for e2e sync, got {}",
                valid.len()
            );
        }
        self.settings.providers = valid;
        Ok(())
    }

    fn clear_repositories(&self) -> Result<()> {
        self.clear_repositories_matching(self.settings.clear_all_repos)
    }

    fn clear_e2e_repositories(&self) -> Result<()> {
        self.clear_repositories_matching(false)
    }

    fn clear_repositories_matching(&self, clear_all: bool) -> Result<()> {
        for provider in &self.settings.providers {
            let repos = provider.list_repos()?;
            for repo in repos {
                if clear_all || repo.name.starts_with(REPO_PREFIX) {
                    provider.delete_repo(&repo.name)?;
                }
            }
        }
        Ok(())
    }

    fn repo_name(&self, label: &str) -> String {
        format!("{REPO_PREFIX}{}-{label}", self.run_id)
    }

    fn write_config(
        &self,
        conflict_mode: ConflictMode,
        whitelist_pattern: Option<&str>,
        create_missing: bool,
    ) -> Result<()> {
        self.write_config_for_sites(conflict_mode, whitelist_pattern, create_missing, None)
    }

    fn write_config_for_sites(
        &self,
        conflict_mode: ConflictMode,
        whitelist_pattern: Option<&str>,
        create_missing: bool,
        endpoint_sites: Option<&[&str]>,
    ) -> Result<()> {
        let default_whitelist = format!("^{REPO_PREFIX}{}-", self.run_id);
        let whitelist = whitelist_pattern.unwrap_or(&default_whitelist);
        let mut contents = "jobs = 1\n\n".to_string();
        for provider in &self.settings.providers {
            contents.push_str(&format!(
                r#"[[sites]]
name = "{}"
provider = "{}"
base_url = "{}"
token = {{ value = "{}" }}

"#,
                provider.site_name,
                provider.kind.config_name(),
                provider.base_url,
                provider.token.replace('"', "\\\"")
            ));
        }
        contents.push_str(&format!(
            r#"[webhook]
install = false
url = "http://127.0.0.1:1/webhook"
secret = {{ value = "{WEBHOOK_SECRET}" }}

[[mirrors]]
name = "all"
sync_visibility = "all"
repo_whitelist = ['{}']
create_missing = {}
visibility = "public"
conflict_resolution = "{}"

"#,
            whitelist.replace('\'', "''"),
            create_missing,
            conflict_mode.config_name()
        ));
        for provider in &self.settings.providers {
            if endpoint_sites.is_some_and(|sites| !sites.contains(&provider.site_name.as_str())) {
                continue;
            }
            contents.push_str(&format!(
                r#"[[mirrors.endpoints]]
site = "{}"
kind = "user"
namespace = "{}"

"#,
                provider.site_name,
                provider.username.replace('"', "\\\"")
            ));
        }
        fs::write(&self.config_path, contents)
            .with_context(|| format!("failed to write {}", self.config_path.display()))
    }

    fn creation_branch_tag_and_read_write_sync(&self) -> Result<()> {
        let repo = self.repo_name("core");
        let (source, peer) = self.provider_pair();
        source.create_repo(&repo)?;
        let work = self.git_worktree("core-seed")?;
        git_init(&work)?;
        write_commit(&work, "README.md", "seed\n", "seed", 1_700_000_001)?;
        git(&work, ["branch", "feature/github"])?;
        git(&work, ["tag", "v1.0.0"])?;
        let remote_url = source.authenticated_repo_url(&repo)?;
        self.git(&work, ["remote", "add", "origin", &remote_url])?;
        self.git(
            &work,
            ["push", "origin", "HEAD:main", "feature/github", "v1.0.0"],
        )?;
        source.wait_branch(
            &repo,
            MAIN_BRANCH,
            &git_output(&work, ["rev-parse", "HEAD"])?,
        )?;

        source.wait_repo_listed(&repo)?;
        self.sync_repo(&repo, [])?;
        self.assert_branch_all_equal_after_optional_resync(&repo, MAIN_BRANCH)?;
        self.assert_branch_all_equal(&repo, "feature/github")?;
        self.assert_tag_all_equal(&repo, "v1.0.0")?;

        let work = self.clone_repo(peer, &repo, "core-peer")?;
        write_commit(
            &work,
            "peer.txt",
            "peer update\n",
            "peer update",
            1_700_000_101,
        )?;
        self.git(&work, ["push", "origin", "HEAD:main"])?;
        peer.wait_branch(
            &repo,
            MAIN_BRANCH,
            &git_output(&work, ["rev-parse", "HEAD"])?,
        )?;
        peer.wait_repo_listed(&repo)?;
        self.sync_repo(&repo, [])?;
        self.assert_branch_all_equal_after_optional_resync(&repo, MAIN_BRANCH)?;
        Ok(())
    }

    fn dry_run_and_no_create_do_not_write(&self) -> Result<()> {
        let repo = self.repo_name("dry-run");
        let source = self.primary_provider();
        source.create_repo(&repo)?;
        self.seed_main(source, &repo, "dry run", 1_700_000_201)?;

        self.sync_repo(&repo, ["--dry-run"])?;
        self.assert_only_provider_has_repo(&repo, &source.site_name)?;

        self.sync_repo(&repo, ["--no-create"])?;
        self.assert_only_provider_has_repo(&repo, &source.site_name)?;
        Ok(())
    }

    fn failed_sync_can_retry_only_failed_repo(&self) -> Result<()> {
        let repo = self.repo_name("retry");
        let (source, peer) = self.provider_pair();
        self.seed_all_main(&repo, "retry base", 1_700_000_301)?;
        self.sync_repo(&repo, [])?;
        self.unprotect_main_all(&repo)?;

        self.commit_to_provider(
            source,
            &repo,
            "source-retry.txt",
            "source\n",
            "source retry",
            1_700_000_401,
        )?;
        self.commit_to_provider(
            peer,
            &repo,
            "peer-retry.txt",
            "peer\n",
            "peer retry",
            1_700_000_402,
        )?;
        self.write_config(ConflictMode::Fail, Some(&exact_pattern(&repo)), true)?;
        self.sync_repo_expect_failure(&repo, [])?;
        self.write_config(ConflictMode::AutoRebase, Some(&exact_pattern(&repo)), true)?;
        self.sync(["--retry-failed"])?;
        self.assert_branch_all_equal(&repo, MAIN_BRANCH)?;
        self.write_config(ConflictMode::AutoRebasePullRequest, None, true)?;
        Ok(())
    }

    fn auto_rebase_resolves_non_conflicting_divergence(&self) -> Result<()> {
        let repo = self.repo_name("rebase");
        let (source, peer) = self.provider_pair();
        self.seed_all_main(&repo, "rebase base", 1_700_000_501)?;
        self.sync_repo(&repo, [])?;
        self.unprotect_main_all(&repo)?;

        self.commit_to_provider(
            source,
            &repo,
            "source-rebase.txt",
            "source\n",
            "source rebase",
            1_700_000_601,
        )?;
        self.commit_to_provider(
            peer,
            &repo,
            "peer-rebase.txt",
            "peer\n",
            "peer rebase",
            1_700_000_602,
        )?;
        self.write_config(ConflictMode::AutoRebase, Some(&exact_pattern(&repo)), true)?;
        self.sync([])?;
        self.assert_branch_all_equal(&repo, MAIN_BRANCH)?;
        let clone = self.clone_repo(source, &repo, "rebase-verify")?;
        assert!(clone.join("source-rebase.txt").exists());
        assert!(clone.join("peer-rebase.txt").exists());
        self.write_config(ConflictMode::AutoRebasePullRequest, None, true)?;
        Ok(())
    }

    fn pull_request_strategy_pushes_conflict_branches(&self) -> Result<()> {
        let repo = self.repo_name("pull-request");
        let (source, peer) = self.provider_pair();
        self.seed_all_main(&repo, "pr base", 1_700_001_001)?;
        self.sync_repo(&repo, [])?;
        self.unprotect_main_all(&repo)?;

        self.commit_to_provider(
            source,
            &repo,
            "conflict.txt",
            "source\n",
            "source pr",
            1_700_001_101,
        )?;
        self.commit_to_provider(
            peer,
            &repo,
            "conflict.txt",
            "peer\n",
            "peer pr",
            1_700_001_102,
        )?;
        let endpoint_sites = [source.site_name.as_str(), peer.site_name.as_str()];
        self.write_config_for_sites(
            ConflictMode::PullRequest,
            Some(&exact_pattern(&repo)),
            true,
            Some(&endpoint_sites),
        )?;
        self.sync([])?;
        self.assert_conflict_branch_exists(&repo)?;
        self.assert_conflict_pull_request_exists(&repo)?;
        self.write_config(ConflictMode::AutoRebasePullRequest, None, true)?;
        Ok(())
    }

    fn auto_rebase_pull_request_falls_back_to_conflict_branches(&self) -> Result<()> {
        let repo = self.repo_name("fallback");
        let (source, peer) = self.provider_pair();
        self.seed_all_main(&repo, "fallback base", 1_700_001_201)?;
        self.sync_repo(&repo, [])?;
        self.unprotect_main_all(&repo)?;

        self.commit_to_provider(
            source,
            &repo,
            "fallback.txt",
            "source\n",
            "source fallback",
            1_700_001_301,
        )?;
        self.commit_to_provider(
            peer,
            &repo,
            "fallback.txt",
            "peer\n",
            "peer fallback",
            1_700_001_302,
        )?;
        let endpoint_sites = [source.site_name.as_str(), peer.site_name.as_str()];
        self.write_config_for_sites(
            ConflictMode::AutoRebasePullRequest,
            Some(&exact_pattern(&repo)),
            true,
            Some(&endpoint_sites),
        )?;
        self.sync([])?;
        self.assert_conflict_branch_exists(&repo)?;
        self.assert_conflict_pull_request_exists(&repo)?;
        self.write_config(ConflictMode::AutoRebasePullRequest, None, true)?;
        Ok(())
    }

    fn branch_deletion_propagates(&self) -> Result<()> {
        let repo = self.repo_name("branch-delete");
        let source = self.primary_provider();
        self.seed_all_main(&repo, "delete branch base", 1_700_001_401)?;
        let work = self.clone_repo(source, &repo, "branch-delete")?;
        git(&work, ["checkout", "-b", "delete-me"])?;
        write_commit(
            &work,
            "delete-me.txt",
            "delete me\n",
            "delete branch",
            1_700_001_402,
        )?;
        self.git(&work, ["push", "origin", "HEAD:delete-me"])?;
        source.wait_branch(
            &repo,
            "delete-me",
            &git_output(&work, ["rev-parse", "HEAD"])?,
        )?;
        self.sync_repo(&repo, [])?;
        source.wait_repo_listed(&repo)?;
        self.assert_branch_all_equal(&repo, "delete-me")?;

        self.git(&work, ["push", "origin", ":refs/heads/delete-me"])?;
        source.wait_branch_absent(&repo, "delete-me")?;
        self.sync_repo(&repo, [])?;
        self.assert_branch_absent_everywhere(&repo, "delete-me")?;
        Ok(())
    }

    fn repo_deletion_propagates(&self) -> Result<()> {
        let repo = self.repo_name("repo-delete");
        let source = self.primary_provider();
        self.seed_all_main(&repo, "repo delete base", 1_700_001_501)?;
        self.sync_repo(&repo, [])?;
        self.assert_repo_exists_everywhere(&repo)?;

        source.delete_repo(&repo)?;
        source.wait_repo_absent(&repo)?;
        self.sync_repo(&repo, [])?;
        self.assert_repo_absent_everywhere(&repo)?;
        Ok(())
    }

    fn webhook_commands_and_receiver_work(&self) -> Result<()> {
        let repo = self.repo_name("webhook");
        let source = self.primary_provider();
        self.seed_all_main(&repo, "webhook base", 1_700_001_601)?;
        self.sync_repo(&repo, [])?;

        self.refray(["webhook", "install", "--dry-run"])?;
        self.refray(["webhook", "uninstall", "--dry-run"])?;
        self.refray([
            "webhook",
            "update",
            "--dry-run",
            "https://example.invalid/new-webhook",
        ])?;

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        drop(listener);
        let mut server = self.spawn_refray(["serve", "--listen", &addr.to_string()])?;
        thread::sleep(Duration::from_millis(750));

        let (body, headers) = source.webhook_payload(&repo, WEBHOOK_SECRET);
        let mut request = Client::new().post(format!("http://{addr}/webhook"));
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request
            .body(body)
            .send()
            .context("failed to post webhook payload")?;
        let status = response.status();
        let text = response.text().unwrap_or_default();
        stop_child(&mut server);
        if !status.is_success() || !text.starts_with("queued ") {
            bail!("webhook receiver returned {status}: {text}");
        }
        Ok(())
    }

    fn seed_main(
        &self,
        provider: &ProviderAccount,
        repo: &str,
        contents: &str,
        timestamp: i64,
    ) -> Result<()> {
        let work = self.git_worktree(&format!("seed-{repo}"))?;
        git_init(&work)?;
        write_commit(
            &work,
            "README.md",
            &format!("{contents}\n"),
            contents,
            timestamp,
        )?;
        let remote_url = provider.authenticated_repo_url(repo)?;
        self.git(&work, ["remote", "add", "origin", &remote_url])?;
        self.git(&work, ["push", "origin", "HEAD:main"])?;
        provider.wait_branch(
            repo,
            MAIN_BRANCH,
            &git_output(&work, ["rev-parse", "HEAD"])?,
        )?;
        provider.wait_repo_listed(repo)?;
        Ok(())
    }

    fn seed_all_main(&self, repo: &str, contents: &str, timestamp: i64) -> Result<()> {
        for provider in &self.settings.providers {
            provider.create_repo(repo)?;
        }
        let work = self.git_worktree(&format!("seed-all-{repo}"))?;
        git_init(&work)?;
        write_commit(
            &work,
            "README.md",
            &format!("{contents}\n"),
            contents,
            timestamp,
        )?;
        let sha = git_output(&work, ["rev-parse", "HEAD"])?;
        for provider in &self.settings.providers {
            let remote_url = provider.authenticated_repo_url(repo)?;
            self.git(&work, ["remote", "add", &provider.site_name, &remote_url])?;
            self.git(&work, ["push", &provider.site_name, "HEAD:main"])?;
            provider.wait_branch(repo, MAIN_BRANCH, &sha)?;
            provider.wait_repo_listed(repo)?;
            provider.unprotect_branch(repo, MAIN_BRANCH)?;
        }
        Ok(())
    }

    fn commit_to_provider(
        &self,
        provider: &ProviderAccount,
        repo: &str,
        path: &str,
        contents: &str,
        message: &str,
        timestamp: i64,
    ) -> Result<()> {
        let work = self.clone_repo(
            provider,
            repo,
            &format!("commit-{}-{repo}", provider.site_name),
        )?;
        write_commit(&work, path, contents, message, timestamp)?;
        self.git(&work, ["push", "origin", "HEAD:main"])?;
        provider.wait_branch(
            repo,
            MAIN_BRANCH,
            &git_output(&work, ["rev-parse", "HEAD"])?,
        )?;
        provider.wait_repo_listed(repo)?;
        Ok(())
    }

    fn clone_repo(&self, provider: &ProviderAccount, repo: &str, label: &str) -> Result<PathBuf> {
        let path = self.git_worktree(label)?;
        let remote_url = provider.authenticated_repo_url(repo)?;
        let output = Command::new("git")
            .args(["clone", &remote_url, path.to_str().unwrap()])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .context("failed to run git clone")?;
        assert_output_success(output, "git clone", &self.redactor)?;
        Ok(path)
    }

    fn git_worktree(&self, label: &str) -> Result<PathBuf> {
        let path = self.temp.path().join("git").join(sanitize_path(label));
        fs::create_dir_all(path.parent().unwrap())?;
        Ok(path)
    }

    fn git<const N: usize>(&self, path: &Path, args: [&str; N]) -> Result<()> {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .context("failed to run git")?;
        assert_output_success(output, "git", &self.redactor)
    }

    fn set_repo_whitelist(&self, pattern: &str) -> Result<()> {
        let contents = fs::read_to_string(&self.config_path)
            .with_context(|| format!("failed to read {}", self.config_path.display()))?;
        let escaped_pattern = pattern.replace('\'', "''");
        let replacement = format!("repo_whitelist = ['{escaped_pattern}']");
        let mut replaced = false;
        let mut updated = contents
            .lines()
            .map(|line| {
                if line.starts_with("repo_whitelist = ") {
                    replaced = true;
                    replacement.clone()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if contents.ends_with('\n') {
            updated.push('\n');
        }
        if !replaced {
            bail!("config is missing repo_whitelist");
        }
        fs::write(&self.config_path, updated)
            .with_context(|| format!("failed to write {}", self.config_path.display()))
    }

    fn sync<const N: usize>(&self, args: [&str; N]) -> Result<()> {
        let mut command = vec!["sync"];
        command.extend(args);
        self.refray(command)
    }

    fn sync_repo<const N: usize>(&self, repo: &str, args: [&str; N]) -> Result<()> {
        self.set_repo_whitelist(&exact_pattern(repo))?;
        self.sync(args)
    }

    fn sync_expect_failure<const N: usize>(&self, args: [&str; N]) -> Result<()> {
        let mut command = vec!["sync"];
        command.extend(args);
        let output = self.refray_output(command)?;
        if output.status.success() {
            bail!("expected refray sync to fail, but it succeeded");
        }
        Ok(())
    }

    fn sync_repo_expect_failure<const N: usize>(&self, repo: &str, args: [&str; N]) -> Result<()> {
        self.set_repo_whitelist(&exact_pattern(repo))?;
        self.sync_expect_failure(args)
    }

    fn refray<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_string())
            .collect::<Vec<_>>();
        let mut output = self.refray_output(args.iter().map(String::as_str))?;
        if output.status.code() == Some(124) {
            output = self.refray_output(args.iter().map(String::as_str))?;
        }
        assert_output_success(output, "refray", &self.redactor)
    }

    fn refray_output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut command = Command::new("timeout");
        command
            .arg("--kill-after=5s")
            .arg("240s")
            .arg(env!("CARGO_BIN_EXE_refray"))
            .arg("--config")
            .arg(&self.config_path);
        for arg in args {
            command.arg(arg.as_ref());
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("XDG_CACHE_HOME", &self.cache_home)
            .output()
            .context("failed to run refray")
    }

    fn spawn_refray<const N: usize>(&self, args: [&str; N]) -> Result<Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_refray"));
        command.arg("--config").arg(&self.config_path);
        command.args(args);
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("XDG_CACHE_HOME", &self.cache_home)
            .spawn()
            .context("failed to spawn refray")
    }

    fn primary_provider(&self) -> &ProviderAccount {
        &self.settings.providers[self.primary_provider_index()]
    }

    fn provider_pair(&self) -> (&ProviderAccount, &ProviderAccount) {
        let first = self.primary_provider_index();
        let second = self
            .settings
            .providers
            .iter()
            .enumerate()
            .find(|(index, provider)| *index != first && provider.site_name == "gitlab")
            .map(|(index, _)| index)
            .or_else(|| {
                self.settings
                    .providers
                    .iter()
                    .enumerate()
                    .find(|(index, _)| *index != first)
                    .map(|(index, _)| index)
            })
            .expect("preflight ensures at least two providers");
        (
            &self.settings.providers[first],
            &self.settings.providers[second],
        )
    }

    fn primary_provider_index(&self) -> usize {
        self.settings
            .providers
            .iter()
            .position(|provider| provider.site_name == "github")
            .unwrap_or(0)
    }

    fn assert_repo_exists_everywhere(&self, repo: &str) -> Result<()> {
        for provider in &self.settings.providers {
            provider.wait_repo_present(repo)?;
        }
        Ok(())
    }

    fn assert_repo_absent_everywhere(&self, repo: &str) -> Result<()> {
        for provider in &self.settings.providers {
            provider.wait_repo_absent(repo)?;
        }
        Ok(())
    }

    fn assert_only_provider_has_repo(&self, repo: &str, site_name: &str) -> Result<()> {
        for provider in &self.settings.providers {
            let exists = provider.repo_exists(repo)?;
            if provider.site_name == site_name && !exists {
                bail!("expected {repo} to exist on {}", provider.site_name);
            }
            if provider.site_name != site_name && exists {
                bail!("expected {repo} to be absent on {}", provider.site_name);
            }
        }
        Ok(())
    }

    fn assert_branch_all_equal(&self, repo: &str, branch: &str) -> Result<()> {
        retry("branch convergence", || {
            let refs = self.refs_by_provider(repo)?;
            let values = refs
                .values()
                .map(|refs| refs.branches.get(branch).cloned())
                .collect::<Vec<_>>();
            if values.iter().any(Option::is_none) {
                bail!("branch {branch} missing on at least one provider: {values:?}");
            }
            let unique = values.into_iter().flatten().collect::<BTreeSet<_>>();
            if unique.len() != 1 {
                bail!("branch {branch} differs across providers: {unique:?}");
            }
            Ok(())
        })
    }

    fn assert_branch_all_equal_after_optional_resync(
        &self,
        repo: &str,
        branch: &str,
    ) -> Result<()> {
        match self.assert_branch_all_equal(repo, branch) {
            Ok(()) => Ok(()),
            Err(first_error) => {
                thread::sleep(Duration::from_secs(2));
                self.sync_repo(repo, [])
                    .with_context(|| format!("initial convergence failed: {first_error:#}"))?;
                self.assert_branch_all_equal(repo, branch)
            }
        }
    }

    fn assert_tag_all_equal(&self, repo: &str, tag: &str) -> Result<()> {
        retry("tag convergence", || {
            let refs = self.refs_by_provider(repo)?;
            let values = refs
                .values()
                .map(|refs| refs.tags.get(tag).cloned())
                .collect::<Vec<_>>();
            if values.iter().any(Option::is_none) {
                bail!("tag {tag} missing on at least one provider: {values:?}");
            }
            let unique = values.into_iter().flatten().collect::<BTreeSet<_>>();
            if unique.len() != 1 {
                bail!("tag {tag} differs across providers: {unique:?}");
            }
            Ok(())
        })
    }

    fn assert_branch_absent_everywhere(&self, repo: &str, branch: &str) -> Result<()> {
        retry("branch deletion", || {
            for (provider, refs) in self.refs_by_provider(repo)? {
                if refs.branches.contains_key(branch) {
                    bail!("branch {branch} still exists on {provider}");
                }
            }
            Ok(())
        })
    }

    fn assert_conflict_branch_exists(&self, repo: &str) -> Result<()> {
        retry("conflict branch", || {
            for refs in self.refs_by_provider(repo)?.values() {
                if refs
                    .branches
                    .keys()
                    .any(|branch| branch.starts_with(CONFLICT_BRANCH_ROOT))
                {
                    return Ok(());
                }
            }
            bail!("no refray/conflicts branch found for {repo}")
        })
    }

    fn assert_conflict_pull_request_exists(&self, repo: &str) -> Result<()> {
        retry("conflict pull request", || {
            for provider in &self.settings.providers {
                let pulls = provider.list_open_pull_requests(repo, MAIN_BRANCH)?;
                if pulls.iter().any(|pull| {
                    pull.head_branch.starts_with(CONFLICT_BRANCH_ROOT)
                        && pull.base_branch.as_deref() == Some(MAIN_BRANCH)
                }) {
                    return Ok(());
                }
            }
            bail!("no open refray conflict pull request found for {repo}")
        })
    }

    fn refs_by_provider(&self, repo: &str) -> Result<BTreeMap<String, GitRefs>> {
        let mut output = BTreeMap::new();
        for provider in &self.settings.providers {
            output.insert(provider.site_name.clone(), provider.ls_remote(repo)?);
        }
        Ok(output)
    }

    fn unprotect_main_all(&self, repo: &str) -> Result<()> {
        for provider in &self.settings.providers {
            provider.unprotect_branch(repo, MAIN_BRANCH)?;
        }
        Ok(())
    }
}

impl Drop for E2eRun {
    fn drop(&mut self) {
        if !self.settings.destructive_guard {
            return;
        }
        for provider in &self.settings.providers {
            let repos = match provider.list_repos() {
                Ok(repos) => repos,
                Err(error) => {
                    eprintln!(
                        "best-effort e2e cleanup could not list {} repos: {error:#}",
                        provider.site_name
                    );
                    continue;
                }
            };
            for repo in repos
                .into_iter()
                .filter(|repo| repo.name.starts_with(REPO_PREFIX))
            {
                if let Err(error) = provider.delete_repo(&repo.name) {
                    eprintln!(
                        "best-effort e2e cleanup could not delete {} on {}: {error:#}",
                        repo.name, provider.site_name
                    );
                }
            }
        }
    }
}

#[derive(Clone)]
struct ProviderAccount {
    site_name: String,
    kind: ProviderKind,
    base_url: String,
    username: String,
    token: String,
    http: Client,
}

impl ProviderAccount {
    fn new(
        site_name: impl Into<String>,
        kind: ProviderKind,
        base_url: String,
        username: String,
        token: String,
    ) -> Self {
        Self {
            site_name: site_name.into(),
            kind,
            base_url: trim_url(&base_url).to_string(),
            username,
            token,
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
        }
    }

    fn api_base(&self) -> String {
        match self.kind {
            ProviderKind::Github if self.base_url == "https://github.com" => {
                "https://api.github.com".to_string()
            }
            ProviderKind::Github => format!("{}/api/v3", self.base_url),
            ProviderKind::Gitlab => format!("{}/api/v4", self.base_url),
            ProviderKind::Gitea | ProviderKind::Forgejo => format!("{}/api/v1", self.base_url),
        }
    }

    fn validate_token(&self) -> Result<String> {
        let value = self
            .get_json::<Value>(&format!("{}/user", self.api_base()))
            .with_context(|| format!("failed to validate {} token", self.site_name))?;
        value
            .get(match self.kind {
                ProviderKind::Github | ProviderKind::Gitea | ProviderKind::Forgejo => "login",
                ProviderKind::Gitlab => "username",
            })
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("{} /user response did not include username", self.site_name))
    }

    fn list_repos(&self) -> Result<Vec<ProviderRepo>> {
        match self.kind {
            ProviderKind::Github => {
                let url = format!(
                    "{}/user/repos?affiliation=owner&visibility=all&per_page=100",
                    self.api_base()
                );
                let repos = self.paged_get(&url)?;
                Ok(repos
                    .into_iter()
                    .filter(|repo| repo.owner_login.as_deref() == Some(self.username.as_str()))
                    .collect())
            }
            ProviderKind::Gitlab => {
                let url = format!(
                    "{}/projects?owned=true&simple=true&per_page=100",
                    self.api_base()
                );
                let repos = self.paged_get(&url)?;
                Ok(repos
                    .into_iter()
                    .filter(|repo| repo.owner_login.as_deref() == Some(self.username.as_str()))
                    .collect())
            }
            ProviderKind::Gitea | ProviderKind::Forgejo => {
                let url = format!("{}/user/repos?limit=50", self.api_base());
                let repos = self.paged_get(&url)?;
                Ok(repos
                    .into_iter()
                    .filter(|repo| repo.owner_login.as_deref() == Some(self.username.as_str()))
                    .collect())
            }
        }
    }

    fn repo_exists(&self, repo: &str) -> Result<bool> {
        let url = self.repo_api_url(repo);
        let response = self.auth(self.http.get(url)).send()?;
        if response.status().is_success() {
            if matches!(self.kind, ProviderKind::Gitlab) {
                let value: Value = response.json()?;
                if value
                    .get("marked_for_deletion_on")
                    .is_some_and(|value| !value.is_null())
                    || value
                        .get("pending_delete")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                {
                    return Ok(false);
                }
            }
            Ok(true)
        } else if response.status().as_u16() == 404 {
            Ok(false)
        } else {
            bail!(
                "{} repo lookup returned {}",
                self.site_name,
                response.status()
            )
        }
    }

    fn wait_repo_present(&self, repo: &str) -> Result<()> {
        retry("repo present", || {
            if self.repo_exists(repo)? {
                Ok(())
            } else {
                bail!("{repo} is not visible on {} yet", self.site_name)
            }
        })
    }

    fn wait_repo_listed(&self, repo: &str) -> Result<()> {
        retry("repo listed", || {
            if self.list_repos()?.iter().any(|item| item.name == repo) {
                Ok(())
            } else {
                bail!("{repo} is not listed on {} yet", self.site_name)
            }
        })
    }

    fn wait_repo_absent(&self, repo: &str) -> Result<()> {
        retry("repo absent", || {
            if self.repo_exists(repo)? {
                bail!("{repo} still exists on {}", self.site_name)
            } else {
                Ok(())
            }
        })
    }

    fn wait_branch(&self, repo: &str, branch: &str, expected: &str) -> Result<()> {
        retry("branch visible", || {
            let refs = self.ls_remote(repo)?;
            match refs.branches.get(branch) {
                Some(actual) if actual == expected => Ok(()),
                Some(actual) => bail!(
                    "{} branch {branch} is at {actual}, expected {expected}",
                    self.site_name
                ),
                None => bail!("{} branch {branch} is not visible yet", self.site_name),
            }
        })
    }

    fn wait_branch_absent(&self, repo: &str, branch: &str) -> Result<()> {
        retry("branch absent", || {
            let refs = self.ls_remote(repo)?;
            if refs.branches.contains_key(branch) {
                bail!("{} branch {branch} is still visible", self.site_name)
            }
            Ok(())
        })
    }

    fn create_repo(&self, name: &str) -> Result<()> {
        if self.repo_exists(name)? {
            self.wait_repo_listed(name)?;
            return Ok(());
        }
        let body = match self.kind {
            ProviderKind::Github => json!({ "name": name, "private": false, "auto_init": false }),
            ProviderKind::Gitlab => json!({ "name": name, "path": name, "visibility": "public" }),
            ProviderKind::Gitea | ProviderKind::Forgejo => {
                json!({ "name": name, "private": false, "auto_init": false })
            }
        };
        let url = match self.kind {
            ProviderKind::Github => format!("{}/user/repos", self.api_base()),
            ProviderKind::Gitlab => format!("{}/projects", self.api_base()),
            ProviderKind::Gitea | ProviderKind::Forgejo => {
                format!("{}/user/repos", self.api_base())
            }
        };
        let response = self.auth(self.http.post(url.clone()).json(&body)).send()?;
        check_response("POST", &url, response)?;
        self.wait_repo_present(name)?;
        self.wait_repo_listed(name)
    }

    fn delete_repo(&self, name: &str) -> Result<()> {
        let url = self.repo_api_url(name);
        let response = self.auth(self.http.delete(url.clone())).send()?;
        if response.status().as_u16() == 404 {
            return Ok(());
        }
        if matches!(self.kind, ProviderKind::Gitlab) && response.status().as_u16() == 400 {
            let body = response.text().unwrap_or_default();
            if body.contains("already been marked for deletion") {
                return Ok(());
            }
            bail!("DELETE {url} returned 400 Bad Request: {body}");
        }
        check_response("DELETE", &url, response).map(|_| ())
    }

    fn unprotect_branch(&self, repo: &str, branch: &str) -> Result<()> {
        if !matches!(self.kind, ProviderKind::Gitlab) {
            return Ok(());
        }
        let url = format!(
            "{}/protected_branches/{}",
            self.repo_api_url(repo),
            urlencoding(branch)
        );
        let response = self.auth(self.http.delete(url.clone())).send()?;
        if response.status().is_success() || response.status().as_u16() == 404 {
            return Ok(());
        }
        check_response("DELETE", &url, response).map(|_| ())
    }

    fn repo_api_url(&self, repo: &str) -> String {
        match self.kind {
            ProviderKind::Github => format!("{}/repos/{}/{}", self.api_base(), self.username, repo),
            ProviderKind::Gitlab => format!(
                "{}/projects/{}",
                self.api_base(),
                urlencoding(&format!("{}/{}", self.username, repo))
            ),
            ProviderKind::Gitea | ProviderKind::Forgejo => {
                format!("{}/repos/{}/{}", self.api_base(), self.username, repo)
            }
        }
    }

    fn authenticated_repo_url(&self, repo: &str) -> Result<String> {
        let username = match self.kind {
            ProviderKind::Github => "x-access-token",
            ProviderKind::Gitlab | ProviderKind::Gitea | ProviderKind::Forgejo => "oauth2",
        };
        let mut url = Url::parse(&self.base_url)
            .with_context(|| format!("invalid {} base URL", self.site_name))?;
        url.set_username(username)
            .map_err(|_| anyhow!("failed to set username on {} clone URL", self.site_name))?;
        url.set_password(Some(&self.token))
            .map_err(|_| anyhow!("failed to set token on {} clone URL", self.site_name))?;
        let base_path = url.path().trim_end_matches('/');
        let repo_path = format!("{}/{}.git", self.username, repo);
        if base_path.is_empty() {
            url.set_path(&repo_path);
        } else {
            url.set_path(&format!("{base_path}/{repo_path}"));
        }
        Ok(url.to_string())
    }

    fn webhook_payload(&self, repo: &str, secret: &str) -> (String, Vec<(&'static str, String)>) {
        match self.kind {
            ProviderKind::Gitlab => {
                let body = json!({
                    "project": {
                        "path": repo,
                        "path_with_namespace": format!("{}/{}", self.username, repo),
                        "namespace": self.username,
                    }
                })
                .to_string();
                (
                    body,
                    vec![
                        ("X-Gitlab-Event", "Push Hook".to_string()),
                        ("X-Gitlab-Token", secret.to_string()),
                    ],
                )
            }
            ProviderKind::Github | ProviderKind::Gitea | ProviderKind::Forgejo => {
                let body = json!({
                    "repository": {
                        "name": repo,
                        "full_name": format!("{}/{}", self.username, repo),
                        "owner": { "login": self.username },
                    }
                })
                .to_string();
                let signature = format!(
                    "sha256={}",
                    hmac_sha256_hex(secret.as_bytes(), body.as_bytes())
                );
                let event_header = match self.kind {
                    ProviderKind::Github => "X-GitHub-Event",
                    ProviderKind::Gitea => "X-Gitea-Event",
                    ProviderKind::Forgejo => "X-Forgejo-Event",
                    ProviderKind::Gitlab => unreachable!(),
                };
                let signature_header = match self.kind {
                    ProviderKind::Github => "X-Hub-Signature-256",
                    ProviderKind::Gitea => "X-Gitea-Signature",
                    ProviderKind::Forgejo => "X-Forgejo-Signature",
                    ProviderKind::Gitlab => unreachable!(),
                };
                (
                    body,
                    vec![
                        (event_header, "push".to_string()),
                        (signature_header, signature),
                    ],
                )
            }
        }
    }

    fn ls_remote(&self, repo: &str) -> Result<GitRefs> {
        let remote_url = self.authenticated_repo_url(repo)?;
        let output = Command::new("git")
            .args(["ls-remote", "--heads", "--tags", "--refs", &remote_url])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .context("failed to run git ls-remote")?;
        if !output.status.success() {
            let redactor = Redactor::new(vec![self.token.clone()]);
            let stdout = redactor.redact(&String::from_utf8_lossy(&output.stdout));
            let stderr = redactor.redact(&String::from_utf8_lossy(&output.stderr));
            bail!(
                "failed to inspect refs for {} on {}: {}\nstdout:\n{}\nstderr:\n{}",
                repo,
                self.site_name,
                output.status,
                stdout,
                stderr
            );
        }
        GitRefs::from_output(&output.stdout)
    }

    fn list_open_pull_requests(
        &self,
        repo: &str,
        base_branch: &str,
    ) -> Result<Vec<ProviderPullRequest>> {
        let url = match self.kind {
            ProviderKind::Github => format!(
                "{}/repos/{}/{}/pulls?state=open&base={}&per_page=100",
                self.api_base(),
                self.username,
                repo,
                urlencoding(base_branch)
            ),
            ProviderKind::Gitlab => format!(
                "{}/projects/{}/merge_requests?state=opened&target_branch={}&per_page=100",
                self.api_base(),
                urlencoding(&format!("{}/{}", self.username, repo)),
                urlencoding(base_branch)
            ),
            ProviderKind::Gitea | ProviderKind::Forgejo => format!(
                "{}/repos/{}/{}/pulls?state=open&base={}&limit=50",
                self.api_base(),
                self.username,
                repo,
                urlencoding(base_branch)
            ),
        };
        Ok(self
            .paged_get_values(&url)?
            .into_iter()
            .map(|value| ProviderPullRequest::from_json(self.kind, &value))
            .collect())
    }

    fn paged_get(&self, first_url: &str) -> Result<Vec<ProviderRepo>> {
        Ok(self
            .paged_get_values(first_url)?
            .into_iter()
            .map(|value| ProviderRepo::from_json(self.kind, &value))
            .collect())
    }

    fn paged_get_values(&self, first_url: &str) -> Result<Vec<Value>> {
        let mut url = first_url.to_string();
        let mut output = Vec::new();
        loop {
            let response = self.auth(self.http.get(url.clone())).send()?;
            let headers = response.headers().clone();
            let response = check_response("GET", &url, response)?;
            let value: Value = response.json()?;
            let items = value
                .as_array()
                .ok_or_else(|| anyhow!("{} did not return an array", self.site_name))?;
            output.extend(items.iter().cloned());
            let Some(next) = next_link(&headers) else {
                break;
            };
            url = next;
        }
        Ok(output)
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let response = self.auth(self.http.get(url.to_string())).send()?;
        check_response("GET", url, response)?
            .json()
            .map_err(Into::into)
    }

    fn auth(&self, request: RequestBuilder) -> RequestBuilder {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("refray-e2e/0.1"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        match self.kind {
            ProviderKind::Github => {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {}", self.token)).unwrap(),
                );
                headers.insert(
                    "X-GitHub-Api-Version",
                    HeaderValue::from_static("2022-11-28"),
                );
            }
            ProviderKind::Gitlab => {
                headers.insert("PRIVATE-TOKEN", HeaderValue::from_str(&self.token).unwrap());
            }
            ProviderKind::Gitea | ProviderKind::Forgejo => {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("token {}", self.token)).unwrap(),
                );
            }
        }
        request.headers(headers)
    }
}

#[derive(Clone, Copy)]
enum ProviderKind {
    Github,
    Gitlab,
    Gitea,
    Forgejo,
}

impl ProviderKind {
    fn config_name(self) -> &'static str {
        match self {
            ProviderKind::Github => "github",
            ProviderKind::Gitlab => "gitlab",
            ProviderKind::Gitea => "gitea",
            ProviderKind::Forgejo => "forgejo",
        }
    }
}

enum ConflictMode {
    Fail,
    AutoRebase,
    PullRequest,
    AutoRebasePullRequest,
}

impl ConflictMode {
    fn config_name(&self) -> &'static str {
        match self {
            ConflictMode::Fail => "fail",
            ConflictMode::AutoRebase => "auto_rebase",
            ConflictMode::PullRequest => "pull_request",
            ConflictMode::AutoRebasePullRequest => "auto_rebase_pull_request",
        }
    }
}

struct ProviderRepo {
    name: String,
    owner_login: Option<String>,
}

impl ProviderRepo {
    fn from_json(kind: ProviderKind, value: &Value) -> Self {
        let name = match kind {
            ProviderKind::Github | ProviderKind::Gitea | ProviderKind::Forgejo => value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            ProviderKind::Gitlab => value
                .get("path")
                .or_else(|| value.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
        }
        .to_string();
        let owner_login = match kind {
            ProviderKind::Github | ProviderKind::Gitea | ProviderKind::Forgejo => value
                .pointer("/owner/login")
                .or_else(|| value.pointer("/owner/username"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            ProviderKind::Gitlab => value
                .pointer("/namespace/path")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        };
        Self { name, owner_login }
    }
}

struct ProviderPullRequest {
    head_branch: String,
    base_branch: Option<String>,
}

impl ProviderPullRequest {
    fn from_json(kind: ProviderKind, value: &Value) -> Self {
        let head_branch = match kind {
            ProviderKind::Github | ProviderKind::Gitea | ProviderKind::Forgejo => value
                .pointer("/head/ref")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            ProviderKind::Gitlab => value
                .get("source_branch")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        }
        .to_string();
        let base_branch = match kind {
            ProviderKind::Github | ProviderKind::Gitea | ProviderKind::Forgejo => value
                .pointer("/base/ref")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            ProviderKind::Gitlab => value
                .get("target_branch")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        };
        Self {
            head_branch,
            base_branch,
        }
    }
}

struct GitRefs {
    branches: BTreeMap<String, String>,
    tags: BTreeMap<String, String>,
}

impl GitRefs {
    fn from_output(output: &[u8]) -> Result<Self> {
        let mut branches = BTreeMap::new();
        let mut tags = BTreeMap::new();
        for line in String::from_utf8_lossy(output).lines() {
            let Some((sha, reference)) = line.split_once('\t') else {
                continue;
            };
            if let Some(branch) = reference.strip_prefix("refs/heads/") {
                branches.insert(branch.to_string(), sha.to_string());
            } else if let Some(tag) = reference.strip_prefix("refs/tags/") {
                tags.insert(tag.to_string(), sha.to_string());
            }
        }
        Ok(Self { branches, tags })
    }
}

#[derive(Clone)]
struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    fn new(secrets: Vec<String>) -> Self {
        Self { secrets }
    }

    fn redact(&self, value: &str) -> String {
        self.secrets
            .iter()
            .fold(value.to_string(), |mut value, secret| {
                if !secret.is_empty() {
                    value = value.replace(secret, "<redacted>");
                }
                value
            })
    }
}

fn git_init(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    git(path, ["init", "-b", MAIN_BRANCH])?;
    git(path, ["config", "user.email", "refray-e2e@example.invalid"])?;
    git(path, ["config", "user.name", "refray e2e"])
}

fn write_commit(
    path: &Path,
    file: &str,
    contents: &str,
    message: &str,
    timestamp: i64,
) -> Result<()> {
    let file_path = path.join(file);
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(file_path, contents)?;
    git(path, ["add", file])?;
    let timestamp = format!("{timestamp} +0000");
    let output = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(path)
        .env("GIT_AUTHOR_DATE", &timestamp)
        .env("GIT_COMMITTER_DATE", &timestamp)
        .output()
        .context("failed to run git commit")?;
    assert_output_success(output, "git commit", &Redactor::new(Vec::new()))
}

fn git<const N: usize>(path: &Path, args: [&str; N]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("failed to run git")?;
    assert_output_success(output, "git", &Redactor::new(Vec::new()))
}

fn git_output<const N: usize>(path: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        return assert_output_success(output, "git", &Redactor::new(Vec::new()))
            .map(|_| String::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn assert_output_success(output: Output, label: &str, redactor: &Redactor) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stdout = redactor.redact(&String::from_utf8_lossy(&output.stdout));
    let stderr = redactor.redact(&String::from_utf8_lossy(&output.stderr));
    bail!(
        "{label} failed with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    )
}

fn retry(label: &str, mut action: impl FnMut() -> Result<()>) -> Result<()> {
    let mut last_error = None;
    for _ in 0..30 {
        match action() {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        thread::sleep(Duration::from_secs(2));
    }
    Err(last_error.unwrap_or_else(|| anyhow!("{label} did not complete")))
        .with_context(|| format!("timed out waiting for {label}"))
}

fn check_response(
    method: &str,
    url: &str,
    response: reqwest::blocking::Response,
) -> Result<reqwest::blocking::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().unwrap_or_default();
    bail!("{method} {url} returned {status}: {body}")
}

fn next_link(headers: &HeaderMap) -> Option<String> {
    let header = headers.get("link")?.to_str().ok()?;
    for part in header.split(',') {
        let mut sections = part.trim().split(';');
        let url = sections.next()?.trim();
        let rel = sections.any(|section| section.trim() == "rel=\"next\"");
        if rel {
            return url
                .strip_prefix('<')
                .and_then(|value| value.strip_suffix('>'))
                .map(ToString::to_string);
        }
    }
    None
}

fn hmac_sha256_hex(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).unwrap();
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn exact_pattern(repo: &str) -> String {
    format!("^{}$", regex::escape(repo))
}

fn trim_url(value: &str) -> &str {
    value.trim_end_matches('/')
}

fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn sanitize_path(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
