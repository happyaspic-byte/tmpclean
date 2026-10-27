use super::*;
use crate::fs::{Access, FailureMode, FakeFs, FakeHandle, Operation, ReparseTag, VolumeCaps};
use crate::report::{exit_code, ResolveStatus};
use std::ffi::OsStr;
use std::sync::Mutex;

const NOW: Duration = Duration::from_secs(1_800_000_000);
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Duration {
        NOW
    }
}

#[derive(Default)]
struct Audit {
    opens: usize,
    closes: usize,
    root_dispositions: usize,
    enumerations: Vec<PathBuf>,
    dispositions: Vec<(PathBuf, DispFlags)>,
}

struct TrackedHandle {
    raw: FakeHandle,
    path: PathBuf,
    root: bool,
}

/// The fake deliberately has no handle-count API. This forwarding adapter counts
/// every engine-owned handle and attempted enumeration, including rejected ones.
#[derive(Default)]
struct TrackedFs {
    fake: FakeFs,
    audit: Mutex<Audit>,
    misleading_hints: bool,
}

impl TrackedFs {
    fn wrap(&self, raw: FakeHandle, path: PathBuf, root: bool) -> TrackedHandle {
        self.audit.lock().unwrap().opens += 1;
        TrackedHandle { raw, path, root }
    }

    fn root(&self) -> TrackedHandle {
        self.wrap(self.fake.root_handle(), PathBuf::from("/"), true)
    }

    fn anchor(&self, name: &str) -> TrackedHandle {
        let root = self.fake.root_handle();
        let raw = self
            .fake
            .open_child_no_follow(&root, OsStr::new(name), Access::DIRECTORY)
            .unwrap();
        self.fake.close(root);
        self.wrap(raw, PathBuf::from("/").join(name), true)
    }

    fn dir(&self, path: impl AsRef<Path>) {
        self.fake.close(self.fake.add_dir(path).unwrap());
    }

    fn file(&self, path: impl AsRef<Path>, info: BasicInfo) {
        self.fake.close(self.fake.add_file(path, info).unwrap());
    }

    fn inject(&self, path: &str, operation: Operation, error: FsError, mode: FailureMode) {
        self.fake
            .inject_failure(path, operation, error, mode)
            .unwrap();
    }

    fn run(&self, policy: Policy) -> RunResult {
        let result = Engine::new(self, &FixedClock, policy).run(self.root(), "/");
        self.assert_balanced();
        result
    }

    fn assert_balanced(&self) {
        let audit = self.audit.lock().unwrap();
        assert_eq!(audit.opens, audit.closes, "every engine handle is closed");
        assert_eq!(audit.root_dispositions, 0, "root never disposed");
        assert_eq!(self.fake.stats().current_dispositions, 0);
    }
}

impl FileSystem for TrackedFs {
    type Handle = TrackedHandle;

    fn open_child_no_follow(
        &self,
        parent: &TrackedHandle,
        name: &OsStr,
        access: Access,
    ) -> Result<TrackedHandle, FsError> {
        self.fake
            .open_child_no_follow(&parent.raw, name, access)
            .map(|raw| self.wrap(raw, parent.path.join(name), false))
    }

    fn enumerate(&self, handle: &TrackedHandle) -> Result<Vec<Entry>, FsError> {
        self.audit
            .lock()
            .unwrap()
            .enumerations
            .push(handle.path.clone());
        let mut entries = self.fake.enumerate(&handle.raw)?;
        if self.misleading_hints {
            for entry in &mut entries {
                entry.is_dir_hint = !entry.is_dir_hint;
                entry.reparse_tag_hint = if entry.reparse_tag_hint.is_none() {
                    Some(ReparseTag::Junction)
                } else {
                    None
                };
            }
        }
        Ok(entries)
    }

    fn basic_info(&self, handle: &TrackedHandle) -> Result<BasicInfo, FsError> {
        self.fake.basic_info(&handle.raw)
    }

    fn set_disposition(&self, handle: &TrackedHandle, flags: DispFlags) -> Result<(), FsError> {
        {
            let mut audit = self.audit.lock().unwrap();
            audit.root_dispositions += usize::from(handle.root);
            audit.dispositions.push((handle.path.clone(), flags));
        }
        self.fake.set_disposition(&handle.raw, flags)
    }

