use std::cell::RefCell;
use std::fmt;
use std::sync::{Mutex, OnceLock};

use console::style;

static OUTPUT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

thread_local! {
    static PREFIX: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub struct PrefixGuard {
    previous: Option<String>,
}

impl Drop for PrefixGuard {
    fn drop(&mut self) {
        PREFIX.with(|prefix| {
            *prefix.borrow_mut() = self.previous.take();
        });
    }
}

pub fn set_prefix(prefix: String) -> PrefixGuard {
    let previous = PREFIX.with(|current| current.borrow_mut().replace(prefix));
    PrefixGuard { previous }
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
    let prefix = PREFIX.with(|prefix| prefix.borrow().clone());
    let lock = OUTPUT_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    match prefix {
        Some(prefix) if !text.is_empty() => {
            for line in text.lines() {
                println!("{} | {}", style(&prefix).cyan().bold(), line);
            }
        }
        _ => {
            println!("{text}");
        }
    }
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
mod tests {
    use super::*;

    #[test]
    fn repo_prefix_pads_and_truncates_to_fixed_width() {
        assert_eq!(repo_prefix("api", 6), "api   ");
        assert_eq!(repo_prefix("very-long-repo", 8), "very-lo~");
    }
}
