#![windows_subsystem = "windows"]

use clap::Parser;

/// Fast, safe Windows temp folder cleaner (window).
#[derive(Parser)]
#[command(name = "tmpclean-gui", version = tmpclean_core::VERSION)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
