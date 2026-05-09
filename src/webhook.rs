use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use console::style;
use hmac::{Hmac, KeyInit, Mac};
use regex::escape;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::config::{
    Config, EndpointConfig, MirrorConfig, ProviderKind, default_work_dir, validate_config,
};
use crate::provider::{EndpointRepo, ProviderClient, RemoteRepo};
use crate::state::{load_toml_or_default, save_toml};
use crate::sync::{SyncOptions, sync_all};

type HmacSha256 = Hmac<Sha256>;
const WEBHOOK_STATE_FILE: &str = "webhook-state.toml";

#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub listen: String,
    pub secret: String,
    pub workers: usize,
    pub work_dir: Option<PathBuf>,
    pub full_sync_interval_minutes: Option<u64>,
    pub reachability_url: Option<String>,
    pub reachability_check_interval_minutes: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct WebhookInstallOptions {
    pub url: String,
    pub secret: String,
    pub dry_run: bool,
    pub work_dir: Option<PathBuf>,
    pub jobs: usize,
}

#[derive(Clone, Debug)]
pub struct WebhookUninstallOptions {
    pub url: String,
    pub dry_run: bool,
    pub work_dir: Option<PathBuf>,
    pub jobs: usize,
}

#[derive(Clone, Debug)]
pub struct WebhookUpdateOptions {
    pub old_url: String,
    pub new_url: String,
    pub secret: String,
    pub dry_run: bool,
    pub work_dir: Option<PathBuf>,
    pub jobs: usize,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct WebhookJob {
    group: String,
    repo: String,
}

#[derive(Clone)]
struct JobQueue {
    sender: mpsc::Sender<WebhookJob>,
    pending: Arc<Mutex<BTreeSet<WebhookJob>>>,
}

pub fn serve(config: Config, options: ServeOptions) -> Result<()> {
    validate_config(&config)?;
    if options.workers == 0 {
        bail!("jobs must be at least 1");
    }
    let server = Server::http(&options.listen)
        .map_err(|error| anyhow::anyhow!("failed to listen on {}: {error}", options.listen))?;
    crate::logln!(
        "{} {}",
        style("Webhook server").cyan().bold(),
        style(&options.listen).bold()
    );

    let config = Arc::new(config);
    let (sender, receiver) = mpsc::channel::<WebhookJob>();
    let pending = Arc::new(Mutex::new(BTreeSet::<WebhookJob>::new()));
    let receiver = Arc::new(Mutex::new(receiver));
    let sync_lock = Arc::new(Mutex::new(()));
    for worker_id in 0..options.workers {
        let receiver = Arc::clone(&receiver);
        let pending = Arc::clone(&pending);
        let config = Arc::clone(&config);
        let sync_lock = Arc::clone(&sync_lock);
        let work_dir = options.work_dir.clone();
        thread::spawn(move || {
            worker_loop(worker_id, receiver, pending, sync_lock, config, work_dir)
        });
    }

    if let Some(minutes) = options
        .full_sync_interval_minutes
        .filter(|minutes| *minutes > 0)
    {
        let config = Arc::clone(&config);
        let sync_lock = Arc::clone(&sync_lock);
        let work_dir = options.work_dir.clone();
        thread::spawn(move || full_sync_timer_loop(config, sync_lock, work_dir, minutes));
    }
    if let Some(url) = options.reachability_url.clone() {
        let minutes = options
            .reachability_check_interval_minutes
            .filter(|minutes| *minutes > 0)
            .unwrap_or(15);
        thread::spawn(move || reachability_timer_loop(url, minutes));
    }

    let queue = JobQueue { sender, pending };
    for request in server.incoming_requests() {
        let response = handle_request(request, &config, &options.secret, &queue);
        if let Err(error) = response {
            crate::logln!("{} {error:#}", style("webhook error").red().bold());
        }
    }
    Ok(())
}

fn full_sync_timer_loop(
    config: Arc<Config>,
    sync_lock: Arc<Mutex<()>>,
    work_dir: Option<PathBuf>,
    minutes: u64,
) {
    loop {
        thread::sleep(Duration::from_secs(minutes * 60));
        crate::logln!(
            "{} {}",
            style("full sync timer").cyan().bold(),
            style(format!("every {minutes} minute(s)")).dim()
        );
        let _sync_guard = sync_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = sync_all(
            &config,
            SyncOptions {
                work_dir: work_dir.clone(),
                ..SyncOptions::default()
            },
        ) {
            crate::logln!("{} {error:#}", style("full sync failed").red().bold());
        }
    }
}

fn reachability_timer_loop(url: String, minutes: u64) {
    loop {
        thread::sleep(Duration::from_secs(minutes * 60));
        if let Err(error) = check_webhook_url_reachable(&url) {
            crate::logln!(
                "{} {}: {error:#}",
                style("webhook URL unreachable").yellow().bold(),
                style(&url).cyan()
            );
        }
    }
}

pub fn install_webhooks(config: &Config, options: WebhookInstallOptions) -> Result<()> {
    validate_config(config)?;
    if options.jobs == 0 {
        bail!("jobs must be at least 1");
    }
    let work_dir = options.work_dir.clone().unwrap_or_else(default_work_dir);
    let state = Arc::new(Mutex::new(load_webhook_state(&work_dir)?));

    for mirror in &config.mirrors {
        crate::logln!();
        crate::logln!(
            "{} {}",
            style("Webhook group").cyan().bold(),
            style(&mirror.name).bold()
        );
        let repo_filter = mirror.repo_filter()?;
        let mut tasks = Vec::new();
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
            for repo in repos
                .into_iter()
                .filter(|repo| mirror.sync_visibility.matches_private(repo.private))
                .filter(|repo| repo_filter.matches(&repo.name))
            {
                tasks.push(WebhookInstallTask {
                    site: site.clone(),
                    group: mirror.name.clone(),
                    endpoint: endpoint.clone(),
                    repo,
                    url: options.url.clone(),
                    secret: options.secret.clone(),
                    dry_run: options.dry_run,
                });
            }
        }
        run_install_tasks(tasks, options.jobs, Arc::clone(&state), false)?;
    }
    if !options.dry_run {
        let state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        save_webhook_state(&work_dir, &state)?;
    }
    Ok(())
}

