use std::cell::RefCell;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex, OnceLock};

use console::style;

static OUTPUT: OnceLock<Mutex<OutputState>> = OnceLock::new();

thread_local! {
    static REPO_LOG: RefCell<Option<ActiveRepoLog>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct OutputState {
    status: Option<StatusState>,
}

struct StatusState {
    slots: Vec<Option<String>>,
    visible: bool,
    interactive: bool,
}

#[derive(Clone)]
pub(crate) struct RepoLogContext {
    inner: Arc<RepoLog>,
}

struct ActiveRepoLog {
    context: RepoLogContext,
    owner: bool,
}

struct RepoLog {
    repo_name: String,
    slot: usize,
    width: usize,
    lines: Mutex<Vec<String>>,
}

pub struct StatusGuard;

impl Drop for StatusGuard {
    fn drop(&mut self) {
        finish_status_area();
    }
}

pub struct RepoLogGuard;

impl Drop for RepoLogGuard {
    fn drop(&mut self) {
        finish_repo_log();
    }
}

pub fn start_status_area(slots: usize) -> StatusGuard {
    with_output(|output| {
        if let Some(status) = output.status.as_mut() {
            clear_status(status);
        }
        output.status = Some(StatusState {
            slots: vec![None; slots],
            visible: false,
            interactive: io::stdout().is_terminal() && slots > 0,
        });
        if let Some(status) = output.status.as_mut() {
            draw_status(status);
        }
    });
    StatusGuard
}

pub fn start_repo_log(repo_name: String, slot: usize, width: usize) -> RepoLogGuard {
    let context = RepoLogContext {
        inner: Arc::new(RepoLog {
            repo_name,
            slot,
            width,
            lines: Mutex::new(Vec::new()),
        }),
    };
    REPO_LOG.with(|repo_log| {
        *repo_log.borrow_mut() = Some(ActiveRepoLog {
            context,
            owner: true,
        });
    });
    RepoLogGuard
}

pub(crate) fn current_repo_log_context() -> Option<RepoLogContext> {
    REPO_LOG.with(|repo_log| {
        repo_log
            .borrow()
            .as_ref()
            .map(|repo_log| repo_log.context.clone())
    })
}

pub(crate) fn inherit_repo_log(context: Option<RepoLogContext>) -> InheritedRepoLogGuard {
    let previous = REPO_LOG.with(|repo_log| {
        let mut repo_log = repo_log.borrow_mut();
        let previous = repo_log.take();
        if let Some(context) = context {
            *repo_log = Some(ActiveRepoLog {
                context,
                owner: false,
            });
        }
        previous
    });
    InheritedRepoLogGuard { previous }
}

pub(crate) struct InheritedRepoLogGuard {
    previous: Option<ActiveRepoLog>,
}

impl Drop for InheritedRepoLogGuard {
    fn drop(&mut self) {
        REPO_LOG.with(|repo_log| {
            *repo_log.borrow_mut() = self.previous.take();
        });
    }
}

pub fn finish_repo_log() {
    let active = REPO_LOG.with(|repo_log| repo_log.borrow_mut().take());
    let Some(active) = active else {
        return;
    };
    if !active.owner {
        return;
    }

    let repo_log = active.context.inner;
    let lines = {
        let mut lines = repo_log
            .lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *lines)
    };

    with_output(|output| {
        if let Some(status) = output.status.as_mut() {
            clear_status(status);
            if repo_log.slot < status.slots.len() {
                status.slots[repo_log.slot] = None;
            }
        }
        for line in lines {
            println!("{line}");
        }
        if let Some(status) = output.status.as_mut() {
            draw_status(status);
        }
    });
}

pub fn repo_prefix(repo_name: &str, width: usize) -> String {
    let mut prefix = repo_name.chars().take(width).collect::<String>();
    if repo_name.chars().count() > width && width > 0 {
        prefix.pop();
        prefix.push('~');
    }
    format!("{prefix:<width$}")
}

pub fn line(args: fmt::Arguments<'_>) {
    let text = args.to_string();
    let context = current_repo_log_context();
    if let Some(context) = context {
        capture_repo_line(&context, &text);
        return;
    }

    with_output(|output| {
        if let Some(status) = output.status.as_mut() {
            clear_status(status);
        }
        println!("{text}");
        if let Some(status) = output.status.as_mut() {
            draw_status(status);
        }
    });
}

fn capture_repo_line(context: &RepoLogContext, text: &str) {
    let mut status_updates = Vec::new();
    {
        let mut lines = context
            .inner
            .lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if text.is_empty() {
            lines.push(String::new());
            return;
        }
        for line in text.lines() {
            lines.push(line.to_string());
            if !line.trim().is_empty() {
                status_updates.push(line.trim().to_string());
            }
        }
    }
    for line in status_updates {
        update_status(context, &line);
    }
}

fn update_status(context: &RepoLogContext, line: &str) {
    let repo_log = &context.inner;
    let repo = repo_prefix(&repo_log.repo_name, repo_log.width);
    let line = truncate_status(line, 96);
    with_output(|output| {
        let Some(status) = output.status.as_mut() else {
            return;
        };
        if repo_log.slot >= status.slots.len() {
            return;
        }
        clear_status(status);
        status.slots[repo_log.slot] = Some(format!(
            "{} {} {}",
            style(format!("worker {}", repo_log.slot + 1)).dim(),
            style(repo).cyan().bold(),
            line
        ));
        draw_status(status);
    });
}

fn truncate_status(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut output = value.chars().take(max_chars).collect::<String>();
    output.pop();
    output.push('~');
    output
}

fn finish_status_area() {
    with_output(|output| {
        if let Some(status) = output.status.as_mut() {
            clear_status(status);
        }
        output.status = None;
    });
}

fn with_output(action: impl FnOnce(&mut OutputState)) {
    let output = OUTPUT.get_or_init(|| Mutex::new(OutputState::default()));
    let mut output = output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    action(&mut output);
    let _ = io::stdout().flush();
}

fn clear_status(status: &mut StatusState) {
    if !status.interactive || !status.visible {
        return;
    }

    let lines = status.slots.len();
    print!("\x1b[{lines}A\r");
    for _ in 0..lines {
        println!("\x1b[2K");
    }
    print!("\x1b[{lines}A\r");
    status.visible = false;
}

fn draw_status(status: &mut StatusState) {
    if !status.interactive {
        return;
    }

    for slot in &status.slots {
        match slot {
            Some(line) => println!("{line}"),
            None => println!("{}", style("idle").dim()),
        }
    }
    status.visible = true;
}

#[macro_export]
macro_rules! logln {
    () => {
        $crate::logging::line(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::logging::line(format_args!($($arg)*))
    };
}

#[cfg(test)]
#[path = "../tests/unit/logging.rs"]
mod tests;
