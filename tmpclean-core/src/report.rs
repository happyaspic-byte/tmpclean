//! Report model: counters, outcomes, exit-code mapping, JSON v1 output,
//! and the clock-driven progress aggregator.
//!
//! Everything here is pure data and pure functions; the only stateful
//! pieces (`CancelToken`, `Aggregator`) are still fully deterministic and
//! testable via an injected [`Clock`].

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::strings;

/// Maximum number of error records kept in a report / emitted in JSON.
pub const MAX_ERRORS: usize = 1000;

/// Throttle window for aggregator snapshots.
pub const SNAPSHOT_WINDOW: Duration = Duration::from_millis(250);

/// Distinct deletion counters. Deliberately no reboot counter: the tool
/// never schedules reboot-time deletion.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub files_removed: u64,
    pub dirs_removed: u64,
    pub links_removed: u64,
    pub classic_pending_unconfirmed: u64,
    pub excluded: u64,
    pub skipped_in_use: u64,
    pub failed: u64,
    pub absent: u64,
    pub canceled: u64,
    pub logical_bytes_removed: u64,
}

impl Counters {
    /// Merge a worker-local delta into these counters.
    pub fn merge(&mut self, delta: &CounterDelta) {
        self.files_removed += delta.files_removed;
        self.dirs_removed += delta.dirs_removed;
        self.links_removed += delta.links_removed;
        self.classic_pending_unconfirmed += delta.classic_pending_unconfirmed;
        self.excluded += delta.excluded;
        self.skipped_in_use += delta.skipped_in_use;
        self.failed += delta.failed;
        self.absent += delta.absent;
        self.canceled += delta.canceled;
        self.logical_bytes_removed += delta.logical_bytes_removed;
    }
}

impl std::ops::AddAssign for Counters {
    fn add_assign(&mut self, rhs: Self) {
        self.merge(&rhs);
    }
}

/// Per-worker counter delta, merged into the shared totals.
pub type CounterDelta = Counters;

/// One recorded failure or notable per-path outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorRecord {
    pub path: String,
    /// Windows error code (or a synthesized code); unsigned 32-bit.
    pub code: u32,
    /// Short machine-readable disposition, e.g. "skipped_in_use", "failed".
    pub disposition: String,
}

/// How a requested target resolved before the run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveStatus {
    Resolved,
    /// Target name known but the directory does not exist.
    Absent,
    RequiresElevation,
    /// Guard rejected the resolved root.
    Refused,
    /// Unknown target name / usage error.
    UnknownTarget,
}

impl ResolveStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => strings::STATUS_RESOLVED,
            Self::Absent => strings::STATUS_ABSENT,
            Self::RequiresElevation => strings::STATUS_REQUIRES_ELEVATION,
            Self::Refused => strings::STATUS_REFUSED,
            Self::UnknownTarget => strings::STATUS_UNKNOWN_TARGET,
        }
    }
}

/// One resolved (or attempted) target for the JSON report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetReport {
    pub name: String,
    pub status: ResolveStatus,
    pub path: String,
}

/// Final outcome of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed { fully_empty: bool },
    Canceled,
    FailedToEnumerate,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed { fully_empty: true } => strings::OUTCOME_COMPLETED,
            Self::Completed { fully_empty: false } => strings::OUTCOME_COMPLETED_NOT_EMPTY,
            Self::Canceled => strings::OUTCOME_CANCELED,
            Self::FailedToEnumerate => strings::OUTCOME_FAILED_TO_ENUMERATE,
        }
    }
}

/// Windows error codes with dedicated counters.
pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_PATH_NOT_FOUND: u32 = 3;
pub const ERROR_SHARING_VIOLATION: u32 = 32;
pub const ERROR_LOCK_VIOLATION: u32 = 33;

