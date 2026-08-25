mod adapters;
mod model;
mod store;

use std::{fs, path::PathBuf, process::Command, time::Duration};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{config::Paths, progress::Activity, util};

const README: &str = r#"# Unified agents archive

This private repository stores normalized chat history from supported AI harnesses.

- `objects/sha256/` contains immutable, content-addressed session objects.
- `refs/<machine>/<source>/` contains the shared metadata index.
- `machines/` describes archive contributors.
- `schema/` defines the portable record format.

Archive clones keep metadata locally and fetch session objects when requested.

Use `agents archive advanced ingest` to ingest local changes.
Use `agents archive sync` to ingest, commit, and push changes.
Use `agents archive show` to fetch one session object.
Use `agents archive cache clear` to remove downloaded session objects.

Do not add credentials, raw tool output, reasoning, or generated binary artifacts.
"#;

const SESSION_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://github.com/tomagranate/agents/agents-archive/session-v1.json",
  "title": "Normalized chat session",
  "type": "object",
  "required": ["type", "schema_version", "logical_id", "source", "native_id", "title"],
  "properties": {
    "type": {"const": "session"},
    "schema_version": {"const": 1},
    "logical_id": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
    "source": {"enum": ["codex", "claude", "opencode", "grok", "chatgpt", "claude-web", "t3chat"]},
    "native_id": {"type": "string"},
    "title": {"type": "string"},
    "parent_session_id": {"type": "string"},
    "parent_event_id": {"type": "string"},
    "models": {"type": "array", "items": {"type": "string"}, "uniqueItems": true}
  }
}
"#;

