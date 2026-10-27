//! Pure Running-state tick transition (safe, tested on Linux).
//!
//! The worker publishes snapshots into a `latest` slot and, once, its
//! [`Completion`] into a `finished` slot. The UI timer reads `latest` first and
//! `finished` second, so a final publication can land between the two reads.
//! The completion therefore carries the final snapshot, and the Done view is
//! rendered from it rather than from whatever `latest` happened to hold.

use tmpclean_core::policy::Refusal;
use tmpclean_core::report::{exit_code, Outcome, ResolveStatus, Snapshot};

use crate::view;

/// Published once by the worker after its last snapshot: the run outcome with
/// the final (drained) snapshot, or the refusal that prevented the run.
pub type Completion = Result<(Outcome, Snapshot), Refusal>;

/// Labels to apply when the run is over.
#[derive(Debug, PartialEq, Eq)]
pub struct DoneView {
    pub status: String,
    pub code: u8,
}

/// What one timer tick in the Running state renders.
#[derive(Debug, PartialEq, Eq)]
pub struct TickView {
    /// New counters label, applied before anything in `done`.
    pub counters: Option<String>,
    /// Set when the run finished; the window then enables Close and sets the
    /// done title.
    pub done: Option<DoneView>,
}

/// One Running-state tick from the values read from the two slots, in the
/// order the UI reads them (`latest`, then `finished`).
pub fn running_tick(
    latest: Option<Snapshot>,
    finished: Option<Completion>,
    resolve_status: ResolveStatus,
    dry_run: bool,
) -> TickView {
    let latest_text = latest.map(|s| view::counters_text(&s, dry_run));
    match finished {
        None => TickView {
            counters: latest_text,
            done: None,
        },
        // `latest` may predate the final publication: render the final
        // snapshot carried by the completion instead.
        Some(Ok((outcome, final_snapshot))) => {
            let code = exit_code(outcome, &final_snapshot.counters, resolve_status, dry_run);
            TickView {
                counters: Some(view::counters_text(&final_snapshot, dry_run)),
                done: Some(DoneView {
                    status: view::done_text(view::outcome_text(outcome), code),
                    code,
                }),
            }
        }
        Some(Err(refusal)) => {
            let code = refusal.exit_code();
            TickView {
                counters: latest_text,
                done: Some(DoneView {
                    status: view::done_text(ResolveStatus::Refused.as_str(), code),
                    code,
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(files_removed: u64, canceled: u64, sequence: u64) -> Snapshot {
        let mut s = view::empty_snapshot();
        s.counters.files_removed = files_removed;
        s.counters.canceled = canceled;
        s.sequence = sequence;
        s
    }

    fn files_removed_line(view: &TickView) -> &str {
        view.counters
            .as_deref()
            .and_then(|t| t.lines().find(|l| l.starts_with("Files removed:")))
            .unwrap_or("<no counters rendered>")
    }

    const DONE: Outcome = Outcome::Completed { fully_empty: true };

    #[test]
    fn gui_race_final_published_between_reads_shows_final_counters() {
        // Tick reads `latest` (a stale mid-run snapshot)...
        let latest = Some(snapshot(150, 0, 3));
        // ...then the worker publishes its final snapshot and completion...
        let finished = Some(Ok((DONE, snapshot(200, 0, 4))));
        // ...and the same tick reads `finished`.
        let v = running_tick(latest, finished, ResolveStatus::Resolved, false);
        assert_eq!(files_removed_line(&v), "Files removed: 200");
        assert_eq!(
            v.done,
            Some(DoneView {
                status: "Done: completed (exit code 0)".into(),
                code: 0
            })
        );
    }

    #[test]
    fn gui_race_nothing_new_in_latest_still_shows_final_counters() {
        // The previous tick already consumed every mid-run snapshot.
        let v = running_tick(
            None,
            Some(Ok((DONE, snapshot(200, 0, 4)))),
            ResolveStatus::Resolved,
            false,
        );
        assert_eq!(files_removed_line(&v), "Files removed: 200");
        assert_eq!(v.done.map(|d| d.code), Some(0));
    }

    #[test]
    fn gui_normal_order_progress_then_done() {
        let running = running_tick(
            Some(snapshot(150, 0, 3)),
            None,
            ResolveStatus::Resolved,
            false,
        );
        assert_eq!(files_removed_line(&running), "Files removed: 150");
        assert_eq!(running.done, None);
        // Final snapshot and completion both visible at the next tick.
        let final_snapshot = snapshot(200, 0, 4);
        let done = running_tick(
            Some(final_snapshot),
            Some(Ok((DONE, final_snapshot))),
            ResolveStatus::Resolved,
            false,
        );
        assert_eq!(files_removed_line(&done), "Files removed: 200");
        assert_eq!(done.done.map(|d| d.code), Some(0));
    }

    #[test]
    fn gui_idle_tick_renders_nothing() {
        assert_eq!(
            running_tick(None, None, ResolveStatus::Resolved, false),
            TickView {
                counters: None,
                done: None
            }
        );
    }

    #[test]
    fn gui_cancel_path_exits_3_with_final_counters() {
        let v = running_tick(
            Some(snapshot(40, 0, 2)),
            Some(Ok((Outcome::Canceled, snapshot(55, 1, 3)))),
            ResolveStatus::Resolved,
            false,
        );
        assert_eq!(files_removed_line(&v), "Files removed: 55");
        assert_eq!(
            v.done,
            Some(DoneView {
                status: "Done: canceled (exit code 3)".into(),
                code: 3
            })
        );
    }

    #[test]
    fn gui_refused_run_is_done_with_refusal_code() {
        let v = running_tick(
            None,
            Some(Err(Refusal::ConfirmationRequired)),
            ResolveStatus::Resolved,
            false,
        );
        assert_eq!(v.counters, None);
        let d = v.done.expect("done");
        assert_eq!(d.code, Refusal::ConfirmationRequired.exit_code());
        assert!(d.status.starts_with("Done: refused"));
    }
}
