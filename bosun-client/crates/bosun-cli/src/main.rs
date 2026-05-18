//! Binary entry-point. Парсит CLI и делегирует в `run::run` либо печатает
//! версию. `version` остаётся без побочных эффектов — diagnostics-friendly.

mod args;
mod bootstrap;
mod exit_code;
mod logging;
mod metric;
mod run;

use clap::Parser;

fn main() {
    let cli = args::Cli::parse();
    let code = match cli.command {
        args::Command::Version => {
            println!("bosun version {}", env!("CARGO_PKG_VERSION"));
            exit_code::SUCCESS
        }
        args::Command::Apply(apply_args) => run::run(&apply_args),
    };
    std::process::exit(code);
}
