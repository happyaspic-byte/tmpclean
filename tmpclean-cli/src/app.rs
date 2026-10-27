//! Windows CLI surface: resolve, confirm, run the scheduler, report.

use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use tmpclean_core::policy::{ConfirmState, Policy, Refusal};
use tmpclean_core::report::{
    format_lines, to_json, CancelToken, Clock, Counters, Outcome, Report, ResolveStatus, Snapshot,
    TargetReport,
};
use tmpclean_core::resolve::Resolution;
use tmpclean_core::sched::{Root, Scheduler};
use tmpclean_core::strings;
use tmpclean_core::win::resolve::resolve_targets;
use tmpclean_core::win::{WinFs, WinHandle};

use crate::args::Cli;

/// Errors listed in the text summary; the JSON report carries up to 1000.
const TEXT_ERROR_LIMIT: usize = 20;

/// Wall clock since UNIX_EPOCH, as the engine's min-age comparison requires.
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
    }
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let mut targets = Vec::new();
    let mut roots: Vec<Root<WinHandle>> = Vec::new();
    let mut blocked = None;
    for (target, resolution) in resolve_targets(&cli.targets()) {
        let status = resolution.status();
        let path = match resolution {
            Resolution::Resolved { handle, final_path } => {
                roots.push(Root {
                    handle,
                    path: final_path.clone().into(),
                });
                final_path
            }
            Resolution::Refused(reason) => {
                eprintln!("{}: {} ({reason})", target.as_str(), status.as_str());
                blocked.get_or_insert(status);
                String::new()
            }
            Resolution::RequiresElevation => {
                eprintln!("{}: {}", target.as_str(), status.as_str());
                blocked.get_or_insert(status);
                String::new()
            }
            Resolution::Absent => {
                eprintln!("{}: {}", target.as_str(), status.as_str());
                String::new()
            }
        };
        targets.push(TargetReport {
            name: target.as_str().to_owned(),
            status,
            path,
        });
    }

    let confirm = if cli.dry_run || cli.yes || roots.is_empty() {
        ConfirmState::Confirmed
    } else if io::stdin().is_terminal() {
        prompt(&targets)
    } else {
        eprintln!("{}", strings::CONFIRMATION_REQUIRED);
        return ExitCode::from(3);
    };
    let policy = Policy {
        dry_run: cli.dry_run,
        min_age: cli.min_age,
        io_permits: cli.io_permits,
        confirm,
    };
    if let Err(refusal) = policy.check() {
        eprintln!("{}", strings::CONFIRM_DECLINED);
        return ExitCode::from(refusal.exit_code());
    }

    let resolve_status = if !roots.is_empty() {
        ResolveStatus::Resolved
    } else {
        blocked.unwrap_or(ResolveStatus::Absent)
    };
    let mut outcome = Outcome::Completed { fully_empty: true };
    let mut counters = Counters::default();
    let mut errors = Vec::new();
    let mut errors_truncated = false;
    if !roots.is_empty() {
        let cancel = CancelToken::new();
        let on_ctrl_c = cancel.clone();
        if let Err(error) = ctrlc::set_handler(move || on_ctrl_c.cancel()) {
            eprintln!("warning: Ctrl+C handler unavailable: {error}");
        }
        let fs = WinFs::new();
        let json = cli.json;
        let result: Result<_, Refusal> = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let mut scheduler = Scheduler::new(&fs, SystemClock, policy)?;
                    let result = scheduler.run(roots, &cancel, |snapshot| {
                        if !json {
                            progress(&snapshot);
                        }
                    });
                    if cli.stats {
                        eprintln!("STATS {:?}", scheduler.stats());
                    }
                    Ok(result)
                })
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
        let result = match result {
            Ok(result) => result,
            Err(refusal) => {
                eprintln!("{}", strings::CONFIRM_DECLINED);
                return ExitCode::from(refusal.exit_code());
            }
        };
        outcome = result.outcome;
        counters = result.counters;
        errors = result.errors;
        errors_truncated = result.errors_truncated;
    }

    let mut report = Report::new(
        outcome,
        resolve_status,
        cli.dry_run,
        targets,
        counters,
        errors,
    );
    report.errors_truncated |= errors_truncated;
    if cli.json {
        println!("{}", to_json(&report));
    } else {
        summary(&report);
    }
    let _ = io::stdout().flush();
    ExitCode::from(report.exit_code)
}

fn prompt(targets: &[TargetReport]) -> ConfirmState {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{}", strings::CONFIRM_TARGETS);
    for target in targets
        .iter()
        .filter(|t| t.status == ResolveStatus::Resolved)
    {
        let _ = writeln!(stderr, "  {}: {}", target.name, target.path);
    }
    let _ = write!(stderr, "{} ", strings::CONFIRM_PROMPT);
    let _ = stderr.flush();
    drop(stderr);
    let mut answer = String::new();
    if io::stdin().lock().read_line(&mut answer).is_err() {
        return ConfirmState::Declined;
    }
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ConfirmState::Confirmed,
        _ => ConfirmState::Declined,
    }
}

/// Called only for snapshots the core aggregator already throttled.
fn progress(snapshot: &Snapshot) {
    eprintln!(
        "{}: {}",
        strings::LABEL_PROGRESS,
        format_lines(snapshot).join(", ")
    );
}

fn summary(report: &Report) {
    if report.outcome == Outcome::Canceled {
        eprintln!("{}", strings::CANCELED_BY_USER);
    }
    for error in report.errors.iter().take(TEXT_ERROR_LIMIT) {
        eprintln!("{} ({}): {}", error.disposition, error.code, error.path);
    }
    let hidden = report.errors.len().saturating_sub(TEXT_ERROR_LIMIT);
    if hidden > 0 || report.errors_truncated {
        eprintln!("{}", strings::MORE_ERRORS_IN_JSON);
    }
    let mut out = io::stdout().lock();
    if report.dry_run {
        let _ = writeln!(out, "{}", strings::DRY_RUN_NOTE);
    }
    let _ = writeln!(
        out,
        "{}: {}",
        strings::LABEL_OUTCOME,
        report.outcome.as_str()
    );
    let snapshot = Snapshot {
        counters: report.counters,
        errors_recorded: report.errors.len() as u64,
        errors_truncated: report.errors_truncated,
        sequence: 0,
    };
    for line in format_lines(&snapshot) {
        let _ = writeln!(out, "{line}");
    }
}
