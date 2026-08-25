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

#[derive(Default, Serialize, Deserialize)]
struct UpdateState {
    checked_at: u64,
    latest_cli: Option<String>,
    agents_home_behind: Option<usize>,
    archive_behind: Option<usize>,
}

pub fn shell_check(paths: &Paths) -> Result<()> {
    let state_path = paths.state_dir.join("update-check.json");
    let state = fs::read(&state_path)
        .ok()
        .and_then(|contents| serde_json::from_slice::<UpdateState>(&contents).ok());
    if let Some(state) = &state {
        print_notices(state);
    }

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

    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let archive = archive_path(paths);
    let (latest_cli, agents_home_behind, archive_behind) = thread::scope(|scope| {
        let cli = scope.spawn(|| {
            updater::latest_version()
                .ok()
                .filter(|latest| latest > &current)
                .map(|latest| latest.to_string())
        });
        let home = scope.spawn(|| repository_behind(&paths.agents_home));
        let archive = scope.spawn(|| archive.and_then(|path| repository_behind(&path)));
        (
            cli.join().unwrap_or(None),
            home.join().unwrap_or(None),
            archive.join().unwrap_or(None),
        )
    });
    let state = UpdateState {
        checked_at: now(),
        latest_cli,
        agents_home_behind,
        archive_behind,
    };
    util::atomic_write(
        &paths.state_dir.join("update-check.json"),
        &serde_json::to_vec_pretty(&state)?,
    )?;
    Ok(())
}

fn print_notices(state: &UpdateState) {
    if let Some(version) = &state.latest_cli {
        eprintln!("agents: CLI {version} is available. Run `agents update`.");
    }
    if state.agents_home_behind.is_some_and(|count| count > 0) {
        eprintln!("agents: agents-home has remote changes. Run `agents sync`.");
    }
    if state.archive_behind.is_some_and(|count| count > 0) {
        eprintln!("agents: the agents archive has remote changes. Run `agents archive sync`.");
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

fn repository_behind(repo: &std::path::Path) -> Option<usize> {
    if !repo.join(".git").is_dir() {
        return None;
    }
    let fetch = Command::new("git")
        .args(["fetch", "--quiet", "--prune", "origin"])
        .current_dir(repo)
        .status()
        .ok()?;
    if !fetch.success() {
        return None;
    }
    let upstream = git_text(repo, &["rev-parse", "--abbrev-ref", "@{upstream}"])?;
    git_text(repo, &["rev-list", "--count", &format!("HEAD..{upstream}")])?
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
