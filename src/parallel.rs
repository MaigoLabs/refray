use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use anyhow::{Context, Result, bail};

pub fn map<I, O, F>(items: Vec<I>, jobs: usize, f: F) -> Result<Vec<O>>
where
    I: Send,
    O: Send,
    F: Fn(I) -> Result<O> + Sync,
{
    if jobs == 0 {
        bail!("jobs must be at least 1");
    }
    if items.is_empty() {
        return Ok(Vec::new());
    }

    let worker_count = jobs.min(items.len());
    let queue = Arc::new(Mutex::new(VecDeque::from(items)));
    let (sender, receiver) = mpsc::channel();
    let repo_log_context = crate::logging::current_repo_log_context();

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let queue = Arc::clone(&queue);
            let sender = sender.clone();
            let f = &f;
            let repo_log_context = repo_log_context.clone();
            scope.spawn(move || {
                let _repo_log_guard = crate::logging::inherit_repo_log(repo_log_context);
                while let Some(item) = pop_item(&queue) {
                    if sender.send(f(item)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);

        collect_results(receiver)
    })
}

fn pop_item<I>(queue: &Arc<Mutex<VecDeque<I>>>) -> Option<I> {
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .pop_front()
}

fn collect_results<O>(receiver: mpsc::Receiver<Result<O>>) -> Result<Vec<O>> {
    let mut outputs = Vec::new();
    let mut first_failure = None;
    let mut failure_count = 0;

    for result in receiver {
        match result {
            Ok(output) => outputs.push(output),
            Err(error) => {
                failure_count += 1;
                first_failure.get_or_insert(error);
            }
        }
    }

    match (failure_count, first_failure) {
        (0, None) => Ok(outputs),
        (1, Some(error)) => Err(error),
        (_, Some(error)) => {
            Err(error).with_context(|| format!("{failure_count} parallel tasks failed"))
        }
        _ => unreachable!(),
    }
}