    fn clear_readonly(&self, handle: &TrackedHandle) -> Result<(), FsError> {
        self.fake.clear_readonly(&handle.raw)
    }

    fn close(&self, handle: TrackedHandle) {
        self.audit.lock().unwrap().closes += 1;
        self.fake.close(handle.raw);
    }

    fn volume_caps(&self, handle: &TrackedHandle) -> VolumeCaps {
        self.fake.volume_caps(&handle.raw)
    }
}

fn sized(size: u64) -> BasicInfo {
    BasicInfo {
        size,
        ..BasicInfo::default()
    }
}

fn recent() -> BasicInfo {
    BasicInfo {
        creation_time: SystemTime::UNIX_EPOCH + NOW - Duration::from_secs(3600),
        ..sized(42)
    }
}

fn aged_policy() -> Policy {
    Policy {
        min_age: DAY,
        dry_run: false,
    }
}

fn assert_expected_retention(result: &RunResult, expected: Counters) {
    assert_eq!(result.outcome, Outcome::Completed { fully_empty: false });
    assert_eq!(result.counters, expected);
    assert_eq!(
        exit_code(
            result.outcome,
            &result.counters,
            ResolveStatus::Resolved,
            false
        ),
        0
    );
}

#[test]
fn engine_three_level_fifty_entries_postorder_root_preserved() {
    let fs = TrackedFs::default();
    for dir in ["one", "one/two", "one/two/three", "empty"] {
        fs.dir(dir);
    }
    for i in 1..=46 {
        let parent = ["", "one/", "one/two/", "one/two/three/"][i % 4];
        fs.file(format!("{parent}file{i}"), sized(i as u64));
    }
    let result = fs.run(Policy::default());
    assert_eq!(result.outcome, Outcome::Completed { fully_empty: true });
    assert_eq!(
        result.counters,
        Counters {
            files_removed: 46,
            dirs_removed: 4,
            logical_bytes_removed: 1081,
            ..Counters::default()
        }
    );
    assert!(result.errors.is_empty());
    let audit = fs.audit.lock().unwrap();
    assert_eq!(audit.dispositions.len(), 50);
    for (index, (path, _)) in audit.dispositions.iter().enumerate() {
        for (child_index, (child, _)) in audit.dispositions.iter().enumerate() {
            if child != path && child.starts_with(path) {
                assert!(child_index < index, "{child:?} before {path:?}");
            }
        }
    }
    assert_eq!((audit.opens, audit.closes), (51, 51));
    let root = fs.fake.root_handle();
    assert_eq!(fs.fake.basic_info(&root).unwrap().attributes, DIRECTORY);
    assert!(fs.fake.enumerate(&root).unwrap().is_empty());
    fs.fake.close(root);
    println!(
        "50 entries: {:?}; handles {}/{}; root dispositions {}",
        result.counters, audit.opens, audit.closes, audit.root_dispositions
    );
}

fn reparse_leaf(tag: ReparseTag, untagged: bool) {
    let fs = TrackedFs::default();
    for dir in ["temp", "outside", "outside/nested"] {
        fs.dir(dir);
    }
    fs.file("outside/nested/secret", sized(99));
    let link = fs.fake.add_reparse("temp/link", tag, "/outside").unwrap();
    let mut info = fs.fake.basic_info(&link).unwrap();
    info.size = 777; // Link sizes must never inflate regular-file bytes.
    if untagged {
        info.reparse_tag = None;
    }
    fs.fake.set_basic_info(&link, info).unwrap();
    fs.fake.close(link);
    let result = Engine::new(&fs, &FixedClock, Policy::default()).run(fs.anchor("temp"), "/temp");
    fs.assert_balanced();
    assert_eq!(result.outcome, Outcome::Completed { fully_empty: true });
    assert_eq!(
        result.counters,
        Counters {
            links_removed: 1,
            ..Counters::default()
        }
    );
    let expected = vec![PathBuf::from("/temp"), PathBuf::from("/temp")];
    assert_eq!(fs.fake.stats().enumerated_directories, expected);
    assert_eq!(fs.audit.lock().unwrap().enumerations, expected);
    println!(
        "{tag:?}, untagged={untagged}: successful/attempted enumeration logs == {expected:?}; link and outside target never enumerated"
    );
    // Check target directory entries, not just an old handle to an unlinked node.
    let outside = fs.anchor("outside");
    let nested = fs
        .open_child_no_follow(&outside, OsStr::new("nested"), Access::DIRECTORY)
        .unwrap();
    let names = fs.fake.enumerate(&nested.raw).unwrap();
    assert_eq!(names.len(), 1);
    assert_eq!(names[0].name, "secret");
    let secret = fs
        .open_child_no_follow(&nested, OsStr::new("secret"), Access::READ)
        .unwrap();
    assert_eq!(fs.basic_info(&secret).unwrap().size, 99);
    fs.close(secret);
    fs.close(nested);
    fs.close(outside);
    fs.assert_balanced();
}

