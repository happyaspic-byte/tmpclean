//! Bounded work stealing over handle-anchored directory and 128-name tasks.
//!
//! Each worker keeps a depth-first ancestor stack. Opened directories and whole
//! name batches enter bounded deques; owners pop the back and thieves take the
//! front. A full queue is processed locally. Waiting ancestors hold no I/O permit. At quiescence,
//! each waiting ancestor has a queued/active descendant, so storage is bounded
//! by worker traversal depth plus the queue, never the number of directory names.
//! SATA and NVMe cannot be distinguished from seek-penalty information: both
//! receive the conservative four-permit SSD ceiling.

use std::collections::{HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::engine::{DirectoryTask, Engine, RunResult, StepStatus};
use crate::fs::{FileSystem, VolumeCaps};
use crate::policy::{Policy, Refusal};
use crate::report::{
    Aggregator, CancelToken, Clock, CounterDelta, ErrorRecord, Outcome, Snapshot, MAX_ERRORS,
};

/// One resolver-validated, opened root. The scheduler owns and closes the handle.
pub struct Root<H> {
    pub handle: H,
    pub path: PathBuf,
}

#[derive(Debug, Default, Clone)]
pub struct SchedulingStats {
    pub workers: usize,
    pub peak_queued: usize,
    pub device_permits: Vec<usize>,
    /// Enumeration and parent references, including abandoned work on cancellation.
    pub pending_references: usize,
    pub worker_stats: Vec<WorkerStats>,
}

#[derive(Debug, Default, Clone)]
pub struct WorkerStats {
    pub steps: u64,
    pub directories: u64,
    pub batches: u64,
    pub io_time: Duration,
    pub reporting_time: Duration,
    pub waiting_time: Duration,
}

pub struct Scheduler<'a, F: FileSystem, C: Clock> {
    fs: &'a F,
    engine: Engine<'a, F>,
    clock: C,
    io_permits: Option<NonZeroUsize>,
    stats: SchedulingStats,
}

struct SampleClock<'a, C>(&'a C);

impl<C: Clock> Clock for SampleClock<'_, C> {
    fn now(&self) -> std::time::Duration {
        self.0.now()
    }
}

pub fn worker_ceiling() -> usize {
    thread::available_parallelism().map_or(2, |cpus| cpus.get().saturating_mul(2).min(16))
}

fn budget(caps: VolumeCaps, override_permits: Option<NonZeroUsize>) -> usize {
    override_permits.map_or(
        match caps.seek_penalty {
            Some(true) => 1,
            Some(false) => 4,
            None => 2,
        },
        NonZeroUsize::get,
    )
}

fn volume_key(path: &Path, index: usize) -> String {
    let path = crate::resolve::normalize_path(&path.to_string_lossy());
    if path.as_bytes().get(1) == Some(&b':') {
        path[..2].to_ascii_uppercase()
    } else {
        // Unknown paths must never accidentally coalesce unrelated devices.
        format!("unknown-{index}")
    }
}

impl<'a, F: FileSystem, C: Clock> Scheduler<'a, F, C> {
    pub fn new(fs: &'a F, clock: C, policy: Policy) -> Result<Self, Refusal> {
        Ok(Self {
            fs,
            engine: Engine::new(fs, &clock, policy)?,
            clock,
            io_permits: policy.io_permits,
            stats: SchedulingStats::default(),
        })
    }

    pub fn stats(&self) -> &SchedulingStats {
        &self.stats
    }

