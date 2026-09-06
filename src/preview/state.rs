//! Preview records. One JSON file per preview under `<state>/previews/`.
//! The CLI and the daemon both read and write these files. The file is the
//! truth for configuration; systemd is the truth for "is it running".

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Parent domain. A preview named `crm` lives at `crm.preview.tomagranate.com`.
pub const DOMAIN: &str = "preview.tomagranate.com";
pub const DEFAULT_IDLE_TTL: &str = "12h";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub name: String,
    pub port: u16,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    /// The caller's PATH at start. Revive reuses it.
    #[serde(default)]
    pub path: String,
    /// Idle limit such as `12h`. The daemon stops the preview after this long
    /// without a request.
    #[serde(default = "default_idle_ttl")]
    pub idle_ttl: String,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    pub started_at: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    pub last_used_at: DateTime<Utc>,
    /// Present when the preview is not running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<Stopped>,
    /// Message from the last failed start. Cleared on a good start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stopped {
    pub at: DateTime<Utc>,
    pub reason: StopReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StopReason {
    /// No requests for `idle_ttl`.
    Idle,
    /// Tom asked for it.
    Stopped,
    /// The command did not become healthy.
    Failed,
    /// The unit went away on its own.
    Exited,
}

fn default_idle_ttl() -> String {
    DEFAULT_IDLE_TTL.to_owned()
}

impl Record {
    pub fn url(&self) -> String {
        format!("https://{}.{DOMAIN}/", self.name)
    }

    pub fn unit(&self) -> String {
        format!("agents-preview-{}.service", self.name)
    }

    pub fn is_running(&self) -> bool {
        self.stopped.is_none()
    }

    pub fn idle_limit(&self) -> Duration {
        parse_duration(&self.idle_ttl).unwrap_or(Duration::from_secs(12 * 3600))
    }

    pub fn mark_stopped(&mut self, reason: StopReason) {
        self.stopped = Some(Stopped {
            at: Utc::now(),
            reason,
        });
    }
}

/// The directory of preview records.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("previews"),
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    pub fn load(&self, name: &str) -> Result<Option<Record>> {
        let path = self.path(name);
        if !path.exists() {
            return Ok(None);
        }
        let record = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("read preview record {}", path.display()))?;
        Ok(Some(record))
    }

    pub fn require(&self, name: &str) -> Result<Record> {
        self.load(name)?
            .ok_or_else(|| anyhow!("preview '{name}' is not registered"))
    }

    /// Writes through a temporary file so readers never see a partial record.
    pub fn save(&self, record: &Record) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path(&record.name);
        let temp = self.dir.join(format!(".{}.json.tmp", record.name));
        fs::write(&temp, serde_json::to_vec_pretty(record)?)?;
        fs::rename(&temp, &path)?;
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let path = self.path(name);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Every record, sorted by name. Unreadable files are skipped with a
    /// warning so one bad file cannot take the index down.
    pub fn all(&self) -> Result<Vec<Record>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json")
                || path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.starts_with('.'))
            {
                continue;
            }
            match serde_json::from_slice::<Record>(&fs::read(&path)?) {
                Ok(record) => records.push(record),
                Err(error) => eprintln!("skip {}: {error}", path.display()),
            }
        }
        records.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(records)
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name.ends_with('-')
        || name.len() > 63
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("preview names use lowercase letters, numbers, and hyphens only");
    }
    Ok(())
}

/// Parses a systemd-style duration with one unit: `30m`, `12h`, `2d`.
pub fn parse_duration(value: &str) -> Result<Duration> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (amount, unit) = value.split_at(split);
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => bail!("duration must be a positive value such as 30m, 12h, or 2d"),
    };
    let amount: u64 = amount
        .parse()
        .map_err(|_| anyhow!("duration must be a positive value such as 30m, 12h, or 2d"))?;
    if amount == 0 {
        bail!("duration must be a positive value such as 30m, 12h, or 2d");
    }
    Ok(Duration::from_secs(amount * multiplier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_safe_preview_names() {
        assert!(validate_name("worldforge-crm").is_ok());
        assert!(validate_name("World Forge").is_err());
        assert!(validate_name("-lead").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn parses_simple_durations() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172_800));
        for bad in ["forever", "0h", "12 hours", ""] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn reads_legacy_records() {
        let legacy = r#"{
          "name": "old", "unit": "agents-preview-old.service", "port": 8791,
          "url": "https://host.ts.net:8791/", "cwd": "/tmp", "command": ["true"], "ttl": "12h"
        }"#;
        let record: Record = serde_json::from_str(legacy).unwrap();
        assert_eq!(record.url(), "https://old.preview.tomagranate.com/");
        assert!(record.is_running());
        assert_eq!(record.idle_ttl, "12h");
    }
}