#[test]
fn engine_reparse_junction_never_enumerated() {
    reparse_leaf(ReparseTag::Junction, false);
}

#[test]
fn engine_reparse_directory_symlink_never_enumerated() {
    reparse_leaf(ReparseTag::DirSymlink, false);
}

#[test]
fn engine_reparse_file_symlink_never_enumerated() {
    reparse_leaf(ReparseTag::FileSymlink, false);
}

#[test]
fn engine_reparse_mount_point_never_enumerated() {
    reparse_leaf(ReparseTag::MountPoint, false);
}

#[test]
fn engine_reparse_unknown_tag_never_enumerated() {
    reparse_leaf(ReparseTag::Unknown(0x8765_4321), false);
}

#[test]
fn engine_reparse_attribute_without_tag_never_enumerated() {
    reparse_leaf(ReparseTag::Junction, true);
}

#[test]
fn engine_classifies_opened_handles_not_enumeration_hints() {
    let fs = TrackedFs {
        misleading_hints: true,
        ..TrackedFs::default()
    };
    fs.dir("directory");
    fs.file("directory/file", sized(7));
    fs.fake.close(
        fs.fake
            .add_reparse("junction", ReparseTag::Junction, "/elsewhere")
            .unwrap(),
    );
    let result = fs.run(Policy::default());
    assert_eq!(result.outcome, Outcome::Completed { fully_empty: true });
    assert_eq!(
        result.counters,
        Counters {
            files_removed: 1,
            dirs_removed: 1,
            links_removed: 1,
            logical_bytes_removed: 7,
            ..Counters::default()
        }
    );
    assert_eq!(
        fs.fake.stats().enumerated_directories,
        vec![
            PathBuf::from("/"),
            PathBuf::from("/directory"),
            PathBuf::from("/")
        ]
    );
}

#[test]
fn engine_min_age_retains_two_ancestors_without_failure() {
    let fs = TrackedFs::default();
    fs.dir("one");
    fs.dir("one/two");
    fs.file("one/two/recent", recent());
    let result = fs.run(aged_policy());
    assert_expected_retention(
        &result,
        Counters {
            excluded: 1,
            ..Counters::default()
        },
    );
    assert!(result.errors.is_empty());
    assert_eq!(fs.fake.stats().disposition_calls, 0);
}

#[test]
fn engine_min_age_zero_deletes_recent_nested_file() {
    let fs = TrackedFs::default();
    fs.dir("one");
    fs.dir("one/two");
    fs.file("one/two/recent", recent());
    let result = fs.run(Policy::default());
    assert_eq!(result.outcome, Outcome::Completed { fully_empty: true });
    assert_eq!(
        result.counters,
        Counters {
            files_removed: 1,
            dirs_removed: 2,
            logical_bytes_removed: 42,
            ..Counters::default()
        }
    );
}

#[test]
fn engine_min_age_uses_maximum_timestamp_and_includes_cutoff() {
    let fs = TrackedFs::default();
    let fresh = SystemTime::UNIX_EPOCH + NOW - Duration::from_secs(3600);
    for (name, index) in [("creation", 0), ("write", 1), ("change", 2)] {
        let mut info = sized(1);
        match index {
            0 => info.creation_time = fresh,
            1 => info.last_write_time = fresh,
            _ => info.change_time = fresh,
        }
        fs.file(name, info);
    }
    fs.file(
        "boundary",
        BasicInfo {
            change_time: SystemTime::UNIX_EPOCH + NOW - DAY,
            ..sized(9)
        },
    );
    fs.file(
        "future",
        BasicInfo {
            change_time: SystemTime::UNIX_EPOCH + NOW + DAY,
            ..sized(1)
        },
    );
    let result = fs.run(aged_policy());
    assert_expected_retention(
        &result,
        Counters {
            files_removed: 1,
            excluded: 4,
            logical_bytes_removed: 9,
            ..Counters::default()
        },
    );
}

