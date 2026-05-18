use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "bosun", version, about = "bosun-client agent")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print version and exit.
    Version,
    /// Apply a bundle to the local system.
    Apply(ApplyArgs),
}

#[derive(Debug, clap::Args)]
pub struct ApplyArgs {
    #[arg(long)]
    pub bundle: std::path::PathBuf,
    #[arg(long)]
    pub inventory: Option<std::path::PathBuf>,
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}
