use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Cursor},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use fs2::FileExt;
use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use walkdir::WalkDir;

use super::{
    ArchiveConfig, adapters,
    model::{ArchiveRef, Artifact, ParsedArtifact, Session},
};
use crate::{config::Paths, progress::Activity, util};

pub struct ArchiveLock(File);

impl ArchiveLock {
    pub fn acquire(paths: &Paths) -> Result<Self> {
        fs::create_dir_all(&paths.state_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(paths.state_dir.join("chat-archive.lock"))?;
        file.lock_exclusive()
            .context("another archive update is running")?;
        Ok(Self(file))
    }
}

impl Drop for ArchiveLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[derive(Default, Debug)]
pub struct UpdateStats {
    pub discovered: usize,
    pub changed: usize,
    pub parsed_sessions: usize,
    pub objects_written: usize,
    pub refs_written: usize,
    pub objects_pruned: usize,
    pub events: usize,
}

pub fn update(paths: &Paths, config: &ArchiveConfig, dry_run: bool) -> Result<UpdateStats> {
    let activity = Activity::new("Scanning local chat history");
    let _lock = ArchiveLock::acquire(paths)?;
    ensure_repository(&config.repo_path)?;
    let mut connection = state_connection(paths)?;
    let artifacts = adapters::discover(paths)?;
    activity.set_message(format!("Checking {} discovered sources", artifacts.len()));
    let changed: Vec<_> = artifacts
        .iter()
        .filter(|artifact| {
            source_changed(&connection, &artifact.path, &artifact.fingerprint).unwrap_or(true)
        })
        .cloned()
        .collect();
    let mut stats = UpdateStats {
        discovered: artifacts.len(),
        changed: changed.len(),
        ..UpdateStats::default()
    };
    if dry_run {
        activity.finish("Source scan complete");
        return Ok(stats);
    }
    if changed.is_empty() {
        activity.finish("Archive is current");
        return Ok(stats);
    }
    activity.set_message(format!("Normalizing {} changed sources", changed.len()));
    let parsed_results: Vec<Result<ParsedArtifact>> = changed
        .into_par_iter()
        .map(|artifact| adapters::parse(paths, artifact))
        .collect();
    let parsed: Vec<ParsedArtifact> = parsed_results
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .with_context(|| "could not parse a changed history source")?;
    let mut selected: HashMap<String, (Artifact, Session)> = HashMap::new();
    for parsed_artifact in &parsed {
        for session in &parsed_artifact.sessions {
            let replace = selected
                .get(&session.logical_id)
                .is_none_or(|(_, current)| rank(session) > rank(current));
            if replace {
                selected.insert(
                    session.logical_id.clone(),
                    (parsed_artifact.artifact.clone(), session.clone()),
                );
            }
        }
    }
    stats.parsed_sessions = selected.len();
    stats.events = selected
        .values()
        .map(|(_, session)| session.events.len())
        .sum();
    activity.set_message(format!("Writing {} normalized sessions", selected.len()));
    let writes: Vec<Result<bool>> = selected
        .into_values()
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(artifact, session)| {
            let (object_hash, object_size, created) = write_object(paths, config, &session)?;
            write_ref(
                config,
                &artifact.path,
                &artifact.fingerprint,
                &session,
                &object_hash,
                object_size,
            )?;
            Ok(created)
        })
        .collect();
    for write in writes {
        stats.objects_written += usize::from(write?);
        stats.refs_written += 1;
    }
    activity.set_message("Pruning superseded session objects");
    stats.objects_pruned = prune_unreferenced_objects(paths, config)?;
    activity.set_message("Saving source fingerprints");
    let transaction = connection.transaction()?;
    for parsed in parsed {
        transaction.execute(
            "INSERT INTO source_state(path, source, fingerprint, scanned_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(path) DO UPDATE SET source=excluded.source, fingerprint=excluded.fingerprint, scanned_at=excluded.scanned_at",
            params![
                parsed.artifact.path.to_string_lossy(),
                parsed.artifact.source,
                parsed.artifact.fingerprint,
                Utc::now().to_rfc3339(),
            ],
        )?;
    }
    transaction.commit()?;
    rebuild_index_with_activity(paths, config, &activity)?;
    activity.finish("Archive update complete");
    Ok(stats)
}

