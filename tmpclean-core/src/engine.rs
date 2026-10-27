//! Handle-anchored, iterative postorder deletion.
//!
//! The resolver supplies an already validated, opened root. Paths here are only
//! diagnostic labels: every child is opened relative to its parent's handle.
//! Reparse points are leaves, including unknown tags and untagged reparse bits.
//! This does not promise protection against hostile concurrent reparse mutation
//! when running elevated.
//!
//! [`Engine::run`] is the serial driver. A scheduler can instead own individual
//! [`DirectoryTask`]s and call [`Engine::step`] in bounded name batches. A task's
//! enumeration holds its initial completion reference; every emitted child holds
//! one more until it finishes or is dropped. Park a waiting parent without an I/O
//! permit, then resume it after its last child releases its reference. Handles
//! need only be `Send`, not `Sync`: each task has exactly one owner.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::fs::{
    BasicInfo, DispFlags, Entry, FileSystem, FsError, DIRECTORY, READONLY, REPARSE_POINT,
};
use crate::report::{Clock, CounterDelta, Counters, ErrorRecord, Outcome, MAX_ERRORS};

/// Engine inputs; confirmation and surface defaults belong to the policy layer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Policy {
    pub dry_run: bool,
    /// Zero disables age filtering. Directories are never age-filtered.
    pub min_age: Duration,
}

#[derive(Debug)]
pub struct RunResult {
    pub outcome: Outcome,
    pub counters: Counters,
    pub errors: Vec<ErrorRecord>,
    pub errors_truncated: bool,
}

struct CompletionState {
    pending: AtomicUsize,
    retained: AtomicBool,
}

/// Read-only scheduler view of a directory's outstanding completion references.
///
/// Cloning this observer does not acquire a completion reference. After every
/// child has finished or been dropped, `pending()` becomes zero if enumeration
/// has also finished. A retained descendant prevents ancestor disposition.
#[derive(Clone)]
pub struct Completion(Arc<CompletionState>);

impl Completion {
    fn new() -> Self {
        Self(Arc::new(CompletionState {
            pending: AtomicUsize::new(1),
            retained: AtomicBool::new(false),
        }))
    }

    pub fn pending(&self) -> usize {
        self.0.pending.load(Ordering::Acquire)
    }

    pub fn retained(&self) -> bool {
        self.0.retained.load(Ordering::Acquire)
    }

    fn retain(&self) {
        self.0.retained.store(true, Ordering::Release);
    }

    fn release(&self) {
        let previous = self.0.pending.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
    }
}

struct ParentRef {
    completion: Completion,
    retained: bool,
}

impl ParentRef {
    fn new(completion: &Completion) -> Self {
        completion.0.pending.fetch_add(1, Ordering::AcqRel);
        Self {
            completion: completion.clone(),
            // An unfinished/dropped child must not make its parent deletable.
            retained: true,
        }
    }
}

impl Drop for ParentRef {
    fn drop(&mut self) {
        if self.retained {
            self.completion.retain();
        }
        self.completion.release();
    }
}

struct OwnedHandle<'a, F: FileSystem> {
    fs: &'a F,
    handle: Option<F::Handle>,
}

impl<'a, F: FileSystem> OwnedHandle<'a, F> {
    fn new(fs: &'a F, handle: F::Handle) -> Self {
        Self {
            fs,
            handle: Some(handle),
        }
    }

    fn get(&self) -> &F::Handle {
        self.handle.as_ref().expect("task handle already closed")
    }

    fn close(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.fs.close(handle);
        }
    }
}

impl<F: FileSystem> Drop for OwnedHandle<'_, F> {
    fn drop(&mut self) {
        self.close();
    }
}

/// Moveable unit of work. Dropping it closes its handle and releases references,
/// including on cancellation. A scheduler must not call `step` after Finished.
pub struct DirectoryTask<'a, F: FileSystem> {
    handle: OwnedHandle<'a, F>,
    path: PathBuf,
    // None means the pinned root: it has no disposition path.
    info: Option<BasicInfo>,
    entries: Option<std::vec::IntoIter<Entry>>,
    enumerating: bool,
    completion: Completion,
    parent: Option<ParentRef>,
    rescans: u8,
    readonly_cleared: bool,
    finished: bool,
}

impl<F: FileSystem> DirectoryTask<'_, F> {
    pub fn completion(&self) -> Completion {
        self.completion.clone()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_root(&self) -> bool {
        self.info.is_none()
    }

    fn end_enumeration(&mut self) {
        if self.enumerating {
            self.entries = None;
            self.enumerating = false;
            self.completion.release();
        }
    }

    fn finish(&mut self, retained: bool) {
        self.end_enumeration();
        debug_assert_eq!(self.completion.pending(), 0);
        // In particular, close classic-delete handles before waking the parent.
        self.handle.close();
        if let Some(mut parent) = self.parent.take() {
            parent.retained = retained;
        }
        self.finished = true;
    }
}