#[test]
fn engine_min_age_applies_to_links_but_not_directories() {
    let fs = TrackedFs::default();
    let dir = fs.fake.add_dir("young_empty_directory").unwrap();
    let mut info = recent();
    info.attributes = DIRECTORY;
    fs.fake.set_basic_info(&dir, info).unwrap();
    fs.fake.close(dir);
    let link = fs
        .fake
        .add_reparse("young_link", ReparseTag::MountPoint, "/outside")
        .unwrap();
    let mut info = fs.fake.basic_info(&link).unwrap();
    info.change_time = recent().creation_time;
    fs.fake.set_basic_info(&link, info).unwrap();
    fs.fake.close(link);
    let result = fs.run(aged_policy());
    assert_expected_retention(
        &result,
        Counters {
            dirs_removed: 1,
            excluded: 1,
            ..Counters::default()
        },
    );
}

#[test]
fn engine_min_age_handles_pre_epoch_and_large_cutoffs() {
    struct EpochClock;
    impl Clock for EpochClock {
        fn now(&self) -> Duration {
            Duration::ZERO
        }
    }
    let fs = TrackedFs::default();
    let before = SystemTime::UNIX_EPOCH - DAY;
    fs.file(
        "before_epoch",
        BasicInfo {
            creation_time: before,
            last_write_time: before,
            change_time: before,
            ..sized(5)
        },
    );
    fs.file("at_epoch", sized(1));
    let result = Engine::new(&fs, &EpochClock, aged_policy()).run(fs.root(), "/");
    assert_expected_retention(
        &result,
        Counters {
            files_removed: 1,
            excluded: 1,
            logical_bytes_removed: 5,
            ..Counters::default()
        },
    );
    fs.assert_balanced();
    let result = fs.run(Policy {
        min_age: Duration::MAX,
        dry_run: false,
    });
    assert_eq!(result.counters.excluded, 1);
}

#[test]
fn engine_nested_sharing_violation_retains_ancestors_without_failure() {
    let fs = TrackedFs::default();
    fs.dir("one");
    fs.dir("one/two");
    let locked = fs
        .fake
        .add_file("one/two/locked", BasicInfo::default())
        .unwrap();
    fs.fake.set_locked(&locked, true).unwrap();
    fs.fake.close(locked);
    let result = fs.run(Policy::default());
    assert_expected_retention(
        &result,
        Counters {
            skipped_in_use: 1,
            ..Counters::default()
        },
    );
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.errors[0].disposition, "skipped_in_use");
    assert_eq!(fs.fake.stats().disposition_calls, 0);
}

#[test]
fn engine_error_classification_and_handle_balance() {
    let fs = TrackedFs::default();
    let cases = [
        ("absent_file", Operation::Open, FsError::NotFound(2)),
        ("absent_path", Operation::BasicInfo, FsError::NotFound(3)),
        (
            "sharing",
            Operation::SetDisposition,
            FsError::SharingViolation,
        ),
        ("lock", Operation::BasicInfo, FsError::Other(33)),
        ("denied", Operation::Open, FsError::AccessDenied),
        ("other", Operation::SetDisposition, FsError::Other(999)),
    ];
    for (name, operation, error) in cases {
        fs.file(name, sized(100));
        fs.inject(name, operation, error, FailureMode::Once);
    }
    fs.file("z_success", sized(7));
    let result = fs.run(Policy::default());
    assert_eq!(
        result.counters,
        Counters {
            files_removed: 1,
            logical_bytes_removed: 7,
            absent: 2,
            skipped_in_use: 2,
            failed: 2,
            ..Counters::default()
        }
    );
    assert_eq!(result.errors.len(), 6);
    for (name, _, error) in cases {
        let record = result
            .errors
            .iter()
            .find(|record| Path::new(&record.path) == PathBuf::from("/").join(name))
            .unwrap();
        let mut classified = Counters::default();
        assert_eq!(record.disposition, classified.classify(error.code()));
        assert_eq!(record.code, error.code());
    }
}