fn state_connection(paths: &Paths) -> Result<Connection> {
    fs::create_dir_all(&paths.state_dir)?;
    let connection = Connection::open(paths.state_dir.join("chat-archive.sqlite"))?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS source_state(
             path TEXT PRIMARY KEY,
             source TEXT NOT NULL,
             fingerprint TEXT NOT NULL,
             scanned_at TEXT NOT NULL
         );",
    )?;
    Ok(connection)
}

fn source_changed(connection: &Connection, path: &Path, fingerprint: &str) -> Result<bool> {
    let previous: Option<String> = connection
        .query_row(
            "SELECT fingerprint FROM source_state WHERE path = ?1",
            [path.to_string_lossy().as_ref()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(previous.as_deref() != Some(fingerprint))
}

fn object_bytes(session: &Session) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut metadata = serde_json::to_value(session)?;
    if let Some(object) = metadata.as_object_mut() {
        object.insert("type".to_owned(), Value::String("session".to_owned()));
        object.remove("events");
    }
    serde_json::to_writer(&mut output, &metadata)?;
    output.push(b'\n');
    for event in &session.events {
        let mut value = serde_json::to_value(event)?;
        if let Some(object) = value.as_object_mut() {
            object.insert("type".to_owned(), Value::String("event".to_owned()));
        }
        serde_json::to_writer(&mut output, &value)?;
        output.push(b'\n');
    }
    Ok(output)
}

fn write_object(
    paths: &Paths,
    config: &ArchiveConfig,
    session: &Session,
) -> Result<(String, u64, bool)> {
    let bytes = object_bytes(session)?;
    let hash = util::sha256_hex(&bytes);
    let path = config.repo_path.join(object_relative_path(&hash)?);
    let created = !path.is_file();
    if created {
        util::atomic_write(&path, &bytes)?;
    }
    let cached = cached_object_path(paths, &hash)?;
    if !cached.is_file() {
        util::atomic_write(&cached, &bytes)?;
    }
    Ok((hash, bytes.len() as u64, created))
}

fn write_ref(
    config: &ArchiveConfig,
    source_path: &Path,
    fingerprint: &str,
    session: &Session,
    object_hash: &str,
    object_size: u64,
) -> Result<()> {
    let reference = ArchiveRef {
        schema_version: 2,
        machine_id: config.machine_id.clone(),
        machine_name: config.machine_name.clone(),
        source: session.source.clone(),
        native_id: session.native_id.clone(),
        logical_id: session.logical_id.clone(),
        title: session.title.clone(),
        parent_session_id: session.parent_session_id.clone(),
        started_at: session.started_at.clone(),
        updated_at: session.updated_at.clone(),
        cwd: session.cwd.clone(),
        git_branch: session.git_branch.clone(),
        provider: session.provider.clone(),
        models: session.models.clone(),
        event_count: session.events.len(),
        object_size,
        object_sha256: object_hash.to_owned(),
        source_path: source_path.to_string_lossy().into_owned(),
        source_fingerprint: fingerprint.to_owned(),
        observed_at: Utc::now().to_rfc3339(),
    };
    let path = config
        .repo_path
        .join("refs")
        .join(&config.machine_id)
        .join(&session.source)
        .join(format!("{}.json", session.logical_id));
    util::atomic_write(&path, &(serde_json::to_vec_pretty(&reference)?))?;
    Ok(())
}

pub fn rebuild_index(paths: &Paths, config: &ArchiveConfig) -> Result<()> {
    let activity = Activity::new("Rebuilding the archive index");
    rebuild_index_with_activity(paths, config, &activity)?;
    activity.finish("Archive index rebuilt");
    Ok(())
}

fn rebuild_index_with_activity(
    paths: &Paths,
    config: &ArchiveConfig,
    activity: &Activity,
) -> Result<()> {
    let mut selected: HashMap<String, ArchiveRef> = HashMap::new();
    let reference_paths = ref_paths(&config.repo_path);
    activity.set_message(format!(
        "Reading {} archive references",
        reference_paths.len()
    ));
    for reference_path in reference_paths {
        let Ok(contents) = fs::read(&reference_path) else {
            continue;
        };
        let Ok(reference) = serde_json::from_slice::<ArchiveRef>(&contents) else {
            continue;
        };
        let replace = selected
            .get(&reference.logical_id)
            .is_none_or(|current| reference_rank(&reference) > reference_rank(current));
        if replace {
            selected.insert(reference.logical_id.clone(), reference);
        }
    }
    activity.set_message(format!("Indexing {} sessions", selected.len()));
    let mut connection = state_connection(paths)?;
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "DROP TABLE IF EXISTS sessions;
         DROP TABLE IF EXISTS messages;
         DROP TABLE IF EXISTS sessions_fts;
         DROP TABLE IF EXISTS messages_fts;
         CREATE TABLE sessions(
             logical_id TEXT PRIMARY KEY,
             source TEXT NOT NULL,
             native_id TEXT NOT NULL,
             title TEXT NOT NULL,
             started_at TEXT,
             updated_at TEXT,
             cwd TEXT,
             provider TEXT,
             models TEXT NOT NULL,
             object_sha256 TEXT NOT NULL,
             machine_id TEXT NOT NULL,
             event_count INTEGER NOT NULL
         );
         CREATE TABLE messages(
             logical_id TEXT NOT NULL,
             sequence INTEGER NOT NULL,
             timestamp TEXT,
             kind TEXT NOT NULL,
             role TEXT,
             text TEXT,
             model TEXT,
             PRIMARY KEY(logical_id, sequence)
         );
         CREATE VIRTUAL TABLE sessions_fts USING fts5(logical_id UNINDEXED, title, cwd, models);
         CREATE VIRTUAL TABLE messages_fts USING fts5(logical_id UNINDEXED, sequence UNINDEXED, text);",
    )?;
    let mut references: Vec<_> = selected.into_values().collect();
    references.sort_by(|left, right| left.logical_id.cmp(&right.logical_id));
    for mut reference in references {
        let cached = read_object(paths, config, &reference.object_sha256, false)?;
        if let Some(session) = &cached
            && reference.title.is_empty()
        {
            copy_session_metadata(&mut reference, session);
        }
        let models = reference
            .models
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(",");
        transaction.execute(
            "INSERT INTO sessions VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                reference.logical_id,
                reference.source,
                reference.native_id,
                reference.title,
                reference.started_at,
                reference.updated_at,
                reference.cwd,
                reference.provider,
                models,
                reference.object_sha256,
                reference.machine_id,
                reference.event_count as i64,
            ],
        )?;
        transaction.execute(
            "INSERT INTO sessions_fts(logical_id, title, cwd, models) VALUES (?1, ?2, ?3, ?4)",
            params![reference.logical_id, reference.title, reference.cwd, models,],
        )?;
        let Some(session) = cached else {
            continue;
        };
        for event in session.events {
            transaction.execute(
                "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session.logical_id,
                    event.sequence as i64,
                    event.timestamp,
                    event.kind,
                    event.role,
                    event.text,
                    event.model,
                ],
            )?;
            if let Some(text) = event.text {
                transaction.execute(
                    "INSERT INTO messages_fts(logical_id, sequence, text) VALUES (?1, ?2, ?3)",
                    params![session.logical_id, event.sequence as i64, text],
                )?;
            }
        }
    }
    transaction.commit()?;
    Ok(())
}

