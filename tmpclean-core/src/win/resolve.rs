//! OS discovery and handle-based root validation. No function elevates the process.

use super::{WinFs, WinHandle};
use crate::fs::{FileSystem, FsError, DIRECTORY, REPARSE_POINT};
use crate::resolve::{normalize_path, validate_root, Resolution, Target};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetFinalPathNameByHandleW, GetTempPathW, GetVolumePathNameW,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;
use windows_sys::Win32::UI::Shell::{
    FOLDERID_InternetCache, FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_ProgramFiles,
    FOLDERID_ProgramFilesX86, SHGetKnownFolderPath,
};

pub type ResolvedTarget = Resolution<WinHandle>;

/// Resolve just the requested logical target. `WindowsTemp` is never added implicitly.
pub fn resolve_target(target: Target) -> ResolvedTarget {
    let path = match target {
        Target::UserTemp => temp_path(),
        Target::InetCache => known_folder(&FOLDERID_InternetCache),
        Target::WindowsTemp => windows_directory().map(|path| path.join("Temp")),
    };
    match path {
        Ok(path) => resolve_path(&path),
        Err(error) => failure(error),
    }
}

/// Preserve every per-target result, including refusals.
pub fn resolve_targets(targets: &[Target]) -> Vec<(Target, ResolvedTarget)> {
    targets
        .iter()
        .map(|&target| (target, resolve_target(target)))
        .collect()
}

fn failure(error: FsError) -> ResolvedTarget {
    match error {
        FsError::NotFound(_) => Resolution::Absent,
        FsError::AccessDenied => Resolution::RequiresElevation,
        _ => Resolution::Refused("io-error"),
    }
}

fn last_error() -> FsError {
    // SAFETY: GetLastError has no preconditions.
    FsError::from_code(unsafe { GetLastError() })
}

fn resolve_path(path: &Path) -> ResolvedTarget {
    // GetTempPath[2]W appends a separator. Remove it before the no-follow open,
    // keeping the directory itself (rather than an empty child) as the last component.
    let path: PathBuf = path.components().collect();
    let handle = match WinFs::open_root(path) {
        Ok(handle) => handle,
        Err(error) => return failure(error),
    };
    let info = match WinFs::new().basic_info(&handle) {
        Ok(info) => info,
        Err(error) => return failure(error),
    };
    // Do not ask path-based APIs to inspect a junction's destination.
    if info.attributes & REPARSE_POINT != 0 {
        return Resolution::Refused("reparse-root");
    }
    if info.attributes & DIRECTORY == 0 {
        return Resolution::Refused("not-directory");
    }
    let final_path = match final_path(&handle) {
        Ok(path) => path,
        Err(error) => return failure(error),
    };
    let protected = match protected_paths() {
        Ok(paths) => paths,
        Err(error) => return failure(error),
    };
    if let Err(reason) = validate_root(&final_path, true, false, &protected) {
        return Resolution::Refused(reason);
    }
    let fixed = match fixed_volume(&final_path) {
        Ok(fixed) => fixed,
        Err(error) => return failure(error),
    };
    match validate_root(&final_path, fixed, false, &protected) {
        Ok(()) => Resolution::Resolved { handle, final_path },
        Err(reason) => Resolution::Refused(reason),
    }
}

fn final_path(handle: &WinHandle) -> Result<String, FsError> {
    let path = read_path(|buffer, length| {
        // SAFETY: handle is owned and live; read_path supplies a writable buffer.
        unsafe { GetFinalPathNameByHandleW(handle.as_raw_handle(), buffer, length, 0) }
    })?;
    path.to_str().map(normalize_path).ok_or(FsError::Other(123))
}

/// APIs report the required capacity including NUL if the buffer is too small.
fn read_path(mut call: impl FnMut(*mut u16, u32) -> u32) -> Result<PathBuf, FsError> {
    let mut buffer = vec![0u16; 261];
    loop {
        let length = call(buffer.as_mut_ptr(), buffer.len() as u32) as usize;
        if length == 0 {
            return Err(last_error());
        }
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(PathBuf::from(OsString::from_wide(&buffer)));
        }
        buffer.resize(length + 1, 0);
    }
}

fn temp_path() -> Result<PathBuf, FsError> {
    type GetTempPath = unsafe extern "system" fn(u32, *mut u16) -> u32;
    let module_name: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
    // SAFETY: both symbol names are terminated and kernel32 remains loaded.
    let function: GetTempPath = unsafe {
        let module = GetModuleHandleW(module_name.as_ptr());
        let symbol = if module.is_null() {
            None
        } else {
            GetProcAddress(module, c"GetTempPath2W".as_ptr().cast())
        };
        match symbol {
            Some(symbol) => {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, GetTempPath>(symbol)
            }
            None => GetTempPathW,
        }
    };
    read_path(|buffer, length| {
        // SAFETY: function has the GetTempPath[2]W signature; buffer has length elements.
        unsafe { function(length, buffer) }
    })
}

fn windows_directory() -> Result<PathBuf, FsError> {
    read_path(|buffer, length| {
        // SAFETY: read_path supplies length writable UTF-16 elements.
        unsafe { GetWindowsDirectoryW(buffer, length) }
    })
}

fn known_folder(id: &GUID) -> Result<PathBuf, FsError> {
    let mut pointer = null_mut();
    // SAFETY: id and the output pointer are valid; null token means current user.
    let result = unsafe { SHGetKnownFolderPath(id, 0, null_mut(), &mut pointer) };
    let path = if result >= 0 {
        // SAFETY: success returns an allocated, NUL-terminated UTF-16 string.
        unsafe {
            let mut length = 0;
            while *pointer.add(length) != 0 {
                length += 1;
            }
            Ok(PathBuf::from(OsString::from_wide(
                std::slice::from_raw_parts(pointer, length),
            )))
        }
    } else {
        // HRESULT_FROM_WIN32 keeps the original error in the low word.
        Err(FsError::from_code((result as u32) & 0xffff))
    };
    // SAFETY: the API allocates with the COM task allocator; null is also allowed.
    unsafe { CoTaskMemFree(pointer.cast()) };
    path
}

fn protected_paths() -> Result<Vec<String>, FsError> {
    let mut paths = vec![windows_directory()?];
    for id in [
        FOLDERID_Profile,
        FOLDERID_LocalAppData,
        FOLDERID_ProgramFiles,
        FOLDERID_ProgramFilesX86,
    ] {
        paths.push(known_folder(&id)?);
    }
    let mut protected = Vec::new();
    for path in paths {
        // Compare both the configured spelling and the handle's canonical spelling,
        // so an 8.3 name or an ancestor junction cannot bypass the protected list.
        let spelling = path.to_str().ok_or(FsError::Other(123))?;
        protected.push(normalize_path(spelling));
        match WinFs::open_root(&path) {
            Ok(handle) => protected.push(final_path(&handle)?),
            Err(FsError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(protected)
}

fn fixed_volume(path: &str) -> Result<bool, FsError> {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let mut volume = vec![0u16; 32768];
    // SAFETY: input is terminated and output has the advertised capacity.
    if unsafe { GetVolumePathNameW(wide.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) } == 0 {
        return Err(last_error());
    }
    // SAFETY: GetVolumePathNameW returned a terminated volume root.
    Ok(unsafe { GetDriveTypeW(volume.as_ptr()) } == DRIVE_FIXED)
}