impl Counters {
    /// Classify a per-path error: increments the matching counter and
    /// returns the machine-readable disposition for its [`ErrorRecord`].
    /// A file that vanished mid-scan counts as `absent`, never `failed`
    /// and never `files_removed`.
    pub fn classify(&mut self, code: u32) -> &'static str {
        match code {
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => {
                self.absent += 1;
                "absent"
            }
            ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION => {
                self.skipped_in_use += 1;
                "skipped_in_use"
            }
            _ => {
                self.failed += 1;
                "failed"
            }
        }
    }

    /// Record a per-path error: classify it into the counters and build
    /// its [`ErrorRecord`].
    pub fn record_error(&mut self, path: impl Into<String>, code: u32) -> ErrorRecord {
        let disposition = self.classify(code);
        ErrorRecord {
            path: path.into(),
            code,
            disposition: disposition.into(),
        }
    }
}

/// Exit codes (locked table):
/// 0 = completed (incl. policy exclusions and in-use skips), every resolved
///     dry-run, and targets that turned out already absent;
/// 1 = completed with failed > 0;
/// 2 = usage error / unknown target name;
/// 3 = refused (guard-rejected root, requires-elevation, or confirmation
///     required / declined / canceled).
pub fn exit_code(
    outcome: Outcome,
    counters: &Counters,
    resolve_status: ResolveStatus,
    dry_run: bool,
) -> u8 {
    match resolve_status {
        ResolveStatus::UnknownTarget => return 2,
        ResolveStatus::Refused | ResolveStatus::RequiresElevation => return 3,
        _ => {}
    }
    // Locked table: EVERY --dry-run whose target resolved exits 0,
    // regardless of outcome or recorded failures.
    if dry_run {
        return 0;
    }
    match outcome {
        Outcome::Canceled => 3,
        Outcome::FailedToEnumerate => 1,
        Outcome::Completed { .. } => {
            if counters.failed == 0 {
                0
            } else {
                1
            }
        }
    }
}

/// The complete run report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub outcome: Outcome,
    pub exit_code: u8,
    pub targets: Vec<TargetReport>,
    pub counters: Counters,
    /// Capped at [`MAX_ERRORS`]; see `errors_truncated`.
    pub errors: Vec<ErrorRecord>,
    pub errors_truncated: bool,
    pub dry_run: bool,
}

impl Report {
    /// Build a report, applying the 1000-error cap.
    pub fn new(
        outcome: Outcome,
        resolve_status: ResolveStatus,
        dry_run: bool,
        targets: Vec<TargetReport>,
        counters: Counters,
        errors: Vec<ErrorRecord>,
    ) -> Self {
        let errors_truncated = errors.len() > MAX_ERRORS;
        let mut errors = errors;
        errors.truncate(MAX_ERRORS);
        let code = exit_code(outcome, &counters, resolve_status, dry_run);
        Self {
            outcome,
            exit_code: code,
            targets,
            counters,
            errors,
            errors_truncated,
            dry_run,
        }
    }
}

/// Escape a string for inclusion inside a JSON string literal.
fn json_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    json_escape(s, &mut out);
    out.push('"');
    out
}

/// Serialize a report to JSON schema v1:
/// `{"schema":1,"outcome":..,"exit_code":N,"targets":[{"name","status","path"}],
///   "counters":{..},"errors":[{"path","code","disposition"}],
///   "errors_truncated":bool}`
pub fn to_json(report: &Report) -> String {
    json_v1(report, report.outcome.as_str())
}

/// Schema-v1 JSON for a run refused before it started (confirmation
/// required or declined): identical to [`to_json`] except that `outcome` is
/// `"refused"`, since no [`Outcome`] occurred.
pub fn to_json_refused(report: &Report) -> String {
    json_v1(report, strings::OUTCOME_REFUSED)
}

