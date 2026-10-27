//! Pure exit-status helpers for the CLI, unit-tested on every host.

use tmpclean_core::report::{Counters, Outcome, Report, ResolveStatus, TargetReport};

/// One status for the exit-code table across independent targets, the same
/// rule as the GUI: any refusal or elevation requirement wins (exit 3) even
/// when other targets ran; else any resolved root makes it a normal run;
/// all-absent (or none) is `Absent` (exit 0).
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

/// The report for a run refused before it started (confirmation required or
/// declined): every target status, zero counters, no errors, `exit_code`.
/// Serialize it with `report::to_json_refused`.
pub fn refusal_report(targets: Vec<TargetReport>, dry_run: bool, exit_code: u8) -> Report {
    Report {
        // Nothing ran; the JSON outcome label is "refused", not this value.
        outcome: Outcome::Canceled,
        exit_code,
        targets,
        counters: Counters::default(),
        errors: Vec::new(),
        errors_truncated: false,
        dry_run,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmpclean_core::report::{exit_code, to_json_refused};
    use ResolveStatus::*;

    #[test]
    fn cli_combined_status_refusal_precedence() {
        let done = Outcome::Completed { fully_empty: true };
        let clean = Counters::default();
        let failed = Counters {
            failed: 1,
            ..Counters::default()
        };
        for (statuses, status, code, failed_code) in [
            (vec![], Absent, 0, 1),
            (vec![Absent, Absent], Absent, 0, 1),
            (vec![Absent, Resolved], Resolved, 0, 1),
            (vec![Resolved, Refused], Refused, 3, 3),
            (vec![Refused, Resolved], Refused, 3, 3),
            (vec![Resolved, RequiresElevation], RequiresElevation, 3, 3),
            (vec![RequiresElevation, Absent], RequiresElevation, 3, 3),
            (vec![RequiresElevation, Refused, Resolved], Refused, 3, 3),
        ] {
            let combined = combined_status(statuses.clone());
            assert_eq!(combined, status, "{statuses:?}");
            assert_eq!(
                exit_code(done, &clean, combined, false),
                code,
                "{statuses:?}"
            );
            assert_eq!(exit_code(done, &failed, combined, false), failed_code);
            // Refusal precedence holds for dry runs too.
            assert_eq!(
                exit_code(done, &clean, combined, true),
                code,
                "{statuses:?}"
            );
        }
    }

    #[test]
    fn cli_refusal_report_json_is_schema_1_exit_3() {
        let targets = vec![
            TargetReport {
                name: "user-temp".into(),
                status: Resolved,
                path: r"C:\Users\x\AppData\Local\Temp".into(),
            },
            TargetReport {
                name: "windows-temp".into(),
                status: RequiresElevation,
                path: String::new(),
            },
        ];
        let json = to_json_refused(&refusal_report(targets, false, 3));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["outcome"], "refused");
        assert_eq!(v["exit_code"], 3);
        assert_eq!(v["dry_run"], false);
        assert_eq!(v["targets"][0]["status"], "resolved");
        assert_eq!(v["targets"][1]["status"], "requires-elevation");
        let counters = v["counters"].as_object().unwrap();
        assert_eq!(counters.len(), 10);
        assert!(counters.values().all(|c| c == 0), "{json}");
        assert_eq!(v["errors"], serde_json::json!([]));
        assert_eq!(v["errors_truncated"], false);
    }
}