#[test]
fn engine_failed_descendant_is_not_retried_or_counted_again_on_ancestors() {
    let fs = TrackedFs::default();
    fs.dir("one");
    fs.dir("one/two");
    fs.file("one/two/denied", sized(1));
    fs.inject(
        "one/two/denied",
        Operation::BasicInfo,
        FsError::AccessDenied,
        FailureMode::Persistent,
    );
    let result = fs.run(Policy::default());
    assert_eq!(result.counters.failed, 1);
    assert_eq!(result.errors.len(), 1);
    assert_eq!(fs.fake.stats().disposition_calls, 0);
}

fn disposition_flags(fs: &TrackedFs) -> Vec<DispFlags> {
    fs.audit
        .lock()
        .unwrap()
        .dispositions
        .iter()
        .map(|(_, flags)| *flags)
        .collect()
}

const POSIX_IGNORE: DispFlags = DispFlags {
    posix: true,
    ignore_readonly: true,
    classic: false,
};
const POSIX: DispFlags = DispFlags {
    posix: true,
    ignore_readonly: false,
    classic: false,
};
const CLASSIC: DispFlags = DispFlags {
    posix: false,
    ignore_readonly: false,
    classic: true,
};

#[test]
fn engine_not_supported_falls_back_to_classic_unconfirmed() {
    let fs = TrackedFs::default();
    fs.file("file", sized(123));
    for code in [87, 50] {
        fs.inject(
            "file",
            Operation::SetDisposition,
            FsError::NotSupported(code),
            FailureMode::Once,
        );
    }
    let result = fs.run(Policy::default());
    assert_eq!(
        result.counters,
        Counters {
            classic_pending_unconfirmed: 1,
            ..Counters::default()
        }
    );
    assert_eq!(disposition_flags(&fs), vec![POSIX_IGNORE, POSIX, CLASSIC]);
    assert!(result.errors.is_empty());
    assert_eq!(result.outcome, Outcome::Completed { fully_empty: true });
}

#[test]
fn engine_not_supported_retries_without_ignore_before_classic() {
    let fs = TrackedFs::default();
    fs.file("file", sized(123));
    fs.inject(
        "file",
        Operation::SetDisposition,
        FsError::NotSupported(87),
        FailureMode::Once,
    );
    let result = fs.run(Policy::default());
    assert_eq!(result.counters.files_removed, 1);
    assert_eq!(result.counters.logical_bytes_removed, 123);
    assert_eq!(result.counters.classic_pending_unconfirmed, 0);
    assert_eq!(disposition_flags(&fs), vec![POSIX_IGNORE, POSIX]);
}

#[test]
fn engine_unsupported_classic_is_a_single_final_failure() {
    let fs = TrackedFs::default();
    fs.file("file", sized(1));
    fs.inject(
        "file",
        Operation::SetDisposition,
        FsError::NotSupported(50),
        FailureMode::Persistent,
    );
    let result = fs.run(Policy::default());
    assert_eq!(result.counters.failed, 1);
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.errors[0].code, 50);
    assert_eq!(disposition_flags(&fs), vec![POSIX_IGNORE, POSIX, CLASSIC]);
}

#[test]
fn engine_readonly_clear_retry_preserves_other_attributes() {
    let fs = TrackedFs::default();
    let raw = fs
        .fake
        .add_file(
            "readonly",
            BasicInfo {
                attributes: READONLY | 0x20,
                ..sized(12)
            },
        )
        .unwrap();
    fs.inject(
        "readonly",
        Operation::SetDisposition,
        FsError::NotSupported(87),
        FailureMode::Once,
    );
    let result = fs.run(Policy::default());
    assert_eq!(result.counters.files_removed, 1);
    assert_eq!(result.counters.logical_bytes_removed, 12);
    assert_eq!(result.counters.failed, 0);
    assert_eq!(fs.fake.stats().clear_readonly_calls, 1);
    assert_eq!(fs.fake.basic_info(&raw).unwrap().attributes, 0x20);
    fs.fake.close(raw);
    assert_eq!(disposition_flags(&fs), vec![POSIX_IGNORE, POSIX, POSIX]);
}