const EVENT_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://github.com/tomagranate/agents/agents-archive/event-v1.json",
  "title": "Normalized chat event",
  "type": "object",
  "required": ["type", "sequence", "kind"],
  "properties": {
    "type": {"const": "event"},
    "sequence": {"type": "integer", "minimum": 0},
    "kind": {"enum": ["message", "summary", "memory", "tool"]},
    "native_id": {"type": "string"},
    "parent_native_id": {"type": "string"},
    "active_branch": {"type": "boolean"},
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
        /// Download every session object during clone.
        #[arg(long)]
        full: bool,
    },
    /// Import a ChatGPT, Claude, or T3 Chat account export.
    Import {
        /// Export ZIP or JSON file.
        path: PathBuf,
    },
    /// Show local and remote archive status.
    Status {
        /// Do not fetch remote state.
        #[arg(long)]
        offline: bool,
        /// Show filesystem paths and more detail.
        #[arg(long)]
        verbose: bool,
    },
    /// Fetch, preserve local work, rebase, ingest, and push the archive.
    Sync {
        #[arg(short = 'm', long, default_value = "Update agents archive")]
        message: String,
    },
    /// Search metadata and locally available message text.
    Search {
        query: String,
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Print one session and fetch its object when needed.
    Show { id: String },
    /// Manage locally cached session data.
    Cache {
        #[command(subcommand)]
        command: ArchiveCacheCommand,
    },
    /// Run individual operations normally handled by sync.
    Advanced {
        #[command(subcommand)]
        command: ArchiveAdvancedCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ArchiveCacheCommand {
    /// Fetch and cache one session object.
    Fetch { id: String },
    /// Fetch and cache every session object.
    Hydrate,
    /// Return the local archive to a thin-clone state.
    Clear,
}

#[derive(Debug, Subcommand)]
pub enum ArchiveAdvancedCommand {
    /// Fetch and rebase archive metadata without ingesting or pushing.
    Pull,
    /// Ingest new and changed local history.
    Ingest {
        /// Report pending sources without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Rebuild the local search index.
    Reindex,
    /// Verify references and available objects.
    Verify {
        /// Fetch and verify every referenced object.
        #[arg(long)]
        full: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveConfig {
    pub schema_version: u32,
    pub repo_path: PathBuf,
    pub machine_id: String,
    pub machine_name: String,
    #[serde(default)]
    pub thin: bool,
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
            full,
        } => init(paths, path, remote.as_deref(), machine, full),
        ArchiveCommand::Import { path } => {
            let config = ArchiveConfig::load(paths)?;
            store::ensure_repository(&config.repo_path)?;
            let stats = store::import_export(paths, &config, &path)?;
            print_update(&stats, false);
            println!("Run `agents archive sync` to commit and push this import.");
            Ok(())
        }
        ArchiveCommand::Status { offline, verbose } => status(paths, offline, verbose),
        ArchiveCommand::Sync { message } => sync_archive(paths, &message),
        ArchiveCommand::Search { query, limit } => {
            let config = ArchiveConfig::load(paths)?;
            store::ensure_repository(&config.repo_path)?;
            store::search(paths, &query, limit)
        }
        ArchiveCommand::Show { id } => {
            let config = ArchiveConfig::load(paths)?;
            store::show(paths, &config, &id)
        }
        ArchiveCommand::Cache {
            command: ArchiveCacheCommand::Fetch { id },
        } => {
            let config = ArchiveConfig::load(paths)?;
            store::fetch(paths, &config, &id)
        }
        ArchiveCommand::Cache {
            command: ArchiveCacheCommand::Hydrate,
        } => {
            let config = ArchiveConfig::load(paths)?;
            let fetched = store::hydrate(paths, &config)?;
            println!("Fetched session objects: {fetched}.");
            Ok(())
        }
        ArchiveCommand::Cache {
            command: ArchiveCacheCommand::Clear,
        } => clear_cache(paths),
        ArchiveCommand::Advanced { command } => run_advanced(paths, command),
    }
}

fn run_advanced(paths: &Paths, command: ArchiveAdvancedCommand) -> Result<()> {
    let config = ArchiveConfig::load(paths)?;
    store::ensure_repository(&config.repo_path)?;
    match command {
        ArchiveAdvancedCommand::Pull => {
            if git_dirty(&config.repo_path)? {
                bail!("archive has local changes; run agents archive sync to preserve them");
            }
            if !has_origin(&config.repo_path) {
                bail!("archive has no origin remote");
            }
            fetch_remote(&config.repo_path, "Fetching archive metadata")?;
            rebase_remote(&config.repo_path)?;
            store::rebuild_index(paths, &config)?;
            println!("Archive metadata pulled and indexed.");
            Ok(())
        }
        ArchiveAdvancedCommand::Ingest { dry_run } => {
            let stats = store::update(paths, &config, dry_run)?;
            print_update(&stats, dry_run);
            Ok(())
        }
        ArchiveAdvancedCommand::Reindex => {
            store::rebuild_index(paths, &config)?;
            println!("Rebuilt the unified search index.");
            Ok(())
        }
        ArchiveAdvancedCommand::Verify { full } => {
            let result = store::verify(paths, &config, full)?;
            println!("Available objects verified: {}.", result.objects);
            println!("References verified: {}.", result.references);
            println!("Remote objects: {}.", result.remote);
            Ok(())
        }
    }
}

fn clear_cache(paths: &Paths) -> Result<()> {
    let _lock = store::ArchiveLock::acquire(paths)?;
    let mut config = ArchiveConfig::load(paths)?;
    store::ensure_repository(&config.repo_path)?;
    if git_dirty(&config.repo_path)? {
        bail!("archive has uncommitted changes; run agents archive sync before clearing cache");
    }
    if !has_origin(&config.repo_path) {
        bail!("archive has no origin remote; a thin clone cannot be restored safely");
    }

    let activity = Activity::new("Checking the archive remote");
    let fetch = Command::new("git")
        .args(["fetch", "--prune", "origin"])
        .current_dir(&config.repo_path)
        .output()?;
    if !fetch.status.success() {
        bail!("could not fetch the archive remote");
    }
    let upstream = git_text(
        &config.repo_path,
        [
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .context("archive branch has no upstream; run agents archive sync first")?;
    let branch = upstream
        .strip_prefix("origin/")
        .context("archive upstream is not on the origin remote")?;
    let ahead = git_text(
        &config.repo_path,
        ["rev-list", "--count", "@{upstream}..HEAD"],
    )?
    .parse::<usize>()
    .context("could not inspect unpushed archive commits")?;
    if ahead > 0 {
        bail!("archive has unpushed commits; run agents archive sync before clearing cache");
    }
    let remote = git_text(&config.repo_path, ["remote", "get-url", "origin"])?;
    let parent = config
        .repo_path
        .parent()
        .context("archive repository has no parent directory")?;
    let staging = tempfile::Builder::new()
        .prefix(".agents-archive-thin-")
        .tempdir_in(parent)?;
    let replacement = staging.path().join("replacement");
    activity.set_message("Creating a fresh thin clone");
    let clone = Command::new("git")
        .args([
            "clone",
            "--filter=blob:none",
            "--sparse",
            "--single-branch",
            "--branch",
            branch,
            &remote,
        ])
        .arg(&replacement)
        .output()?;
    if !clone.status.success() {
        bail!(
            "could not create the thin clone: {}",
            String::from_utf8_lossy(&clone.stderr).trim()
        );
    }
    activity.finish("Thin archive clone downloaded");
    configure_thin_checkout(&replacement)?;

    let activity = Activity::new("Replacing the hydrated checkout");
    let previous = staging.path().join("previous");
    fs::rename(&config.repo_path, &previous)?;
    if let Err(error) = fs::rename(&replacement, &config.repo_path) {
        let _ = fs::rename(&previous, &config.repo_path);
        return Err(error).context("could not install the thin archive checkout");
    }
    config.thin = true;
    config.save(paths)?;
    store::remove_cached_objects(paths)?;
    activity.set_message("Removing hydrated Git objects");
    staging.close()?;
    activity.finish("Thin archive checkout restored");

    store::rebuild_index(paths, &config)?;
    let activity = Activity::new("Compacting the local archive index");
    store::compact_index(paths)?;
    activity.finish("Local archive index compacted");
    println!("Local session cache cleared. The archive remains configured as a thin clone.");
    Ok(())
}

fn git_text<const N: usize>(repo: &std::path::Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(repo).output()?;
    if !output.status.success() {
        bail!("git command failed");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn init(
    paths: &Paths,
    requested_path: Option<PathBuf>,
    remote: Option<&str>,
    machine: Option<String>,
    full: bool,
) -> Result<()> {
    let repo_path =
        requested_path.unwrap_or_else(|| paths.home.join(".local/share/agents/agents-archive"));
    if let Some(remote) = remote
        && !repo_path.exists()
    {
        let mut command = Command::new("git");
        command.arg("clone");
        if !full {
            command.args(["--filter=blob:none", "--sparse"]);
        }
        let status = command.arg(remote).arg(&repo_path).status()?;
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
        util::command_status("git", ["init", "-b", "master"], Some(&repo_path))?;
    }
    if let Some(remote) = remote
        && !has_origin(&repo_path)
    {
        util::command_status("git", ["remote", "add", "origin", remote], Some(&repo_path))?;
    }
    let thin = remote.is_some() && !full;
    if thin {
        configure_thin_checkout(&repo_path)?;
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
        thin,
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
    store::rebuild_index(paths, &config)?;
    println!("Archive repository: {}", config.repo_path.display());
    println!("Machine: {} ({})", config.machine_name, config.machine_id);
    println!("Storage: {}", if config.thin { "thin" } else { "full" });
    println!("Run: agents archive sync");
    Ok(())
}

fn status(paths: &Paths, offline: bool, verbose: bool) -> Result<()> {
    let config_path = paths.config_dir.join("archive.toml");
    if !config_path.is_file() {
        println!("Archive: not configured");
        println!("Run: agents archive init");
        if verbose {
            println!("Expected config: {}", config_path.display());
        }
        return Ok(());
    }
    let config = ArchiveConfig::load(paths)?;
    store::ensure_repository(&config.repo_path)?;
    let remote_state = if offline {
        "offline".to_owned()
    } else if !has_origin(&config.repo_path) {
        "no origin".to_owned()
    } else {
        match fetch_remote(&config.repo_path, "Checking archive remote") {
            Ok(()) => "current remote state fetched".to_owned(),
            Err(error) => {
                eprintln!("warning: could not fetch archive: {error:#}");
                "cached remote state".to_owned()
            }
        }
    };
    let pending = store::inspect(paths, &config)?;
    let (objects, referenced_objects, references, sessions, _) = store::counts(paths, &config)?;
    println!("Archive: {}", config.repo_path.display());
    println!("Machine: {} ({})", config.machine_name, config.machine_id);
    println!("Local");
    println!("  Storage: {}", if config.thin { "thin" } else { "full" });
    println!(
        "  Working tree: {}",
        if git_dirty(&config.repo_path)? {
            "changed"
        } else {
            "clean"
        }
    );
    let (ahead, behind) = archive_upstream(&config.repo_path)
        .and_then(|upstream| ahead_behind(&config.repo_path, &upstream).ok())
        .unwrap_or((0, 0));
    println!("  Commits: {ahead} ahead, {behind} behind");
    println!("  Session content cached: {objects} of {referenced_objects}");
    println!("  Metadata references: {references}");
    println!("  Searchable metadata sessions: {sessions}");
    println!(
        "  Changed history sources: {} of {}",
        pending.changed, pending.discovered
    );
    println!("Remote");
    println!(
        "  Origin: {}",
        git_text(&config.repo_path, ["remote", "get-url", "origin"])
            .unwrap_or_else(|_| "not configured".to_owned())
    );
    println!("  State: {remote_state}");
    if verbose {
        println!("Paths");
        println!("  Repository: {}", config.repo_path.display());
        println!("  Config: {}", config_path.display());
        println!(
            "  Index: {}",
            paths.state_dir.join("chat-archive.sqlite").display()
        );
        println!(
            "  Object cache: {}",
            paths.state_dir.join("chat-archive-objects").display()
        );
    }
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
    commit_archive_changes(&config, message)?;
    if has_origin(&config.repo_path) && remote_has_head(&config.repo_path) {
        fetch_remote(&config.repo_path, "Fetching archive metadata")?;
        rebase_remote(&config.repo_path)?;
    }
    let stats = store::update(paths, &config, false)?;
    print_update(&stats, false);
    if stats.changed == 0 {
        store::rebuild_index(paths, &config)?;
    }
    commit_archive_changes(&config, message)?;
    if has_origin(&config.repo_path) {
        push_with_retry(&config.repo_path)?;
        println!("Pushed the unified archive.");
    } else {
        println!("No origin remote is configured. Changes remain local.");
    }
    Ok(())
}

fn commit_archive_changes(config: &ArchiveConfig, message: &str) -> Result<()> {
    if config.thin {
        util::command_status("git", ["add", "--sparse", "-A"], Some(&config.repo_path))?;
    } else {
        util::command_status("git", ["add", "-A"], Some(&config.repo_path))?;
    }
    if git_dirty(&config.repo_path)? {
        util::command_status("git", ["commit", "-m", message], Some(&config.repo_path))?;
    } else {
        println!("Nothing to commit.");
    }
    Ok(())
}

fn configure_thin_checkout(repo: &std::path::Path) -> Result<()> {
    util::command_status(
        "git",
        [
            "sparse-checkout",
            "set",
            "--no-cone",
            "/.gitignore",
            "/README.md",
            "/machines/",
            "/refs/",
            "/schema/",
        ],
        Some(repo),
    )
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

fn fetch_remote(repo: &std::path::Path, message: &str) -> Result<()> {
    let activity = Activity::delayed(message, Duration::from_millis(300));
    let output = Command::new("git")
        .args(["fetch", "--prune", "origin"])
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        bail!(
            "git fetch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    activity.finish("Archive remote state fetched");
    Ok(())
}

fn rebase_remote(repo: &std::path::Path) -> Result<()> {
    let Some(upstream) = archive_upstream(repo) else {
        return Ok(());
    };
    let status = Command::new("git")
        .args(["rebase", &upstream])
        .current_dir(repo)
        .status()?;
    if status.success() {
        return Ok(());
    }
    let _ = Command::new("git")
        .args(["rebase", "--abort"])
        .current_dir(repo)
        .status();
    bail!("archive rebase conflicted; local commits remain unchanged")
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
        fetch_remote(repo, "Refreshing archive before retry")?;
        rebase_remote(repo)?;
    }
    unreachable!()
}

fn archive_upstream(repo: &std::path::Path) -> Option<String> {
    git_text(
        repo,
        [
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .ok()
}

fn ahead_behind(repo: &std::path::Path, upstream: &str) -> Result<(usize, usize)> {
    let range = format!("HEAD...{upstream}");
    let text = git_text(repo, ["rev-list", "--left-right", "--count", &range])?;
    let mut fields = text.split_whitespace();
    let ahead = fields.next().context("missing ahead count")?.parse()?;
    let behind = fields.next().context("missing behind count")?.parse()?;
    Ok((ahead, behind))
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
