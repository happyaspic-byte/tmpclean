use super::*;
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

static NEXT_FS: AtomicU64 = AtomicU64::new(1);

/// Owned, non-cloneable handle. Closing it invalidates its slot, not the node.
#[derive(Debug, Eq, PartialEq)]
pub struct FakeHandle {
    owner: u64,
    slot: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Open,
    Enumerate,
    BasicInfo,
    SetDisposition,
    ClearReadonly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureMode {
    Once,
    Persistent,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FakeStats {
    pub disposition_calls: usize,
    pub clear_readonly_calls: usize,
    pub current_dispositions: usize,
    pub max_concurrent_dispositions: usize,
    pub enumerated_directories: Vec<PathBuf>,
    pub in_flight_entries: usize,
    pub peak_in_flight_entries: usize,
}

#[derive(Default)]
pub struct FakeCursor {
    pub(crate) started: bool,
    after: Option<OsString>,
    done: bool,
}

struct Node {
    path: PathBuf,
    parent: Option<(u64, OsString)>,
    children: BTreeMap<OsString, u64>,
    info: BasicInfo,
    locked: bool,
    denied: bool,
    // Kept as fixture metadata only. No operation resolves a reparse target.
    _target: Option<PathBuf>,
}

struct Failure {
    path: PathBuf,
    operation: Operation,
    error: FsError,
    mode: FailureMode,
}

struct State {
    next_node: u64,
    next_handle: u64,
    nodes: HashMap<u64, Node>,
    handles: HashMap<u64, (u64, Access)>,
    failures: Vec<Failure>,
    enumerated: Vec<PathBuf>,
    caps: VolumeCaps,
    disposition_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// A tree with explicit handles and atomic instrumentation.
///
/// Builder paths are relative to the fake root (or start with `/`). Builders
/// require existing parents. Reparse targets may exist anywhere in this tree;
/// they are never reachable by opening or enumerating the link.
pub struct FakeFs {
    owner: u64,
    state: Mutex<State>,
    dispositions: AtomicUsize,
    clear_readonly: AtomicUsize,
    current: AtomicUsize,
    maximum: AtomicUsize,
    entry_metrics: Arc<EntryMetrics>,
}

impl Default for FakeFs {
    fn default() -> Self {
        let root = Node {
            path: PathBuf::from("/"),
            parent: None,
            children: BTreeMap::new(),
            info: BasicInfo {
                attributes: DIRECTORY,
                ..BasicInfo::default()
            },
            locked: false,
            denied: false,
            _target: None,
        };
        Self {
            owner: NEXT_FS.fetch_add(1, Ordering::Relaxed),
            state: Mutex::new(State {
                next_node: 1,
                next_handle: 0,
                nodes: HashMap::from([(0, root)]),
                handles: HashMap::new(),
                failures: Vec::new(),
                enumerated: Vec::new(),
                caps: VolumeCaps::default(),
                disposition_hook: None,
            }),
            dispositions: AtomicUsize::new(0),
            clear_readonly: AtomicUsize::new(0),
            current: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            entry_metrics: Arc::default(),
        }
    }
}

impl FakeFs {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("fake filesystem mutex poisoned")
    }

    fn handle(&self, state: &mut State, node: u64, access: Access) -> FakeHandle {
        let slot = state.next_handle;
        state.next_handle += 1;
        state.handles.insert(slot, (node, access));
        FakeHandle {
            owner: self.owner,
            slot,
        }
    }

    fn node(&self, state: &State, handle: &FakeHandle) -> Result<(u64, Access), FsError> {
        if handle.owner != self.owner {
            return Err(FsError::Other(6)); // ERROR_INVALID_HANDLE
        }
        state
            .handles
            .get(&handle.slot)
            .copied()
            .ok_or(FsError::Other(6))
    }

    pub fn root_handle(&self) -> FakeHandle {
        self.handle(&mut self.state(), 0, Access::DIRECTORY)
    }

    fn components(path: &Path) -> Result<Vec<OsString>, FsError> {
        path.components()
            .filter(|c| !matches!(c, Component::RootDir))
            .map(|c| match c {
                Component::Normal(name) if Self::valid_name(name) => Ok(name.to_os_string()),
                _ => Err(FsError::Other(123)), // ERROR_INVALID_NAME
            })
            .collect()
    }

    fn valid_name(name: &OsStr) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name
                .as_encoded_bytes()
                .iter()
                .any(|c| matches!(c, b'/' | b'\\' | 0))
    }

    fn lookup(state: &State, components: &[OsString]) -> Result<u64, FsError> {
        let mut id = 0;
        for component in components {
            let node = &state.nodes[&id];
            if node.info.reparse_tag.is_some()
                || node.info.attributes & REPARSE_POINT != 0
                || node.info.attributes & DIRECTORY == 0
            {
                return Err(FsError::NotFound(3));
            }
            id = *node.children.get(component).ok_or(FsError::NotFound(2))?;
        }
        Ok(id)
    }

    fn add(
        &self,
        path: &Path,
        info: BasicInfo,
        target: Option<PathBuf>,
    ) -> Result<FakeHandle, FsError> {
        let components = Self::components(path)?;
        let (name, parents) = components.split_last().ok_or(FsError::AccessDenied)?;
        let mut state = self.state();
        let parent = Self::lookup(&state, parents)?;
        let parent_node = &state.nodes[&parent];
        if parent_node.info.attributes & DIRECTORY == 0
            || parent_node.info.attributes & REPARSE_POINT != 0
            || parent_node.info.reparse_tag.is_some()
        {
            return Err(FsError::NotFound(3));
        }
        if parent_node.children.contains_key(name) {
            return Err(FsError::Other(183)); // ERROR_ALREADY_EXISTS
        }
        let path = parent_node.path.join(name);
        let id = state.next_node;
        state.next_node += 1;
        state.nodes.insert(
            id,
            Node {
                path,
                parent: Some((parent, name.clone())),
                children: BTreeMap::new(),
                info,
                locked: false,
                denied: false,
                _target: target,
            },
        );
        state
            .nodes
            .get_mut(&parent)
            .unwrap()
            .children
            .insert(name.clone(), id);
        Ok(self.handle(&mut state, id, Access::DIRECTORY))
    }

    pub fn add_dir(&self, path: impl AsRef<Path>) -> Result<FakeHandle, FsError> {
        self.add(
            path.as_ref(),
            BasicInfo {
                attributes: DIRECTORY,
                ..BasicInfo::default()
            },
            None,
        )
    }

    /// `info` supplies size, times, and attributes; this always creates a plain file.
    pub fn add_file(
        &self,
        path: impl AsRef<Path>,
        mut info: BasicInfo,
    ) -> Result<FakeHandle, FsError> {
        info.attributes &= !(DIRECTORY | REPARSE_POINT);
        info.reparse_tag = None;
        self.add(path.as_ref(), info, None)
    }

    pub fn add_reparse(
        &self,
        path: impl AsRef<Path>,
        tag: ReparseTag,
        target: impl AsRef<Path>,
    ) -> Result<FakeHandle, FsError> {
        self.add(
            path.as_ref(),
            BasicInfo {
                attributes: REPARSE_POINT
                    | if tag == ReparseTag::FileSymlink {
                        0
                    } else {
                        DIRECTORY
                    },
                reparse_tag: Some(tag),
                ..BasicInfo::default()
            },
            Some(target.as_ref().to_path_buf()),
        )
    }

    /// Override metadata to model enumeration/open races or custom age fixtures.
    pub fn set_basic_info(&self, handle: &FakeHandle, info: BasicInfo) -> Result<(), FsError> {
        let mut state = self.state();
        let (id, _) = self.node(&state, handle)?;
        state.nodes.get_mut(&id).unwrap().info = info;
        Ok(())
    }

    pub fn set_locked(&self, handle: &FakeHandle, locked: bool) -> Result<(), FsError> {
        let mut state = self.state();
        let (id, _) = self.node(&state, handle)?;
        state.nodes.get_mut(&id).unwrap().locked = locked;
        Ok(())
    }

    pub fn set_denied(&self, handle: &FakeHandle, denied: bool) -> Result<(), FsError> {
        let mut state = self.state();
        let (id, _) = self.node(&state, handle)?;
        state.nodes.get_mut(&id).unwrap().denied = denied;
        Ok(())
    }

    pub fn inject_failure(
        &self,
        path: impl AsRef<Path>,
        operation: Operation,
        error: FsError,
        mode: FailureMode,
    ) -> Result<(), FsError> {
        let components = Self::components(path.as_ref())?;
        let path = components.iter().fold(PathBuf::from("/"), |p, c| p.join(c));
        self.state().failures.push(Failure {
            path,
            operation,
            error,
            mode,
        });
        Ok(())
    }

    fn check(state: &mut State, path: &Path, operation: Operation) -> Result<(), FsError> {
        if let Some(index) = state
            .failures
            .iter()
            .position(|f| f.path == path && f.operation == operation)
        {
            let error = state.failures[index].error;
            if state.failures[index].mode == FailureMode::Once {
                state.failures.remove(index);
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn set_volume_caps(&self, caps: VolumeCaps) {
        self.state().caps = caps;
    }

    /// Called after counting an in-flight disposition and outside the tree lock.
    /// Tests can use barriers or channels here; no timing delays are needed.
    pub fn set_disposition_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.state().disposition_hook = hook;
    }

    pub fn stats(&self) -> FakeStats {
        FakeStats {
            disposition_calls: self.dispositions.load(Ordering::SeqCst),
            clear_readonly_calls: self.clear_readonly.load(Ordering::SeqCst),
            current_dispositions: self.current.load(Ordering::SeqCst),
            max_concurrent_dispositions: self.maximum.load(Ordering::SeqCst),
            enumerated_directories: self.state().enumerated.clone(),
            in_flight_entries: self.entry_metrics.current.load(Ordering::SeqCst),
            peak_in_flight_entries: self.entry_metrics.peak.load(Ordering::SeqCst),
        }
    }
}

struct InFlight<'a>(&'a AtomicUsize);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl FileSystem for FakeFs {
    type Handle = FakeHandle;
    type Cursor = FakeCursor;

    fn open_child_no_follow(
        &self,
        parent: &FakeHandle,
        name: &OsStr,
        access: Access,
    ) -> Result<FakeHandle, FsError> {
        if !Self::valid_name(name) {
            return Err(FsError::Other(123));
        }
        let mut state = self.state();
        let (parent, _) = self.node(&state, parent)?;
        let node = &state.nodes[&parent];
        if node.denied {
            return Err(FsError::AccessDenied);
        }
        if node.info.attributes & DIRECTORY == 0
            || node.info.attributes & REPARSE_POINT != 0
            || node.info.reparse_tag.is_some()
        {
            return Err(FsError::NotFound(3));
        }
        let path = node.path.join(name);
        Self::check(&mut state, &path, Operation::Open)?;
        let id = *state.nodes[&parent]
            .children
            .get(name)
            .ok_or(FsError::NotFound(2))?;
        let node = &state.nodes[&id];
        if node.denied {
            return Err(FsError::AccessDenied);
        }
        if node.locked && access.delete {
            return Err(FsError::SharingViolation);
        }
        Ok(self.handle(&mut state, id, access))
    }

    fn enumerate_batch(
        &self,
        directory: &FakeHandle,
        cursor: &mut FakeCursor,
    ) -> Result<EntryBatch, FsError> {
        if cursor.done {
            return Ok(EntryBatch::new(Vec::new()));
        }
        let mut state = self.state();
        let (id, access) = self.node(&state, directory)?;
        let path = state.nodes[&id].path.clone();
        Self::check(&mut state, &path, Operation::Enumerate)?;
        let node = &state.nodes[&id];
        if node.denied || !access.list_directory {
            return Err(FsError::AccessDenied);
        }
        if node.info.attributes & DIRECTORY == 0
            || node.info.attributes & REPARSE_POINT != 0
            || node.info.reparse_tag.is_some()
        {
            return Err(FsError::Other(267)); // ERROR_DIRECTORY; never resolves the target
        }
        let start = match &cursor.after {
            Some(name) => std::ops::Bound::Excluded(name.clone()),
            None => std::ops::Bound::Unbounded,
        };
        let entries: Vec<_> = node
            .children
            .range((start, std::ops::Bound::Unbounded))
            .take(ENUM_BATCH_SIZE)
            .map(|(name, id)| {
                let info = &state.nodes[id].info;
                Entry {
                    name: name.clone(),
                    is_dir_hint: info.attributes & DIRECTORY != 0,
                    reparse_tag_hint: info.reparse_tag,
                }
            })
            .collect();
        if !cursor.started {
            state.enumerated.push(path);
        }
        cursor.started = true;
        cursor.done = entries.len() < ENUM_BATCH_SIZE;
        if let Some(entry) = entries.last() {
            cursor.after = Some(entry.name.clone());
        }
        Ok(EntryBatch::tracked(entries, self.entry_metrics.clone()))
    }

    fn basic_info(&self, handle: &FakeHandle) -> Result<BasicInfo, FsError> {
        let mut state = self.state();
        let (id, access) = self.node(&state, handle)?;
        let path = state.nodes[&id].path.clone();
        Self::check(&mut state, &path, Operation::BasicInfo)?;
        let node = &state.nodes[&id];
        if node.denied || !access.read_attributes {
            return Err(FsError::AccessDenied);
        }
        Ok(node.info.clone())
    }

    fn set_disposition(&self, handle: &FakeHandle, flags: DispFlags) -> Result<(), FsError> {
        self.dispositions.fetch_add(1, Ordering::SeqCst);
        let current = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        let _in_flight = InFlight(&self.current);
        self.maximum.fetch_max(current, Ordering::SeqCst);
        let hook = self.state().disposition_hook.clone();
        if let Some(hook) = hook {
            hook();
        }
        let mut state = self.state();
        let (id, access) = self.node(&state, handle)?;
        let path = state.nodes[&id].path.clone();
        Self::check(&mut state, &path, Operation::SetDisposition)?;
        let node = &state.nodes[&id];
        if node.denied || !access.delete {
            return Err(FsError::AccessDenied);
        }
        if node.locked {
            return Err(FsError::SharingViolation);
        }
        if node.info.attributes & READONLY != 0 && !flags.ignore_readonly {
            return Err(FsError::AccessDenied);
        }
        if !node.children.is_empty() {
            return Err(FsError::DirectoryNotEmpty);
        }
        let (parent, name) = node.parent.clone().ok_or(FsError::AccessDenied)?;
        let children = &mut state.nodes.get_mut(&parent).unwrap().children;
        if children.get(&name) != Some(&id) {
            return Err(FsError::NotFound(2));
        }
        children.remove(&name);
        Ok(())
    }

    fn clear_readonly(&self, handle: &FakeHandle) -> Result<(), FsError> {
        self.clear_readonly.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state();
        let (id, _) = self.node(&state, handle)?;
        let path = state.nodes[&id].path.clone();
        Self::check(&mut state, &path, Operation::ClearReadonly)?;
        let node = state.nodes.get_mut(&id).unwrap();
        if node.denied {
            return Err(FsError::AccessDenied);
        }
        node.info.attributes &= !READONLY;
        Ok(())
    }

    fn close(&self, handle: FakeHandle) {
        if handle.owner == self.owner {
            self.state().handles.remove(&handle.slot);
        }
    }

    fn volume_caps(&self, handle: &FakeHandle) -> VolumeCaps {
        let state = self.state();
        if self.node(&state, handle).is_err() {
            return VolumeCaps::default();
        }
        state.caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn fs_fake_reparse_targets_are_never_reachable() {
        let fs = FakeFs::new();
        let root = fs.root_handle();
        let target = fs.add_dir("outside").unwrap();
        let secret = fs.add_file("outside/secret", BasicInfo::default()).unwrap();
        for (i, tag) in [
            ReparseTag::Junction,
            ReparseTag::DirSymlink,
            ReparseTag::FileSymlink,
            ReparseTag::MountPoint,
            ReparseTag::Unknown(123),
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("link{i}");
            fs.close(fs.add_reparse(&name, tag, "/outside").unwrap());
            let link = fs
                .open_child_no_follow(&root, OsStr::new(&name), Access::DIRECTORY)
                .unwrap();
            assert_eq!(fs.basic_info(&link).unwrap().reparse_tag, Some(tag));
            assert!(fs.enumerate(&link).is_err());
            assert_eq!(
                fs.open_child_no_follow(&link, OsStr::new("secret"), Access::READ),
                Err(FsError::NotFound(3))
            );
            fs.set_disposition(&link, DispFlags::default()).unwrap();
            fs.close(link);
        }
        assert_eq!(fs.basic_info(&secret).unwrap().size, 0);
        assert_eq!(fs.enumerate(&root).unwrap().len(), 1);
        assert_eq!(fs.stats().enumerated_directories, vec![PathBuf::from("/")]);
        assert!(!fs
            .stats()
            .enumerated_directories
            .contains(&PathBuf::from("/outside")));
        fs.close(target);
    }

    #[test]
    fn fs_fake_junction_parent_lists_link() {
        let fs = FakeFs::new();
        let root = fs.root_handle();
        fs.add_dir("target").unwrap();
        fs.add_file("target/secret", BasicInfo::default()).unwrap();
        let link = fs
            .add_reparse("junction", ReparseTag::Junction, "/target")
            .unwrap();
        let entries = fs.enumerate(&root).unwrap();
        assert!(entries
            .iter()
            .any(|e| e.name == "junction" && e.reparse_tag_hint == Some(ReparseTag::Junction)));
        assert!(!entries.iter().any(|e| e.name == "secret"));
        assert!(fs.enumerate(&link).is_err());
        assert_eq!(fs.stats().enumerated_directories, vec![PathBuf::from("/")]);
    }

    #[test]
    fn fs_fake_missing_child_is_notfound() {
        let fs = FakeFs::new();
        let root = fs.root_handle();
        let error = fs
            .open_child_no_follow(&root, OsStr::new("missing"), Access::DELETE)
            .unwrap_err();
        assert_eq!(error, FsError::NotFound(2));
        assert_eq!(error.code(), 2);
        println!("missing child -> {error:?}, Win32 code {}", error.code());
    }

    #[test]
    fn fs_fake_injected_not_supported_is_verbatim() {
        let fs = FakeFs::new();
        let file = fs.add_file("file", BasicInfo::default()).unwrap();
        let expected = FsError::NotSupported(87);
        fs.inject_failure(
            "file",
            Operation::SetDisposition,
            expected,
            FailureMode::Once,
        )
        .unwrap();
        assert_eq!(
            fs.set_disposition(&file, DispFlags::default()),
            Err(expected)
        );
        fs.set_disposition(&file, DispFlags::default()).unwrap();
        fs.inject_failure(
            "/",
            Operation::Enumerate,
            FsError::AccessDenied,
            FailureMode::Persistent,
        )
        .unwrap();
        let root = fs.root_handle();
        for _ in 0..2 {
            assert_eq!(fs.enumerate(&root), Err(FsError::AccessDenied));
        }
        for code in [1, 2, 3, 5, 32, 50, 87, 145, 999] {
            assert_eq!(FsError::from_code(code).code(), code);
        }
    }

    #[test]
    fn fs_fake_locked_and_denied() {
        let fs = FakeFs::new();
        let root = fs.root_handle();
        let file = fs.add_file("locked", BasicInfo::default()).unwrap();
        fs.set_locked(&file, true).unwrap();
        assert_eq!(
            fs.open_child_no_follow(&root, OsStr::new("locked"), Access::DELETE),
            Err(FsError::SharingViolation)
        );
        let read = fs
            .open_child_no_follow(&root, OsStr::new("locked"), Access::READ)
            .unwrap();
        assert!(fs.basic_info(&read).is_ok());
        assert_eq!(
            fs.set_disposition(&file, DispFlags::default()),
            Err(FsError::SharingViolation)
        );
        fs.set_locked(&file, false).unwrap();
        fs.set_denied(&file, true).unwrap();
        assert_eq!(
            fs.open_child_no_follow(&root, OsStr::new("locked"), Access::READ),
            Err(FsError::AccessDenied)
        );
        assert_eq!(
            fs.set_disposition(&file, DispFlags::default()),
            Err(FsError::AccessDenied)
        );
    }

    #[test]
    fn fs_fake_nonempty_directory_disposition() {
        let fs = FakeFs::new();
        let dir = fs.add_dir("dir").unwrap();
        fs.add_file("dir/child", BasicInfo::default()).unwrap();
        assert_eq!(
            fs.set_disposition(&dir, DispFlags::default()),
            Err(FsError::DirectoryNotEmpty)
        );
    }

    #[test]
    fn fs_fake_long_name_roundtrips() {
        let fs = FakeFs::new();
        let root = fs.root_handle();
        let name = "x".repeat(300);
        fs.add_file(&name, BasicInfo::default()).unwrap();
        assert_eq!(fs.enumerate(&root).unwrap()[0].name, OsStr::new(&name));
        let file = fs
            .open_child_no_follow(&root, OsStr::new(&name), Access::DELETE)
            .unwrap();
        fs.set_disposition(&file, DispFlags::default()).unwrap();
        assert!(fs.enumerate(&root).unwrap().is_empty());
    }

    #[test]
    fn fs_fake_three_level_roundtrip() {
        let fs = FakeFs::new();
        let root = fs.root_handle();
        fs.close(fs.add_dir("one").unwrap());
        fs.close(fs.add_dir("one/two").unwrap());
        let info = BasicInfo {
            size: 4096,
            creation_time: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            last_write_time: SystemTime::UNIX_EPOCH + Duration::from_secs(20),
            change_time: SystemTime::UNIX_EPOCH + Duration::from_secs(30),
            ..BasicInfo::default()
        };
        fs.close(fs.add_file("one/two/file", info.clone()).unwrap());
        let one = fs
            .open_child_no_follow(
                &root,
                &fs.enumerate(&root).unwrap()[0].name,
                Access::DIRECTORY,
            )
            .unwrap();
        let two = fs
            .open_child_no_follow(
                &one,
                &fs.enumerate(&one).unwrap()[0].name,
                Access::DIRECTORY,
            )
            .unwrap();
        let file = fs
            .open_child_no_follow(&two, &fs.enumerate(&two).unwrap()[0].name, Access::DELETE)
            .unwrap();
        assert_eq!(fs.basic_info(&file).unwrap(), info);
        for handle in [file, two, one] {
            fs.set_disposition(&handle, DispFlags::default()).unwrap();
            fs.close(handle);
        }
        assert!(fs.enumerate(&root).unwrap().is_empty());
        assert_eq!(fs.basic_info(&root).unwrap().attributes, DIRECTORY);
        assert_eq!(fs.stats().disposition_calls, 3);
    }

    #[test]
    fn fs_fake_readonly_and_caps() {
        let fs = FakeFs::new();
        let file = fs
            .add_file(
                "readonly",
                BasicInfo {
                    attributes: READONLY | 0x20,
                    ..BasicInfo::default()
                },
            )
            .unwrap();
        assert_eq!(
            fs.set_disposition(&file, DispFlags::default()),
            Err(FsError::AccessDenied)
        );
        fs.clear_readonly(&file).unwrap();
        assert_eq!(fs.basic_info(&file).unwrap().attributes, 0x20);
        fs.set_disposition(&file, DispFlags::default()).unwrap();
        assert_eq!(fs.stats().clear_readonly_calls, 1);
        let readonly = fs
            .add_file(
                "ignore",
                BasicInfo {
                    attributes: READONLY,
                    ..BasicInfo::default()
                },
            )
            .unwrap();
        fs.set_disposition(
            &readonly,
            DispFlags {
                posix: true,
                ignore_readonly: true,
                classic: false,
            },
        )
        .unwrap();
        let caps = VolumeCaps {
            posix_delete: Some(true),
            seek_penalty: Some(false),
        };
        fs.set_volume_caps(caps);
        assert_eq!(fs.volume_caps(&fs.root_handle()), caps);
        assert_eq!(fs.stats().clear_readonly_calls, 1);
    }

    #[test]
    fn fs_fake_concurrency_eight_threads() {
        let fs = Arc::new(FakeFs::new());
        let entered = Arc::new(Barrier::new(9));
        let release = Arc::new(Barrier::new(9));
        let (e, r) = (entered.clone(), release.clone());
        fs.set_disposition_hook(Some(Arc::new(move || {
            e.wait();
            r.wait();
        })));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                fs.add_file(format!("file{i}"), BasicInfo::default())
                    .unwrap()
            })
            .collect();
        thread::scope(|scope| {
            for handle in handles {
                let fs = fs.clone();
                scope.spawn(move || {
                    fs.set_disposition(&handle, DispFlags::default()).unwrap();
                    fs.close(handle);
                });
            }
            entered.wait();
            let stats = fs.stats();
            release.wait();
            assert_eq!(stats.disposition_calls, 8);
            assert_eq!(stats.current_dispositions, 8);
            assert_eq!(stats.max_concurrent_dispositions, 8);
        });
        assert_eq!(fs.stats().current_dispositions, 0);
        assert_eq!(fs.stats().max_concurrent_dispositions, 8);
        assert!(fs.enumerate(&fs.root_handle()).unwrap().is_empty());
    }
}