    /// Synchronous driver; surfaces run it on their background thread. Snapshots
    /// arrive on this calling thread and are throttled only by `Aggregator`.
    /// Clone `cancel` for Ctrl+C/GUI cancellation. In-flight steps finish; no new
    /// step starts after a worker observes cancellation. `canceled` counts runs,
    /// not unseen entries (their count cannot be known without more enumeration).
    pub fn run(
        &mut self,
        roots: Vec<Root<F::Handle>>,
        cancel: &CancelToken,
        mut on_snapshot: impl FnMut(Snapshot),
    ) -> RunResult {
        let keys: Vec<_> = roots
            .iter()
            .enumerate()
            .map(|(i, root)| volume_key(&root.path, i))
            .collect();
        let multiple_volumes = keys.iter().collect::<HashSet<_>>().len() > 1;
        let mut devices: Vec<(String, Option<u32>)> = Vec::new();
        let mut permits: Vec<usize> = Vec::new();
        let mut tasks = Vec::new();
        for (root, key) in roots.into_iter().zip(keys) {
            if cancel.is_canceled() {
                self.fs.close(root.handle);
                continue;
            }
            let caps = self.fs.volume_caps(&root.handle, multiple_volumes);
            let device = devices.iter().position(|(volume, disk)| {
                *volume == key
                    || (multiple_volumes && caps.device_id.is_some() && *disk == caps.device_id)
            });
            let limit = budget(caps, self.io_permits);
            let device = match device {
                Some(device) => {
                    permits[device] = permits[device].min(limit);
                    device
                }
                None => {
                    devices.push((key, caps.device_id));
                    permits.push(limit);
                    devices.len() - 1
                }
            };
            tasks.push(Work {
                task: self.engine.root_task(root.handle, root.path),
                device,
            });
        }
        let workers = permits
            .iter()
            .fold(0usize, |total, limit| total.saturating_add(*limit))
            .clamp(1, worker_ceiling());
        // Up to four batches/worker, and at most 4096 live names for a flat
        // directory even at the global worker ceiling (one producer reserve).
        let capacity = (workers * 4).min(31 - workers);
        let mut state = PoolState {
            queues: (0..workers).map(|_| VecDeque::new()).collect(),
            queued: tasks.len(),
            peak: tasks.len(),
            outstanding: tasks.len(),
            active_io: vec![0; permits.len()],
        };
        for (i, task) in tasks.into_iter().enumerate() {
            state.queues[i % workers].push_back(task);
        }
        let pool = Pool {
            state: Mutex::new(state),
            changed: Condvar::new(),
            permits,
            capacity,
        };
        let mut aggregator = Aggregator::new(SampleClock(&self.clock));
        let mut errors = Vec::new();
        let mut outcome = Outcome::Completed { fully_empty: true };
        let (sender, receiver) = mpsc::sync_channel(workers * 4);
        let mut worker_stats = vec![WorkerStats::default(); workers];
        thread::scope(|scope| {
            for (worker, stats) in worker_stats.iter_mut().enumerate() {
                let sender = sender.clone();
                let pool = &pool;
                let engine = &self.engine;
                scope.spawn(move || drive(worker, engine, pool, cancel, sender, stats));
            }
            drop(sender);
            for event in receiver {
                for error in event.error.into_iter().chain(event.restore_error) {
                    aggregator.record_error();
                    if errors.len() < MAX_ERRORS {
                        errors.push(error);
                    }
                }
                if let Some(snapshot) = aggregator.record(event.delta) {
                    on_snapshot(snapshot);
                }
                match event.outcome {
                    Some(Outcome::FailedToEnumerate) => outcome = Outcome::FailedToEnumerate,
                    Some(Outcome::Completed { fully_empty: false })
                        if outcome != Outcome::FailedToEnumerate =>
                    {
                        outcome = Outcome::Completed { fully_empty: false };
                    }
                    _ => {}
                }
            }
        });
        if cancel.is_canceled() {
            outcome = Outcome::Canceled;
            aggregator.record(CounterDelta {
                canceled: 1,
                ..CounterDelta::default()
            });
        }
        let final_snapshot = aggregator.drain();
        on_snapshot(final_snapshot);
        self.stats = SchedulingStats {
            workers,
            peak_queued: lock(&pool.state).peak,
            device_permits: pool.permits.clone(),
            pending_references: self.engine.pending_references(),
            worker_stats,
        };
        RunResult {
            outcome,
            counters: final_snapshot.counters,
            errors,
            errors_truncated: final_snapshot.errors_truncated,
        }
    }
}

