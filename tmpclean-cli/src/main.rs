#[cfg(windows)]
mod app;
#[cfg(any(windows, test))]
mod args;

use std::process::ExitCode;

#[cfg(windows)]
fn main() -> ExitCode {
    // Restrict DLL search to System32 before anything else runs.
    tmpclean_core::win::harden();
    app::run()
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!(
        "{}: {}",
        tmpclean_core::strings::APP_NAME,
        tmpclean_core::strings::WINDOWS_ONLY
    );
    ExitCode::from(2)
}
