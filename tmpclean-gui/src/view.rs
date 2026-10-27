//! Pure label text and state helpers for the window (tested on Linux).

use std::time::Duration;

use tmpclean_core::report::{format_lines, Counters, Outcome, ResolveStatus, Snapshot};
use tmpclean_core::strings;

/// Visible countdown before an unconfirmed run starts.
pub const COUNTDOWN: Duration = Duration::from_secs(5);
/// The single UI timer period; it only pulls the latest snapshot.
pub const TICK_MS: u32 = 250;

/// One requested target as shown in the targets label.
pub struct TargetLine {
    pub name: &'static str,
    pub status: ResolveStatus,
    /// Final path when resolved, refusal reason when refused, else empty.
    pub detail: String,
}

pub fn empty_snapshot() -> Snapshot {
    Snapshot {
        counters: Counters::default(),
        errors_recorded: 0,
        errors_truncated: false,
        sequence: 0,
    }
}

/// Whole seconds left, rounded up, so the label never shows 0 before start.
pub fn countdown_text(remaining: Duration) -> String {
    let secs = remaining.as_millis().div_ceil(1000);
    format!(
        "{}{secs}{}",
        strings::GUI_COUNTDOWN_PREFIX,
        strings::GUI_COUNTDOWN_SUFFIX
    )
}

pub fn targets_text(lines: &[TargetLine]) -> String {
    let mut out = format!("{}:", strings::GUI_TARGETS);
    for l in lines {
        out.push_str("\r\n  ");
        out.push_str(l.name);
        out.push_str(": ");
        out.push_str(l.status.as_str());
        if !l.detail.is_empty() {
            out.push_str(" - ");
            out.push_str(&l.detail);
        }
    }
    out
}

/// Counter label from `report::format_lines`; dry-run totals are would-remove.
pub fn counters_text(snapshot: &Snapshot, dry_run: bool) -> String {
    let mut lines = Vec::with_capacity(11);
    if dry_run {
        lines.push(strings::GUI_DRY_RUN_NOTE.to_owned());
    }
    lines.extend(format_lines(snapshot));
    lines.join("\r\n")
}

pub fn running_text(dry_run: bool) -> &'static str {
    if dry_run {
        strings::GUI_RUNNING_DRY
    } else {
        strings::GUI_RUNNING
    }
}

pub fn done_text(outcome: &str, exit_code: u8) -> String {
    format!(
        "{}: {outcome} ({} {exit_code})",
        strings::GUI_DONE_PREFIX,
        strings::GUI_EXIT_CODE
    )
}

/// One status for the exit-code table across independent targets: any
/// refusal or elevation requirement wins (exit 3), else any resolved root
/// makes it a normal run; all-absent (or none) is `Absent` (exit 0).
pub fn combined_status(statuses: impl IntoIterator<Item = ResolveStatus>) -> ResolveStatus {
    let mut combined = ResolveStatus::Absent;
    for s in statuses {
        combined = match (combined, s) {
            (ResolveStatus::Refused, _) | (_, ResolveStatus::Refused) => ResolveStatus::Refused,
            (ResolveStatus::RequiresElevation, _) | (_, ResolveStatus::RequiresElevation) => {
                ResolveStatus::RequiresElevation
            }
            (ResolveStatus::UnknownTarget, _) | (_, ResolveStatus::UnknownTarget) => {
                ResolveStatus::UnknownTarget
            }
            (ResolveStatus::Resolved, _) | (_, ResolveStatus::Resolved) => ResolveStatus::Resolved,
            _ => ResolveStatus::Absent,
        };
    }
    combined
}

/// Outcome label used in the done status line.
pub fn outcome_text(outcome: Outcome) -> &'static str {
    outcome.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmpclean_core::report::exit_code;

    #[test]
    fn gui_countdown_text_rounds_up_and_counts_down() {
        assert_eq!(
            countdown_text(COUNTDOWN),
            "Starting in 5 s - Enter: start now, Esc: cancel"
        );
        assert!(countdown_text(Duration::from_millis(4001)).starts_with("Starting in 5 s"));
        assert!(countdown_text(Duration::from_millis(4000)).starts_with("Starting in 4 s"));
        assert!(countdown_text(Duration::from_millis(1)).starts_with("Starting in 1 s"));
    }

    #[test]
    fn gui_counters_text_uses_format_lines() {
        let mut s = empty_snapshot();
        s.counters.files_removed = 200;
        s.counters.logical_bytes_removed = 4096;
        let text = counters_text(&s, false);
        assert_eq!(text, format_lines(&s).join("\r\n"));
        assert!(text.starts_with("Files removed: 200\r\n"));
        assert!(text.ends_with("Logical bytes removed: 4096"));
        let dry = counters_text(&s, true);
        assert!(dry.starts_with(strings::GUI_DRY_RUN_NOTE));
        assert_eq!(dry.lines().count(), 11);
    }

    #[test]
    fn gui_targets_text_shows_status_and_detail() {
        let text = targets_text(&[
            TargetLine {
                name: "user-temp",
                status: ResolveStatus::Resolved,
                detail: r"C:\Users\x\AppData\Local\Temp".into(),
            },
            TargetLine {
                name: "inet-cache",
                status: ResolveStatus::Refused,
                detail: "reparse-root".into(),
            },
            TargetLine {
                name: "windows-temp",
                status: ResolveStatus::Absent,
                detail: String::new(),
            },
        ]);
        assert_eq!(
            text,
            "Targets:\r\n  user-temp: resolved - C:\\Users\\x\\AppData\\Local\\Temp\
             \r\n  inet-cache: refused - reparse-root\r\n  windows-temp: absent"
        );
    }

    #[test]
    fn gui_combined_status_drives_exit_code() {
        use ResolveStatus::*;
        let done = Outcome::Completed { fully_empty: true };
        let c = Counters::default();
        for (statuses, status, code) in [
            (vec![], Absent, 0),
            (vec![Absent, Absent], Absent, 0),
            (vec![Absent, Resolved], Resolved, 0),
            (vec![Resolved, RequiresElevation], RequiresElevation, 3),
            (vec![RequiresElevation, Refused, Resolved], Refused, 3),
        ] {
            let combined = combined_status(statuses);
            assert_eq!(combined, status);
            assert_eq!(exit_code(done, &c, combined, false), code);
        }
        assert_eq!(
            exit_code(Outcome::Canceled, &c, Resolved, false),
            3,
            "Cancel during the run exits 3"
        );
    }

    #[test]
    fn gui_done_text() {
        assert_eq!(
            done_text(outcome_text(Outcome::Completed { fully_empty: true }), 0),
            "Done: completed (exit code 0)"
        );
        assert_eq!(running_text(true), strings::GUI_RUNNING_DRY);
    }
}
