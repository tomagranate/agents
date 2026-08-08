use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
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
use crate::{config::Paths, util};

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
    let _lock = ArchiveLock::acquire(paths)?;
    ensure_repository(&config.repo_path)?;
    let mut connection = state_connection(paths)?;
    let artifacts = adapters::discover(paths)?;
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
        return Ok(stats);
    }
    if changed.is_empty() {
        return Ok(stats);
    }
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
    let writes: Vec<Result<bool>> = selected
        .into_values()
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(artifact, session)| {
            let (object_hash, created) = write_object(&config.repo_path, &session)?;
            write_ref(
                config,
                &artifact.path,
                &artifact.fingerprint,
                &session,
                &object_hash,
            )?;
            Ok(created)
        })
        .collect();
    for write in writes {
        stats.objects_written += usize::from(write?);
        stats.refs_written += 1;
    }
    stats.objects_pruned = prune_unreferenced_objects(&config.repo_path)?;
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
    rebuild_index(paths, config)?;
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

fn write_object(repo: &Path, session: &Session) -> Result<(String, bool)> {
    let bytes = object_bytes(session)?;
    let hash = util::sha256_hex(&bytes);
    let path = repo
        .join("objects/sha256")
        .join(&hash[..2])
        .join(format!("{hash}.jsonl"));
    let created = !path.is_file();
    if created {
        util::atomic_write(&path, &bytes)?;
    }
    Ok((hash, created))
}

fn write_ref(
    config: &ArchiveConfig,
    source_path: &Path,
    fingerprint: &str,
    session: &Session,
    object_hash: &str,
) -> Result<()> {
    let reference = ArchiveRef {
        schema_version: 1,
        machine_id: config.machine_id.clone(),
        machine_name: config.machine_name.clone(),
        source: session.source.clone(),
        native_id: session.native_id.clone(),
        logical_id: session.logical_id.clone(),
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
    let mut selected: HashMap<String, (Session, ArchiveRef)> = HashMap::new();
    for reference_path in ref_paths(&config.repo_path) {
        let Ok(contents) = fs::read(&reference_path) else {
            continue;
        };
        let Ok(reference) = serde_json::from_slice::<ArchiveRef>(&contents) else {
            continue;
        };
        let Ok(session) = read_object(&config.repo_path, &reference.object_sha256) else {
            continue;
        };
        let replace = selected
            .get(&session.logical_id)
            .is_none_or(|(current, _)| rank(&session) > rank(current));
        if replace {
            selected.insert(session.logical_id.clone(), (session, reference));
        }
    }
    let mut connection = state_connection(paths)?;
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "DROP TABLE IF EXISTS sessions;
         DROP TABLE IF EXISTS messages;
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
         CREATE VIRTUAL TABLE messages_fts USING fts5(logical_id UNINDEXED, sequence UNINDEXED, text);",
    )?;
    let mut sessions: Vec<_> = selected.into_values().collect();
    sessions.sort_by(|left, right| left.0.logical_id.cmp(&right.0.logical_id));
    for (session, reference) in sessions {
        transaction.execute(
            "INSERT INTO sessions VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                session.logical_id,
                session.source,
                session.native_id,
                session.title,
                session.started_at,
                session.updated_at,
                session.cwd,
                session.provider,
                session.models.into_iter().collect::<Vec<_>>().join(","),
                reference.object_sha256,
                reference.machine_id,
                session.events.len() as i64,
            ],
        )?;
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

fn rank(session: &Session) -> (&str, usize) {
    (
        session.updated_at.as_deref().unwrap_or_default(),
        session.events.len(),
    )
}

pub fn read_object(repo: &Path, hash: &str) -> Result<Session> {
    if hash.len() < 2 {
        bail!("invalid object hash {hash}");
    }
    let path = repo
        .join("objects/sha256")
        .join(&hash[..2])
        .join(format!("{hash}.jsonl"));
    let mut lines =
        BufReader::new(File::open(&path).with_context(|| format!("missing object {hash}"))?)
            .lines();
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

fn prune_unreferenced_objects(repo: &Path) -> Result<usize> {
    let referenced: HashSet<String> = ref_paths(repo)
        .into_iter()
        .filter_map(|path| fs::read(path).ok())
        .filter_map(|contents| serde_json::from_slice::<ArchiveRef>(&contents).ok())
        .map(|reference| reference.object_sha256)
        .collect();
    let mut pruned = 0;
    for entry in WalkDir::new(repo.join("objects/sha256"))
        .into_iter()
        .filter_map(Result::ok)
    {
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
    Ok(pruned)
}

pub fn verify(config: &ArchiveConfig) -> Result<(usize, usize)> {
    let mut objects = 0;
    for entry in WalkDir::new(config.repo_path.join("objects/sha256"))
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let bytes = fs::read(path)?;
        let hash = util::sha256_hex(&bytes);
        let expected = path.file_stem().unwrap_or_default().to_string_lossy();
        if hash != expected {
            bail!("object hash mismatch: {}", path.display());
        }
        read_object(&config.repo_path, &hash)?;
        objects += 1;
    }
    let mut references = 0;
    for path in ref_paths(&config.repo_path) {
        let reference: ArchiveRef = serde_json::from_slice(&fs::read(&path)?)?;
        read_object(&config.repo_path, &reference.object_sha256)
            .with_context(|| format!("invalid reference {}", path.display()))?;
        references += 1;
    }
    Ok((objects, references))
}

pub fn counts(paths: &Paths, config: &ArchiveConfig) -> Result<(usize, usize, usize, usize)> {
    let objects = WalkDir::new(config.repo_path.join("objects/sha256"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .count();
    let references = ref_paths(&config.repo_path).len();
    let connection = state_connection(paths)?;
    let sessions = connection
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .unwrap_or(0);
    let messages = connection
        .query_row("SELECT count(*) FROM messages_fts", [], |row| row.get(0))
        .unwrap_or(0);
    Ok((objects, references, sessions, messages))
}

pub fn search(paths: &Paths, query: &str, limit: usize) -> Result<()> {
    let connection = state_connection(paths)?;
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
        println!("{}  {}  {}", &id[..12.min(id.len())], source, title);
        println!(
            "  {}: {}",
            role.unwrap_or_else(|| "text".to_owned()),
            snippet.replace('\n', " ")
        );
    }
    Ok(())
}

pub fn show(paths: &Paths, config: &ArchiveConfig, id: &str) -> Result<()> {
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
    let session = read_object(&config.repo_path, &matches[0].1)?;
    println!("# {}", session.title);
    println!();
    println!("Source: {}", session.source);
    println!("ID: {}", session.logical_id);
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
    Ok(())
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