impl<F: FileSystem> Drop for DirectoryTask<'_, F> {
    fn drop(&mut self) {
        self.handle.close();
        self.end_enumeration();
        // ParentRef's default retains the parent if this task was abandoned.
        self.parent.take();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepStatus {
    Continue,
    /// No work until outstanding children release their completion references.
    Waiting,
    Finished {
        retained: bool,
        /// Only a finished root returns an outcome.
        outcome: Option<Outcome>,
    },
}

/// One entry (or directory completion) and its worker-local reporting delta.
pub struct Step<'a, F: FileSystem> {
    pub status: StepStatus,
    /// A child directory to schedule; the parent may continue enumerating.
    pub child: Option<DirectoryTask<'a, F>>,
    pub delta: CounterDelta,
    pub error: Option<ErrorRecord>,
}

impl<F: FileSystem> Step<'_, F> {
    fn new() -> Self {
        Self {
            status: StepStatus::Continue,
            child: None,
            delta: CounterDelta::default(),
            error: None,
        }
    }

    fn record_error(&mut self, path: &Path, error: FsError) -> bool {
        let record = self
            .delta
            .record_error(path.to_string_lossy().into_owned(), error.code());
        let retained = record.disposition != "absent";
        self.error = Some(record);
        retained
    }
}

pub struct Engine<'a, F: FileSystem> {
    fs: &'a F,
    policy: Policy,
    now: Duration,
}

impl<'a, F: FileSystem> Engine<'a, F> {
    /// `clock.now()` must be time since UNIX_EPOCH for age comparisons, rather
    /// than an arbitrary monotonic origin. Sample once for a stable run cutoff.
    pub fn new(fs: &'a F, clock: &impl Clock, policy: Policy) -> Self {
        Self {
            fs,
            policy,
            now: clock.now(),
        }
    }