fn reference_rank(reference: &ArchiveRef) -> (&str, usize, &str) {
    (
        reference.updated_at.as_deref().unwrap_or_default(),
        reference.event_count,
        &reference.observed_at,
    )
}

fn copy_session_metadata(reference: &mut ArchiveRef, session: &Session) {
    reference.title = session.title.clone();
    reference.parent_session_id = session.parent_session_id.clone();
    reference.started_at = session.started_at.clone();
    reference.updated_at = session.updated_at.clone();
    reference.cwd = session.cwd.clone();
    reference.git_branch = session.git_branch.clone();
    reference.provider = session.provider.clone();
    reference.models = session.models.clone();
    reference.event_count = session.events.len();
}

fn rank(session: &Session) -> (&str, usize) {
    (
        session.updated_at.as_deref().unwrap_or_default(),
        session.events.len(),
    )
}

fn object_relative_path(hash: &str) -> Result<PathBuf> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid object hash {hash}");
    }
    Ok(PathBuf::from("objects/sha256")
        .join(&hash[..2])
        .join(format!("{hash}.jsonl")))
}

fn cached_object_path(paths: &Paths, hash: &str) -> Result<PathBuf> {
    Ok(paths
        .state_dir
        .join("chat-archive-objects/sha256")
        .join(&hash[..2])
        .join(format!("{hash}.jsonl")))
}

