//! Handle-relative filesystem contract and a deterministic, thread-safe test fake.
//!
//! Enumeration metadata is only a hint. Callers classify the opened handle using
//! `basic_info`, and treat every reparse point as a leaf.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

mod fake;
pub use fake::{FailureMode, FakeCursor, FakeFs, FakeHandle, FakeStats, Operation};

/// A name batch, rather than one task per file. Traversal never collects a directory.
pub const ENUM_BATCH_SIZE: usize = 128;

#[derive(Default, Debug)]
pub(crate) struct EntryMetrics {
    pub current: AtomicUsize,
    pub peak: AtomicUsize,
}

/// Owns a bounded batch. The fake tracks its allocation until the batch is dropped,
/// including consumed entries, so its peak is a conservative in-flight bound.
#[derive(Debug)]
pub struct EntryBatch {
    entries: std::vec::IntoIter<Entry>,
    metrics: Option<Arc<EntryMetrics>>,
    allocated: usize,
}

impl EntryBatch {
    pub fn new(entries: Vec<Entry>) -> Self {
        Self {
            allocated: entries.len(),
            entries: entries.into_iter(),
            metrics: None,
        }
    }

    pub(crate) fn tracked(entries: Vec<Entry>, metrics: Arc<EntryMetrics>) -> Self {
        let mut batch = Self::new(entries);
        let current =
            metrics.current.fetch_add(batch.allocated, Ordering::SeqCst) + batch.allocated;
        metrics.peak.fetch_max(current, Ordering::SeqCst);
        batch.metrics = Some(metrics);
        batch
    }

    pub fn is_empty(&self) -> bool {
        self.entries.len() == 0
    }

    pub fn as_mut_slice(&mut self) -> &mut [Entry] {
        self.entries.as_mut_slice()
    }
}

impl Iterator for EntryBatch {
    type Item = Entry;

    fn next(&mut self) -> Option<Entry> {
        self.entries.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl ExactSizeIterator for EntryBatch {}

impl Drop for EntryBatch {
    fn drop(&mut self) {
        if let Some(metrics) = &self.metrics {
            metrics.current.fetch_sub(self.allocated, Ordering::SeqCst);
        }
    }
}

pub const READONLY: u32 = 0x0000_0001;
pub const DIRECTORY: u32 = 0x0000_0010;
pub const REPARSE_POINT: u32 = 0x0000_0400;

/// Minimum rights required by the operation, independent of platform constants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Access {
    pub read_attributes: bool,
    pub list_directory: bool,
    pub delete: bool,
}

impl Access {
    pub const READ: Self = Self {
        read_attributes: true,
        list_directory: false,
        delete: false,
    };
    pub const DELETE: Self = Self {
        delete: true,
        ..Self::READ
    };
    pub const DIRECTORY: Self = Self {
        list_directory: true,
        ..Self::DELETE
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReparseTag {
    Junction,
    DirSymlink,
    FileSymlink,
    MountPoint,
    Unknown(u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub name: OsString,
    pub is_dir_hint: bool,
    pub reparse_tag_hint: Option<ReparseTag>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BasicInfo {
    /// Native FILE_ATTRIBUTE_* bits, including READONLY, DIRECTORY, REPARSE_POINT.
    pub attributes: u32,
    pub reparse_tag: Option<ReparseTag>,
    pub creation_time: SystemTime,
    pub last_write_time: SystemTime,
    pub change_time: SystemTime,
    /// Logical file length, not allocated disk space.
    pub size: u64,
}

impl Default for BasicInfo {
    fn default() -> Self {
        Self {
            attributes: 0,
            reparse_tag: None,
            creation_time: SystemTime::UNIX_EPOCH,
            last_write_time: SystemTime::UNIX_EPOCH,
            change_time: SystemTime::UNIX_EPOCH,
            size: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DispFlags {
    pub posix: bool,
    pub ignore_readonly: bool,
    /// Select FileDispositionInfo rather than FileDispositionInfoEx.
    pub classic: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VolumeCaps {
    pub posix_delete: Option<bool>,
    pub seek_penalty: Option<bool>,
}

/// Original codes are retained even when several codes share a classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsError {
    NotFound(u32),
    AccessDenied,
    SharingViolation,
    DirectoryNotEmpty,
    NotSupported(u32),
    Other(u32),
}

impl FsError {
    pub const fn from_code(code: u32) -> Self {
        match code {
            2 | 3 => Self::NotFound(code),
            5 => Self::AccessDenied,
            32 => Self::SharingViolation,
            145 => Self::DirectoryNotEmpty,
            1 | 50 | 87 => Self::NotSupported(code),
            _ => Self::Other(code),
        }
    }

    pub const fn code(self) -> u32 {
        match self {
            Self::NotFound(code) | Self::NotSupported(code) | Self::Other(code) => code,
            Self::AccessDenied => 5,
            Self::SharingViolation => 32,
            Self::DirectoryNotEmpty => 145,
        }
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "filesystem error {} ({self:?})", self.code())
    }
}

impl std::error::Error for FsError {}

pub trait FileSystem: Send + Sync {
    type Handle: Send;
    /// Default starts a fresh scan. A cursor belongs to one handle, exclusively.
    type Cursor: Default + Send;

    /// `name` is one child component, never a path. The final entry is not followed.
    fn open_child_no_follow(
        &self,
        parent: &Self::Handle,
        name: &OsStr,
        access: Access,
    ) -> Result<Self::Handle, FsError>;
    /// Continue on the same handle, returning at most `ENUM_BATCH_SIZE` names.
    /// Empty means EOF. An error terminates this scan; callers may continue other
    /// directories but must not retry a cursor whose position is now unknown.
    fn enumerate_batch(
        &self,
        directory: &Self::Handle,
        cursor: &mut Self::Cursor,
    ) -> Result<EntryBatch, FsError>;

    /// Collect a snapshot for small inspections and fixtures, not traversal.
    fn enumerate(&self, directory: &Self::Handle) -> Result<Vec<Entry>, FsError> {
        let mut cursor = Self::Cursor::default();
        let mut entries = Vec::new();
        loop {
            let batch = self.enumerate_batch(directory, &mut cursor)?;
            if batch.is_empty() {
                return Ok(entries);
            }
            entries.extend(batch);
        }
    }
    fn basic_info(&self, handle: &Self::Handle) -> Result<BasicInfo, FsError>;
    fn set_disposition(&self, handle: &Self::Handle, flags: DispFlags) -> Result<(), FsError>;
    fn clear_readonly(&self, handle: &Self::Handle) -> Result<(), FsError>;
    fn close(&self, handle: Self::Handle);
    fn volume_caps(&self, handle: &Self::Handle) -> VolumeCaps;
}