    /// Takes ownership of the validated root handle; it is closed, never disposed.
    /// `path` is used only in error records, never to resolve filesystem objects.
    pub fn root_task(&self, root: F::Handle, path: impl Into<PathBuf>) -> DirectoryTask<'a, F> {
        DirectoryTask {
            handle: OwnedHandle::new(self.fs, root),
            path: path.into(),
            info: None,
            entries: None,
            enumerating: true,
            completion: Completion::new(),
            parent: None,
            rescans: 0,
            readonly_cleared: false,
            finished: false,
        }
    }

    /// Serial explicit-stack postorder driver; there is no recursive traversal
    /// or recursive parent destruction, even for very deep trees.
    pub fn run(&self, root: F::Handle, path: impl Into<PathBuf>) -> RunResult {
        let mut stack = vec![self.root_task(root, path)];
        let mut counters = Counters::default();
        let mut errors = Vec::new();
        let mut errors_truncated = false;
        loop {
            let step = self.step(stack.last_mut().expect("root still pending"));
            counters.merge(&step.delta);
            if let Some(error) = step.error {
                if errors.len() < MAX_ERRORS {
                    errors.push(error);
                } else {
                    errors_truncated = true;
                }
            }
            if let Some(child) = step.child {
                stack.push(child);
            }
            match step.status {
                StepStatus::Continue => {}
                StepStatus::Waiting => unreachable!("serial children finish before their parent"),
                StepStatus::Finished { outcome, .. } => {
                    stack.pop();
                    if let Some(outcome) = outcome {
                        return RunResult {
                            outcome,
                            counters,
                            errors,
                            errors_truncated,
                        };
                    }
                }
            }
        }
    }

    /// Processes at most one child name or completes the directory. The caller
    /// owns `task` exclusively; child tasks can run on different workers.
    pub fn step(&self, task: &mut DirectoryTask<'a, F>) -> Step<'a, F> {
        assert!(!task.finished, "cannot step a finished directory");
        let mut step = Step::new();
        if task.enumerating {
            if task.entries.is_none() {
                match self.fs.enumerate(task.handle.get()) {
                    Ok(entries) => task.entries = Some(entries.into_iter()),
                    Err(error) => {
                        let retained = step.record_error(&task.path, error);
                        let outcome = task.is_root().then_some(Outcome::FailedToEnumerate);
                        task.finish(retained);
                        step.status = StepStatus::Finished { retained, outcome };
                        return step;
                    }
                }
            }
            if let Some(entry) = task.entries.as_mut().unwrap().next() {
                self.child(task, entry, &mut step);
                return step;
            }
            task.end_enumeration();
        }
        if task.completion.pending() != 0 {
            step.status = StepStatus::Waiting;
            return step;
        }
        let mut retained = task.completion.retained();
        let outcome = if task.is_root() {
            // This fresh enumeration, not counters or the initial snapshot,
            // determines whether the pinned root is actually empty.
            Some(match self.fs.enumerate(task.handle.get()) {
                Ok(entries) => Outcome::Completed {
                    fully_empty: entries.is_empty(),
                },
                Err(error) => {
                    retained |= step.record_error(&task.path, error);
                    Outcome::FailedToEnumerate
                }
            })
        } else {
            if !retained {
                let info = task.info.as_ref().unwrap();
                match self.remove(task.handle.get(), info, &mut task.readonly_cleared) {
                    Ok(classic) => count_removed(&mut step.delta, info, classic),
                    Err(FsError::DirectoryNotEmpty) if task.rescans < 2 => {
                        task.rescans += 1;
                        task.enumerating = true;
                        task.completion.0.pending.fetch_add(1, Ordering::AcqRel);
                        return step;
                    }
                    Err(error) => retained |= step.record_error(&task.path, error),
                }
            }
            None
        };
        task.finish(retained);
        step.status = StepStatus::Finished { retained, outcome };
        step
    }

    fn child(&self, task: &mut DirectoryTask<'a, F>, entry: Entry, step: &mut Step<'a, F>) {
        let path = task.path.join(&entry.name);
        // Hints are deliberately ignored, including for access selection: the
        // opened object can differ from the enumerated one.
        let handle = match self.fs.open_child_no_follow(
            task.handle.get(),
            &entry.name,
            crate::fs::Access::DIRECTORY,
        ) {
            Ok(handle) => OwnedHandle::new(self.fs, handle),
            Err(error) => {
                if step.record_error(&path, error) {
                    task.completion.retain();
                }
                return;
            }
        };
        let info = match self.fs.basic_info(handle.get()) {
            Ok(info) => info,
            Err(error) => {
                if step.record_error(&path, error) {
                    task.completion.retain();
                }
                return;
            }
        };
        if !is_link(&info) && info.attributes & DIRECTORY != 0 {
            step.child = Some(DirectoryTask {
                handle,
                path,
                info: Some(info),
                entries: None,
                enumerating: true,
                completion: Completion::new(),
                parent: Some(ParentRef::new(&task.completion)),
                rescans: 0,
                readonly_cleared: false,
                finished: false,
            });
        } else if self.excluded(&info) {
            step.delta.excluded += 1;
            task.completion.retain();
        } else {
            match self.remove(handle.get(), &info, &mut false) {
                Ok(classic) => count_removed(&mut step.delta, &info, classic),
                Err(error) => {
                    if step.record_error(&path, error) {
                        task.completion.retain();
                    }
                }
            }
        }
    }

    fn excluded(&self, info: &BasicInfo) -> bool {
        if self.policy.min_age.is_zero() {
            return false;
        }
        let newest = info
            .creation_time
            .max(info.last_write_time)
            .max(info.change_time);
        // Comparing ages avoids an overflowing/underflowing SystemTime cutoff,
        // and preserves both the exact threshold and pre-epoch timestamps.
        let age = match newest.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(time) => match self.now.checked_sub(time) {
                Some(age) => age,
                None => return true,
            },
            Err(before_epoch) => self.now.saturating_add(before_epoch.duration()),
        };
        age < self.policy.min_age
    }

    fn remove(
        &self,
        handle: &F::Handle,
        info: &BasicInfo,
        readonly_cleared: &mut bool,
    ) -> Result<bool, FsError> {
        if self.policy.dry_run {
            return Ok(false);
        }
        let mut flags = DispFlags {
            posix: true,
            ignore_readonly: true,
            classic: false,
        };
        loop {
            match self.fs.set_disposition(handle, flags) {
                Ok(()) => return Ok(flags.classic),
                Err(FsError::NotSupported(_)) if !flags.classic => {
                    flags = if flags.ignore_readonly {
                        DispFlags {
                            ignore_readonly: false,
                            ..flags
                        }
                    } else {
                        DispFlags {
                            classic: true,
                            ..DispFlags::default()
                        }
                    };
                }
                Err(FsError::AccessDenied)
                    if info.attributes & READONLY != 0 && !*readonly_cleared =>
                {
                    *readonly_cleared = true;
                    self.fs.clear_readonly(handle)?;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn is_link(info: &BasicInfo) -> bool {
    info.reparse_tag.is_some() || info.attributes & REPARSE_POINT != 0
}

fn count_removed(delta: &mut CounterDelta, info: &BasicInfo, classic: bool) {
    if classic {
        // Acknowledged classic delete-pending is not confirmed name removal.
        delta.classic_pending_unconfirmed += 1;
    } else if is_link(info) {
        delta.links_removed += 1;
    } else if info.attributes & DIRECTORY != 0 {
        delta.dirs_removed += 1;
    } else {
        delta.files_removed += 1;
        delta.logical_bytes_removed += info.size;
    }
}

#[cfg(test)]
mod tests;