fn available_object_bytes(
    paths: &Paths,
    config: &ArchiveConfig,
    hash: &str,
) -> Result<Option<Vec<u8>>> {
    let relative = object_relative_path(hash)?;
    let worktree = config.repo_path.join(relative);
    if worktree.is_file() {
        return Ok(Some(fs::read(worktree)?));
    }
    let cached = cached_object_path(paths, hash)?;
    if cached.is_file() {
        return Ok(Some(fs::read(cached)?));
    }
    Ok(None)
}

fn fetch_object_bytes(paths: &Paths, config: &ArchiveConfig, hash: &str) -> Result<Vec<u8>> {
    if let Some(bytes) = available_object_bytes(paths, config, hash)? {
        return Ok(bytes);
    }
    let relative = object_relative_path(hash)?;
    let specification = format!("HEAD:{}", relative.to_string_lossy());
    let output = Command::new("git")
        .args(["show", &specification])
        .current_dir(&config.repo_path)
        .output()
        .with_context(|| format!("could not fetch object {hash}"))?;
    if !output.status.success() {
        bail!("could not fetch object {hash} from the archive remote");
    }
    validate_object_bytes(hash, &output.stdout)?;
    util::atomic_write(&cached_object_path(paths, hash)?, &output.stdout)?;
    Ok(output.stdout)
}

fn validate_object_bytes(hash: &str, bytes: &[u8]) -> Result<()> {
    let actual = util::sha256_hex(bytes);
    if actual != hash {
        bail!("object hash mismatch: expected {hash}, found {actual}");
    }
    Ok(())
}

fn parse_object_bytes(hash: &str, bytes: &[u8]) -> Result<Session> {
    validate_object_bytes(hash, bytes)?;
    let mut lines = BufReader::new(Cursor::new(bytes)).lines();
    let first = lines.next().context("object is empty")??;
    let mut value: Value = serde_json::from_str(&first)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("type");
        object.insert("events".to_owned(), Value::Array(Vec::new()));
    }
    let mut session: Session = serde_json::from_value(value)?;
    for line in lines {
        let mut value: Value = serde_json::from_str(&line?)?;
        if let Some(object) = value.as_object_mut() {
            object.remove("type");
        }
        session.events.push(serde_json::from_value(value)?);
    }
    Ok(session)
}

fn read_object(
    paths: &Paths,
    config: &ArchiveConfig,
    hash: &str,
    fetch: bool,
) -> Result<Option<Session>> {
    let bytes = if fetch {
        Some(fetch_object_bytes(paths, config, hash)?)
    } else {
        available_object_bytes(paths, config, hash)?
    };
    bytes
        .as_deref()
        .map(|bytes| parse_object_bytes(hash, bytes))
        .transpose()
}

fn ref_paths(repo: &Path) -> Vec<PathBuf> {
    WalkDir::new(repo.join("refs"))
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "json")
        })
        .collect()
}

