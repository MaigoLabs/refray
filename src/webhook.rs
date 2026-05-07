use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use console::style;
use hmac::{Hmac, Mac};
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
    pub group: Option<String>,
    pub repo_pattern: Option<String>,
    pub dry_run: bool,
    pub work_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct WebhookUninstallOptions {
    pub group: Option<String>,
    pub dry_run: bool,
    pub work_dir: Option<PathBuf>,
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
        bail!("--jobs must be at least 1");
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
    let work_dir = options.work_dir.clone().unwrap_or_else(default_work_dir);
    let mut state = load_webhook_state(&work_dir)?;
    let repo_pattern = options
        .repo_pattern
        .as_deref()
        .map(regex::Regex::new)
        .transpose()
        .with_context(|| "invalid --repo-pattern regex")?;

    for mirror in &config.mirrors {
        if options
            .group
            .as_ref()
            .is_some_and(|group| group != &mirror.name)
        {
            continue;
        }
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
                if repo_pattern
                    .as_ref()
                    .is_some_and(|pattern| !pattern.is_match(&repo.name))
                {
                    continue;
                }
                install_repo_webhook(
                    &WebhookInstallRequest {
                        client: &client,
                        group: &mirror.name,
                        endpoint,
                        repo: &repo,
                        url: &options.url,
                        secret: &options.secret,
                        dry_run: options.dry_run,
                    },
                    &mut state,
                )?;
            }
        }
    }
    if !options.dry_run {
        save_webhook_state(&work_dir, &state)?;
    }
    Ok(())
}

pub fn uninstall_webhooks(config: &Config, options: WebhookUninstallOptions) -> Result<()> {
    validate_config(config)?;
    let work_dir = options.work_dir.clone().unwrap_or_else(default_work_dir);
    let mut state = load_webhook_state(&work_dir)?;
    if state.installations.is_empty() {
        crate::logln!(
            "{} no webhook installations recorded",
            style("skip").yellow().bold()
        );
        return Ok(());
    }

    let mut removed_keys = Vec::new();
    for (key, installation) in &state.installations {
        if options
            .group
            .as_ref()
            .is_some_and(|group| group != &installation.group)
        {
            continue;
        }
        crate::logln!(
            "  {} {} {}",
            style(if options.dry_run {
                "would uninstall"
            } else {
                "uninstall"
            })
            .red()
            .bold(),
            style(&installation.repo).cyan(),
            style(format!("from {}", installation.endpoint.label())).dim()
        );
        if options.dry_run {
            continue;
        }
        let Some(site) = config.site(&installation.endpoint.site) else {
            crate::logln!(
                "  {} {} {}",
                style("skip").yellow().bold(),
                style(&installation.repo).cyan(),
                style(format!("unknown site {}", installation.endpoint.site)).dim()
            );
            continue;
        };
        let client = ProviderClient::new(site)?;
        client
            .uninstall_webhook(
                &installation.endpoint,
                &installation.repo,
                &installation.url,
            )
            .with_context(|| {
                format!(
                    "failed to uninstall webhook for {} from {}",
                    installation.repo,
                    installation.endpoint.label()
                )
            })?;
        removed_keys.push(key.clone());
    }

    if !options.dry_run {
        for key in removed_keys {
            state.installations.remove(&key);
        }
        save_webhook_state(&work_dir, &state)?;
    }
    Ok(())
}

pub fn ensure_configured_webhooks(
    config: &Config,
    mirror: &MirrorConfig,
    repos: &[EndpointRepo],
    work_dir: &Path,
) -> Result<()> {
    let Some(webhook) = &config.webhook else {
        return Ok(());
    };
    if !webhook.install {
        return Ok(());
    }
    let secret = webhook.secret()?;
    let mut state = load_webhook_state(work_dir)?;
    for endpoint_repo in repos {
        let Some(site) = config.site(&endpoint_repo.endpoint.site) else {
            continue;
        };
        let client = ProviderClient::new(site)?;
        install_repo_webhook(
            &WebhookInstallRequest {
                client: &client,
                group: &mirror.name,
                endpoint: &endpoint_repo.endpoint,
                repo: &endpoint_repo.repo,
                url: &webhook.url,
                secret: &secret,
                dry_run: false,
            },
            &mut state,
        )?;
    }
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WebhookInstallation {
    group: String,
    endpoint: EndpointConfig,
    repo: String,
    url: String,
}

struct WebhookInstallRequest<'a> {
    client: &'a ProviderClient<'a>,
    group: &'a str,
    endpoint: &'a EndpointConfig,
    repo: &'a RemoteRepo,
    url: &'a str,
    secret: &'a str,
    dry_run: bool,
}

fn install_repo_webhook(
    request: &WebhookInstallRequest<'_>,
    state: &mut WebhookState,
) -> Result<()> {
    let key = webhook_installation_key(request.group, request.endpoint, &request.repo.name);
    if state
        .installations
        .get(&key)
        .is_some_and(|installation| installation.url == request.url)
    {
        return Ok(());
    }
    crate::logln!(
        "  {} {} {}",
        style(if request.dry_run {
            "would install"
        } else {
            "install"
        })
        .green()
        .bold(),
        style(&request.repo.name).cyan(),
        style(format!("webhook on {}", request.endpoint.label())).dim()
    );
    if request.dry_run {
        return Ok(());
    }
    request
        .client
        .install_webhook(request.endpoint, request.repo, request.url, request.secret)
        .with_context(|| {
            format!(
                "failed to install webhook for {} on {}",
                request.repo.name,
                request.endpoint.label()
            )
        })?;
    state.installations.insert(
        key,
        WebhookInstallation {
            group: request.group.to_string(),
            endpoint: request.endpoint.clone(),
            repo: request.repo.name.clone(),
            url: request.url.to_string(),
        },
    );
    Ok(())
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
            mirror.endpoints.iter().any(|endpoint| {
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
