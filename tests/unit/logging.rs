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
