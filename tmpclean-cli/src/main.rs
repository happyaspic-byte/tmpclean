use clap::Parser;

/// Fast, safe Windows temp folder cleaner.
#[derive(Parser)]
#[command(name = "tmpclean", version = tmpclean_core::VERSION)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