fn prune_unreferenced_objects(paths: &Paths, config: &ArchiveConfig) -> Result<usize> {
    let referenced: HashSet<String> = ref_paths(&config.repo_path)
        .into_iter()
        .filter_map(|path| fs::read(path).ok())
        .filter_map(|contents| serde_json::from_slice::<ArchiveRef>(&contents).ok())
        .map(|reference| reference.object_sha256)
        .collect();
    let mut pruned = 0;
    for root in [
        config.repo_path.join("objects/sha256"),
        paths.state_dir.join("chat-archive-objects/sha256"),
    ] {
        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_file()
                || path
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            let hash = path.file_stem().unwrap_or_default().to_string_lossy();
            if !referenced.contains(hash.as_ref()) {
                fs::remove_file(path)?;
                pruned += 1;
            }
        }
    }
    Ok(pruned)
}

pub struct VerifyResult {
    pub objects: usize,
    pub references: usize,
    pub remote: usize,
}

pub fn verify(paths: &Paths, config: &ArchiveConfig, full: bool) -> Result<VerifyResult> {
    let activity = Activity::new("Reading archive references");
    let mut hashes = HashSet::new();
    let mut references = 0;
    let reference_paths = ref_paths(&config.repo_path);
    activity.set_message(format!(
        "Checking {} archive references",
        reference_paths.len()
    ));
    for path in reference_paths {
        let reference: ArchiveRef = serde_json::from_slice(&fs::read(&path)?)?;
        object_relative_path(&reference.object_sha256)
            .with_context(|| format!("invalid reference {}", path.display()))?;
        hashes.insert(reference.object_sha256);
        references += 1;
    }
    let verb = if full {
        "Fetching and verifying"
    } else {
        "Verifying available"
    };
    activity.set_message(format!("{verb} {} session objects", hashes.len()));
    let mut objects = 0;
    let mut remote = 0;
    for hash in hashes {
        match read_object(paths, config, &hash, full)? {
            Some(_) => objects += 1,
            None => remote += 1,
        }
    }
    activity.finish("Archive verification complete");
    Ok(VerifyResult {
        objects,
        references,
        remote,
    })
}

pub fn counts(
    paths: &Paths,
    config: &ArchiveConfig,
) -> Result<(usize, usize, usize, usize, usize)> {
    let reference_paths = ref_paths(&config.repo_path);
    let hashes: HashSet<String> = reference_paths
        .iter()
        .filter_map(|path| fs::read(path).ok())
        .filter_map(|contents| serde_json::from_slice::<ArchiveRef>(&contents).ok())
        .map(|reference| reference.object_sha256)
        .collect();
    let objects = hashes
        .iter()
        .filter(|hash| object_is_available(paths, config, hash))
        .count();
    let references = reference_paths.len();
    let connection = state_connection(paths)?;
    let sessions = connection
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .unwrap_or(0);
    let messages = connection
        .query_row("SELECT count(*) FROM messages_fts", [], |row| row.get(0))
        .unwrap_or(0);
    Ok((objects, hashes.len(), references, sessions, messages))
}

pub fn search(paths: &Paths, query: &str, limit: usize) -> Result<()> {
    let connection = state_connection(paths)?;
    let mut shown = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT s.logical_id, s.source, s.title, m.role, snippet(messages_fts, 2, '[', ']', '…', 18) \
         FROM messages_fts JOIN sessions s USING(logical_id) JOIN messages m \
         ON m.logical_id=messages_fts.logical_id AND m.sequence=messages_fts.sequence \
         WHERE messages_fts MATCH ?1 ORDER BY bm25(messages_fts) LIMIT ?2",
    )?;
    let rows = statement.query_map(params![query, limit as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (id, source, title, role, snippet) = row?;
        if shown.len() >= limit {
            break;
        }
        if !shown.insert(id.clone()) {
            continue;
        }
        println!("{}  {}  {}", &id[..12.min(id.len())], source, title);
        println!(
            "  {}: {}",
            role.unwrap_or_else(|| "text".to_owned()),
            snippet.replace('\n', " ")
        );
    }
    if shown.len() < limit {
        let mut metadata = connection.prepare(
            "SELECT s.logical_id, s.source, s.title, snippet(sessions_fts, 1, '[', ']', '…', 18) \
             FROM sessions_fts JOIN sessions s USING(logical_id) \
             WHERE sessions_fts MATCH ?1 ORDER BY bm25(sessions_fts) LIMIT ?2",
        )?;
        let rows = metadata.query_map(params![query, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, source, title, snippet) = row?;
            if shown.len() >= limit {
                break;
            }
            if !shown.insert(id.clone()) {
                continue;
            }
            println!("{}  {}  {}", &id[..12.min(id.len())], source, title);
            println!("  metadata: {}", snippet.replace('\n', " "));
        }
    }
    Ok(())
}

