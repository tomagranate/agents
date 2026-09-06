//! Starting and stopping preview processes through systemd user units.
//! Shared by the CLI and the daemon so revive is the same code as start.

use std::{
    env,
    process::{Command, Output},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::state::{Record, StopReason, Store};

/// Hard cap on a preview's life. Idle expiry is the normal path; this only
/// matters when the daemon is not running to enforce it.
const MAX_RUNTIME: &str = "7d";

/// Result of asking systemd about a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Active,
    Inactive,
    /// systemd could not answer, for example when no user manager runs.
    Unknown,
}

/// Starts the record's command and waits for its local port. On success the
/// record is marked running and saved. On failure the record is marked
/// failed with the reason and saved, then the error is returned.
pub fn start(store: &Store, record: &mut Record) -> Result<()> {
    match try_start(record) {
        Ok(()) => {
            record.started_at = Utc::now();
            record.last_used_at = record.started_at;
            record.stopped = None;
            record.last_error = None;
            store.save(record)
        }
        Err(error) => {
            let _ = systemctl(&["stop", &record.unit()]);
            record.mark_stopped(StopReason::Failed);
            record.last_error = Some(format!("{error:#}"));
            store.save(record)?;
            Err(error)
        }
    }
}

fn try_start(record: &Record) -> Result<()> {
    if !record.cwd.is_dir() {
        bail!("directory {} does not exist", record.cwd.display());
    }
    if record.command.is_empty() {
        bail!("a development server command is required");
    }
    let unit = record.unit();
    let _ = systemctl(&["stop", &unit]);
    let _ = systemctl(&["reset-failed", &unit]);

    let path = if record.path.is_empty() {
        env::var("PATH").unwrap_or_default()
    } else {
        record.path.clone()
    };
    let mut args = vec![
        "--user".to_owned(),
        format!("--unit={unit}"),
        "--collect".to_owned(),
        format!("--property=RuntimeMaxSec={MAX_RUNTIME}"),
        format!("--property=WorkingDirectory={}", record.cwd.display()),
        format!("--setenv=PATH={path}"),
        format!("--setenv=PORT={}", record.port),
        "--".to_owned(),
    ];
    args.extend(record.command.iter().cloned());
    checked(
        Command::new("systemd-run").args(&args),
        "start preview service",
    )?;
    wait_for_http(&format!("http://127.0.0.1:{}/", record.port), 30)
}

/// Stops the unit and records why. Keeps the record.
pub fn stop(store: &Store, record: &mut Record, reason: StopReason) -> Result<()> {
    let _ = systemctl(&["stop", &record.unit()]);
    record.mark_stopped(reason);
    store.save(record)
}

pub fn activity(unit: &str) -> Activity {
    let Ok(output) = Command::new("systemctl")
        .args(["--user", "is-active", unit])
        .output()
    else {
        return Activity::Unknown;
    };
    match String::from_utf8_lossy(&output.stdout).trim() {
        "active" | "activating" | "reloading" => Activity::Active,
        "inactive" | "failed" | "deactivating" => Activity::Inactive,
        _ => Activity::Unknown,
    }
}

pub fn systemctl(args: &[&str]) -> Result<Output> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("run systemctl")
}

/// Polls a URL until it answers with a success status.
pub fn wait_for_http(url: &str, seconds: u64) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    for _ in 0..seconds * 2 {
        if client
            .get(url)
            .send()
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    bail!("preview did not become healthy at {url}")
}

pub fn checked(command: &mut Command, action: &str) -> Result<Output> {
    let output = command.output().with_context(|| action.to_owned())?;
    if !output.status.success() {
        bail!(
            "{action}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}