#[test]
fn engine_readonly_clear_is_attempted_only_once() {
    let fs = TrackedFs::default();
    fs.file(
        "readonly",
        BasicInfo {
            attributes: READONLY,
            ..sized(1)
        },
    );
    fs.inject(
        "readonly",
        Operation::SetDisposition,
        FsError::AccessDenied,
        FailureMode::Persistent,
    );
    let result = fs.run(Policy::default());
    assert_eq!(result.counters.failed, 1);
    assert_eq!(fs.fake.stats().clear_readonly_calls, 1);
    assert_eq!(disposition_flags(&fs), vec![POSIX_IGNORE, POSIX_IGNORE]);
}

#[test]
fn engine_readonly_clear_failure_is_recorded_and_closed() {
    let fs = TrackedFs::default();
    fs.file(
        "readonly",
        BasicInfo {
            attributes: READONLY,
            ..sized(1)
        },
    );
    fs.inject(
        "readonly",
        Operation::SetDisposition,
        FsError::AccessDenied,
        FailureMode::Once,
    );
    fs.inject(
        "readonly",
        Operation::ClearReadonly,
        FsError::Other(1234),
        FailureMode::Once,
    );
    let result = fs.run(Policy::default());
    assert_eq!(result.counters.failed, 1);
    assert_eq!(result.errors[0].code, 1234);
    assert_eq!(fs.fake.stats().clear_readonly_calls, 1);
    assert_eq!(disposition_flags(&fs), vec![POSIX_IGNORE]);
}

#[test]
fn engine_dry_run_counts_would_remove_without_mutation() {
    let fs = TrackedFs::default();
    fs.dir("dir");
    for i in 0..28 {
        fs.file(
            format!("dir/file{i}"),
            BasicInfo {
                attributes: READONLY,
                ..sized(2)
            },
        );
    }
    fs.fake.close(
        fs.fake
            .add_reparse("link", ReparseTag::DirSymlink, "/outside")
            .unwrap(),
    );
    let result = fs.run(Policy {
        dry_run: true,
        min_age: Duration::ZERO,
    });
    assert_eq!(
        result.counters,
        Counters {
            files_removed: 28,
            dirs_removed: 1,
            links_removed: 1,
            logical_bytes_removed: 56,
            ..Counters::default()
        }
    );
    assert_eq!(result.outcome, Outcome::Completed { fully_empty: false });
    assert_eq!(fs.fake.stats().disposition_calls, 0);
    assert_eq!(fs.fake.stats().clear_readonly_calls, 0);
    let actual = fs.run(Policy::default());
    assert_eq!(actual.counters, result.counters);
    assert_eq!(actual.outcome, Outcome::Completed { fully_empty: true });
    assert_eq!(fs.fake.stats().clear_readonly_calls, 0);
}

#[test]
fn engine_root_enumeration_failure_never_reports_completed() {
    let fs = TrackedFs::default();
    fs.file("untouched", sized(1));
    fs.inject(
        "/",
        Operation::Enumerate,
        FsError::AccessDenied,
        FailureMode::Once,
    );
    let result = fs.run(Policy::default());
    assert_eq!(result.outcome, Outcome::FailedToEnumerate);
    assert_eq!(result.counters.failed, 1);
    assert_eq!(result.errors[0].code, 5);
    assert_eq!(fs.fake.stats().disposition_calls, 0);
    assert_eq!(fs.audit.lock().unwrap().enumerations.len(), 1);
}

#[test]
fn engine_final_root_enumeration_failure_never_reports_completed() {
    let fs = TrackedFs::default();
    fs.file("file", sized(1));
    let engine = Engine::new(&fs, &FixedClock, Policy::default());
    let mut root = engine.root_task(fs.root(), "/");
    let first = engine.step(&mut root);
    assert_eq!(first.delta.files_removed, 1);
    fs.inject(
        "/",
        Operation::Enumerate,
        FsError::SharingViolation,
        FailureMode::Once,
    );
    let last = engine.step(&mut root);
    assert_eq!(
        last.status,
        StepStatus::Finished {
            retained: true,
            outcome: Some(Outcome::FailedToEnumerate)
        }
    );
    assert_eq!(last.delta.skipped_in_use, 1);
    assert_eq!(root.completion().pending(), 0);
    fs.assert_balanced();
}