pub fn show(paths: &Paths, config: &ArchiveConfig, id: &str) -> Result<()> {
    let (logical_id, hash) = resolve_session(paths, id)?;
    let was_available = object_is_available(paths, config, &hash);
    let activity = (!was_available).then(|| Activity::new("Fetching the session object"));
    let session =
        read_object(paths, config, &hash, true)?.context("session object is unavailable")?;
    if let Some(activity) = activity {
        activity.finish("Session object fetched");
    }
    if !was_available {
        rebuild_index(paths, config)?;
    }
    print_session(session, &logical_id);
    Ok(())
}

pub fn fetch(paths: &Paths, config: &ArchiveConfig, id: &str) -> Result<()> {
    let (logical_id, hash) = resolve_session(paths, id)?;
    let was_available = object_is_available(paths, config, &hash);
    let activity = (!was_available).then(|| Activity::new("Fetching the session object"));
    read_object(paths, config, &hash, true)?.context("session object is unavailable")?;
    if let Some(activity) = activity {
        activity.finish("Session object fetched");
    }
    if !was_available {
        rebuild_index(paths, config)?;
        println!("Fetched session {logical_id}.");
    } else {
        println!("Session {logical_id} is already available.");
    }
    Ok(())
}

pub fn hydrate(paths: &Paths, config: &ArchiveConfig) -> Result<usize> {
    let activity = Activity::new("Reading archive references");
    let hashes: HashSet<String> = ref_paths(&config.repo_path)
        .into_iter()
        .filter_map(|path| fs::read(path).ok())
        .filter_map(|contents| serde_json::from_slice::<ArchiveRef>(&contents).ok())
        .map(|reference| reference.object_sha256)
        .collect();
    activity.set_message(format!("Fetching {} session objects", hashes.len()));
    let mut fetched = 0;
    for hash in hashes {
        if !object_is_available(paths, config, &hash) {
            read_object(paths, config, &hash, true)?;
            fetched += 1;
        }
    }
    rebuild_index_with_activity(paths, config, &activity)?;
    activity.finish("Archive hydration complete");
    Ok(fetched)
}

fn object_is_available(paths: &Paths, config: &ArchiveConfig, hash: &str) -> bool {
    object_relative_path(hash).is_ok_and(|relative| {
        config.repo_path.join(relative).is_file()
            || cached_object_path(paths, hash).is_ok_and(|path| path.is_file())
    })
}

fn resolve_session(paths: &Paths, id: &str) -> Result<(String, String)> {
    let connection = state_connection(paths)?;
    let pattern = format!("{id}%");
    let mut statement = connection.prepare(
        "SELECT logical_id, object_sha256 FROM sessions WHERE logical_id LIKE ?1 ORDER BY logical_id LIMIT 2",
    )?;
    let matches: Vec<(String, String)> = statement
        .query_map([pattern], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if matches.is_empty() {
        bail!("no archived session starts with {id}");
    }
    if matches.len() > 1 {
        bail!("session prefix {id} is ambiguous");
    }
    Ok(matches.into_iter().next().unwrap())
}

fn print_session(session: Session, logical_id: &str) {
    println!("# {}", session.title);
    println!();
    println!("Source: {}", session.source);
    println!("ID: {logical_id}");
    if let Some(cwd) = session.cwd {
        println!("Project: {cwd}");
    }
    for event in session.events {
        if let Some(text) = event.text {
            println!();
            println!("## {}", event.role.unwrap_or(event.kind));
            println!();
            println!("{text}");
        }
    }
}

pub fn ensure_repository(path: &Path) -> Result<()> {
    if !path.is_dir() {
        bail!(
            "archive repository does not exist: {}; run agents archive init",
            path.display()
        );
    }
    Ok(())
}
