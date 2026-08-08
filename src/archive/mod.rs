mod adapters;
mod model;
mod store;

use std::{fs, path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{config::Paths, util};

const README: &str = r#"# Unified chat archive

This private repository stores normalized chat history from supported AI harnesses.

- `objects/sha256/` contains immutable, content-addressed session objects.
- `refs/<machine>/<source>/` points to the newest object seen by each machine.
- `machines/` describes archive contributors.
- `schema/` defines the portable record format.

Use `agents archive update` to ingest local changes.
Use `agents archive sync` to ingest, commit, and push changes.

Do not add credentials, raw tool output, reasoning, or generated binary artifacts.
"#;

const SESSION_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://github.com/tomagranate/agents/chat-archive/session-v1.json",
  "title": "Normalized chat session",
  "type": "object",
  "required": ["type", "schema_version", "logical_id", "source", "native_id", "title"],
  "properties": {
    "type": {"const": "session"},
    "schema_version": {"const": 1},
    "logical_id": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
    "source": {"enum": ["codex", "claude", "opencode", "grok"]},
    "native_id": {"type": "string"},
    "title": {"type": "string"},
    "models": {"type": "array", "items": {"type": "string"}, "uniqueItems": true}
  }
}
"#;

const EVENT_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://github.com/tomagranate/agents/chat-archive/event-v1.json",
  "title": "Normalized chat event",
  "type": "object",
  "required": ["type", "sequence", "kind"],
  "properties": {
    "type": {"const": "event"},
    "sequence": {"type": "integer", "minimum": 0},
    "kind": {"enum": ["message", "summary", "memory", "tool"]},
    "role": {"enum": ["user", "assistant", "system"]},
    "text": {"type": "string"},
    "tool_name": {"type": "string"},
    "model": {"type": "string"},
    "provider": {"type": "string"}
  }
}
"#;

