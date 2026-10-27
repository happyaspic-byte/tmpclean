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
//! need only be `Send`, not `Sync`: one directory owns the cursor, and its name
//! batches serialize relative opens through the pinned parent handle.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::fs::{
    BasicInfo, DispFlags, Entry, EntryBatch, FileSystem, FsError, DIRECTORY, READONLY,
    REPARSE_POINT,
};
use crate::policy::{Policy, Refusal};
use crate::report::{Clock, CounterDelta, Counters, ErrorRecord, Outcome, MAX_ERRORS};

// ERROR_INVALID_DATA: an internal traversal/completion protocol violation.
const INVALID_STATE: FsError = FsError::Other(13);

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
    invalid: AtomicBool,
    references: Arc<AtomicUsize>,
}

/// Read-only scheduler view of a directory's outstanding completion references.
///
/// Cloning this observer does not acquire a completion reference. After every
/// child has finished or been dropped, `pending()` becomes zero if enumeration
/// has also finished. A retained descendant prevents ancestor disposition.
#[derive(Clone)]
pub struct Completion(Arc<CompletionState>);

impl Completion {
    fn new(references: &Arc<AtomicUsize>) -> Self {
        references.fetch_add(1, Ordering::AcqRel);
        Self(Arc::new(CompletionState {
            pending: AtomicUsize::new(1),
            retained: AtomicBool::new(false),
            invalid: AtomicBool::new(false),
            references: references.clone(),
        }))
    }

