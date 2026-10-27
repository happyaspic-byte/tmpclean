#![cfg(windows)]

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tmpclean_core::fs::{
    Access, DispFlags, FileSystem, FsError, ReparseTag, DIRECTORY, READONLY, REPARSE_POINT,
};
use tmpclean_core::win::{WinFs, WinHandle};

const DELETE: DispFlags = DispFlags {
    posix: true,
    ignore_readonly: true,
    classic: false,
};
static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let base = std::env::var_os("RUNNER_TEMP")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = base.join(format!(
            "tmpclean-{label}-{}-{unique}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if let Err(error) = cleanup(&extended(&self.0)) {
            if std::thread::panicking() {
                eprintln!("fixture cleanup failed for {}: {error}", self.0.display());
            } else {
                panic!("fixture cleanup failed for {}: {error}", self.0.display());
            }
        }
    }
}

fn extended(path: &Path) -> PathBuf {
    let mut result = OsString::from(r"\\?\");
    result.push(path.as_os_str());
    PathBuf::from(result)
}

fn cleanup(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let reparse = metadata.file_attributes() & REPARSE_POINT != 0;
    // FileType::is_dir excludes symlinks, even for a directory junction. The
    // native bit still identifies how to remove a dangling directory link.
    let directory = metadata.file_attributes() & DIRECTORY != 0;
    if directory && !reparse {
        for child in fs::read_dir(path)? {
            cleanup(&child?.path())?;
        }
    }
    // Never follow a link to clear its target's attributes during cleanup.
    if !reparse && metadata.permissions().readonly() {
        // Use Windows' independent fixture cleanup, not the WinFs under test.
        let output = Command::new("attrib").arg("-R").arg(path).output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
    }
    if directory {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

fn mklink(kind: Option<&str>, link: &Path, target: &Path) {
    let mut command = Command::new("cmd");
    command.args(["/d", "/c", "mklink"]);
    if let Some(kind) = kind {
        command.arg(kind);
    }
    let output = command.arg(link).arg(target).output().unwrap();
    assert!(
        output.status.success(),
        "mklink must succeed (symlink privilege is required): {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn names(fs: &WinFs, root: &WinHandle) -> BTreeSet<OsString> {
    fs.enumerate(root)
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

fn remove(fs: &WinFs, handle: WinHandle) -> DispFlags {
    fs.set_disposition(&handle, DELETE).unwrap();
    let used = fs.disposition_flags(&handle).unwrap();
    fs.close(handle);
    used
}

#[test]
fn readonly_file_deleted() {
    let dir = TestDir::new("readonly");
    let path = dir.0.join("readonly.txt");
    fs::write(&path, b"readonly data").unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();

    let fs = WinFs::new();
    let root = WinFs::open_root(&dir.0).unwrap();
    let file = fs
        .open_child_no_follow(&root, OsStr::new("readonly.txt"), Access::DELETE)
        .unwrap();
    let info = fs.basic_info(&file).unwrap();
    assert_ne!(info.attributes & READONLY, 0);
    assert_eq!(info.size, 13);
    assert_eq!(
        info.last_write_time,
        fs::metadata(&path).unwrap().modified().unwrap()
    );
    remove(&fs, file);
    assert!(!path.exists());
    assert!(fs.enumerate(&root).unwrap().is_empty());
}

#[test]
fn junction_deletion_preserves_all_three_out_of_tree_target_files() {
    let dir = TestDir::new("junction");
    let target = TestDir::new("outside-junction");
    let files = ["first.txt", "second.txt", "third.txt"];
    for name in files {
        fs::write(target.0.join(name), name.as_bytes()).unwrap();
    }
    let link = dir.0.join("junction");
    mklink(Some("/J"), &link, &target.0);

    let fs = WinFs::new();
    let root = WinFs::open_root(&dir.0).unwrap();
    assert_eq!(
        names(&fs, &root),
        BTreeSet::from([OsString::from("junction")])
    );
    let handle = fs
        .open_child_no_follow(&root, OsStr::new("junction"), Access::DIRECTORY)
        .unwrap();
    assert_eq!(
        fs.basic_info(&handle).unwrap().reparse_tag,
        Some(ReparseTag::Junction)
    );
    assert_eq!(fs.enumerate(&handle).unwrap_err(), FsError::Other(267));
    assert_eq!(
        fs.open_child_no_follow(&handle, OsStr::new(files[0]), Access::DELETE)
            .unwrap_err(),
        FsError::NotFound(3)
    );
    remove(&fs, handle);
    assert!(fs::symlink_metadata(&link).is_err());
    assert!(names(&fs, &root).is_empty());
    for name in files {
        assert_eq!(fs::read(target.0.join(name)).unwrap(), name.as_bytes());
        // Direct Write is intentional: libtest captures println! on success,
        // but the CI evidence must show each surviving target file.
        writeln!(
            io::stdout().lock(),
            "JUNCTION_TARGET_INTACT {name}: {}",
            target.0.join(name).display()
        )
        .unwrap();
    }
    writeln!(
        io::stdout().lock(),
        "JUNCTION_TARGETS_INTACT 3/3; link removed"
    )
    .unwrap();
}

#[test]
fn directory_symlink_deleted_without_touching_target() {
    let dir = TestDir::new("dir-symlink");
    let target = TestDir::new("outside-dir-symlink");
    fs::write(target.0.join("keep.txt"), b"keep").unwrap();
    let link = dir.0.join("dir-link");
    mklink(Some("/D"), &link, &target.0);

    let fs = WinFs::new();
    let root = WinFs::open_root(&dir.0).unwrap();
    let entries = fs.enumerate(&root).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "dir-link");
    assert!(entries[0].is_dir_hint);
    let handle = fs
        .open_child_no_follow(&root, &entries[0].name, Access::DIRECTORY)
        .unwrap();
    assert_eq!(
        fs.basic_info(&handle).unwrap().reparse_tag,
        Some(ReparseTag::DirSymlink)
    );
    assert_eq!(fs.enumerate(&handle).unwrap_err(), FsError::Other(267));
    remove(&fs, handle);
    assert!(fs::symlink_metadata(&link).is_err());
    assert_eq!(fs::read(target.0.join("keep.txt")).unwrap(), b"keep");
}

#[test]
fn file_symlink_deleted_without_touching_target() {
    let dir = TestDir::new("file-symlink");
    let target = TestDir::new("outside-file-symlink");
    let target_file = target.0.join("keep.txt");
    fs::write(&target_file, b"keep").unwrap();
    let link = dir.0.join("file-link");
    mklink(None, &link, &target_file);

    let fs = WinFs::new();
    let root = WinFs::open_root(&dir.0).unwrap();
    let entries = fs.enumerate(&root).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "file-link");
    assert!(!entries[0].is_dir_hint);
    let handle = fs
        .open_child_no_follow(&root, &entries[0].name, Access::DELETE)
        .unwrap();
    assert_eq!(
        fs.basic_info(&handle).unwrap().reparse_tag,
        Some(ReparseTag::FileSymlink)
    );
    remove(&fs, handle);
    assert!(fs::symlink_metadata(&link).is_err());
    assert_eq!(fs::read(&target_file).unwrap(), b"keep");
}

#[test]
fn long_path_deleted_via_relative_single_component_opens() {
    let dir = TestDir::new("long-path");
    let components: Vec<String> = (0..5).map(|i| format!("{i}-{}", "x".repeat(60))).collect();
    let nested = components
        .iter()
        .fold(extended(&dir.0), |path, name| path.join(name));
    fs::create_dir_all(&nested).unwrap();
    let path = nested.join("deep.txt");
    assert!(path.as_os_str().encode_wide().count() > 300);
    fs::write(&path, b"deep").unwrap();

    let fs = WinFs::new();
    let mut parents = vec![WinFs::open_root(&dir.0).unwrap()];
    for name in &components {
        let child = fs
            .open_child_no_follow(parents.last().unwrap(), OsStr::new(name), Access::DIRECTORY)
            .unwrap();
        assert_eq!(
            fs.basic_info(&child).unwrap().attributes & DIRECTORY,
            DIRECTORY
        );
        parents.push(child);
    }
    let parent = parents.last().unwrap();
    assert_eq!(
        names(&fs, parent),
        BTreeSet::from([OsString::from("deep.txt")])
    );
    let file = fs
        .open_child_no_follow(parent, OsStr::new("deep.txt"), Access::DELETE)
        .unwrap();
    remove(&fs, file);
    assert!(!path.exists());
    assert!(fs.enumerate(parent).unwrap().is_empty());
}

#[test]
fn exclusive_open_reports_sharing_violation_and_keeps_file() {
    let dir = TestDir::new("sharing");
    let path = dir.0.join("busy.txt");
    fs::write(&path, b"busy").unwrap();
    let held = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .unwrap();

    let fs = WinFs::new();
    let root = WinFs::open_root(&dir.0).unwrap();
    assert_eq!(
        names(&fs, &root),
        BTreeSet::from([OsString::from("busy.txt")])
    );
    assert_eq!(
        fs.open_child_no_follow(&root, OsStr::new("busy.txt"), Access::DELETE)
            .unwrap_err(),
        FsError::SharingViolation
    );
    assert!(path.exists());
    drop(held);
    assert_eq!(fs::read(&path).unwrap(), b"busy");
}

#[test]
fn enumeration_returns_exact_names_across_multiple_buffers_and_restarts() {
    let dir = TestDir::new("enumerate");
    let mut expected = BTreeSet::new();
    for i in 0..700 {
        let name = format!("entry-{i:04}-{}", "n".repeat(60));
        fs::write(dir.0.join(&name), []).unwrap();
        expected.insert(OsString::from(name));
    }
    let unicode = OsString::from("name-\u{d55c}\u{ae00}-\u{1f680}");
    fs::write(dir.0.join(&unicode), []).unwrap();
    expected.insert(unicode);
    fs::create_dir(dir.0.join("directory")).unwrap();
    expected.insert(OsString::from("directory"));

    let fs = WinFs::new();
    let root = WinFs::open_root(&dir.0).unwrap();
    let entries = fs.enumerate(&root).unwrap();
    assert_eq!(entries.len(), expected.len());
    for entry in entries {
        assert_eq!(entry.is_dir_hint, entry.name == "directory");
        assert_eq!(entry.reparse_tag_hint, None);
    }
    assert_eq!(names(&fs, &root), expected);
    assert_eq!(names(&fs, &root), expected);
}

#[test]
fn clear_readonly_preserves_other_attributes_and_classic_receipt() {
    let dir = TestDir::new("attributes");
    let path = dir.0.join("attributes.txt");
    drop(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .attributes(0x2 | 0x20)
            .open(&path)
            .unwrap(),
    );
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();

    let fs = WinFs::new();
    let root = WinFs::open_root(&dir.0).unwrap();
    let file = fs
        .open_child_no_follow(&root, OsStr::new("attributes.txt"), Access::DELETE)
        .unwrap();
    let before = fs.basic_info(&file).unwrap().attributes;
    assert_ne!(before & READONLY, 0);
    assert_eq!(before & (0x2 | 0x20), 0x2 | 0x20);
    fs.clear_readonly(&file).unwrap();
    assert_eq!(fs.basic_info(&file).unwrap().attributes, before & !READONLY);
    let classic = DispFlags {
        classic: true,
        ..DispFlags::default()
    };
    fs.set_disposition(&file, classic).unwrap();
    assert_eq!(fs.disposition_flags(&file), Some(classic));
    fs.close(file);
    assert!(!path.exists());
}

#[test]
fn classic_readonly_denial_does_not_retry_or_change_attributes() {
    let dir = TestDir::new("classic-readonly");
    let path = dir.0.join("readonly.txt");
    fs::write(&path, b"preserve").unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();

    let fs = WinFs::new();
    let root = WinFs::open_root(&dir.0).unwrap();
    let file = fs
        .open_child_no_follow(&root, OsStr::new("readonly.txt"), Access::DELETE)
        .unwrap();
    let before = fs.basic_info(&file).unwrap().attributes;
    let classic = DispFlags {
        classic: true,
        ..DispFlags::default()
    };
    assert_eq!(
        fs.set_disposition(&file, classic),
        Err(FsError::AccessDenied)
    );
    assert_eq!(fs.disposition_flags(&file), None);
    assert_eq!(fs.basic_info(&file).unwrap().attributes, before);
    assert!(path.exists());
    fs.clear_readonly(&file).unwrap();
    fs.set_disposition(&file, classic).unwrap();
    assert_eq!(fs.disposition_flags(&file), Some(classic));
    fs.close(file);
    assert!(!path.exists());
}

#[test]
fn root_is_no_follow_and_cannot_be_deleted_or_renamed_while_pinned() {
    let dir = TestDir::new("root");
    let target = TestDir::new("outside-root");
    let link = dir.0.join("root-link");
    mklink(Some("/J"), &link, &target.0);

    let fs = WinFs::new();
    let root = WinFs::open_root(&dir.0).unwrap();
    let link_root = WinFs::open_root(&link).unwrap();
    assert_eq!(
        fs.basic_info(&link_root).unwrap().reparse_tag,
        Some(ReparseTag::Junction)
    );
    assert_eq!(fs.enumerate(&link_root).unwrap_err(), FsError::Other(267));
    assert_eq!(
        fs.set_disposition(&root, DELETE),
        Err(FsError::AccessDenied)
    );
    assert_eq!(fs.disposition_flags(&root), None);
    let renamed = dir.0.with_extension("renamed");
    assert_eq!(
        fs::rename(&dir.0, &renamed).unwrap_err().raw_os_error(),
        Some(32)
    );
}

#[test]
fn nonempty_directory_and_missing_child_errors_match_fake() {
    let dir = TestDir::new("errors");
    fs::create_dir(dir.0.join("nonempty")).unwrap();
    fs::write(dir.0.join("nonempty").join("keep"), b"keep").unwrap();

    let fs = WinFs::new();
    let root = WinFs::open_root(&dir.0).unwrap();
    let child = fs
        .open_child_no_follow(&root, OsStr::new("nonempty"), Access::DIRECTORY)
        .unwrap();
    assert_eq!(
        fs.set_disposition(&child, DELETE),
        Err(FsError::DirectoryNotEmpty)
    );
    assert_eq!(fs.disposition_flags(&child), None);
    assert_eq!(
        fs.open_child_no_follow(&root, OsStr::new("missing"), Access::READ)
            .unwrap_err(),
        FsError::NotFound(2)
    );
    for invalid in [
        "",
        ".",
        "..",
        "../outside",
        r"..\outside",
        "file:stream",
        "nul\0name",
    ] {
        assert_eq!(
            fs.open_child_no_follow(&root, OsStr::new(invalid), Access::DELETE)
                .unwrap_err(),
            FsError::Other(123)
        );
    }
}
