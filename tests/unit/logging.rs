use super::*;

#[test]
fn repo_prefix_pads_and_truncates_to_fixed_width() {
    assert_eq!(repo_prefix("api", 6), "api   ");
    assert_eq!(repo_prefix("very-long-repo", 8), "very-lo~");
}

#[test]
fn status_text_truncates_to_fixed_width() {
    assert_eq!(truncate_status("short", 8), "short");
    assert_eq!(truncate_status("very-long-status", 8), "very-lo~");
}

#[test]
fn repo_log_context_is_inherited_by_parallel_workers() {
    let _guard = start_repo_log("repo-a".to_string(), 0, 8);

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