fn json_v1(report: &Report, outcome: &str) -> String {
    let c = &report.counters;
    let mut s = String::with_capacity(512);
    s.push_str("{\"schema\":1,\"outcome\":");
    s.push_str(&json_str(outcome));
    s.push_str(",\"exit_code\":");
    s.push_str(&report.exit_code.to_string());
    s.push_str(",\"dry_run\":");
    s.push_str(if report.dry_run { "true" } else { "false" });
    s.push_str(",\"targets\":[");
    for (i, t) in report.targets.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"name\":");
        s.push_str(&json_str(&t.name));
        s.push_str(",\"status\":");
        s.push_str(&json_str(t.status.as_str()));
        s.push_str(",\"path\":");
        s.push_str(&json_str(&t.path));
        s.push('}');
    }
    s.push_str("],\"counters\":{");
    s.push_str(&format!(
        "\"files_removed\":{},\"dirs_removed\":{},\"links_removed\":{},\
         \"classic_pending_unconfirmed\":{},\"excluded\":{},\
         \"skipped_in_use\":{},\"failed\":{},\"absent\":{},\
         \"canceled\":{},\"logical_bytes_removed\":{}",
        c.files_removed,
        c.dirs_removed,
        c.links_removed,
        c.classic_pending_unconfirmed,
        c.excluded,
        c.skipped_in_use,
        c.failed,
        c.absent,
        c.canceled,
        c.logical_bytes_removed
    ));
    s.push_str("},\"errors\":[");
    // Enforce the 1000-error output bound for ANY Report, not only ones
    // built through Report::new.
    let truncated = report.errors_truncated || report.errors.len() > MAX_ERRORS;
    for (i, e) in report.errors.iter().take(MAX_ERRORS).enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"path\":");
        s.push_str(&json_str(&e.path));
        s.push_str(",\"code\":");
        s.push_str(&e.code.to_string());
        s.push_str(",\"disposition\":");
        s.push_str(&json_str(&e.disposition));
        s.push('}');
    }
    s.push_str("],\"errors_truncated\":");
    s.push_str(if truncated { "true" } else { "false" });
    s.push('}');
    s
}

/// Progress snapshot emitted by the aggregator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub counters: Counters,
    pub errors_recorded: u64,
    pub errors_truncated: bool,
    /// Sequence number of this snapshot (0-based).
    pub sequence: u64,
}

/// Source of time for the aggregator. Injected so tests never sleep.
pub trait Clock {
    fn now(&self) -> Duration;
}

/// Merges worker-local [`CounterDelta`]s into totals and emits progress
/// snapshots at most once per [`SNAPSHOT_WINDOW`] of the injected clock.
/// `drain` always returns the exact final totals.
pub struct Aggregator<C: Clock> {
    clock: C,
    totals: Counters,
    error_count: u64,
    errors_truncated: bool,
    last_emit: Option<Duration>,
    sequence: u64,
}

impl<C: Clock> Aggregator<C> {
    pub fn new(clock: C) -> Self {
        Self {
            clock,
            totals: Counters::default(),
            error_count: 0,
            errors_truncated: false,
            last_emit: None,
            sequence: 0,
        }
    }

    /// Merge a worker-local delta. Returns a snapshot iff at least
    /// `SNAPSHOT_WINDOW` of the injected clock elapsed since the last emit
    /// (the first delta always emits).
    pub fn record(&mut self, delta: CounterDelta) -> Option<Snapshot> {
        self.totals.merge(&delta);
        let now = self.clock.now();
        let due = match self.last_emit {
            None => true,
            Some(t) => now.saturating_sub(t) >= SNAPSHOT_WINDOW,
        };
        if due {
            self.last_emit = Some(now);
            Some(self.snapshot())
        } else {
            None
        }
    }

    /// Record an error record for counting purposes (the records themselves
    /// are held by the caller / report builder).
    pub fn record_error(&mut self) {
        self.error_count += 1;
        if self.error_count > MAX_ERRORS as u64 {
            self.errors_truncated = true;
        }
    }

    fn snapshot(&mut self) -> Snapshot {
        let s = Snapshot {
            counters: self.totals,
            errors_recorded: self.error_count,
            errors_truncated: self.errors_truncated,
            sequence: self.sequence,
        };
        self.sequence += 1;
        s
    }

