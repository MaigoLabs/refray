use super::*;

#[test]
fn repo_log_context_is_inherited_by_parallel_workers() {
    let _guard = start_repo_log();

    crate::logln!("outer line");
    crate::parallel::map(vec!["worker line"], 1, |line| {
        crate::logln!("{line}");
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    let lines = {
        let context = current_repo_log_context().unwrap();
        context
            .inner
            .lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    };
    finish_repo_log();

    assert_eq!(lines, vec!["outer line", "worker line"]);
}