pub fn uninstall_webhooks(config: &Config, options: WebhookUninstallOptions) -> Result<()> {
    validate_config(config)?;
    if options.jobs == 0 {
        bail!("jobs must be at least 1");
    }
    let work_dir = options.work_dir.clone().unwrap_or_else(default_work_dir);
    let mut state = load_webhook_state(&work_dir)?;
    let mut tasks = Vec::new();
    for mirror in &config.mirrors {
        crate::logln!();
        crate::logln!(
            "{} {}",
            style("Webhook group").cyan().bold(),
            style(&mirror.name).bold()
        );
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
                tasks.push(WebhookUninstallTask {
                    group: mirror.name.clone(),
                    site: site.clone(),
                    endpoint: endpoint.clone(),
                    repo,
                    url: options.url.clone(),
                    dry_run: options.dry_run,
                });
            }
        }
    }
    let removed_keys = run_uninstall_tasks(tasks, options.jobs)?;

    if !options.dry_run {
        remove_webhook_state_keys(&mut state, removed_keys, &options.url);
        save_webhook_state(&work_dir, &state)?;
    }
    Ok(())
}

pub fn update_webhooks(config: &Config, options: WebhookUpdateOptions) -> Result<()> {
    validate_config(config)?;
    if options.jobs == 0 {
        bail!("jobs must be at least 1");
    }
    if options.old_url != options.new_url {
        crate::logln!(
            "{} {} -> {}",
            style("Webhook URL").cyan().bold(),
            style(&options.old_url).dim(),
            style(&options.new_url).cyan()
        );
        uninstall_webhooks(
            config,
            WebhookUninstallOptions {
                url: options.old_url.clone(),
                dry_run: options.dry_run,
                work_dir: options.work_dir.clone(),
                jobs: options.jobs,
            },
        )?;
    }

    install_webhooks(
        config,
        WebhookInstallOptions {
            url: options.new_url,
            secret: options.secret,
            dry_run: options.dry_run,
            work_dir: options.work_dir,
            jobs: options.jobs,
        },
    )
}