#[test]
fn engine_child_enumeration_failure_continues_other_entries() {
    let fs = TrackedFs::default();
    fs.dir("denied");
    fs.file("z_file", sized(2));
    fs.inject(
        "denied",
        Operation::Enumerate,
        FsError::AccessDenied,
        FailureMode::Persistent,
    );
    let result = fs.run(Policy::default());
    assert_eq!(result.outcome, Outcome::Completed { fully_empty: false });
    assert_eq!(result.counters.failed, 1);
    assert_eq!(result.counters.files_removed, 1);
    assert_eq!(result.counters.dirs_removed, 0);
}

#[test]
fn engine_unexplained_directory_not_empty_gets_exactly_two_rescans() {
    let fs = TrackedFs::default();
    fs.dir("stubborn");
    fs.inject(
        "stubborn",
        Operation::SetDisposition,
        FsError::DirectoryNotEmpty,
        FailureMode::Persistent,
    );
    let result = fs.run(Policy::default());
    assert_eq!(result.counters.failed, 1);
    assert_eq!(result.counters.dirs_removed, 0);
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.errors[0].code, 145);
    assert_eq!(fs.fake.stats().disposition_calls, 3);
    assert_eq!(
        fs.fake
            .stats()
            .enumerated_directories
            .iter()
            .filter(|path| path.as_path() == Path::new("/stubborn"))
            .count(),
        3,
        "one original enumeration plus exactly two fresh rescans"
    );
}

fn concurrent_arrival(retain: bool) {
    let fs = Arc::new(TrackedFs::default());
    fs.dir("dir");
    let weak = Arc::downgrade(&fs);
    let inserted = AtomicBool::new(false);
    fs.fake.set_disposition_hook(Some(Arc::new(move || {
        if !inserted.swap(true, Ordering::SeqCst) {
            let fs = weak.upgrade().unwrap();
            fs.file("dir/arrived", if retain { recent() } else { sized(8) });
        }
    })));
    let result = fs.run(aged_policy());
    fs.fake.set_disposition_hook(None);
    if retain {
        assert_expected_retention(
            &result,
            Counters {
                excluded: 1,
                ..Counters::default()
            },
        );
        assert_eq!(fs.fake.stats().disposition_calls, 1);
    } else {
        assert_eq!(result.outcome, Outcome::Completed { fully_empty: true });
        assert_eq!(result.counters.files_removed, 1);
        assert_eq!(result.counters.dirs_removed, 1);
        assert_eq!(result.counters.failed, 0);
        assert_eq!(fs.fake.stats().disposition_calls, 3);
    }
    assert_eq!(
        fs.fake
            .stats()
            .enumerated_directories
            .iter()
            .filter(|path| path.as_path() == Path::new("/dir"))
            .count(),
        2
    );
}

#[test]
fn engine_fresh_rescan_discovers_concurrent_arrival() {
    concurrent_arrival(false);
}

#[test]
fn engine_fresh_rescan_retains_newly_excluded_descendant() {
    concurrent_arrival(true);
}

#[test]
fn engine_long_and_odd_names_and_empty_directories() {
    let fs = TrackedFs::default();
    fs.dir("empty [] ; ' directory");
    fs.file("x".repeat(300), sized(3));
    fs.file(" space ;[$] ' odd .tmp", sized(4));
    let result = fs.run(Policy::default());
    assert_eq!(result.outcome, Outcome::Completed { fully_empty: true });
    assert_eq!(result.counters.files_removed, 2);
    assert_eq!(result.counters.dirs_removed, 1);
    assert_eq!(result.counters.logical_bytes_removed, 7);
}

#[test]
fn engine_empty_root_is_checked_twice_and_never_disposed() {
    let fs = TrackedFs::default();
    let result = fs.run(Policy::default());
    assert_eq!(result.outcome, Outcome::Completed { fully_empty: true });
    assert_eq!(result.counters, Counters::default());
    assert_eq!(fs.fake.stats().disposition_calls, 0);
    assert_eq!(fs.fake.stats().enumerated_directories.len(), 2);
}

