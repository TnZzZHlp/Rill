mod check;
mod reload;
mod run;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "rill",
    version,
    about = "HTTP reverse proxy and configuration checker"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Reload {
        #[arg(long, default_value = "/run/rill/control.sock")]
        socket: PathBuf,
        #[arg(long, default_value = "30s", value_parser = reload::parse_timeout)]
        timeout: std::time::Duration,
    },
    #[command(about = "Run the HTTP reverse proxy")]
    Run {
        #[arg(long, value_name = "PATH", default_value = "/etc/rill/config.kdl")]
        config: PathBuf,
    },
    #[command(about = "Validate configuration without starting or contacting a service")]
    Check {
        #[arg(long, value_name = "PATH", default_value = "/etc/rill/config.kdl")]
        config: PathBuf,
    },
}

pub fn run() -> ExitCode {
    match Cli::parse().command {
        Command::Reload { socket, timeout } => ExitCode::from(reload::run(&socket, timeout)),
        Command::Check { config } => ExitCode::from(check::check(&config)),
        Command::Run { config } => ExitCode::from(run::run(&config)),
    }
}