    /// Exact final totals; always returns a snapshot regardless of window.
    pub fn drain(&mut self) -> Snapshot {
        self.snapshot()
    }

    pub fn totals(&self) -> &Counters {
        &self.totals
    }
}

/// Human-readable summary lines from a snapshot (pure; consumed by CLI/GUI).
pub fn format_lines(s: &Snapshot) -> Vec<String> {
    let c = &s.counters;
    vec![
        format!("{}: {}", strings::LABEL_FILES_REMOVED, c.files_removed),
        format!("{}: {}", strings::LABEL_DIRS_REMOVED, c.dirs_removed),
        format!("{}: {}", strings::LABEL_LINKS_REMOVED, c.links_removed),
        format!(
            "{}: {}",
            strings::LABEL_CLASSIC_PENDING,
            c.classic_pending_unconfirmed
        ),
        format!("{}: {}", strings::LABEL_EXCLUDED, c.excluded),
        format!("{}: {}", strings::LABEL_SKIPPED_IN_USE, c.skipped_in_use),
        format!("{}: {}", strings::LABEL_FAILED, c.failed),
        format!("{}: {}", strings::LABEL_ABSENT, c.absent),
        format!("{}: {}", strings::LABEL_CANCELED, c.canceled),
        format!(
            "{}: {}",
            strings::LABEL_LOGICAL_BYTES_REMOVED,
            c.logical_bytes_removed
        ),
    ]
}

