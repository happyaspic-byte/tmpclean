//! Window state machine (safe): countdown -> running -> done.
//!
//! The engine runs on a worker thread; its snapshots land in a mutex slot
//! (already throttled by the core `Aggregator`). The 250 ms UI timer only pulls
//! the latest slot value; there is no second throttle.

use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tmpclean_core::policy::{ConfirmState, Policy, Refusal};
use tmpclean_core::report::{CancelToken, Clock, Snapshot};
use tmpclean_core::resolve::Resolution;
use tmpclean_core::sched::{Root, Scheduler};
use tmpclean_core::strings;
use tmpclean_core::win::resolve::resolve_targets;
use tmpclean_core::win::{WinFs, WinHandle};

use crate::args::Cli;
use crate::machine::{self, Completion};
use crate::view::{self, TargetLine};
use crate::win32::{self, Event, Ui};

/// Time since UNIX_EPOCH, as the engine's age comparison requires.
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
    }
}

type Slot<T> = Arc<Mutex<Option<T>>>;

fn take<T>(slot: &Slot<T>) -> Option<T> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

struct Worker {
    cancel: CancelToken,
    latest: Slot<Snapshot>,
    /// Set once, after the last `latest` publication; carries the final
    /// snapshot so the Done view never depends on the read order.
    finished: Slot<Completion>,
}

fn start(roots: Vec<Root<WinHandle>>, policy: Policy) -> Worker {
    let worker = Worker {
        cancel: CancelToken::new(),
        latest: Arc::default(),
        finished: Arc::default(),
    };
    let (cancel, latest, finished) = (
        worker.cancel.clone(),
        Arc::clone(&worker.latest),
        Arc::clone(&worker.finished),
    );
    thread::spawn(move || {
        let fs = WinFs::new();
        let result = Scheduler::new(&fs, SystemClock, policy).map(|mut scheduler| {
            let mut last = None;
            let run = scheduler.run(roots, &cancel, |snapshot| {
                last = Some(snapshot);
                *latest.lock().unwrap_or_else(PoisonError::into_inner) = Some(snapshot);
            });
            // The scheduler always reports its drained final snapshot last;
            // the fallback only rebuilds it from the run result.
            let final_snapshot = last.unwrap_or(Snapshot {
                counters: run.counters,
                errors_recorded: run.errors.len() as u64,
                errors_truncated: run.errors_truncated,
                sequence: 0,
            });
            (run.outcome, final_snapshot)
        });
        *finished.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
    });
    worker
}

enum State {
    Countdown(Instant, Vec<Root<WinHandle>>),
    Running(Worker),
    Done(u8),
}

/// Show the dialog and drive it to completion; returns the process exit code.
pub fn run(cli: &Cli) -> u8 {
    win32::init_common_controls();
    let mut lines = Vec::new();
    let mut roots = Vec::new();
    for (target, resolution) in resolve_targets(&cli.target_list()) {
        let status = resolution.status();
        let detail = match resolution {
            Resolution::Resolved { handle, final_path } => {
                roots.push(Root {
                    handle,
                    path: final_path.clone().into(),
                });
                final_path
            }
            Resolution::Refused(reason) => reason.to_owned(),
            Resolution::Absent | Resolution::RequiresElevation => String::new(),
        };
        lines.push(TargetLine {
            name: target.as_str(),
            status,
            detail,
        });
    }
    let resolve_status = view::combined_status(lines.iter().map(|l| l.status));

    let Some(ui) = Ui::create(
        strings::GUI_TITLE,
        strings::GUI_BUTTON_CANCEL,
        strings::GUI_BUTTON_CLOSE,
    ) else {
        win32::message_box(strings::APP_NAME, strings::GUI_WINDOW_FAILED, true);
        return 1;
    };
    ui.set_targets(&view::targets_text(&lines));
    ui.set_counters(&view::counters_text(&view::empty_snapshot(), cli.dry_run));
    ui.start_timer(view::TICK_MS);

    let policy = Policy {
        dry_run: cli.dry_run,
        min_age: cli.min_age,
        io_permits: cli.io_permits,
        confirm: ConfirmState::Confirmed,
    };
    let begin = |ui: &Ui, roots| {
        ui.set_status(view::running_text(cli.dry_run));
        ui.progress_marquee();
        State::Running(start(roots, policy))
    };

    let mut state = if cli.yes || cli.dry_run {
        begin(&ui, roots)
    } else {
        ui.set_status(&view::countdown_text(view::COUNTDOWN));
        State::Countdown(Instant::now(), roots)
    };

    loop {
        let event = ui.next_event();
        state = match (state, event) {
            (State::Countdown(..), Event::Escape | Event::Cancel | Event::WindowClose) => {
                // Declined before any work started: nothing was touched.
                return Refusal::ConfirmationDeclined.exit_code();
            }
            (State::Countdown(_, roots), Event::Enter) => begin(&ui, roots),
            (State::Countdown(started, roots), Event::Tick) => {
                let elapsed = started.elapsed();
                if elapsed >= view::COUNTDOWN {
                    begin(&ui, roots)
                } else {
                    ui.set_status(&view::countdown_text(view::COUNTDOWN - elapsed));
                    State::Countdown(started, roots)
                }
            }
            (State::Running(w), Event::Escape | Event::Cancel | Event::WindowClose) => {
                w.cancel.cancel();
                ui.set_cancel_enabled(false);
                ui.set_status(strings::GUI_CANCELING);
                State::Running(w)
            }
            (State::Running(w), Event::Tick) => {
                let latest = take(&w.latest);
                let finished = take(&w.finished);
                let tick = machine::running_tick(latest, finished, resolve_status, cli.dry_run);
                if let Some(counters) = &tick.counters {
                    ui.set_counters(counters);
                }
                match tick.done {
                    None => State::Running(w),
                    Some(done) => {
                        ui.set_status(&done.status);
                        ui.progress_full();
                        ui.set_cancel_enabled(false);
                        ui.enable_close();
                        ui.set_title(strings::GUI_TITLE_DONE);
                        State::Done(done.code)
                    }
                }
            }
            (
                State::Done(code),
                Event::Close | Event::Enter | Event::Escape | Event::WindowClose,
            ) => {
                return code;
            }
            (State::Done(code), Event::Quit) => return code,
            (_, Event::Quit) => return Refusal::ConfirmationDeclined.exit_code(),
            (state, _) => state,
        };
    }
}