pub fn ensure_configured_webhooks(
    config: &Config,
    mirror: &MirrorConfig,
    repos: &[EndpointRepo],
    work_dir: &Path,
    jobs: usize,
) -> Result<()> {
    let Some(webhook) = &config.webhook else {
        return Ok(());
    };
    if !webhook.install {
        return Ok(());
    }
    if jobs == 0 {
        bail!("jobs must be at least 1");
    }
    let secret = webhook.secret()?;
    let state = Arc::new(Mutex::new(load_webhook_state(work_dir)?));
    let mut tasks = Vec::new();
    for endpoint_repo in repos {
        let Some(site) = config.site(&endpoint_repo.endpoint.site) else {
            continue;
        };
        tasks.push(WebhookInstallTask {
            site: site.clone(),
            group: mirror.name.clone(),
            endpoint: endpoint_repo.endpoint.clone(),
            repo: endpoint_repo.repo.clone(),
            url: webhook.url.clone(),
            secret: secret.clone(),
            dry_run: false,
        });
    }
    run_install_tasks(tasks, jobs, Arc::clone(&state), true)?;
    let state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    save_webhook_state(work_dir, &state)
}

pub fn check_webhook_url_reachable(url: &str) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    client
        .get(url)
        .send()
        .with_context(|| format!("failed to reach {url}"))?;
    Ok(())
}

