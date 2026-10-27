use crate::fs::{
    Access, BasicInfo, DispFlags, Entry, EntryBatch, FileSystem, FsError, ReparseTag, VolumeCaps,
    ENUM_BATCH_SIZE,
};
use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::mem::{offset_of, size_of, size_of_val, MaybeUninit};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::{Duration, SystemTime};
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    NtCreateFile, FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
};
use windows_sys::Win32::Foundation::{
    GetLastError, RtlNtStatusToDosError, ERROR_DIRECTORY, ERROR_INVALID_DATA, ERROR_INVALID_NAME,
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, DEVICE_SEEK_PENALTY_DESCRIPTOR,
    FSCTL_GET_REPARSE_POINT, IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_PROPERTY_QUERY,
    VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Kernel::OBJ_CASE_INSENSITIVE;
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};

const ENUM_BYTES: usize = 64 * 1024;
const WINDOWS_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

/// An owned handle. Dropping it closes it, including during error unwinding.
///
/// Handles are Send but not Sync: enumeration has a cursor and a reusable buffer.
#[derive(Debug)]
pub struct WinHandle {
    raw: OwnedHandle,
    access: Access,
    enumeration: RefCell<Vec<u64>>,
    disposition: Cell<Option<DispFlags>>,
}

/// Position in the handle's reusable native buffer and native enumeration.
pub struct WinCursor {
    started: bool,
    done: bool,
    extended: bool,
    offset: Option<usize>,
}

impl Default for WinCursor {
    fn default() -> Self {
        Self {
            started: false,
            done: false,
            extended: true,
            offset: None,
        }
    }
}

impl AsRawHandle for WinHandle {
    fn as_raw_handle(&self) -> RawHandle {
        self.raw.as_raw_handle()
    }
}

/// Filesystem operations do not retain or re-resolve child paths.
#[derive(Debug, Default)]
pub struct WinFs;

impl WinFs {
    pub fn new() -> Self {
        Self
    }

