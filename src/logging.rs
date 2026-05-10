use std::cell::RefCell;
use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

static OUTPUT: OnceLock<Mutex<()>> = OnceLock::new();

thread_local! {
    static REPO_LOG: RefCell<Option<ActiveRepoLog>> = const { RefCell::new(None) };
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
    lines: Mutex<Vec<String>>,
}

pub struct RepoLogGuard;

impl Drop for RepoLogGuard {
    fn drop(&mut self) {
        finish_repo_log();
    }
}

pub fn start_repo_log() -> RepoLogGuard {
    let context = RepoLogContext {
        inner: Arc::new(RepoLog {
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

    with_output(|| {
        for line in lines {
            println!("{line}");
        }
    });
}

pub fn line(args: fmt::Arguments<'_>) {
    let text = args.to_string();
    let context = current_repo_log_context();
    if let Some(context) = context {
        capture_repo_line(&context, &text);
        return;
    }

    with_output(|| {
        println!("{text}");
    });
}

fn capture_repo_line(context: &RepoLogContext, text: &str) {
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
    }
}

fn with_output(action: impl FnOnce()) {
    let output = OUTPUT.get_or_init(|| Mutex::new(()));
    let _output = output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    action();
    let _ = io::stdout().flush();
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