/// Cooperative cancellation: an atomic flag plus a wake handle so a blocked
/// worker can be woken instead of polling.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[derive(Default)]
struct CancelInner {
    flag: AtomicBool,
    lock: Mutex<()>,
    cond: Condvar,
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelToken")
            .field("canceled", &self.is_canceled())
            .finish()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.flag.store(true, Ordering::SeqCst);
        let _guard = self.inner.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.inner.cond.notify_all();
    }

    pub fn is_canceled(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst)
    }

    /// Block until canceled. Returns immediately if already canceled.
    pub fn wait(&self) {
        let mut guard = self.inner.lock.lock().unwrap_or_else(|e| e.into_inner());
        while !self.is_canceled() {
            guard = self
                .inner
                .cond
                .wait(guard)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Block until canceled or `timeout` elapses. Returns `true` if canceled.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inner.lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.is_canceled() {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (g, timed_out) = self
                .inner
                .cond
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner());
            guard = g;
            if timed_out.timed_out() && !self.is_canceled() {
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::Ordering as AOrd;

    fn counters_with_failed(failed: u64) -> Counters {
        Counters {
            failed,
            ..Default::default()
        }
    }

    // ---- exit_code table ----

    #[test]
    fn exit_completed_clean_is_0() {
        let c = Counters {
            excluded: 5,
            skipped_in_use: 3,
            ..Default::default()
        };
        assert_eq!(
            exit_code(
                Outcome::Completed { fully_empty: false },
                &c,
                ResolveStatus::Resolved,
                false
            ),
            0
        );
    }

    #[test]
    fn exit_completed_with_failures_is_1() {
        let c = counters_with_failed(1);
        assert_eq!(
            exit_code(
                Outcome::Completed { fully_empty: false },
                &c,
                ResolveStatus::Resolved,
                false
            ),
            1
        );
    }

    #[test]
    fn exit_unknown_target_is_2() {
        assert_eq!(
            exit_code(
                Outcome::Completed { fully_empty: true },
                &Counters::default(),
                ResolveStatus::UnknownTarget,
                false
            ),
            2
        );
    }

    #[test]
    fn exit_refused_and_elevation_and_canceled_are_3() {
        let c = Counters::default();
        for rs in [ResolveStatus::Refused, ResolveStatus::RequiresElevation] {
            assert_eq!(
                exit_code(Outcome::Completed { fully_empty: true }, &c, rs, false),
                3
            );
        }
        // Confirmation declined / canceled -> 3.
        assert_eq!(
            exit_code(Outcome::Canceled, &c, ResolveStatus::Resolved, false),
            3
        );
    }

    #[test]
    fn exit_dry_run_resolved_is_0() {
        // Even with failures recorded, a resolved dry-run exits 0.
        let c = counters_with_failed(7);
        assert_eq!(
            exit_code(
                Outcome::Completed { fully_empty: true },
                &c,
                ResolveStatus::Resolved,
                true
            ),
            0
        );
    }

    #[test]
    fn exit_dry_run_resolved_is_0_for_every_outcome() {
        // Locked table: EVERY resolved --dry-run exits 0, whatever the
        // outcome or failure count.
        let c = counters_with_failed(9);
        for outcome in [
            Outcome::Completed { fully_empty: true },
            Outcome::Completed { fully_empty: false },
            Outcome::Canceled,
            Outcome::FailedToEnumerate,
        ] {
            assert_eq!(exit_code(outcome, &c, ResolveStatus::Resolved, true), 0);
            assert_eq!(exit_code(outcome, &c, ResolveStatus::Absent, true), 0);
        }
    }

    #[test]
    fn exit_dry_run_does_not_mask_resolve_failures() {
        // Resolution failures still win over the dry-run rule.
        let c = Counters::default();
        assert_eq!(
            exit_code(
                Outcome::Completed { fully_empty: true },
                &c,
                ResolveStatus::UnknownTarget,
                true
            ),
            2
        );
        for rs in [ResolveStatus::Refused, ResolveStatus::RequiresElevation] {
            assert_eq!(
                exit_code(Outcome::Completed { fully_empty: true }, &c, rs, true),
                3
            );
        }
    }

    #[test]
    fn exit_failed_to_enumerate_is_1() {
        assert_eq!(
            exit_code(
                Outcome::FailedToEnumerate,
                &Counters::default(),
                ResolveStatus::Resolved,
                false
            ),
            1
        );
    }

    // ---- to_json ----

    fn sample_report() -> Report {
        Report::new(
            Outcome::Completed { fully_empty: true },
            ResolveStatus::Resolved,
            false,
            vec![TargetReport {
                name: "user-temp".into(),
                status: ResolveStatus::Resolved,
                path: "C:\\Users\\x\\AppData\\Local\\Temp".into(),
            }],
            Counters {
                files_removed: 12,
                logical_bytes_removed: 4096,
                ..Default::default()
            },
            vec![ErrorRecord {
                path: "C:\\Temp\\locked.tmp".into(),
                code: 32,
                disposition: "skipped_in_use".into(),
            }],
        )
    }

    #[test]
    fn json_is_schema_v1_with_counters() {
        let j = to_json(&sample_report());
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["schema"], 1);
        assert!(v["counters"].get("files_removed").is_some());
        assert_eq!(v["counters"]["files_removed"], 12);
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["errors_truncated"], false);
    }

    #[test]
    fn json_escapes_quotes_backslashes_controls_and_unicode() {
        let r = Report::new(
            Outcome::Completed { fully_empty: true },
            ResolveStatus::Resolved,
            false,
            vec![],
            Counters::default(),
            vec![ErrorRecord {
                path: "C:\\한국어\\we\"ird\\\nfile.tmp".into(),
                code: 5,
                disposition: "failed".into(),
            }],
        );
        let j = to_json(&r);
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["errors"][0]["path"], "C:\\한국어\\we\"ird\\\nfile.tmp");
    }

    #[test]
    fn errors_capped_at_1000() {
        let errs: Vec<ErrorRecord> = (0..1500)
            .map(|i| ErrorRecord {
                path: format!("p{i}"),
                code: i as u32,
                disposition: "failed".into(),
            })
            .collect();
        let r = Report::new(
            Outcome::Completed { fully_empty: false },
            ResolveStatus::Resolved,
            false,
            vec![],
            counters_with_failed(1500),
            errs,
        );
        assert_eq!(r.errors.len(), MAX_ERRORS);
        assert!(r.errors_truncated);
        let v: serde_json::Value = serde_json::from_str(&to_json(&r)).unwrap();
        assert_eq!(v["errors"].as_array().unwrap().len(), 1000);
        assert_eq!(v["errors_truncated"], true);
    }

    #[test]
    fn to_json_caps_errors_for_any_report() {
        // The serializer itself enforces the bound, even for a Report
        // constructed directly via public fields with 1500 errors and a
        // false errors_truncated flag.
        let r = Report {
            outcome: Outcome::Completed { fully_empty: false },
            exit_code: 1,
            targets: vec![],
            counters: counters_with_failed(1500),
            errors: (0..1500)
                .map(|i| ErrorRecord {
                    path: format!("p{i}"),
                    code: 5,
                    disposition: "failed".into(),
                })
                .collect(),
            errors_truncated: false,
            dry_run: false,
        };
        let v: serde_json::Value = serde_json::from_str(&to_json(&r)).unwrap();
        assert_eq!(v["errors"].as_array().unwrap().len(), 1000);
        assert_eq!(v["errors_truncated"], true);
    }

    #[test]
    fn json_written_for_evidence() {
        // Used by the harness jq check; writes via TMPCL_TEST_JSON_OUT.
        if let Ok(p) = std::env::var("TMPCL_TEST_JSON_OUT") {
            std::fs::write(p, to_json(&sample_report())).unwrap();
        }
    }

    // ---- aggregator with fake clock ----

    struct FakeClock {
        nanos: Cell<u64>,
    }
    impl FakeClock {
        fn new() -> Self {
            Self {
                nanos: Cell::new(0),
            }
        }
        fn advance(&self, d: Duration) {
            self.nanos.set(self.nanos.get() + d.as_nanos() as u64);
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            Duration::from_nanos(self.nanos.get())
        }
    }

    #[test]
    fn aggregator_throttles_to_one_snapshot_per_window() {
        let clock = FakeClock::new();
        let mut agg = Aggregator::new(clock);
        let mut emitted = 0u64;
        for _ in 0..10_000 {
            let d = CounterDelta {
                files_removed: 1,
                logical_bytes_removed: 10,
                ..Default::default()
            };
            if agg.record(d).is_some() {
                emitted += 1;
            }
        }
        assert_eq!(emitted, 1, "10k deltas inside one window -> 1 snapshot");
        let final_snap = agg.drain();
        assert_eq!(final_snap.counters.files_removed, 10_000);
        assert_eq!(final_snap.counters.logical_bytes_removed, 100_000);
    }

    #[test]
    fn aggregator_emits_again_after_window() {
        let clock = FakeClock::new();
        let mut agg = Aggregator::new(clock);
        assert!(agg
            .record(CounterDelta {
                files_removed: 1,
                ..Default::default()
            })
            .is_some());
        assert!(agg
            .record(CounterDelta {
                files_removed: 1,
                ..Default::default()
            })
            .is_none());
        agg.clock.advance(SNAPSHOT_WINDOW);
        assert!(agg
            .record(CounterDelta {
                files_removed: 1,
                ..Default::default()
            })
            .is_some());
        assert_eq!(agg.drain().counters.files_removed, 3);
    }

    // ---- absent vs removed (shipped Counters::record_error/classify) ----

    #[test]
    fn file_not_found_counts_absent_not_removed() {
        let mut c = Counters::default();
        let rec = c.record_error("x", ERROR_FILE_NOT_FOUND);
        assert_eq!(c.absent, 1);
        assert_eq!(
            c.files_removed, 0,
            "FILE_NOT_FOUND must not count as removed"
        );
        assert_eq!(rec.disposition, "absent");
        // PATH_NOT_FOUND is the same class.
        let rec2 = c.record_error("y", ERROR_PATH_NOT_FOUND);
        assert_eq!(c.absent, 2);
        assert_eq!(c.files_removed, 0);
        assert_eq!(rec2.disposition, "absent");
    }

    #[test]
    fn sharing_violation_counts_skipped_in_use() {
        let mut c = Counters::default();
        let rec = c.record_error("x", ERROR_SHARING_VIOLATION);
        assert_eq!(c.skipped_in_use, 1);
        assert_eq!(c.files_removed, 0);
        assert_eq!(rec.disposition, "skipped_in_use");
        let rec2 = c.record_error("y", ERROR_LOCK_VIOLATION);
        assert_eq!(c.skipped_in_use, 2);
        assert_eq!(rec2.disposition, "skipped_in_use");
    }

    #[test]
    fn access_denied_counts_failed() {
        const ERROR_ACCESS_DENIED: u32 = 5;
        let mut c = Counters::default();
        let rec = c.record_error("x", ERROR_ACCESS_DENIED);
        assert_eq!(c.failed, 1);
        assert_eq!(c.absent, 0);
        assert_eq!(c.skipped_in_use, 0);
        assert_eq!(c.files_removed, 0);
        assert_eq!(rec.disposition, "failed");
        assert_eq!(rec.code, ERROR_ACCESS_DENIED);
    }

    // ---- format_lines ----

    #[test]
    fn format_lines_never_says_reclaimed() {
        let s = Snapshot {
            counters: Counters {
                files_removed: 1,
                logical_bytes_removed: 99,
                ..Default::default()
            },
            errors_recorded: 0,
            errors_truncated: false,
            sequence: 0,
        };
        let joined = format_lines(&s).join("\n");
        assert!(!joined.to_lowercase().contains("reclaimed"));
        assert!(joined.to_lowercase().contains("logical bytes removed"));
    }

    // ---- CancelToken ----

    #[test]
    fn cancel_token_wakes_waiter() {
        use std::sync::atomic::AtomicBool as AB;
        let token = CancelToken::new();
        let t2 = token.clone();
        let woke = std::sync::Arc::new(AB::new(false));
        let woke2 = woke.clone();
        let h = std::thread::spawn(move || {
            t2.wait();
            woke2.store(true, AOrd::SeqCst);
        });
        // Let the thread reach wait(), then cancel; wait must return.
        std::thread::yield_now();
        token.cancel();
        h.join().unwrap();
        assert!(woke.load(AOrd::SeqCst));
        assert!(token.is_canceled());
    }

    #[test]
    fn wait_timeout_returns_false_on_timeout() {
        // Bounded guard: a regression that never returns FAILS instead of hanging.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let token = CancelToken::new();
            let r = token.wait_timeout(Duration::from_millis(20));
            let _ = tx.send(r);
        });
        let r = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("wait_timeout did not return within 2 s guard");
        assert!(!r);
    }

    #[test]
    fn wait_timeout_returns_true_on_cancel() {
        let token = CancelToken::new();
        let t2 = token.clone();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let b2 = barrier.clone();
        let h = std::thread::spawn(move || {
            b2.wait();
            t2.cancel();
        });
        barrier.wait();
        let start = Instant::now();
        let r = token.wait_timeout(Duration::from_secs(5));
        h.join().unwrap();
        assert!(r);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "cancel should wake well before the timeout"
        );
    }

    #[test]
    fn wait_timeout_returns_true_when_already_canceled() {
        let token = CancelToken::new();
        token.cancel();
        assert!(token.wait_timeout(Duration::from_secs(5)));
    }

    #[test]
    fn counter_merge_sums_all_fields() {
        let mut a = Counters {
            files_removed: 1,
            absent: 2,
            ..Default::default()
        };
        a.merge(&CounterDelta {
            files_removed: 4,
            skipped_in_use: 1,
            ..Default::default()
        });
        assert_eq!(a.files_removed, 5);
        assert_eq!(a.absent, 2);
        assert_eq!(a.skipped_in_use, 1);
    }
}
