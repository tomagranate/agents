//! Development previews: named dev servers on tombook-linux, each served at
//! `<name>.preview.tomagranate.com` through Caddy and the preview daemon.

mod serve;
mod service;
mod state;

use std::{env, fs, net::SocketAddr, path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Subcommand;

use crate::config::Paths;
use service::checked;
use state::{DEFAULT_IDLE_TTL, Record, StopReason, Store};

const DEFAULT_LISTEN: &str = "127.0.0.1:8770";
const DAEMON_UNIT: &str = "agents-preview.service";

#[derive(Debug, Subcommand)]
pub enum PreviewCommand {
    /// Start or replace a named preview.
    Start {
        /// Stable preview name. Becomes the subdomain label.
        name: String,
        /// Local HTTP port used by the development server.
        #[arg(long)]
        port: u16,
        /// Stop after this long without a request, such as 12h or 2d.
        #[arg(long, default_value = DEFAULT_IDLE_TTL)]
        idle: String,
        /// Working directory. Defaults to the current directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Development server command and arguments.
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Show one preview or all previews.
    Status { name: Option<String> },
    /// Alias for status without a name.
    List,
    /// Stop a preview. Its record stays so it can be revived.
    Stop { name: String },
    /// Start a stopped preview again from its record.
    Revive { name: String },
    /// Stop a preview and forget it.
    Rm { name: String },
    /// Run the preview daemon: index page, API, and per-host proxy.
    Serve {
        /// Address for Caddy to reach.
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: SocketAddr,
    },
    /// Install and start the daemon as a systemd user service.
    Install,
}

pub fn run(paths: &Paths, command: PreviewCommand) -> Result<()> {
    let store = Store::new(&paths.state_dir);
    match command {
        PreviewCommand::Serve { listen } => serve::serve(store, listen),
        other => {
            require_linux()?;
            match other {
                PreviewCommand::Start {
                    name,
                    port,
                    idle,
                    cwd,
                    command,
                } => start(&store, name, port, idle, cwd, command),
                PreviewCommand::Status { name } => status(&store, name.as_deref()),
                PreviewCommand::List => status(&store, None),
                PreviewCommand::Stop { name } => stop(&store, &name),
                PreviewCommand::Revive { name } => revive(&store, &name),
                PreviewCommand::Rm { name } => remove(&store, &name),
                PreviewCommand::Install => install(&store),
                PreviewCommand::Serve { .. } => unreachable!(),
            }
        }
    }
}

fn start(
    store: &Store,
    name: String,
    port: u16,
    idle: String,
    cwd: Option<PathBuf>,
    command: Vec<String>,
) -> Result<()> {
    state::validate_name(&name)?;
    state::parse_duration(&idle)?;
    let cwd = cwd
        .unwrap_or(env::current_dir()?)
        .canonicalize()
        .context("resolve preview working directory")?;
    let now = Utc::now();
    let created_at = store
        .load(&name)?
        .map_or(now, |previous| previous.created_at);
    let mut record = Record {
        name,
        port,
        cwd,
        command,
        path: env::var("PATH").unwrap_or_default(),
        idle_ttl: idle,
        created_at,
        started_at: now,
        last_used_at: now,
        stopped: None,
        last_error: None,
    };
    service::start(store, &mut record)?;
    service::wait_for_http(&record.url(), 30)?;
    println!("{}", record.url());
    println!("status: agents preview status {}", record.name);
    println!("logs:   journalctl --user -fu {}", record.unit());
    Ok(())
}

fn revive(store: &Store, name: &str) -> Result<()> {
    let mut record = store.require(name)?;
    if record.is_running() {
        bail!("preview '{name}' is already running");
    }
    service::start(store, &mut record)?;
    println!("{}", record.url());
    Ok(())
}

fn status(store: &Store, name: Option<&str>) -> Result<()> {
    let records = match name {
        Some(name) => vec![store.require(name)?],
        None => store.all()?,
    };
    if records.is_empty() {
        println!("No previews.");
        return Ok(());
    }
    for record in records {
        let state = match &record.stopped {
            None => "active".to_owned(),
            Some(stopped) => format!("{:?}", stopped.reason).to_lowercase(),
        };
        println!(
            "{}\t{}\tused {}\t{}",
            record.name,
            state,
            record.last_used_at.format("%Y-%m-%d %H:%M"),
            record.url()
        );
        if let Some(error) = &record.last_error {
            println!("\terror: {error}");
        }
    }
    Ok(())
}

fn stop(store: &Store, name: &str) -> Result<()> {
    let mut record = store.require(name)?;
    service::stop(store, &mut record, StopReason::Stopped)?;
    println!(
        "Stopped {name}. Revive it from https://{}/ or `agents preview revive {name}`.",
        state::DOMAIN
    );
    Ok(())
}

fn remove(store: &Store, name: &str) -> Result<()> {
    let record = store.require(name)?;
    let _ = service::systemctl(&["stop", &record.unit()]);
    store.remove(name)?;
    println!("Removed {name}.");
    Ok(())
}

/// Writes the daemon's user unit, starts it, and migrates old records.
fn install(store: &Store) -> Result<()> {
    migrate(store)?;

    let exe = env::current_exe()?.canonicalize()?;
    let path = env::var("PATH").unwrap_or_default();
    // The daemon must read the same state directory as the CLI.
    let state_home = env::var("XDG_STATE_HOME")
        .map(|value| format!("Environment=XDG_STATE_HOME={value}\n"))
        .unwrap_or_default();
    let unit = format!(
        "[Unit]\nDescription=agents preview daemon\nAfter=network.target\n\n[Service]\nExecStart={} preview serve\nEnvironment=PATH={path}\n{state_home}Restart=on-failure\nRestartSec=2s\n\n[Install]\nWantedBy=default.target\n",
        exe.display()
    );
    let dir = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("systemd/user");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(DAEMON_UNIT), unit)?;

    checked(
        Command::new("systemctl").args(["--user", "daemon-reload"]),
        "reload user units",
    )?;
    checked(
        Command::new("systemctl").args(["--user", "enable", "--now", DAEMON_UNIT]),
        "enable preview daemon",
    )?;
    checked(
        Command::new("systemctl").args(["--user", "restart", DAEMON_UNIT]),
        "restart preview daemon",
    )?;
    service::wait_for_http(&format!("http://{DEFAULT_LISTEN}/"), 10)?;
    println!("Preview daemon running on http://{DEFAULT_LISTEN}/");
    println!("logs: journalctl --user -fu {DAEMON_UNIT}");
    Ok(())
}

/// Rewrites records from the Tailscale Serve era and turns off their routes.
fn migrate(store: &Store) -> Result<()> {
    for record in store.all()? {
        let raw = fs::read_to_string(store.path(&record.name))?;
        if !raw.contains("ts.net") {
            continue;
        }
        let _ = Command::new("tailscale")
            .args(["serve", &format!("--https={}", record.port), "off"])
            .output();
        let mut record = record;
        if service::activity(&record.unit()) != service::Activity::Active {
            record.mark_stopped(StopReason::Exited);
        }
        store.save(&record)?;
        println!("Migrated {}.", record.name);
    }
    Ok(())
}

fn require_linux() -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("app previews currently require Linux and systemd");
    }
    Ok(())
}
