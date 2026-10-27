#![windows_subsystem = "windows"]
// The pure modules are only driven by the Windows window; Linux builds a stub.
#![cfg_attr(not(windows), allow(dead_code))]

mod args;
mod view;

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod win32;

#[cfg(windows)]
fn main() {
    tmpclean_core::win::harden();
    use clap::Parser;
    let cli = match args::Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // No console in the windows subsystem: show clap's text, keep its
            // exit code (2 = usage error, 0 = --help/--version).
            let code = err.exit_code();
            let text = err.render().to_string();
            eprint!("{text}");
            win32::message_box(tmpclean_core::strings::APP_NAME, &text, code != 0);
            std::process::exit(code);
        }
    };
    std::process::exit(i32::from(app::run(&cli)));
}

#[cfg(not(windows))]
fn main() {
    eprintln!("{}", tmpclean_core::strings::GUI_WINDOWS_ONLY);
    std::process::exit(2);
}
