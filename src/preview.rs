use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

const DEFAULT_TTL: &str = "12h";

#[derive(Debug, Subcommand)]
pub enum PreviewCommand {
    /// Start or replace a named preview.
    Start {
        /// Stable preview name. Use lowercase letters, numbers, and hyphens.
        name: String,
        /// Local HTTP port used by the development server.
        #[arg(long)]
        port: u16,
        /// Preview lifetime understood by systemd, such as 12h or 2d.
        #[arg(long, default_value = DEFAULT_TTL)]
        ttl: String,
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
    /// Extend an active preview lease.
    Extend { name: String, ttl: String },
    /// Stop a preview and remove its Tailscale route.
    Stop { name: String },
    /// Remove routes and metadata for previews that are no longer running.
    Prune,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreviewState {
    name: String,
    unit: String,
    port: u16,
    url: String,
    cwd: PathBuf,
    command: Vec<String>,
    ttl: String,
}

pub fn run(command: PreviewCommand) -> Result<()> {
    require_linux()?;
    match command {
        PreviewCommand::Start {
            name,
            port,
            ttl,
            cwd,
            command,
        } => start(name, port, ttl, cwd, command),
        PreviewCommand::Status { name } => status(name.as_deref()),
        PreviewCommand::List => status(None),
        PreviewCommand::Extend { name, ttl } => extend(&name, &ttl),
        PreviewCommand::Stop { name } => stop(&name),
        PreviewCommand::Prune => prune(),
    }
}

fn start(
    name: String,
    port: u16,
    ttl: String,
    cwd: Option<PathBuf>,
    command: Vec<String>,
) -> Result<()> {
    validate_name(&name)?;
    validate_ttl(&ttl)?;
    if command.is_empty() {
        bail!("a development server command is required");
    }
    let cwd = cwd
        .unwrap_or(env::current_dir()?)
        .canonicalize()
        .context("resolve preview working directory")?;
    let unit = unit_name(&name);

    if let Some(previous) = load_state(&name)? {
        stop_state(&previous, false)?;
    } else {
        let _ = systemctl(&["stop", &unit]);
        tailscale_off(port)?;
    }

    let tailscale = command_path("tailscale")?;
    let path = env::var("PATH").unwrap_or_default();
    let stop_post = format!("{} serve --https={} off", tailscale.display(), port);
    let mut args = vec![
        "--user".to_owned(),
        format!("--unit={unit}"),
        "--collect".to_owned(),
        format!("--property=RuntimeMaxSec={ttl}"),
        format!("--property=WorkingDirectory={}", cwd.display()),
        format!("--property=ExecStopPost={stop_post}"),
        format!("--setenv=PATH={path}"),
        "--".to_owned(),
    ];
    args.extend(command.iter().cloned());
    checked(
        Command::new("systemd-run").args(&args),
        "start preview service",
    )?;
    wait_for_http(&format!("http://127.0.0.1:{port}/"), 30)?;

    checked(
        Command::new(&tailscale).args([
            "serve",
            "--bg",
            &format!("--https={port}"),
            "--yes",
            &format!("http://127.0.0.1:{port}"),
        ]),
        "create Tailscale Serve route",
    )?;
    let hostname = tailscale_hostname(&tailscale)?;
    let url = format!("https://{hostname}:{port}/");
    wait_for_http(&url, 30)?;

    let state = PreviewState {
        name,
        unit,
        port,
        url,
        cwd,
        command,
        ttl,
    };
    save_state(&state)?;
    println!("{}", state.url);
    println!("status: agents preview status {}", state.name);
    println!("logs:   journalctl --user -fu {}", state.unit);
    Ok(())
}

fn status(name: Option<&str>) -> Result<()> {
    let states = states()?;
    if let Some(name) = name {
        let state = states
            .into_iter()
            .find(|state| state.name == name)
            .ok_or_else(|| anyhow!("preview '{name}' is not registered"))?;
        print_state(&state)?;
        return Ok(());
    }
    if states.is_empty() {
        println!("No previews.");
        return Ok(());
    }
    for state in states {
        print_state(&state)?;
    }
    Ok(())
}

fn print_state(state: &PreviewState) -> Result<()> {
    let active = is_active(&state.unit);
    let remaining = if active {
        systemctl_value(&state.unit, "RuntimeMaxUSec")?
    } else {
        "expired".to_owned()
    };
    println!(
        "{}\t{}\t{}\t{}",
        state.name,
        if active { "active" } else { "stopped" },
        remaining,
        state.url
    );
    Ok(())
}

fn extend(name: &str, ttl: &str) -> Result<()> {
    validate_ttl(ttl)?;
    let state = load_state(name)?.ok_or_else(|| anyhow!("preview '{name}' is not registered"))?;
    if !is_active(&state.unit) {
        bail!("preview '{name}' is not running");
    }
    println!("Restarting {name} with a {ttl} lease.");
    start(
        state.name,
        state.port,
        ttl.to_owned(),
        Some(state.cwd),
        state.command,
    )
}

fn stop(name: &str) -> Result<()> {
    let state = load_state(name)?.ok_or_else(|| anyhow!("preview '{name}' is not registered"))?;
    stop_state(&state, true)
}

fn stop_state(state: &PreviewState, announce: bool) -> Result<()> {
    let _ = systemctl(&["stop", &state.unit]);
    tailscale_off(state.port)?;
    let path = state_path(&state.name)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    if announce {
        println!("Stopped {} and removed its Tailscale route.", state.name);
    }
    Ok(())
}

fn prune() -> Result<()> {
    let mut removed = 0;
    for state in states()? {
        if !is_active(&state.unit) {
            tailscale_off(state.port)?;
            fs::remove_file(state_path(&state.name)?)?;
            removed += 1;
        }
    }
    println!("Pruned {removed} preview(s).");
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("preview names use lowercase letters, numbers, and hyphens only");
    }
    Ok(())
}

fn validate_ttl(ttl: &str) -> Result<()> {
    let split = ttl
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(ttl.len());
    let (amount, unit) = ttl.split_at(split);
    if amount.is_empty() || amount == "0" || !matches!(unit, "s" | "m" | "h" | "d") {
        bail!("TTL must be a positive duration such as 30m, 12h, or 2d");
    }
    Ok(())
}

fn require_linux() -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("app previews currently require Linux and systemd");
    }
    Ok(())
}

