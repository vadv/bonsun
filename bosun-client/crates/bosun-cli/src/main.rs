mod args;

use clap::Parser;

fn main() {
    let cli = args::Cli::parse();
    match cli.command {
        args::Command::Version => {
            println!("bosun version {}", env!("CARGO_PKG_VERSION"));
        }
        args::Command::Apply(_) => {
            eprintln!("apply: not yet implemented");
            std::process::exit(2);
        }
    }
}
