use std::{
    fs::{self, OpenOptions},
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use fs2::FileExt;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{config::Paths, updater, util};

const MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);
/// The archive only earns a shell notice when its oldest unpulled commit is this old.
const ARCHIVE_NOTICE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Cached result of the network checks. Repository status is not cached: the
/// refresh fetches remote-tracking refs, and the shell check compares them to
/// HEAD locally, so a sync or pull clears its notice right away.
#[derive(Default, Serialize, Deserialize)]
struct UpdateState {
    checked_at: u64,
    latest_cli: Option<String>,
}

pub fn shell_check(paths: &Paths) -> Result<()> {
    let state_path = paths.state_dir.join("update-check.json");
    let state = fs::read(&state_path)
        .ok()
        .and_then(|contents| serde_json::from_slice::<UpdateState>(&contents).ok());
    print_notices(paths, state.as_ref());

    let stale = state
        .as_ref()
        .map(|state| now().saturating_sub(state.checked_at) > MAX_AGE.as_secs())
        .unwrap_or(true);
    if stale {
        let executable = std::env::current_exe()?;
        let _ = Command::new(executable)
            .arg("_refresh-updates")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    Ok(())
}

pub fn refresh(paths: &Paths) -> Result<()> {
    fs::create_dir_all(&paths.state_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(paths.state_dir.join("update-check.lock"))?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }

    let archive = archive_path(paths);
    let latest_cli = thread::scope(|scope| {
        let cli = scope.spawn(|| {
            updater::latest_version()
                .ok()
                .map(|latest| latest.to_string())
        });
        scope.spawn(|| fetch(&paths.agents_home));
        if let Some(archive) = &archive {
            scope.spawn(|| fetch(archive));
        }
        cli.join().unwrap_or(None)
    });
    let state = UpdateState {
        checked_at: now(),
        latest_cli,
    };
    util::atomic_write(
        &paths.state_dir.join("update-check.json"),
        &serde_json::to_vec_pretty(&state)?,
    )?;
    Ok(())
}

fn print_notices(paths: &Paths, state: Option<&UpdateState>) {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).ok();
    let newer_cli = state
        .and_then(|state| state.latest_cli.as_deref())
        .and_then(|latest| Version::parse(latest).ok())
        .filter(|latest| current.as_ref().is_some_and(|current| latest > current));
    if let Some(version) = newer_cli {
        eprintln!("agents: CLI {version} is available. Run `agents update`.");
    }
    if oldest_unpulled_commit(&paths.agents_home).is_some() {
        eprintln!("agents: agents-home has remote changes. Run `agents sync`.");
    }
    let archive_stale = archive_path(paths)
        .and_then(|archive| oldest_unpulled_commit(&archive))
        .is_some_and(|time| now().saturating_sub(time) > ARCHIVE_NOTICE_AGE.as_secs());
    if archive_stale {
        eprintln!("agents: the agents archive is over 30 days behind. Run `agents archive sync`.");
    }
}

fn archive_path(paths: &Paths) -> Option<std::path::PathBuf> {
    let contents = fs::read_to_string(paths.config_dir.join("archive.toml")).ok()?;
    let value = toml::from_str::<toml::Value>(&contents).ok()?;
    value
        .get("repo_path")?
        .as_str()
        .map(std::path::PathBuf::from)
}

fn fetch(repo: &std::path::Path) {
    if repo.join(".git").is_dir() {
        let _ = Command::new("git")
            .args(["fetch", "--quiet", "--prune", "origin"])
            .current_dir(repo)
            .status();
    }
}

/// Returns the commit time of the oldest upstream commit missing from HEAD.
/// Reads only local refs, so it is cheap enough for every shell startup.
fn oldest_unpulled_commit(repo: &std::path::Path) -> Option<u64> {
    if !repo.join(".git").is_dir() {
        return None;
    }
    git_text(repo, &["log", "--format=%ct", "HEAD..@{upstream}"])?
        .lines()
        .last()?
        .parse()
        .ok()
}

fn git_text(repo: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
