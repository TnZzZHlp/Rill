mod check;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "rill",
    version,
    about = "Stage 002 configuration checker; run and reload are not yet available"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Validate configuration without starting or contacting a service")]
    Check {
        #[arg(long, value_name = "PATH", default_value = "/etc/rill/config.kdl")]
        config: PathBuf,
    },
}

pub fn run() -> ExitCode {
    match Cli::parse().command {
        Command::Check { config } => ExitCode::from(check::check(&config)),
    }
}
