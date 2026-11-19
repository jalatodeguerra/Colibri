//! The `colibri` command line tool.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "colibri", version, about = "The Colibri programming language")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a Colibri program
    Run { file: PathBuf },
    /// Type-check a Colibri program without running it
    Check { file: PathBuf },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let name = match cli.command {
        Command::Run { .. } => "run",
        Command::Check { .. } => "check",
    };
    eprintln!("colibri {name}: not implemented yet");
    ExitCode::from(2)
}