#[derive(Debug, Subcommand)]
pub enum ArchiveCommand {
    /// Configure a local archive repository.
    Init {
        /// Local repository path.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Existing Git remote URL.
        #[arg(long)]
        remote: Option<String>,
        /// Friendly machine name.
        #[arg(long)]
        machine: Option<String>,
    },
    /// Show archive and pending-source counts.
    Status,
    /// Ingest new and changed local history.
    Update {
        /// Report pending sources without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Pull, ingest, commit, rebase, and push the archive.
    Sync {
        #[arg(short = 'm', long, default_value = "Update chat archive")]
        message: String,
    },
    /// Search all retained message text.
    Search {
        query: String,
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Print one normalized session.
    Show { id: String },
    /// Rebuild the local full-text index.
    Reindex,
    /// Verify object hashes and references.
    Verify,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveConfig {
    pub schema_version: u32,
    pub repo_path: PathBuf,
    pub machine_id: String,
    pub machine_name: String,
}

impl ArchiveConfig {
    fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config_dir.join("archive.toml");
        let contents = fs::read_to_string(&path).with_context(|| {
            format!(
                "archive is not configured; run agents archive init ({})",
                path.display()
            )
        })?;
        toml::from_str(&contents).context("archive configuration is invalid")
    }

    fn save(&self, paths: &Paths) -> Result<()> {
        fs::create_dir_all(&paths.config_dir)?;
        util::atomic_write(
            &paths.config_dir.join("archive.toml"),
            toml::to_string_pretty(self)?.as_bytes(),
        )
    }
}

pub fn run(paths: &Paths, command: ArchiveCommand) -> Result<()> {
    match command {
        ArchiveCommand::Init {
            path,
            remote,
            machine,
        } => init(paths, path, remote.as_deref(), machine),
        ArchiveCommand::Status => status(paths),
        ArchiveCommand::Update { dry_run } => {
            let config = ArchiveConfig::load(paths)?;
            let stats = store::update(paths, &config, dry_run)?;
            print_update(&stats, dry_run);
            Ok(())
        }
        ArchiveCommand::Sync { message } => sync_archive(paths, &message),
        ArchiveCommand::Search { query, limit } => store::search(paths, &query, limit),
        ArchiveCommand::Show { id } => {
            let config = ArchiveConfig::load(paths)?;
            store::show(paths, &config, &id)
        }
        ArchiveCommand::Reindex => {
            let config = ArchiveConfig::load(paths)?;
            store::rebuild_index(paths, &config)?;
            println!("Rebuilt the unified search index.");
            Ok(())
        }
        ArchiveCommand::Verify => {
            let config = ArchiveConfig::load(paths)?;
            let (objects, references) = store::verify(&config)?;
            println!("Verified {objects} objects and {references} references.");
            Ok(())
        }
    }
}

fn init(
    paths: &Paths,
    requested_path: Option<PathBuf>,
    remote: Option<&str>,
    machine: Option<String>,
) -> Result<()> {
    let repo_path =
        requested_path.unwrap_or_else(|| paths.home.join(".local/share/agents/chat-archive"));
    if let Some(remote) = remote
        && !repo_path.exists()
    {
        let status = Command::new("git")
            .args(["clone", remote])
            .arg(&repo_path)
            .status()?;
        if !status.success() {
            bail!("could not clone {remote}");
        }
    }
    if repo_path.exists() && !repo_path.is_dir() {
        bail!("archive path is not a directory: {}", repo_path.display());
    }
    fs::create_dir_all(&repo_path)?;
    if !repo_path.join(".git").is_dir() {
        if fs::read_dir(&repo_path)?.next().is_some() {
            bail!(
                "archive path is not empty and has no Git repository: {}",
                repo_path.display()
            );
        }
        util::command_status("git", ["init", "-b", "main"], Some(&repo_path))?;
    }
    if let Some(remote) = remote
        && !has_origin(&repo_path)
    {
        util::command_status("git", ["remote", "add", "origin", remote], Some(&repo_path))?;
    }
    for directory in ["objects/sha256", "refs", "machines", "schema/v1"] {
        fs::create_dir_all(repo_path.join(directory))?;
    }
    util::atomic_write(&repo_path.join("README.md"), README.as_bytes())?;
    util::atomic_write(
        &repo_path.join("schema/v1/session.schema.json"),
        SESSION_SCHEMA.as_bytes(),
    )?;
    util::atomic_write(
        &repo_path.join("schema/v1/event.schema.json"),
        EVENT_SCHEMA.as_bytes(),
    )?;
    util::atomic_write(&repo_path.join(".gitignore"), b".DS_Store\n")?;

    let canonical_repo = repo_path.canonicalize()?;
    let existing = ArchiveConfig::load(paths)
        .ok()
        .filter(|config| config.repo_path == canonical_repo);
    let machine_name = machine
        .or_else(|| existing.as_ref().map(|config| config.machine_name.clone()))
        .unwrap_or_else(|| {
            hostname::get()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    let config = ArchiveConfig {
        schema_version: 1,
        repo_path: canonical_repo,
        machine_id: existing
            .map(|config| config.machine_id)
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        machine_name,
    };
    let machine_record = serde_json::json!({
        "schema_version": 1,
        "machine_id": config.machine_id,
        "machine_name": config.machine_name,
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    let machine_path = config
        .repo_path
        .join("machines")
        .join(format!("{}.json", config.machine_id));
    if !machine_path.is_file() {
        util::atomic_write(&machine_path, &serde_json::to_vec_pretty(&machine_record)?)?;
    }
    config.save(paths)?;
    println!("Archive repository: {}", config.repo_path.display());
    println!("Machine: {} ({})", config.machine_name, config.machine_id);
    println!("Run: agents archive update");
    Ok(())
}

fn status(paths: &Paths) -> Result<()> {
    let config = ArchiveConfig::load(paths)?;
    store::ensure_repository(&config.repo_path)?;
    let pending = store::update(paths, &config, true)?;
    let (objects, references, sessions, messages) = store::counts(paths, &config)?;
    println!("Archive: {}", config.repo_path.display());
    println!("Machine: {} ({})", config.machine_name, config.machine_id);
    println!("Objects: {objects}");
    println!("References: {references}");
    println!("Indexed sessions: {sessions}");
    println!("Indexed text events: {messages}");
    println!(
        "Changed sources: {} of {}",
        pending.changed, pending.discovered
    );
    Ok(())
}

fn print_update(stats: &store::UpdateStats, dry_run: bool) {
    if dry_run {
        println!("Changed sources: {} of {}", stats.changed, stats.discovered);
        return;
    }
    println!(
        "Scanned {} sources; {} changed.",
        stats.discovered, stats.changed
    );
    println!(
        "Normalized {} sessions and {} events.",
        stats.parsed_sessions, stats.events
    );
    println!(
        "Wrote {} new objects and {} references.",
        stats.objects_written, stats.refs_written
    );
    if stats.objects_pruned > 0 {
        println!(
            "Pruned {} unreferenced objects. Git retains committed revisions.",
            stats.objects_pruned
        );
    }
}

fn sync_archive(paths: &Paths, message: &str) -> Result<()> {
    let config = ArchiveConfig::load(paths)?;
    store::ensure_repository(&config.repo_path)?;
    if has_origin(&config.repo_path) && remote_has_head(&config.repo_path) {
        pull_remote(&config.repo_path)?;
    }
    let stats = store::update(paths, &config, false)?;
    print_update(&stats, false);
    if stats.changed == 0 {
        store::rebuild_index(paths, &config)?;
    }
    util::command_status("git", ["add", "-A"], Some(&config.repo_path))?;
    if git_dirty(&config.repo_path)? {
        util::command_status("git", ["commit", "-m", message], Some(&config.repo_path))?;
    } else {
        println!("Nothing to commit.");
    }
    if has_origin(&config.repo_path) {
        push_with_retry(&config.repo_path)?;
        println!("Pushed the unified archive.");
    } else {
        println!("No origin remote is configured. Changes remain local.");
    }
    Ok(())
}

fn has_origin(repo: &std::path::Path) -> bool {
    Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn remote_has_head(repo: &std::path::Path) -> bool {
    Command::new("git")
        .args(["ls-remote", "--exit-code", "origin", "HEAD"])
        .current_dir(repo)
        .output()
        .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
}

fn pull_remote(repo: &std::path::Path) -> Result<()> {
    util::command_status("git", ["pull", "--rebase", "origin", "HEAD"], Some(repo))
}

fn push_with_retry(repo: &std::path::Path) -> Result<()> {
    for attempt in 1..=3 {
        let status = Command::new("git")
            .args(["push", "-u", "origin", "HEAD"])
            .current_dir(repo)
            .status()?;
        if status.success() {
            return Ok(());
        }
        if attempt == 3 {
            bail!("archive push failed after three attempts");
        }
        pull_remote(repo)?;
    }
    unreachable!()
}

fn git_dirty(repo: &std::path::Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        bail!("could not inspect archive Git status");
    }
    Ok(!output.stdout.is_empty())
}