    fn acquire(&self) {
        self.0.references.fetch_add(1, Ordering::AcqRel);
        self.0.pending.fetch_add(1, Ordering::AcqRel);
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
        if self
            .0
            .pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                pending.checked_sub(1)
            })
            .is_err()
        {
            // Drop cannot return an error; the owning task reports this state.
            self.0.invalid.store(true, Ordering::Release);
            self.retain();
        } else {
            self.0.references.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

struct ParentRef {
    completion: Completion,
    retained: bool,
}

impl ParentRef {
    fn new(completion: &Completion) -> Self {
        completion.acquire();
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

    fn get(&self) -> Result<&F::Handle, FsError> {
        match self.handle.as_ref() {
            Some(handle) => Ok(handle),
            None => Err(FsError::Other(6)), // ERROR_INVALID_HANDLE
        }
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

/// Name batches share only their pinned parent handle. The short lock covers
/// relative opens/enumeration, never child dispositions or descendant waits.
struct SharedHandle<'a, F: FileSystem>(Option<Arc<Mutex<OwnedHandle<'a, F>>>>);

impl<'a, F: FileSystem> SharedHandle<'a, F> {
    fn new(handle: OwnedHandle<'a, F>) -> Self {
        Self(Some(Arc::new(Mutex::new(handle))))
    }

    fn share(&self) -> Self {
        Self(self.0.clone())
    }

    fn with<T>(
        &self,
        operation: impl FnOnce(&F::Handle) -> Result<T, FsError>,
    ) -> Result<T, FsError> {
        let shared = self.0.as_ref().ok_or(FsError::Other(6))?;
        let handle = shared.lock().map_err(|_| INVALID_STATE)?;
        operation(handle.get()?)
    }

    fn close(&mut self) {
        self.0.take();
    }
}

/// Moveable unit of work. Dropping it closes its handle and releases references,
/// including on cancellation. Stepping a finished task reports an invariant error.
pub struct DirectoryTask<'a, F: FileSystem> {
    handle: SharedHandle<'a, F>,
    path: PathBuf,
    // None means a pinned root or a name batch: neither has a disposition path.
    info: Option<BasicInfo>,
    entries: Option<EntryBatch>,
    cursor: F::Cursor,
    enumeration_failed: bool,
    batch: bool,
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
        self.info.is_none() && !self.batch
    }

    pub(crate) fn is_batch(&self) -> bool {
        self.batch
    }

    pub(crate) fn ready(&self) -> bool {
        self.enumerating || self.completion.pending() == 0
    }

    fn end_enumeration(&mut self) {
        if self.enumerating {
            self.entries = None;
            self.enumerating = false;
            self.completion.release();
        }
    }

    fn finish(&mut self, retained: bool) -> Result<(), FsError> {
        self.end_enumeration();
        let invalid =
            self.completion.pending() != 0 || self.completion.0.invalid.load(Ordering::Acquire);
        if invalid {
            self.completion.retain();
        }
        // In particular, close classic-delete handles before waking the parent.
        self.handle.close();
        if let Some(mut parent) = self.parent.take() {
            parent.retained = retained || invalid;
        }
        self.finished = true;
        if invalid {
            Err(INVALID_STATE)
        } else {
            Ok(())
        }
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
    /// A child directory (or scheduler name batch); its parent may continue.
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

    fn finish(
        &mut self,
        task: &mut DirectoryTask<'_, F>,
        mut retained: bool,
        mut outcome: Option<Outcome>,
    ) {
        if let Err(error) = task.finish(retained) {
            // Preserve an operation error already recorded for this entry.
            if self.error.is_none() {
                self.record_error(&task.path, error);
            }
            retained = true;
            outcome = task.is_root().then_some(Outcome::FailedToEnumerate);
        }
        self.status = StepStatus::Finished { retained, outcome };
    }
}

pub struct Engine<'a, F: FileSystem> {
    fs: &'a F,
    policy: Policy,
    now: Duration,
    references: Arc<AtomicUsize>,
}

impl<'a, F: FileSystem> Engine<'a, F> {
    /// `clock.now()` must be time since UNIX_EPOCH for age comparisons, rather
    /// than an arbitrary monotonic origin. Sample once for a stable run cutoff.
    pub fn new(fs: &'a F, clock: &impl Clock, policy: Policy) -> Result<Self, Refusal> {
        policy.check()?;
        Ok(Self {
            fs,
            policy,
            now: clock.now(),
            references: Arc::default(),
        })
    }

    /// Takes ownership of the validated root handle; it is closed, never disposed.
    /// `path` is used only in error records, never to resolve filesystem objects.
    pub fn root_task(&self, root: F::Handle, path: impl Into<PathBuf>) -> DirectoryTask<'a, F> {
        DirectoryTask {
            handle: SharedHandle::new(OwnedHandle::new(self.fs, root)),
            path: path.into(),
            info: None,
            entries: None,
            cursor: F::Cursor::default(),
            enumeration_failed: false,
            batch: false,
            enumerating: true,
            completion: Completion::new(&self.references),
            parent: None,
            rescans: 0,
            readonly_cleared: false,
            finished: false,
        }
    }

    /// Serial explicit-stack postorder driver; there is no recursive traversal
    /// or recursive parent destruction, even for very deep trees.
    pub fn run(&self, root: F::Handle, path: impl Into<PathBuf>) -> RunResult {
        let path = path.into();
        let mut stack = vec![self.root_task(root, path.clone())];
        let mut counters = Counters::default();
        let mut errors = Vec::new();
        let mut errors_truncated = false;
        loop {
            let mut step = Step::new();
            match stack.last_mut() {
                Some(task) => {
                    step = self.step(task);
                    if step.status == StepStatus::Waiting {
                        // The serial driver has no outstanding worker to await.
                        step.record_error(&task.path, INVALID_STATE);
                        step.status = StepStatus::Finished {
                            retained: true,
                            outcome: Some(Outcome::FailedToEnumerate),
                        };
                    }
                }
                None => {
                    step.record_error(&path, INVALID_STATE);
                    step.status = StepStatus::Finished {
                        retained: true,
                        outcome: Some(Outcome::FailedToEnumerate),
                    };
                }
            }
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
            if let StepStatus::Finished { outcome, .. } = step.status {
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

    /// Processes at most one child name or completes the directory. The caller
    /// owns `task` exclusively; child tasks can run on different workers.
    pub fn step(&self, task: &mut DirectoryTask<'a, F>) -> Step<'a, F> {
        self.step_inner(task, false)
    }

    /// Scheduler entry point: detach whole name batches before processing leaves.
    /// A batch owns exactly one parent reference, not one task/reference per file.
    pub(crate) fn step_batch(&self, task: &mut DirectoryTask<'a, F>) -> Step<'a, F> {
        self.step_inner(task, true)
    }

    pub fn pending_references(&self) -> usize {
        self.references.load(Ordering::Acquire)
    }

    fn step_inner(&self, task: &mut DirectoryTask<'a, F>, batching: bool) -> Step<'a, F> {
        let mut step = Step::new();
        if task.finished || task.completion.0.invalid.load(Ordering::Acquire) {
            step.record_error(&task.path, INVALID_STATE);
            let outcome = task.is_root().then_some(Outcome::FailedToEnumerate);
            step.finish(task, true, outcome);
            return step;
        }
        if task.enumerating {
            if !task.batch && task.entries.as_ref().is_none_or(EntryBatch::is_empty) {
                task.entries = None;
                match task
                    .handle
                    .with(|handle| self.fs.enumerate_batch(handle, &mut task.cursor))
                {
                    Ok(entries) => task.entries = Some(entries),
                    Err(error) => {
                        step.record_error(&task.path, error);
                        task.enumeration_failed = true;
                        task.completion.retain();
                        task.end_enumeration();
                    }
                }
            }
            if batching
                && !task.batch
                && task
                    .entries
                    .as_ref()
                    .is_some_and(|entries| !entries.is_empty())
            {
                step.child = Some(DirectoryTask {
                    handle: task.handle.share(),
                    path: task.path.clone(),
                    info: None,
                    entries: task.entries.take(),
                    cursor: F::Cursor::default(),
                    enumeration_failed: false,
                    batch: true,
                    enumerating: true,
                    completion: Completion::new(&self.references),
                    parent: Some(ParentRef::new(&task.completion)),
                    rescans: 0,
                    readonly_cleared: false,
                    finished: false,
                });
                return step;
            }
            let entry = match task.entries.as_mut() {
                Some(entries) => entries.next(),
                None => None,
            };
            if let Some(entry) = entry {
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
        if task.batch {
            step.finish(task, retained, None);
            return step;
        }
        let outcome = match task.info.as_ref() {
            None => {
                if task.enumeration_failed {
                    step.finish(task, true, Some(Outcome::FailedToEnumerate));
                    return step;
                }
                // This fresh enumeration, not counters or the initial snapshot,
                // determines whether the pinned root is actually empty.
                let entries = task
                    .handle
                    .with(|handle| self.fs.enumerate_batch(handle, &mut F::Cursor::default()));
                Some(match entries {
                    Ok(entries) => Outcome::Completed {
                        fully_empty: entries.is_empty(),
                    },
                    Err(error) => {
                        retained |= step.record_error(&task.path, error);
                        Outcome::FailedToEnumerate
                    }
                })
            }
            Some(info) => {
                if !retained {
                    match task
                        .handle
                        .with(|handle| self.remove(handle, info, &mut task.readonly_cleared))
                    {
                        Ok(classic) => count_removed(&mut step.delta, info, classic),
                        Err(FsError::DirectoryNotEmpty) if task.rescans < 2 => {
                            task.rescans += 1;
                            task.enumerating = true;
                            task.cursor = F::Cursor::default();
                            task.completion.acquire();
                            return step;
                        }
                        Err(error) => retained |= step.record_error(&task.path, error),
                    }
                }
                None
            }
        };
        step.finish(task, retained, outcome);
        step
    }

    fn child(&self, task: &mut DirectoryTask<'a, F>, entry: Entry, step: &mut Step<'a, F>) {
        let path = task.path.join(&entry.name);
        // Hints are deliberately ignored, including for access selection: the
        // opened object can differ from the enumerated one.
        let handle = match task.handle.with(|parent| {
            self.fs
                .open_child_no_follow(parent, &entry.name, crate::fs::Access::DIRECTORY)
        }) {
            Ok(handle) => OwnedHandle::new(self.fs, handle),
            Err(error) => {
                if step.record_error(&path, error) {
                    task.completion.retain();
                }
                return;
            }
        };
        let info = match handle.get().and_then(|handle| self.fs.basic_info(handle)) {
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
                handle: SharedHandle::new(handle),
                path,
                info: Some(info),
                entries: None,
                cursor: F::Cursor::default(),
                enumeration_failed: false,
                batch: false,
                enumerating: true,
                completion: Completion::new(&self.references),
                parent: Some(ParentRef::new(&task.completion)),
                rescans: 0,
                readonly_cleared: false,
                finished: false,
            });
        } else if self.excluded(&info) {
            step.delta.excluded += 1;
            task.completion.retain();
        } else {
            match handle
                .get()
                .and_then(|handle| self.remove(handle, &info, &mut false))
            {
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