#[test]
fn engine_deep_tree_uses_explicit_stack() {
    let fs = TrackedFs::default();
    let mut path = PathBuf::new();
    for _ in 0..1024 {
        path.push("d");
        fs.dir(&path);
    }
    fs.file(path.join("file"), sized(1));
    let result = fs.run(Policy::default());
    assert_eq!(result.outcome, Outcome::Completed { fully_empty: true });
    assert_eq!(result.counters.dirs_removed, 1024);
    assert_eq!(result.counters.files_removed, 1);
}

#[test]
fn engine_directory_steps_support_parallel_children_and_completion_refs() {
    let fs = TrackedFs::default();
    for name in ["one", "two"] {
        fs.dir(name);
        fs.file(format!("{name}/file"), sized(5));
    }
    let engine = Engine::new(&fs, &FixedClock, Policy::default());
    let mut root = engine.root_task(fs.root(), "/");
    let completion = root.completion();
    let one = engine.step(&mut root).child.unwrap();
    assert_eq!(completion.pending(), 2); // Enumeration still owns one reference.
    let two = engine.step(&mut root).child.unwrap();
    assert_eq!(completion.pending(), 3);
    assert_eq!(engine.step(&mut root).status, StepStatus::Waiting);
    assert_eq!(completion.pending(), 2);
    let totals = std::thread::scope(|scope| {
        let handles: Vec<_> = [one, two]
            .into_iter()
            .map(|mut child| {
                let engine = &engine;
                scope.spawn(move || {
                    let completion = child.completion();
                    let mut delta = Counters::default();
                    loop {
                        let step = engine.step(&mut child);
                        delta.merge(&step.delta);
                        assert!(step.child.is_none());
                        assert!(step.error.is_none());
                        if let StepStatus::Finished { .. } = step.status {
                            assert_eq!(completion.pending(), 0);
                            return delta;
                        }
                        assert_ne!(step.status, StepStatus::Waiting);
                    }
                })
            })
            .collect();
        let mut total = Counters::default();
        for handle in handles {
            total.merge(&handle.join().unwrap());
        }
        total
    });
    assert_eq!(completion.pending(), 0);
    assert!(!completion.retained());
    assert_eq!(
        engine.step(&mut root).status,
        StepStatus::Finished {
            retained: false,
            outcome: Some(Outcome::Completed { fully_empty: true })
        }
    );
    assert_eq!(totals.files_removed, 2);
    assert_eq!(totals.dirs_removed, 2);
    assert_eq!(totals.logical_bytes_removed, 10);
    fs.assert_balanced();
}

#[test]
fn engine_dropped_tasks_release_all_refs_and_close_handles() {
    let fs = TrackedFs::default();
    fs.dir("one");
    fs.dir("one/two");
    let engine = Engine::new(&fs, &FixedClock, Policy::default());
    let mut root = engine.root_task(fs.root(), "/");
    let root_completion = root.completion();
    let mut one = engine.step(&mut root).child.unwrap();
    assert_eq!(engine.step(&mut root).status, StepStatus::Waiting);
    let one_completion = one.completion();
    let two = engine.step(&mut one).child.unwrap();
    let two_completion = two.completion();
    assert_eq!(engine.step(&mut one).status, StepStatus::Waiting);
    drop(two);
    drop(one);
    assert_eq!(two_completion.pending(), 0);
    assert_eq!(one_completion.pending(), 0);
    assert_eq!(root_completion.pending(), 0);
    assert!(root_completion.retained());
    assert_eq!(
        engine.step(&mut root).status,
        StepStatus::Finished {
            retained: true,
            outcome: Some(Outcome::Completed { fully_empty: false })
        }
    );
    fs.assert_balanced();
    let abandoned = engine.root_task(fs.root(), "/");
    let completion = abandoned.completion();
    drop(abandoned);
    assert_eq!(completion.pending(), 0);
    fs.assert_balanced();
}

#[test]
fn engine_error_records_are_bounded_but_counters_are_exact() {
    let fs = TrackedFs::default();
    for index in 0..1001 {
        let name = format!("file{index}");
        fs.file(&name, sized(1));
        fs.inject(
            &name,
            Operation::Open,
            FsError::AccessDenied,
            FailureMode::Persistent,
        );
    }
    let result = fs.run(Policy::default());
    assert_eq!(result.counters.failed, 1001);
    assert_eq!(result.errors.len(), MAX_ERRORS);
    assert!(result.errors_truncated);
}