fn unit_name(name: &str) -> String {
    format!("agents-preview-{name}.service")
}

fn state_dir() -> Result<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| anyhow!("HOME or XDG_STATE_HOME is required"))?;
    Ok(base.join("agents/previews"))
}

fn state_path(name: &str) -> Result<PathBuf> {
    Ok(state_dir()?.join(format!("{name}.json")))
}

fn save_state(state: &PreviewState) -> Result<()> {
    let dir = state_dir()?;
    fs::create_dir_all(&dir)?;
    fs::write(state_path(&state.name)?, serde_json::to_vec_pretty(state)?)?;
    Ok(())
}

fn load_state(name: &str) -> Result<Option<PreviewState>> {
    let path = state_path(name)?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

fn states() -> Result<Vec<PreviewState>> {
    let dir = state_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut states: Vec<PreviewState> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            states.push(serde_json::from_slice(&fs::read(path)?)?);
        }
    }
    states.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(states)
}

fn is_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .status()
        .is_ok_and(|status| status.success())
}

fn systemctl(args: &[&str]) -> Result<Output> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("run systemctl")
}

fn systemctl_value(unit: &str, property: &str) -> Result<String> {
    let output = checked(
        Command::new("systemctl").args([
            "--user",
            "show",
            unit,
            &format!("--property={property}"),
            "--value",
        ]),
        "inspect preview service",
    )?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn tailscale_off(port: u16) -> Result<()> {
    let tailscale = command_path("tailscale")?;
    let _ = Command::new(tailscale)
        .args(["serve", &format!("--https={port}"), "off"])
        .output();
    Ok(())
}

fn tailscale_hostname(tailscale: &Path) -> Result<String> {
    let output = checked(
        Command::new(tailscale).args(["status", "--self", "--json"]),
        "read Tailscale hostname",
    )?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    value
        .pointer("/Self/DNSName")
        .and_then(|value| value.as_str())
        .map(|value| value.trim_end_matches('.').to_owned())
        .ok_or_else(|| anyhow!("Tailscale did not report this machine's DNS name"))
}

fn command_path(name: &str) -> Result<PathBuf> {
    let output = checked(
        Command::new("sh").args(["-c", &format!("command -v {name}")]),
        &format!("find {name}"),
    )?;
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

fn wait_for_http(url: &str, seconds: u64) -> Result<()> {
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

fn checked(command: &mut Command, action: &str) -> Result<Output> {
    let output = command.output().with_context(|| action.to_owned())?;
    if !output.status.success() {
        bail!(
            "{action}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_safe_preview_names() {
        assert!(validate_name("worldforge-crm").is_ok());
        assert!(validate_name("World Forge").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn accepts_simple_expiring_leases() {
        for ttl in ["30m", "12h", "2d"] {
            assert!(validate_ttl(ttl).is_ok());
        }
        for ttl in ["forever", "0h", "12 hours"] {
            assert!(validate_ttl(ttl).is_err());
        }
    }
}
