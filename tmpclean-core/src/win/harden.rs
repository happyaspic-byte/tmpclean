//! Process hardening applied first thing in both executables.

use windows_sys::Win32::System::LibraryLoader::{
    SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

/// Restrict the default DLL search path to `%SystemRoot%\System32` so a
/// planted DLL next to the exe or in the current directory is never loaded
/// by later `LoadLibrary` calls (including delay-loaded imports).
///
/// Returns `true` when the setting was applied. Failure is not fatal: the
/// process keeps the OS default search order and continues; this function
/// never panics.
pub fn harden() -> bool {
    // SAFETY: SetDefaultDllDirectories takes a plain flag value, has no
    // pointer arguments, and only changes process-wide loader state. The
    // flag is a documented constant supported on every target build
    // (Windows 8+ / KB2533623, so all of Windows 10 1709+).
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) != 0 }
}

#[cfg(test)]
mod tests {
    use super::harden;

    #[test]
    fn harden_succeeds_and_is_idempotent() {
        assert!(harden());
        assert!(harden());
    }
}