    /// Open an absolute root without following its final reparse point.
    ///
    /// The resolver must validate ancestors and classify this handle before use.
    /// The root has no DELETE access, so it cannot be disposed by this interface.
    pub fn open_root(path: impl AsRef<Path>) -> Result<WinHandle, FsError> {
        let path = path.as_ref();
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if !path.is_absolute() || wide.contains(&0) {
            return Err(FsError::from_code(ERROR_INVALID_NAME));
        }
        for c in &mut wide {
            if *c == b'/' as u16 {
                *c = b'\\' as u16;
            }
        }
        let (prefix, rest) = if wide.starts_with(&[92, 92, 63, 92]) {
            (r"\??\", &wide[4..])
        } else if wide.starts_with(&[92, 92]) {
            (r"\??\UNC\", &wide[2..])
        } else {
            (r"\??\", wide.as_slice())
        };
        let mut native: Vec<u16> = prefix.encode_utf16().chain(rest.iter().copied()).collect();
        Self::open_native(
            null_mut(),
            &mut native,
            Access {
                read_attributes: true,
                list_directory: true,
                delete: false,
            },
        )
    }

    /// The flags of the successful disposition, or None before success/after failure.
    ///
    /// Inspect this before closing the handle. If `classic` is true, the engine
    /// must count `classic_pending_unconfirmed`, not a confirmed removal. This
    /// per-handle receipt remains unambiguous when different workers fall back.
    pub fn disposition_flags(&self, handle: &WinHandle) -> Option<DispFlags> {
        handle.disposition.get()
    }

    fn open_native(parent: HANDLE, name: &mut [u16], access: Access) -> Result<WinHandle, FsError> {
        let length =
            u16::try_from(size_of_val(name)).map_err(|_| FsError::from_code(ERROR_INVALID_NAME))?;
        let name = UNICODE_STRING {
            Length: length,
            MaximumLength: length,
            Buffer: name.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent,
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE as u32,
            SecurityDescriptor: null(),
            SecurityQualityOfService: null(),
        };
        let mut desired = SYNCHRONIZE;
        if access.read_attributes {
            desired |= FILE_READ_ATTRIBUTES;
        }
        if access.list_directory {
            desired |= FILE_LIST_DIRECTORY;
        }
        if access.delete {
            desired |= DELETE;
        }
        let mut raw = null_mut();
        let mut status_block = MaybeUninit::<IO_STATUS_BLOCK>::zeroed();
        // SAFETY: all input buffers live through this synchronous call. NtCreateFile
        // writes only the supplied HANDLE/IO_STATUS_BLOCK; FILE_OPEN never creates.
        let status = unsafe {
            NtCreateFile(
                &mut raw,
                desired,
                &attributes,
                status_block.as_mut_ptr(),
                null(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_OPEN,
                FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
                null(),
                0,
            )
        };
        if status < 0 {
            // SAFETY: RtlNtStatusToDosError takes a scalar NTSTATUS and no pointers.
            return Err(FsError::from_code(unsafe { RtlNtStatusToDosError(status) }));
        }
        // SAFETY: successful synchronous NtCreateFile transfers one valid, owned
        // handle. OwnedHandle closes it on every return/unwind path.
        let raw = unsafe { OwnedHandle::from_raw_handle(raw) };
        Ok(WinHandle {
            raw,
            access,
            enumeration: RefCell::new(Vec::new()),
            disposition: Cell::new(None),
        })
    }

    fn attribute_tag(handle: &WinHandle) -> Result<FILE_ATTRIBUTE_TAG_INFO, FsError> {
        // SAFETY: FileAttributeTagInfo writes exactly FILE_ATTRIBUTE_TAG_INFO.
        unsafe { query_info(handle.as_raw_handle(), FileAttributeTagInfo) }
    }

    fn basic(handle: &WinHandle) -> Result<FILE_BASIC_INFO, FsError> {
        // SAFETY: FileBasicInfo writes exactly FILE_BASIC_INFO.
        unsafe { query_info(handle.as_raw_handle(), FileBasicInfo) }
    }

    fn mount_tag(handle: &WinHandle) -> Result<ReparseTag, FsError> {
        // REPARSE_DATA_BUFFER has a maximum size of 16 KiB. u64 keeps it aligned.
        let mut buffer = [0u64; 16 * 1024 / 8];
        let mut returned = 0;
        // SAFETY: the live no-follow handle and writable aligned buffer are valid;
        // the synchronous IOCTL is bounded by the provided buffer length.
        let ok = unsafe {
            DeviceIoControl(
                handle.as_raw_handle(),
                FSCTL_GET_REPARSE_POINT,
                null(),
                0,
                buffer.as_mut_ptr().cast(),
                size_of_val(&buffer) as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        let bytes = buffer_bytes(&buffer);
        let bytes = bytes
            .get(..returned as usize)
            .ok_or(FsError::from_code(ERROR_INVALID_DATA))?;
        mount_tag_from_buffer(bytes)
    }

    fn disposition_once(handle: &WinHandle, flags: DispFlags) -> Result<(), FsError> {
        if flags.classic {
            set_info(
                handle.as_raw_handle(),
                FileDispositionInfo,
                &FILE_DISPOSITION_INFO { DeleteFile: 1 },
            )
        } else {
            let mut native = FILE_DISPOSITION_FLAG_DELETE;
            if flags.posix {
                native |= FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
            }
            if flags.ignore_readonly {
                native |= FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE;
            }
            set_info(
                handle.as_raw_handle(),
                FileDispositionInfoEx,
                &FILE_DISPOSITION_INFO_EX { Flags: native },
            )
        }
    }

    fn writable_attributes(handle: &WinHandle) -> Result<OwnedHandle, FsError> {
        // SAFETY: ReOpenFile reopens this live object, not a reconstructed path.
        // It does not follow reparse points. Only attribute-write rights are added.
        let raw = unsafe {
            ReOpenFile(
                handle.as_raw_handle(),
                FILE_WRITE_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        // SAFETY: a successful ReOpenFile returned a fresh handle owned by us.
        Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
    }

    fn volume(handle: &WinHandle) -> Option<OwnedHandle> {
        // Query the device belonging to the pinned handle, not its original path.
        let mut name = vec![0u16; 32_768];
        // SAFETY: the handle is live and the writable UTF-16 buffer has this length.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                handle.as_raw_handle(),
                name.as_mut_ptr(),
                name.len() as u32,
                VOLUME_NAME_GUID,
            )
        } as usize;
        if length == 0 || length >= name.len() {
            return None;
        }
        name.truncate(length);
        let prefix: Vec<u16> = r"\\?\Volume{".encode_utf16().collect();
        if !name.starts_with(&prefix) {
            return None;
        }
        let end = name.iter().position(|c| *c == b'}' as u16)? + 1;
        name.truncate(end);
        name.push(0);
        // SAFETY: name is NUL-terminated and names only the volume device. No
        // data access is requested; output is checked before taking ownership.
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: successful CreateFileW returned one fresh owned volume handle.
        Some(unsafe { OwnedHandle::from_raw_handle(raw) })
    }

    fn seek_penalty(volume: &OwnedHandle) -> Option<bool> {
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceSeekPenaltyProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let mut descriptor = MaybeUninit::<DEVICE_SEEK_PENALTY_DESCRIPTOR>::zeroed();
        let mut returned = 0;
        // SAFETY: input/output types match this storage property and remain live
        // through the synchronous IOCTL; both buffer sizes are exact.
        let ok = unsafe {
            DeviceIoControl(
                volume.as_raw_handle(),
                IOCTL_STORAGE_QUERY_PROPERTY,
                (&query as *const STORAGE_PROPERTY_QUERY).cast(),
                size_of_val(&query) as u32,
                descriptor.as_mut_ptr().cast(),
                size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 || returned < size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32 {
            return None;
        }
        // SAFETY: successful IOCTL filled the complete descriptor; its fields
        // are integer types, and the backing storage was zero-initialized.
        let descriptor = unsafe { descriptor.assume_init() };
        (descriptor.Size >= size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32)
            .then_some(descriptor.IncursSeekPenalty != 0)
    }

    fn device_id(volume: &OwnedHandle) -> Option<u32> {
        let mut extents = MaybeUninit::<VOLUME_DISK_EXTENTS>::zeroed();
        let mut returned = 0;
        // SAFETY: the live volume handle and aligned SDK output structure match
        // this synchronous IOCTL. A buffer holding one extent is intentional:
        // more extents (including ERROR_MORE_DATA) fall back to separate devices.
        let ok = unsafe {
            DeviceIoControl(
                volume.as_raw_handle(),
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                null(),
                0,
                extents.as_mut_ptr().cast(),
                size_of::<VOLUME_DISK_EXTENTS>() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 || returned < size_of::<VOLUME_DISK_EXTENTS>() as u32 {
            return None;
        }
        // SAFETY: success returned the complete, zero-initialized integer-only
        // structure. Never index beyond its one declared extent.
        let extents = unsafe { extents.assume_init() };
        (extents.NumberOfDiskExtents == 1).then_some(extents.Extents[0].DiskNumber)
    }
}

impl FileSystem for WinFs {
    type Handle = WinHandle;
    type Cursor = WinCursor;

    fn open_child_no_follow(
        &self,
        parent: &WinHandle,
        name: &OsStr,
        access: Access,
    ) -> Result<WinHandle, FsError> {
        let mut name: Vec<u16> = name.encode_wide().collect();
        if name.is_empty()
            || name == [46]
            || name == [46, 46]
            || name.iter().any(|c| matches!(*c, 0 | 47 | 92 | 58))
        {
            return Err(FsError::from_code(ERROR_INVALID_NAME));
        }
        let parent_info = Self::attribute_tag(parent)?;
        if parent_info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || parent_info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || parent_info.ReparseTag != 0
        {
            return Err(FsError::NotFound(3));
        }
        // No FILE_SHARE_DELETE even if a stale enumeration called this a file:
        // a directory substituted for that entry must still be pinned.
        Self::open_native(parent.as_raw_handle(), &mut name, access)
    }

    fn enumerate_batch(
        &self,
        directory: &WinHandle,
        cursor: &mut WinCursor,
    ) -> Result<EntryBatch, FsError> {
        if cursor.done {
            return Ok(EntryBatch::new(Vec::new()));
        }
        if !directory.access.list_directory {
            return Err(FsError::AccessDenied);
        }
        let info = Self::attribute_tag(directory)?;
        if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || info.ReparseTag != 0
        {
            return Err(FsError::from_code(ERROR_DIRECTORY));
        }
        let mut storage = directory.enumeration.borrow_mut();
        storage.resize(ENUM_BYTES / 8, 0);
        let mut entries = Vec::new();
        loop {
            if cursor.offset.is_some() {
                append_entries(
                    buffer_bytes(&storage),
                    cursor.extended,
                    &mut entries,
                    &mut cursor.offset,
                    ENUM_BATCH_SIZE,
                )?;
                if !entries.is_empty() {
                    return Ok(EntryBatch::new(entries));
                }
            }
            let class = match (cursor.extended, !cursor.started) {
                (true, true) => FileIdExtdDirectoryRestartInfo,
                (true, false) => FileIdExtdDirectoryInfo,
                (false, true) => FileIdBothDirectoryRestartInfo,
                (false, false) => FileIdBothDirectoryInfo,
            };
            // Clear unused bytes so a final short batch never exposes old data.
            storage.fill(0);
            // SAFETY: Vec<u64> supplies 8-byte alignment and exactly ENUM_BYTES
            // writable bytes. The synchronous API cannot retain the buffer.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    directory.as_raw_handle(),
                    class,
                    storage.as_mut_ptr().cast(),
                    ENUM_BYTES as u32,
                )
            };
            if ok == 0 {
                let error = last_error();
                if error.code() == ERROR_NO_MORE_FILES {
                    cursor.done = true;
                    return Ok(EntryBatch::new(entries));
                }
                if !cursor.started && cursor.extended && error.code() == ERROR_INVALID_PARAMETER {
                    cursor.extended = false;
                    continue;
                }
                return Err(error);
            }
            cursor.started = true;
            cursor.offset = Some(0);
        }
    }

    fn basic_info(&self, handle: &WinHandle) -> Result<BasicInfo, FsError> {
        if !handle.access.read_attributes {
            return Err(FsError::AccessDenied);
        }
        let basic = Self::basic(handle)?;
        let tag = Self::attribute_tag(handle)?;
        // SAFETY: FileStandardInfo writes exactly FILE_STANDARD_INFO.
        let standard: FILE_STANDARD_INFO =
            unsafe { query_info(handle.as_raw_handle(), FileStandardInfo)? };
        let reparse_tag = if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            None
        } else if tag.ReparseTag == IO_REPARSE_TAG_MOUNT_POINT {
            Some(Self::mount_tag(handle)?)
        } else {
            Some(tag_hint(tag.ReparseTag, tag.FileAttributes))
        };
        Ok(BasicInfo {
            attributes: basic.FileAttributes,
            reparse_tag,
            creation_time: system_time(basic.CreationTime),
            last_write_time: system_time(basic.LastWriteTime),
            change_time: system_time(basic.ChangeTime),
            size: standard.EndOfFile.max(0) as u64,
        })
    }

    fn set_disposition(&self, handle: &WinHandle, flags: DispFlags) -> Result<(), FsError> {
        handle.disposition.set(None);
        if !handle.access.delete {
            return Err(FsError::AccessDenied);
        }
        // One attempt only. The engine owns unsupported-version fallbacks,
        // readonly retries, and classic_pending_unconfirmed accounting.
        let result = Self::disposition_once(handle, flags);
        if result.is_ok() {
            handle.disposition.set(Some(flags));
        }
        result
    }

    fn clear_readonly(&self, handle: &WinHandle) -> Result<(), FsError> {
        let basic = Self::basic(handle)?;
        if basic.FileAttributes & FILE_ATTRIBUTE_READONLY == 0 {
            return Ok(());
        }
        let writable = Self::writable_attributes(handle)?;
        set_attributes(
            writable.as_raw_handle(),
            without_readonly(basic.FileAttributes),
        )
    }

    fn close(&self, handle: WinHandle) {
        drop(handle);
    }

    fn volume_caps(&self, handle: &WinHandle, query_extents: bool) -> VolumeCaps {
        let Some(volume) = Self::volume(handle) else {
            return VolumeCaps::default();
        };
        VolumeCaps {
            // Support is established by the disposition operation, not guessed
            // from OS version or a different file on the same device.
            posix_delete: None,
            seek_penalty: Self::seek_penalty(&volume),
            device_id: if query_extents {
                Self::device_id(&volume)
            } else {
                None
            },
        }
    }
}

fn last_error() -> FsError {
    // SAFETY: GetLastError has no preconditions and reads this thread's last error.
    FsError::from_code(unsafe { GetLastError() })
}

/// The caller must select the SDK output type corresponding to `class`.
unsafe fn query_info<T>(raw: HANDLE, class: FILE_INFO_BY_HANDLE_CLASS) -> Result<T, FsError> {
    let mut info = MaybeUninit::<T>::zeroed();
    // SAFETY: callers supply the exact SDK type for class; storage is aligned and
    // sized for T. The synchronous API writes no more than the supplied length.
    let ok = unsafe {
        GetFileInformationByHandleEx(raw, class, info.as_mut_ptr().cast(), size_of::<T>() as u32)
    };
    if ok == 0 {
        Err(last_error())
    } else {
        // SAFETY: the API succeeded and initialized the requested SDK structure.
        Ok(unsafe { info.assume_init() })
    }
}

fn set_info<T>(raw: HANDLE, class: FILE_INFO_BY_HANDLE_CLASS, info: &T) -> Result<(), FsError> {
    // SAFETY: all call sites pair class with its matching SDK structure. The API
    // only reads the supplied size and does not retain this pointer.
    let ok = unsafe {
        SetFileInformationByHandle(raw, class, (info as *const T).cast(), size_of::<T>() as u32)
    };
    if ok == 0 {
        Err(last_error())
    } else {
        Ok(())
    }
}

fn set_attributes(raw: HANDLE, attributes: u32) -> Result<(), FsError> {
    set_info(
        raw,
        FileBasicInfo,
        &FILE_BASIC_INFO {
            CreationTime: 0,
            LastAccessTime: 0,
            LastWriteTime: 0,
            ChangeTime: 0,
            FileAttributes: attributes,
        },
    )
}

fn without_readonly(attributes: u32) -> u32 {
    match attributes & !FILE_ATTRIBUTE_READONLY {
        0 => FILE_ATTRIBUTE_NORMAL,
        attributes => attributes,
    }
}

fn buffer_bytes(storage: &[u64]) -> &[u8] {
    // SAFETY: every byte of initialized u64 storage is initialized, a byte slice
    // needs no stronger alignment, and it cannot outlive the borrowed storage.
    unsafe { std::slice::from_raw_parts(storage.as_ptr().cast(), size_of_val(storage)) }
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, FsError> {
    let field = bytes
        .get(offset..offset + 4)
        .ok_or(FsError::from_code(ERROR_INVALID_DATA))?;
    Ok(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
}

fn append_entries(
    bytes: &[u8],
    extended: bool,
    entries: &mut Vec<Entry>,
    position: &mut Option<usize>,
    limit: usize,
) -> Result<(), FsError> {
    let name_offset = if extended {
        offset_of!(FILE_ID_EXTD_DIR_INFO, FileName)
    } else {
        offset_of!(FILE_ID_BOTH_DIR_INFO, FileName)
    };
    while let Some(offset) = *position {
        let rest = bytes
            .get(offset..)
            .ok_or(FsError::from_code(ERROR_INVALID_DATA))?;
        let next = u32_at(rest, 0)? as usize;
        let record = if next == 0 {
            rest
        } else {
            if !next.is_multiple_of(8) || next < name_offset || next >= rest.len() {
                return Err(FsError::from_code(ERROR_INVALID_DATA));
            }
            &rest[..next]
        };
        // These fields are in the common prefix of both SDK structures.
        let attributes = u32_at(record, offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes))?;
        let name_length =
            u32_at(record, offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength))? as usize;
        if name_length == 0 || !name_length.is_multiple_of(2) {
            return Err(FsError::from_code(ERROR_INVALID_DATA));
        }
        let name = record
            .get(name_offset..name_offset + name_length)
            .ok_or(FsError::from_code(ERROR_INVALID_DATA))?;
        let wide: Vec<u16> = name
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        if wide != [46] && wide != [46, 46] {
            let reparse_tag_hint = if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                let tag = if extended {
                    u32_at(record, offset_of!(FILE_ID_EXTD_DIR_INFO, ReparsePointTag))?
                } else {
                    0 // FileIdBothDirectoryInfo has no reparse tag.
                };
                Some(tag_hint(tag, attributes))
            } else {
                None
            };
            entries.push(Entry {
                name: OsString::from_wide(&wide),
                is_dir_hint: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                reparse_tag_hint,
            });
        }
        *position = if next == 0 { None } else { Some(offset + next) };
        if entries.len() >= limit {
            return Ok(());
        }
    }
    Ok(())
}

fn tag_hint(tag: u32, attributes: u32) -> ReparseTag {
    match tag {
        IO_REPARSE_TAG_SYMLINK if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 => {
            ReparseTag::DirSymlink
        }
        IO_REPARSE_TAG_SYMLINK => ReparseTag::FileSymlink,
        // The directory record cannot distinguish junctions from volume mounts.
        // basic_info reads the substitute name from the opened reparse handle.
        _ => ReparseTag::Unknown(tag),
    }
}

fn mount_tag_from_buffer(bytes: &[u8]) -> Result<ReparseTag, FsError> {
    if bytes.len() < 16 || u32_at(bytes, 0)? != IO_REPARSE_TAG_MOUNT_POINT {
        return Err(FsError::from_code(ERROR_INVALID_DATA));
    }
    let data_length = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let offset = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let length = u16::from_le_bytes([bytes[10], bytes[11]]) as usize;
    if data_length < 8
        || !offset.is_multiple_of(2)
        || !length.is_multiple_of(2)
        || offset + length > data_length - 8
    {
        return Err(FsError::from_code(ERROR_INVALID_DATA));
    }
    let substitute = bytes
        .get(16 + offset..16 + offset + length)
        .ok_or(FsError::from_code(ERROR_INVALID_DATA))?;
    let wide: Vec<u16> = substitute
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let prefix: Vec<u16> = r"\??\Volume{".encode_utf16().collect();
    if wide.starts_with(&prefix) {
        Ok(ReparseTag::MountPoint)
    } else {
        Ok(ReparseTag::Junction)
    }
}

fn system_time(ticks: i64) -> SystemTime {
    let ticks = i128::from(ticks) - i128::from(WINDOWS_EPOCH_TICKS);
    let magnitude = ticks.unsigned_abs();
    let duration = Duration::new(
        (magnitude / 10_000_000) as u64,
        ((magnitude % 10_000_000) * 100) as u32,
    );
    if ticks >= 0 {
        SystemTime::UNIX_EPOCH + duration
    } else {
        SystemTime::UNIX_EPOCH - duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_substitute_names_distinguish_volumes_from_junctions() {
        for (name, expected) in [
            (r"\??\C:\outside", ReparseTag::Junction),
            (
                r"\??\Volume{11111111-2222-3333-4444-555555555555}\",
                ReparseTag::MountPoint,
            ),
        ] {
            let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let mut bytes = vec![0u8; 16 + name.len()];
            bytes[..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
            bytes[4..6].copy_from_slice(&((8 + name.len()) as u16).to_le_bytes());
            bytes[10..12].copy_from_slice(&(name.len() as u16).to_le_bytes());
            bytes[16..].copy_from_slice(&name);
            assert_eq!(mount_tag_from_buffer(&bytes), Ok(expected));
        }
        assert_eq!(
            without_readonly(FILE_ATTRIBUTE_READONLY),
            FILE_ATTRIBUTE_NORMAL
        );
        assert_eq!(
            tag_hint(0x80000042, FILE_ATTRIBUTE_DIRECTORY),
            ReparseTag::Unknown(0x80000042)
        );
    }

    #[test]
    fn directory_record_fallback_reads_utf16_byte_lengths_and_skips_dot_entries() {
        for extended in [true, false] {
            let offset = if extended {
                offset_of!(FILE_ID_EXTD_DIR_INFO, FileName)
            } else {
                offset_of!(FILE_ID_BOTH_DIR_INFO, FileName)
            };
            let mut bytes = Vec::new();
            for (i, name) in [".", "..", "file-\u{1f680}"].into_iter().enumerate() {
                let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
                let length = (offset + name.len()).next_multiple_of(8);
                let mut record = vec![0u8; length];
                if i != 2 {
                    record[..4].copy_from_slice(&(length as u32).to_le_bytes());
                }
                let name_length = offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength);
                record[name_length..name_length + 4]
                    .copy_from_slice(&(name.len() as u32).to_le_bytes());
                record[offset..offset + name.len()].copy_from_slice(&name);
                bytes.extend(record);
            }
            let mut entries = Vec::new();
            append_entries(
                &bytes,
                extended,
                &mut entries,
                &mut Some(0),
                ENUM_BATCH_SIZE,
            )
            .unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "file-\u{1f680}");
        }
    }
}
