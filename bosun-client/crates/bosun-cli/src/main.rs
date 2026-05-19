//! Binary entry-point. Парсит CLI и делегирует в соответствующий subcommand.
//! `version` остаётся без побочных эффектов — diagnostics-friendly.

mod args;
mod bootstrap;
mod bundle_validate;
mod exit_code;
mod logging;
mod metric;
mod run;
mod tags_metric;

use clap::Parser;

fn main() {
    let cli = args::Cli::parse();
    let code = match cli.command {
        args::Command::Version => {
            println!("bosun version {}", env!("CARGO_PKG_VERSION"));
            exit_code::SUCCESS
        }
        args::Command::Apply(apply_args) => run::run(&apply_args),
        args::Command::Bundle(bundle_cli) => match bundle_cli.command {
            args::BundleSubcommand::Validate(validate_args) => bundle_validate::run(&validate_args),
        },
    };
    std::process::exit(code);
}
