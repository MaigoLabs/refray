use console::style;

use crate::git::{BranchDecision, BranchDeletion, TagDecision};

use super::state::SyncFailure;

pub(super) fn print_failure(scope: &str, error: &anyhow::Error) {
    crate::logln!(
        "  {} {} {}",
        style("fail").red().bold(),
        style(scope).cyan(),
        style(error_headline(error)).dim()
    );
}

pub(super) fn print_failure_summary(failures: &[SyncFailure]) {
    crate::logln!();
    crate::logln!(
        "{} {}",
        style("Failures").red().bold(),
        style(format!("({})", failures.len())).dim()
    );
    for (index, failure) in failures.iter().enumerate() {
        crate::logln!("  {}. {}", index + 1, style(&failure.scope).cyan().bold());
        for line in failure.error.lines() {
            crate::logln!("     {line}");
        }
    }
}

fn error_headline(error: &anyhow::Error) -> String {
    format_error(error)
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("unknown error")
        .to_string()
}

pub(super) fn format_error(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

pub(super) fn print_branch_decisions(branches: &[BranchDecision]) {
    crate::logln!(
        "  {} {}",
        style("branches").cyan().bold(),
        style(format!("({})", branches.len())).dim()
    );
    for branch in branches {
        crate::logln!(
            "    {} {} {}",
            style(&branch.branch).cyan(),
            style(format!("@{}", short_sha(&branch.sha))).dim(),
            style(format!(
                "{} -> {}",
                branch.source_remotes.join("+"),
                branch.target_remotes.join("+")
            ))
            .dim()
        );
    }
}

pub(super) fn print_branch_deletions(deletions: &[BranchDeletion]) {
    crate::logln!(
        "  {} {}",
        style("deleted branches").red().bold(),
        style(format!("({})", deletions.len())).dim()
    );
    for deletion in deletions {
        crate::logln!(
            "    {} {}",
            style(&deletion.branch).cyan(),
            style(format!(
                "deleted on {} -> {}",
                deletion.deleted_remotes.join("+"),
                deletion.target_remotes.join("+")
            ))
            .dim()
        );
    }
}

pub(super) fn print_tag_decisions(tags: &[TagDecision]) {
    crate::logln!(
        "  {} {}",
        style("tags").cyan().bold(),
        style(format!("({})", tags.len())).dim()
    );
    for tag in tags {
        crate::logln!(
            "    {} {} {}",
            style(&tag.tag).cyan(),
            style(format!("@{}", short_sha(&tag.sha))).dim(),
            style(format!(
                "{} -> {}",
                tag.source_remotes.join("+"),
                tag.target_remotes.join("+")
            ))
            .dim()
        );
    }
}

pub(super) fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}
