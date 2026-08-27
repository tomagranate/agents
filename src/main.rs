mod archive;
mod background;
mod config;
mod home;
mod mcp;
mod plans;
mod progress;
mod settings;
mod updater;
mod util;

use agents::sudo;
use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agents",
    version,
    about = "Manage shared AI agent configuration and agents archives"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show local and remote configuration status.
    Status {
        /// Do not fetch remote state.
        #[arg(long)]
        offline: bool,
        /// Show filesystem paths and more detail.
        #[arg(long)]
        verbose: bool,
    },
    /// Scaffold the shared configuration.
    Init {
        #[arg(long)]
        force: bool,
        #[arg(long)]
        no_apply: bool,
    },
    /// Reconcile, apply, commit, and push agents-home.
    Sync {
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    /// Show shared and harness-specific skills.
    Skills { harness: Option<String> },
    /// Print effective shared and harness-specific instructions.
    Md { harness: Option<String> },
    /// Show managed harness settings and local drift.
    Settings { harness: Option<String> },
    /// Open agents-home in Zed.
    Edit,
    /// Run individual agents-home operations.
    Home {
        #[command(subcommand)]
        command: HomeCommand,
    },
    /// Print the installed version.
    Version,
    /// Manage the unified agents archive.
    Archive {
        #[command(subcommand)]
        command: archive::ArchiveCommand,
    },
    /// Manage plans in the plans archive.
    Plans(plans::PlansArgs),
    /// Manage public media in the plans archive.
    Media(plans::MediaArgs),
    /// Update this command to the newest release.
    #[command(visible_alias = "upgrade")]
    Update(updater::UpdateArgs),
    /// Grant sudo and 1Password tickets on a machine.
    Sudo(sudo::SudoArgs),
    /// Read cached update state and start a background refresh.
    #[command(name = "_shell-check", hide = true)]
    ShellCheck,
    /// Refresh cached update state.
    #[command(name = "_refresh-updates", hide = true)]
    RefreshUpdates,
}

#[derive(Subcommand)]
enum HomeCommand {
    /// Run individual operations normally handled by sync.
    Advanced {
        #[command(subcommand)]
        command: HomeAdvancedCommand,
    },
}

#[derive(Subcommand)]
enum HomeAdvancedCommand {
    /// Fetch and rebase agents-home without applying or pushing.
    Pull,
    /// Push existing agents-home commits.
    Push,
    /// Apply current content to installed harnesses.
    Apply,
    /// Capture changed managed settings without Git operations.
    Capture,
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
    match cli.command.unwrap_or(Command::Status {
        offline: false,
        verbose: false,
    }) {
        Command::Status { offline, verbose } => home::status(&paths, offline, verbose),
        Command::Init { force, no_apply } => home::init(&paths, force, !no_apply),
        Command::Sync { message } => {
            home::sync(&paths, message.as_deref().unwrap_or("Update agents home"))
        }
        Command::Skills { harness } => home::skills(&paths, harness.as_deref()),
        Command::Md { harness } => home::md(&paths, harness.as_deref()),
        Command::Settings { harness } => settings::show(&paths, harness.as_deref()),
        Command::Edit => home::edit(&paths),
        Command::Home {
            command: HomeCommand::Advanced { command },
        } => match command {
            HomeAdvancedCommand::Pull => home::pull(&paths),
            HomeAdvancedCommand::Push => home::push(&paths),
            HomeAdvancedCommand::Apply => home::apply(&paths),
            HomeAdvancedCommand::Capture => {
                settings::capture(&paths)?;
                Ok(())
            }
        },
        Command::Version => {
            println!("agents {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Archive { command } => archive::run(&paths, command),
        Command::Plans(args) => plans::run(&paths, args),
        Command::Media(args) => plans::run_media(&paths, args),
        Command::Update(args) => updater::run(args),
        Command::Sudo(args) => sudo::run(args),
        Command::ShellCheck => background::shell_check(&paths),
        Command::RefreshUpdates => background::refresh(&paths),
    }
}
