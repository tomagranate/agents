mod archive;
mod config;
mod legacy;
mod updater;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agents",
    version,
    about = "Manage shared AI agent configuration and chat archives"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show the configuration status.
    #[command(alias = "st")]
    Status,
    /// Scaffold the shared configuration.
    Init {
        #[arg(long)]
        force: bool,
        #[arg(long)]
        no_sync: bool,
    },
    /// Pull agents-home and wire all harnesses.
    Pull {
        #[arg(long)]
        no_sync: bool,
    },
    /// Commit and push agents-home.
    Push {
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    /// Show the shared skill matrix.
    #[command(alias = "sk")]
    Skills,
    /// Print resolved AGENTS text.
    #[command(alias = "agents-md")]
    Md { harness: Option<String> },
    /// Wire rules and shared skills into each harness.
    Sync,
    /// Print important filesystem paths.
    #[command(alias = "path")]
    Paths,
    /// Print the installed version.
    Version,
    /// Manage the unified chat archive.
    Archive {
        #[command(subcommand)]
        command: archive::ArchiveCommand,
    },
    /// Update this command to the newest release.
    #[command(alias = "upgrade")]
    Update(updater::UpdateArgs),
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = config::Paths::discover()?;
    match cli.command.unwrap_or(Command::Status) {
        Command::Status => legacy::status(&paths),
        Command::Init { force, no_sync } => legacy::init(&paths, force, !no_sync),
        Command::Pull { no_sync } => legacy::pull(&paths, !no_sync),
        Command::Push { message } => legacy::push(&paths, message.as_deref()),
        Command::Skills => legacy::skills(&paths),
        Command::Md { harness } => legacy::md(&paths, harness.as_deref()),
        Command::Sync => legacy::sync(&paths),
        Command::Paths => legacy::print_paths(&paths),
        Command::Version => {
            println!("agents {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Archive { command } => archive::run(&paths, command),
        Command::Update(args) => updater::run(args),
    }
}