fn worker_loop(
    worker_id: usize,
    receiver: Arc<Mutex<mpsc::Receiver<WebhookJob>>>,
    pending: Arc<Mutex<BTreeSet<WebhookJob>>>,
    sync_lock: Arc<Mutex<()>>,
    config: Arc<Config>,
    work_dir: Option<PathBuf>,
) {
    loop {
        let job = {
            let receiver = receiver
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            receiver.recv()
        };
        let Ok(job) = job else {
            return;
        };

        crate::logln!(
            "{} {} {}",
            style(format!("worker {worker_id}")).cyan().bold(),
            style(&job.group).bold(),
            style(&job.repo).cyan()
        );
        let _sync_guard = sync_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = sync_all(
            &config,
            SyncOptions {
                group: Some(job.group.clone()),
                repo_pattern: Some(format!("^{}$", escape(&job.repo))),
                work_dir: work_dir.clone(),
                jobs: 1,
                ..SyncOptions::default()
            },
        );
        match result {
            Ok(()) => crate::logln!(
                "{} {}/{}",
                style("webhook sync done").green().bold(),
                job.group,
                job.repo
            ),
            Err(error) => crate::logln!(
                "{} {}/{}: {error:#}",
                style("webhook sync failed").red().bold(),
                job.group,
                job.repo
            ),
        }
        pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&job);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct WebhookState {
    #[serde(default)]
    installations: BTreeMap<String, WebhookInstallation>,
    #[serde(default)]
    skipped: BTreeMap<String, SkippedWebhookInstallation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WebhookInstallation {
    group: String,
    endpoint: EndpointConfig,
    repo: String,
    url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SkippedWebhookInstallation {
    group: String,
    endpoint: EndpointConfig,
    repo: String,
    url: String,
    reason: String,
}

#[derive(Clone, Debug)]
struct WebhookInstallTask {
    site: crate::config::SiteConfig,
    group: String,
    endpoint: EndpointConfig,
    repo: RemoteRepo,
    url: String,
    secret: String,
    dry_run: bool,
}

#[derive(Clone, Debug)]
struct WebhookUninstallTask {
    group: String,
    site: crate::config::SiteConfig,
    endpoint: EndpointConfig,
    repo: RemoteRepo,
    url: String,
    dry_run: bool,
}

fn run_install_tasks(
    tasks: Vec<WebhookInstallTask>,
    jobs: usize,
    state: Arc<Mutex<WebhookState>>,
    use_state_cache: bool,
) -> Result<()> {
    if tasks.is_empty() {
        return Ok(());
    }
    let worker_count = jobs.min(tasks.len());
    let (task_sender, task_receiver) = mpsc::channel();
    let (result_sender, result_receiver) = mpsc::channel();
    for task in tasks {
        task_sender.send(task)?;
    }
    drop(task_sender);
    let task_receiver = Arc::new(Mutex::new(task_receiver));
    let mut handles = Vec::new();
    for _ in 0..worker_count {
        let task_receiver = Arc::clone(&task_receiver);
        let result_sender = result_sender.clone();
        let state = Arc::clone(&state);
        handles.push(thread::spawn(move || {
            loop {
                let task = {
                    let task_receiver = task_receiver
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    task_receiver.recv()
                };
                let Ok(task) = task else {
                    break;
                };
                if result_sender
                    .send(install_webhook_task(task, &state, use_state_cache))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
    drop(result_sender);

    let mut failures = Vec::new();
    for result in result_receiver {
        if let Err(error) = result {
            crate::logln!("  {} {error:#}", style("fail").red().bold());
            failures.push(error);
        }
    }
    for handle in handles {
        let _ = handle.join();
    }
    if !failures.is_empty() {
        bail!(
            "webhook installation completed with {} failure(s)",
            failures.len()
        );
    }
    Ok(())
}

fn run_uninstall_tasks(tasks: Vec<WebhookUninstallTask>, jobs: usize) -> Result<Vec<String>> {
    if tasks.is_empty() {
        return Ok(Vec::new());
    }
    let worker_count = jobs.min(tasks.len());
    let (task_sender, task_receiver) = mpsc::channel();
    let (result_sender, result_receiver) = mpsc::channel();
    for task in tasks {
        task_sender.send(task)?;
    }
    drop(task_sender);
    let task_receiver = Arc::new(Mutex::new(task_receiver));
    let mut handles = Vec::new();
    for _ in 0..worker_count {
        let task_receiver = Arc::clone(&task_receiver);
        let result_sender = result_sender.clone();
        handles.push(thread::spawn(move || {
            loop {
                let task = {
                    let task_receiver = task_receiver
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    task_receiver.recv()
                };
                let Ok(task) = task else {
                    break;
                };
                if result_sender.send(uninstall_webhook_task(task)).is_err() {
                    break;
                }
            }
        }));
    }
    drop(result_sender);

    let mut removed_keys = Vec::new();
    let mut failures = Vec::new();
    for result in result_receiver {
        match result {
            Ok(Some(key)) => removed_keys.push(key),
            Ok(None) => {}
            Err(error) => {
                crate::logln!("  {} {error:#}", style("fail").red().bold());
                failures.push(error);
            }
        }
    }
    for handle in handles {
        let _ = handle.join();
    }
    if !failures.is_empty() {
        bail!(
            "webhook uninstall completed with {} failure(s)",
            failures.len()
        );
    }
    Ok(removed_keys)
}

fn install_webhook_task(
    task: WebhookInstallTask,
    state: &Arc<Mutex<WebhookState>>,
    use_state_cache: bool,
) -> Result<()> {
    let key = webhook_installation_key(&task.group, &task.endpoint, &task.repo.name);
    if use_state_cache {
        let state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .installations
            .get(&key)
            .is_some_and(|installation| installation.url == task.url)
        {
            return Ok(());
        }
        if state
            .skipped
            .get(&key)
            .is_some_and(|skipped| skipped.url == task.url)
        {
            return Ok(());
        }
    }
    crate::logln!(
        "  {} {} {}",
        style(if task.dry_run {
            "would install"
        } else {
            "install"
        })
        .green()
        .bold(),
        style(&task.repo.name).cyan(),
        style(format!("webhook on {}", task.endpoint.label())).dim()
    );
    if task.dry_run {
        return Ok(());
    }
    let client = ProviderClient::new(&task.site)?;
    if let Err(error) = client.install_webhook(&task.endpoint, &task.repo, &task.url, &task.secret)
    {
        if is_duplicate_webhook_error(&error) {
            crate::logln!(
                "  {} {} {}",
                style("exists").green().bold(),
                style(&task.repo.name).cyan(),
                style(format!("webhook on {}", task.endpoint.label())).dim()
            );
            record_webhook_installation(state, key, task);
            return Ok(());
        }
        if let Some(reason) = non_actionable_webhook_failure_reason(&error) {
            crate::logln!(
                "  {} {} {}",
                style("skip").yellow().bold(),
                style(&task.repo.name).cyan(),
                style(format!("webhook on {}: {reason}", task.endpoint.label())).dim()
            );
            let mut state = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.skipped.insert(
                key,
                SkippedWebhookInstallation {
                    group: task.group,
                    endpoint: task.endpoint,
                    repo: task.repo.name,
                    url: task.url,
                    reason,
                },
            );
            return Ok(());
        }
        return Err(error).with_context(|| {
            format!(
                "failed to install webhook for {} on {}",
                task.repo.name,
                task.endpoint.label()
            )
        });
    }
    record_webhook_installation(state, key, task);
    Ok(())
}

fn record_webhook_installation(
    state: &Arc<Mutex<WebhookState>>,
    key: String,
    task: WebhookInstallTask,
) {
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.skipped.remove(&key);
    state.installations.insert(
        key,
        WebhookInstallation {
            group: task.group,
            endpoint: task.endpoint,
            repo: task.repo.name,
            url: task.url,
        },
    );
}

fn remove_webhook_state_keys(state: &mut WebhookState, keys: Vec<String>, url: &str) {
    for key in keys {
        if state
            .installations
            .get(&key)
            .is_some_and(|installation| installation.url == url)
        {
            state.installations.remove(&key);
        }
        if state
            .skipped
            .get(&key)
            .is_some_and(|skipped| skipped.url == url)
        {
            state.skipped.remove(&key);
        }
    }
}

fn uninstall_webhook_task(task: WebhookUninstallTask) -> Result<Option<String>> {
    let key = webhook_installation_key(&task.group, &task.endpoint, &task.repo.name);
    crate::logln!(
        "  {} {} {}",
        style(if task.dry_run {
            "would uninstall"
        } else {
            "uninstall"
        })
        .red()
        .bold(),
        style(&task.repo.name).cyan(),
        style(format!("from {}", task.endpoint.label())).dim()
    );
    if task.dry_run {
        return Ok(None);
    }
    let client = ProviderClient::new(&task.site)?;
    client
        .uninstall_webhook(&task.endpoint, &task.repo.name, &task.url)
        .with_context(|| {
            format!(
                "failed to uninstall webhook for {} from {}",
                task.repo.name,
                task.endpoint.label()
            )
        })?;
    Ok(Some(key))
}

fn non_actionable_webhook_failure_reason(error: &anyhow::Error) -> Option<String> {
    let text = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    if text.contains("repository access blocked")
        || text.contains("access to this repository has been disabled")
        || text.contains("repository has been disabled")
        || text.contains("disabled by github staff")
        || text.contains("github.com/tos")
    {
        return Some("provider blocked access".to_string());
    }
    if text.contains("repository was archived")
        || text.contains("archived so is read-only")
        || text.contains("repository is archived")
    {
        return Some("repository is archived/read-only".to_string());
    }
    None
}

fn is_duplicate_webhook_error(error: &anyhow::Error) -> bool {
    let text = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    text.contains("422 unprocessable entity") && text.contains("hook already exists")
}

fn webhook_installation_key(group: &str, endpoint: &EndpointConfig, repo: &str) -> String {
    format!(
        "{}\t{}\t{:?}\t{}\t{}",
        group, endpoint.site, endpoint.kind, endpoint.namespace, repo
    )
}

fn load_webhook_state(work_dir: &Path) -> Result<WebhookState> {
    load_toml_or_default(&webhook_state_path(work_dir))
}

fn save_webhook_state(work_dir: &Path, state: &WebhookState) -> Result<()> {
    save_toml(&webhook_state_path(work_dir), state)
}

fn webhook_state_path(work_dir: &Path) -> PathBuf {
    work_dir.join(WEBHOOK_STATE_FILE)
}

fn handle_request(
    mut request: Request,
    config: &Config,
    secret: &str,
    queue: &JobQueue,
) -> Result<()> {
    if request.method() != &Method::Post {
        respond(request, StatusCode(405), "method not allowed")?;
        return Ok(());
    }
    let path = request.url().split('?').next().unwrap_or(request.url());
    if path != "/" && path != "/webhook" {
        respond(request, StatusCode(404), "not found")?;
        return Ok(());
    }

    let headers = headers_map(request.headers());
    let mut body = Vec::new();
    request
        .as_reader()
        .read_to_end(&mut body)
        .context("failed to read webhook request body")?;
    let provider = detect_provider(&headers);
    if !verify_signature(provider.as_ref(), &headers, &body, secret) {
        respond(request, StatusCode(401), "invalid signature")?;
        return Ok(());
    }

    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            respond(request, StatusCode(400), "invalid JSON")?;
            return Ok(());
        }
    };
    let Some(event) = parse_event(provider, &headers, &value) else {
        respond(request, StatusCode(202), "ignored")?;
        return Ok(());
    };
    let jobs = matching_jobs(config, &event);
    if jobs.is_empty() {
        respond(request, StatusCode(202), "no matching mirror group")?;
        return Ok(());
    }
    let mut enqueued = 0;
    for job in jobs {
        if enqueue(queue, job)? {
            enqueued += 1;
        }
    }
    respond(request, StatusCode(202), &format!("queued {enqueued}"))?;
    Ok(())
}

fn enqueue(queue: &JobQueue, job: WebhookJob) -> Result<bool> {
    let mut pending = queue
        .pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !pending.insert(job.clone()) {
        return Ok(false);
    }
    if queue.sender.send(job.clone()).is_err() {
        pending.remove(&job);
        bail!("webhook worker queue is closed");
    }
    Ok(true)
}

fn respond(request: Request, status: StatusCode, body: &str) -> Result<()> {
    request
        .respond(Response::from_string(body.to_string()).with_status_code(status))
        .map_err(|error| anyhow::anyhow!("failed to send webhook response: {error}"))
}

fn headers_map(headers: &[Header]) -> HashMap<String, String> {
    headers
        .iter()
        .map(|header| {
            (
                header.field.to_string().to_ascii_lowercase(),
                header.value.as_str().to_string(),
            )
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WebhookEvent {
    provider: Option<ProviderKind>,
    repo: String,
    namespace: Option<String>,
}

fn detect_provider(headers: &HashMap<String, String>) -> Option<ProviderKind> {
    if headers.contains_key("x-forgejo-event") {
        Some(ProviderKind::Forgejo)
    } else if headers.contains_key("x-gitea-event") {
        Some(ProviderKind::Gitea)
    } else if headers.contains_key("x-gitlab-event") {
        Some(ProviderKind::Gitlab)
    } else if headers.contains_key("x-github-event") {
        Some(ProviderKind::Github)
    } else {
        None
    }
}

fn parse_event(
    provider: Option<ProviderKind>,
    headers: &HashMap<String, String>,
    value: &Value,
) -> Option<WebhookEvent> {
    if !is_push_event(headers) {
        return None;
    }
    match provider {
        Some(ProviderKind::Gitlab) => parse_gitlab_event(provider, value),
        Some(ProviderKind::Github)
        | Some(ProviderKind::Gitea)
        | Some(ProviderKind::Forgejo)
        | None => parse_github_like_event(provider, value),
    }
}

fn is_push_event(headers: &HashMap<String, String>) -> bool {
    let github = headers
        .get("x-github-event")
        .is_some_and(|event| event == "push");
    let gitea = headers
        .get("x-gitea-event")
        .is_some_and(|event| event == "push");
    let forgejo = headers
        .get("x-forgejo-event")
        .is_some_and(|event| event == "push");
    let gitlab = headers
        .get("x-gitlab-event")
        .is_some_and(|event| event == "Push Hook" || event == "Tag Push Hook");
    github || gitea || forgejo || gitlab
}

fn parse_github_like_event(provider: Option<ProviderKind>, value: &Value) -> Option<WebhookEvent> {
    let repo = value.pointer("/repository/name")?.as_str()?.to_string();
    let namespace = value
        .pointer("/repository/owner/login")
        .or_else(|| value.pointer("/repository/owner/username"))
        .or_else(|| value.pointer("/repository/owner/name"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            value
                .pointer("/repository/full_name")
                .and_then(Value::as_str)
                .and_then(|full_name| {
                    full_name
                        .rsplit_once('/')
                        .map(|(owner, _)| owner.to_string())
                })
        });
    Some(WebhookEvent {
        provider,
        repo,
        namespace,
    })
}

fn parse_gitlab_event(provider: Option<ProviderKind>, value: &Value) -> Option<WebhookEvent> {
    let path = value.pointer("/project/path")?.as_str()?.to_string();
    let namespace = value
        .pointer("/project/path_with_namespace")
        .and_then(Value::as_str)
        .and_then(|path| {
            path.rsplit_once('/')
                .map(|(namespace, _)| namespace.to_string())
        })
        .or_else(|| {
            value
                .pointer("/project/namespace")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
    Some(WebhookEvent {
        provider,
        repo: path,
        namespace,
    })
}

fn matching_jobs(config: &Config, event: &WebhookEvent) -> Vec<WebhookJob> {
    config
        .mirrors
        .iter()
        .filter(|mirror| {
            mirror
                .repo_filter()
                .is_ok_and(|filter| filter.matches(&event.repo))
                && mirror.endpoints.iter().any(|endpoint| {
                    let Some(site) = config.site(&endpoint.site) else {
                        return false;
                    };
                    event
                        .provider
                        .as_ref()
                        .is_none_or(|provider| &site.provider == provider)
                        && event
                            .namespace
                            .as_ref()
                            .is_none_or(|namespace| namespace == &endpoint.namespace)
                })
        })
        .map(|mirror| WebhookJob {
            group: mirror.name.clone(),
            repo: event.repo.clone(),
        })
        .collect()
}

fn verify_signature(
    provider: Option<&ProviderKind>,
    headers: &HashMap<String, String>,
    body: &[u8],
    secret: &str,
) -> bool {
    match provider {
        Some(ProviderKind::Gitlab) => headers
            .get("x-gitlab-token")
            .is_some_and(|token| fixed_time_eq(token.as_bytes(), secret.as_bytes())),
        Some(ProviderKind::Github) => {
            verify_hmac_header(headers, "x-hub-signature-256", body, secret)
        }
        Some(ProviderKind::Gitea) | Some(ProviderKind::Forgejo) => {
            verify_hmac_header(headers, "x-gitea-signature", body, secret)
                || verify_hmac_header(headers, "x-forgejo-signature", body, secret)
                || verify_hmac_header(headers, "x-hub-signature-256", body, secret)
        }
        None => false,
    }
}

fn verify_hmac_header(
    headers: &HashMap<String, String>,
    header: &str,
    body: &[u8],
    secret: &str,
) -> bool {
    let Some(signature) = headers.get(header) else {
        return false;
    };
    let expected = hmac_sha256_hex(secret.as_bytes(), body);
    let signature = signature
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or_else(|| signature.trim());
    fixed_time_eq(signature.as_bytes(), expected.as_bytes())
}

fn hmac_sha256_hex(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn fixed_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (left, right) in left.iter().zip(right) {
        diff |= left ^ right;
    }
    diff == 0
}

#[cfg(test)]
#[path = "../tests/unit/webhook.rs"]
mod tests;