struct Work<'a, F: FileSystem> {
    task: DirectoryTask<'a, F>,
    device: usize,
}

struct PoolState<'a, F: FileSystem> {
    queues: Vec<VecDeque<Work<'a, F>>>,
    queued: usize,
    peak: usize,
    outstanding: usize,
    active_io: Vec<usize>,
}

impl<'a, F: FileSystem> PoolState<'a, F> {
    fn take(&mut self, worker: usize) -> Option<Work<'a, F>> {
        let mut work = self.queues[worker].pop_back();
        for offset in 1..self.queues.len() {
            if work.is_some() {
                break;
            }
            let victim = (worker + offset) % self.queues.len();
            work = self.queues[victim].pop_front();
        }
        if work.is_some() {
            self.queued -= 1;
        }
        work
    }
}

struct Pool<'a, F: FileSystem> {
    state: Mutex<PoolState<'a, F>>,
    changed: Condvar,
    permits: Vec<usize>,
    capacity: usize,
}

struct Event {
    delta: CounterDelta,
    error: Option<ErrorRecord>,
    restore_error: Option<ErrorRecord>,
    outcome: Option<Outcome>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn drive<'a, F: FileSystem>(
    worker: usize,
    engine: &Engine<'a, F>,
    pool: &Pool<'a, F>,
    cancel: &CancelToken,
    sender: mpsc::SyncSender<Event>,
    stats: &mut WorkerStats,
) {
    let mut stack: Vec<Work<'a, F>> = Vec::new();
    let mut state = lock(&pool.state);
    loop {
        if cancel.is_canceled() {
            state.outstanding -= stack.len() + state.queued;
            stack.clear();
            for queue in &mut state.queues {
                queue.clear();
            }
            state.queued = 0;
            pool.changed.notify_all();
            return;
        }
        if stack.last().is_none_or(|work| !work.task.ready()) {
            if let Some(work) = state.take(worker) {
                stack.push(work);
            }
        }
        if let Some(work) = stack.last_mut() {
            let device = work.device;
            if work.task.ready() && state.active_io[device] < pool.permits[device] {
                state.active_io[device] += 1;
                drop(state);
                let started = Instant::now();
                let step = engine.step_batch(&mut work.task);
                stats.io_time += started.elapsed();
                stats.steps += 1;
                state = lock(&pool.state);
                state.active_io[device] -= 1;
                let outcome = if let StepStatus::Finished { outcome, .. } = step.status {
                    stack.pop();
                    state.outstanding -= 1;
                    outcome
                } else {
                    None
                };
                if let Some(task) = step.child {
                    if task.is_batch() {
                        stats.batches += 1;
                    } else {
                        stats.directories += 1;
                    }
                    state.outstanding += 1;
                    let work = Work { task, device };
                    // Publish opened subtrees as well as flat-directory batches.
                    // Keeping a child directory local parks its siblings behind
                    // the entire subtree, even when other workers are idle.
                    if state.queued < pool.capacity {
                        state.queues[worker].push_back(work);
                        state.queued += 1;
                        state.peak = state.peak.max(state.queued);
                    } else {
                        stack.push(work);
                    }
                }
                pool.changed.notify_all();
                // Release both the permit and pool lock before bounded reporting.
                drop(state);
                let started = Instant::now();
                if sender
                    .send(Event {
                        delta: step.delta,
                        error: step.error,
                        restore_error: step.restore_error,
                        outcome,
                    })
                    .is_err()
                {
                    return;
                }
                stats.reporting_time += started.elapsed();
                state = lock(&pool.state);
                continue;
            }
        }
        if state.outstanding == 0 {
            return;
        }
        let started = Instant::now();
        state = match pool.changed.wait(state) {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        stats.waiting_time += started.elapsed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{BasicInfo, FakeFs, FakeStats};
    use crate::report::Counters;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn scheduler(
        fs: &FakeFs,
        io_permits: Option<NonZeroUsize>,
    ) -> Scheduler<'_, FakeFs, FixedClock> {
        Scheduler::new(
            fs,
            FixedClock,
            Policy {
                confirm: crate::policy::ConfirmState::Confirmed,
                min_age: Duration::ZERO,
                io_permits,
                ..Policy::default()
            },
        )
        .unwrap()
    }

    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> Duration {
            Duration::from_secs(1_800_000_000)
        }
    }

    // The timeout is a failure bound, never synchronization by timing. A broken
    // scheduler cannot hang libtest in a scoped-thread join.
    fn bounded<T: Send + 'static>(run: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)))
                .unwrap();
        });
        let result = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("scheduler must terminate");
        worker.join().unwrap();
        match result {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn flat(fs: &FakeFs, prefix: &str, entries: usize) {
        for i in 0..entries {
            fs.close(
                fs.add_file(
                    format!("{prefix}file{i:06}"),
                    BasicInfo {
                        size: 3,
                        ..BasicInfo::default()
                    },
                )
                .unwrap(),
            );
        }
    }

    fn parallel_run(permits: usize) -> (RunResult, SchedulingStats, FakeStats) {
        bounded(move || {
            let fs = FakeFs::new();
            flat(&fs, "", 1000);
            let width = permits.min(worker_ceiling());
            let rendezvous = Arc::new((Mutex::new(0usize), Condvar::new()));
            let calls = AtomicUsize::new(0);
            fs.set_disposition_hook(Some(Arc::new(move || {
                if calls.fetch_add(1, Ordering::SeqCst) < width {
                    let (mutex, changed) = &*rendezvous;
                    let mut arrived = mutex.lock().unwrap();
                    *arrived += 1;
                    changed.notify_all();
                    let (arrived, _) = changed
                        .wait_timeout_while(arrived, Duration::from_secs(5), |arrived| {
                            *arrived < width
                        })
                        .unwrap();
                    assert_eq!(*arrived, width, "all permitted dispositions must overlap");
                }
            })));
            let mut scheduler = scheduler(&fs, NonZeroUsize::new(permits));
            let mut snapshots = Vec::new();
            let result = scheduler.run(
                vec![Root {
                    handle: fs.root_handle(),
                    path: "/".into(),
                }],
                &CancelToken::new(),
                |snapshot| snapshots.push(snapshot),
            );
            assert_eq!(snapshots.last().unwrap().counters, result.counters);
            assert_eq!(
                snapshots.len(),
                2,
                "only aggregator first and final snapshots"
            );
            fs.set_disposition_hook(None);
            (result, scheduler.stats().clone(), fs.stats())
        })
    }

    #[test]
    fn sched_reports_readonly_restore_failure_as_its_own_record() {
        use crate::fs::{FailureMode, FileSystem, FsError, Operation, READONLY};
        let fs = FakeFs::new();
        let raw = fs
            .add_file(
                "readonly",
                BasicInfo {
                    attributes: READONLY,
                    ..BasicInfo::default()
                },
            )
            .unwrap();
        fs.inject_failure(
            "readonly",
            Operation::SetDisposition,
            FsError::AccessDenied,
            FailureMode::Persistent,
        )
        .unwrap();
        fs.inject_failure(
            "readonly",
            Operation::RestoreReadonly,
            FsError::Other(4321),
            FailureMode::Once,
        )
        .unwrap();
        let mut scheduler = scheduler(&fs, NonZeroUsize::new(2));
        let result = scheduler.run(
            vec![Root {
                handle: fs.root_handle(),
                path: "/".into(),
            }],
            &CancelToken::new(),
            |_| {},
        );
        assert_eq!(result.counters.failed, 2);
        let codes: Vec<_> = result.errors.iter().map(|e| e.code).collect();
        assert_eq!(codes, vec![5, 4321]);
        assert_eq!(fs.stats().restore_readonly_calls, 1);
        assert_eq!(fs.basic_info(&raw).unwrap().attributes & READONLY, 0);
        fs.close(raw);
    }

    #[test]
    fn sched_permits_one_and_four_have_exact_equal_totals() {
        let (serial, one, serial_fs) = parallel_run(1);
        let (parallel, four, parallel_fs) = parallel_run(4);
        assert_eq!(serial_fs.max_concurrent_dispositions, 1);
        assert!(parallel_fs.max_concurrent_dispositions > 1);
        assert!(parallel_fs.max_concurrent_dispositions <= 4);
        assert_eq!(one.device_permits, [1]);
        assert_eq!(four.device_permits, [4]);
        assert!(four.workers <= worker_ceiling());
        assert_eq!(serial.counters, parallel.counters);
        assert_eq!(parallel.counters.files_removed, 1000);
        assert_eq!(parallel.counters.logical_bytes_removed, 3000);
        assert_eq!(parallel.outcome, Outcome::Completed { fully_empty: true });
        assert_eq!(four.pending_references, 0);
        println!(
            "1000 entries: serial max={} parallel max={}; counters={:?}; stats={four:?}",
            serial_fs.max_concurrent_dispositions,
            parallel_fs.max_concurrent_dispositions,
            parallel.counters
        );
    }

    #[test]
    fn sched_sibling_subtrees_make_progress_concurrently() {
        bounded(|| {
            let fs = FakeFs::new();
            // Each directory fits in one batch, as in the Windows benchmark.
            for i in 0..25 {
                let dir = format!("dir{i:02}");
                fs.close(fs.add_dir(&dir).unwrap());
                flat(&fs, &format!("{dir}/"), 40);
            }
            let width = 4.min(worker_ceiling());
            let rendezvous = Arc::new((Mutex::new(0usize), Condvar::new()));
            let calls = AtomicUsize::new(0);
            fs.set_disposition_hook(Some(Arc::new(move || {
                if calls.fetch_add(1, Ordering::SeqCst) < width {
                    let (mutex, changed) = &*rendezvous;
                    let mut arrived = mutex.lock().unwrap();
                    *arrived += 1;
                    changed.notify_all();
                    // A timeout makes the final concurrency assertion fail,
                    // rather than panicking inside a scheduler worker.
                    let _ = changed
                        .wait_timeout_while(arrived, Duration::from_secs(2), |arrived| {
                            *arrived < width
                        })
                        .unwrap();
                }
            })));
            let mut scheduler = scheduler(&fs, NonZeroUsize::new(4));
            let result = scheduler.run(
                vec![Root {
                    handle: fs.root_handle(),
                    path: "/".into(),
                }],
                &CancelToken::new(),
                |_| {},
            );
            fs.set_disposition_hook(None);
            assert_eq!(result.counters.files_removed, 1000);
            assert_eq!(result.counters.dirs_removed, 25);
            assert_eq!(result.counters.logical_bytes_removed, 3000);
            assert_eq!(result.counters.failed, 0);
            assert_eq!(result.outcome, Outcome::Completed { fully_empty: true });
            assert_eq!(scheduler.stats().pending_references, 0);
            assert!(
                fs.stats().max_concurrent_dispositions > 1,
                "sibling subtrees must be stealable, not only their name batches"
            );
            assert!(fs.stats().max_concurrent_dispositions <= 4);
        });
    }

    #[test]
    fn sched_cancel_at_one_hundred_releases_all_references_and_resumes() {
        bounded(|| {
            let fs = FakeFs::new();
            fs.close(fs.add_dir("nested").unwrap());
            flat(&fs, "nested/", 1000);
            let cancel = CancelToken::new();
            let token = cancel.clone();
            let calls = AtomicUsize::new(0);
            fs.set_disposition_hook(Some(Arc::new(move || {
                if calls.fetch_add(1, Ordering::SeqCst) == 99 {
                    token.cancel();
                }
            })));
            let mut scheduler = scheduler(&fs, NonZeroUsize::new(1));
            let result = scheduler.run(
                vec![Root {
                    handle: fs.root_handle(),
                    path: "/".into(),
                }],
                &cancel,
                |_| {},
            );
            assert_eq!(result.outcome, Outcome::Canceled);
            assert_eq!(
                result.counters,
                Counters {
                    files_removed: 100,
                    logical_bytes_removed: 300,
                    canceled: 1,
                    ..Counters::default()
                }
            );
            assert_eq!(scheduler.stats().pending_references, 0);
            assert_eq!(fs.stats().in_flight_entries, 0);
            assert_eq!(fs.stats().current_dispositions, 0);
            println!(
                "cancel after 100: {:?}; stats={:?}",
                result.counters,
                scheduler.stats()
            );
            fs.set_disposition_hook(None);
            let resumed = scheduler.run(
                vec![Root {
                    handle: fs.root_handle(),
                    path: "/".into(),
                }],
                &CancelToken::new(),
                |_| {},
            );
            assert_eq!(resumed.counters.files_removed, 900);
            assert_eq!(resumed.counters.dirs_removed, 1);
            assert_eq!(resumed.counters.failed, 0);
            assert_eq!(resumed.outcome, Outcome::Completed { fully_empty: true });
            assert_eq!(scheduler.stats().pending_references, 0);
        });
    }

    #[test]
    fn sched_default_budgets_and_explicit_override() {
        for (seek_penalty, expected) in [(Some(true), 1), (Some(false), 4), (None, 2)] {
            let caps = VolumeCaps {
                seek_penalty,
                ..VolumeCaps::default()
            };
            assert_eq!(budget(caps, None), expected);
            assert_eq!(budget(caps, NonZeroUsize::new(3)), 3);
            bounded(move || {
                let fs = FakeFs::new();
                fs.set_volume_caps(caps);
                flat(&fs, "", 10);
                let mut scheduler = scheduler(&fs, None);
                let result = scheduler.run(
                    vec![Root {
                        handle: fs.root_handle(),
                        path: "C:\\Temp".into(),
                    }],
                    &CancelToken::new(),
                    |_| {},
                );
                assert_eq!(result.counters.files_removed, 10);
                assert_eq!(scheduler.stats().device_permits, [expected]);
                assert_eq!(fs.stats().extents_queries, 0);
            });
        }
    }

    #[test]
    fn sched_multiple_volume_grouping_and_unknown_devices() {
        for (same_volume, same_disk, expected, queries) in [
            (true, true, vec![4], 0),
            (false, true, vec![4], 2),
            (false, false, vec![4, 4], 2),
        ] {
            bounded(move || {
                let fs = FakeFs::new();
                let mut roots = Vec::new();
                for (i, name) in ["a", "b"].into_iter().enumerate() {
                    let handle = fs.add_dir(name).unwrap();
                    flat(&fs, &format!("{name}/"), 10);
                    fs.set_handle_volume_caps(
                        &handle,
                        VolumeCaps {
                            seek_penalty: Some(false),
                            device_id: if same_disk { Some(7) } else { None },
                            ..VolumeCaps::default()
                        },
                    )
                    .unwrap();
                    roots.push(Root {
                        handle,
                        path: if i == 0 || same_volume {
                            format!("C:\\Temp\\{name}")
                        } else {
                            "D:\\Temp".into()
                        }
                        .into(),
                    });
                }
                let mut scheduler = scheduler(&fs, None);
                let result = scheduler.run(roots, &CancelToken::new(), |_| {});
                assert_eq!(result.counters.files_removed, 20);
                assert_eq!(scheduler.stats().device_permits, expected);
                assert_eq!(scheduler.stats().pending_references, 0);
                assert_eq!(fs.stats().extents_queries, queries);
            });
        }
    }

    #[test]
    fn sched_batch_flat_hundred_thousand_entries_stays_bounded() {
        bounded(|| {
            let fs = FakeFs::new();
            flat(&fs, "", 100_000);
            let mut scheduler = scheduler(&fs, NonZeroUsize::new(4));
            let result = scheduler.run(
                vec![Root {
                    handle: fs.root_handle(),
                    path: "/".into(),
                }],
                &CancelToken::new(),
                |_| {},
            );
            let stats = fs.stats();
            assert_eq!(result.counters.files_removed, 100_000);
            assert_eq!(result.counters.logical_bytes_removed, 300_000);
            assert_eq!(result.counters.failed, 0);
            assert!(stats.peak_in_flight_entries <= 4096, "{stats:?}");
            assert_eq!(stats.in_flight_entries, 0);
            assert_eq!(scheduler.stats().pending_references, 0);
            println!(
                "100000 scheduled entries: {:?}; peak names={}; {:?}",
                result.counters,
                stats.peak_in_flight_entries,
                scheduler.stats()
            );
        });
    }

    #[test]
    fn sched_policy_dryrun_and_parallel_cancel_keep_partial_counts_exact() {
        bounded(|| {
            let fs = FakeFs::new();
            flat(&fs, "", 1000);
            let policy = Policy {
                dry_run: true,
                min_age: Duration::ZERO,
                io_permits: NonZeroUsize::new(4),
                ..Policy::default()
            };
            let mut preview = Scheduler::new(&fs, FixedClock, policy).unwrap();
            let result = preview.run(
                vec![Root {
                    handle: fs.root_handle(),
                    path: "/".into(),
                }],
                &CancelToken::new(),
                |_| {},
            );
            assert_eq!(result.counters.files_removed, 1000);
            assert_eq!(fs.stats().disposition_calls, 0);
            assert_eq!(fs.stats().clear_readonly_calls, 0);
            assert_eq!(preview.stats().pending_references, 0);
            let cancel = CancelToken::new();
            let token = cancel.clone();
            let calls = AtomicUsize::new(0);
            fs.set_disposition_hook(Some(Arc::new(move || {
                if calls.fetch_add(1, Ordering::SeqCst) == 99 {
                    token.cancel();
                }
            })));
            let mut scheduler = scheduler(&fs, NonZeroUsize::new(4));
            let result = scheduler.run(
                vec![Root {
                    handle: fs.root_handle(),
                    path: "/".into(),
                }],
                &cancel,
                |_| {},
            );
            assert_eq!(result.outcome, Outcome::Canceled);
            assert!((100..=103).contains(&result.counters.files_removed));
            assert_eq!(
                result.counters.files_removed,
                fs.stats().disposition_calls as u64
            );
            assert_eq!(
                result.counters.logical_bytes_removed,
                result.counters.files_removed * 3
            );
            assert_eq!(result.counters.failed, 0);
            assert_eq!(scheduler.stats().pending_references, 0);
            assert_eq!(fs.stats().current_dispositions, 0);
            assert_eq!(fs.stats().in_flight_entries, 0);
            fs.set_disposition_hook(None);
        });
    }

    #[test]
    fn sched_pre_canceled_never_enumerates_or_disposes() {
        bounded(|| {
            let fs = FakeFs::new();
            flat(&fs, "", 100);
            let cancel = CancelToken::new();
            cancel.cancel();
            let mut scheduler = scheduler(&fs, None);
            let result = scheduler.run(
                vec![Root {
                    handle: fs.root_handle(),
                    path: "/".into(),
                }],
                &cancel,
                |_| {},
            );
            assert_eq!(result.outcome, Outcome::Canceled);
            assert_eq!(result.counters.canceled, 1);
            assert!(fs.stats().enumerated_directories.is_empty());
            assert_eq!(fs.stats().disposition_calls, 0);
            assert_eq!(scheduler.stats().pending_references, 0);
        });
    }
}
