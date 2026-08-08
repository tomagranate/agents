use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result};
use percent_encoding::percent_decode_str;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use walkdir::WalkDir;

use super::model::{
    Artifact, ArtifactKind, Event, ParsedArtifact, Session, message_event, text_event, tool_event,
};
use crate::{config::Paths, util};

static CODEX_TITLES: OnceLock<HashMap<String, (String, Option<String>)>> = OnceLock::new();
const NORMALIZER_VERSION: u32 = 1;

pub fn discover(paths: &Paths) -> Result<Vec<Artifact>> {
    let mut artifacts = Vec::new();
    collect_jsonl(
        &paths.home.join(".codex/sessions"),
        "codex",
        ArtifactKind::Codex,
        &mut artifacts,
    )?;
    collect_jsonl(
        &paths.home.join(".codex/archived_sessions"),
        "codex",
        ArtifactKind::Codex,
        &mut artifacts,
    )?;
    collect_jsonl(
        &paths.home.join(".claude/projects"),
        "claude",
        ArtifactKind::Claude,
        &mut artifacts,
    )?;
    for path in walk_files(&paths.home.join(".claude/projects"), |path| {
        path.extension().is_some_and(|extension| extension == "md")
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "memory")
    }) {
        artifacts.push(artifact("claude", path, ArtifactKind::ClaudeMemory)?);
    }
    let opencode = paths.home.join(".local/share/opencode/opencode.db");
    if opencode.is_file() {
        artifacts.push(artifact("opencode", opencode, ArtifactKind::OpenCode)?);
    }
    for path in walk_files(&paths.home.join(".grok/sessions"), |path| {
        path.file_name()
            .is_some_and(|name| name == "chat_history.jsonl")
    }) {
        artifacts.push(artifact("grok", path, ArtifactKind::Grok)?);
    }
    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(artifacts)
}

fn collect_jsonl(
    root: &Path,
    source: &'static str,
    kind: ArtifactKind,
    artifacts: &mut Vec<Artifact>,
) -> Result<()> {
    for path in walk_files(root, |path| {
        path.extension()
            .is_some_and(|extension| extension == "jsonl")
    }) {
        artifacts.push(artifact(source, path, kind)?);
    }
    Ok(())
}

fn walk_files(root: &Path, predicate: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .filter(|path| path.is_file() && predicate(path))
        .collect()
}

fn artifact(source: &'static str, path: PathBuf, kind: ArtifactKind) -> Result<Artifact> {
    let metadata = fs::metadata(&path)?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut fingerprint = format!("v{NORMALIZER_VERSION}:{}:{modified}", metadata.len());
    let companion = match kind {
        ArtifactKind::Grok => path.parent().map(|parent| parent.join("summary.json")),
        ArtifactKind::OpenCode => Some(PathBuf::from(format!("{}-wal", path.to_string_lossy()))),
        _ => None,
    };
    if let Some(companion) = companion.filter(|candidate| candidate.is_file()) {
        let companion_metadata = fs::metadata(companion)?;
        let companion_modified = companion_metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        fingerprint.push_str(&format!(
            ":{}:{companion_modified}",
            companion_metadata.len()
        ));
    }
    Ok(Artifact {
        source,
        fingerprint,
        path,
        kind,
    })
}

pub fn parse(paths: &Paths, artifact: Artifact) -> Result<ParsedArtifact> {
    let sessions = match artifact.kind {
        ArtifactKind::Codex => {
            let mut session = parse_codex(&artifact.path)?;
            if let Some(session) = &mut session
                && let Some((title, updated_at)) = codex_titles(paths).get(&session.native_id)
            {
                session.title = title.clone();
                session.updated_at = updated_at.clone().or(session.updated_at.take());
            }
            session.into_iter().collect()
        }
        ArtifactKind::Claude => parse_claude(paths, &artifact.path)?.into_iter().collect(),
        ArtifactKind::ClaudeMemory => vec![parse_claude_memory(paths, &artifact.path)?],
        ArtifactKind::OpenCode => parse_opencode(&artifact.path)?,
        ArtifactKind::Grok => parse_grok(&artifact.path)?.into_iter().collect(),
    };
    Ok(ParsedArtifact { artifact, sessions })
}

fn codex_titles(paths: &Paths) -> &HashMap<String, (String, Option<String>)> {
    CODEX_TITLES.get_or_init(|| {
        let mut titles = HashMap::new();
        let Ok(file) = File::open(paths.home.join(".codex/session_index.jsonl")) else {
            return titles;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(row) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let (Some(id), Some(title)) = (string(row.get("id")), string(row.get("thread_name")))
            {
                titles.insert(id, (title, string(row.get("updated_at"))));
            }
        }
        titles
    })
}

fn logical_id(source: &str, native_id: &str) -> String {
    util::sha256_hex(format!("{source}:{native_id}").as_bytes())
}

fn parse_codex(path: &Path) -> Result<Option<Session>> {
    let mut native_id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("unknown")
        .to_owned();
    let mut title = None;
    let mut started_at: Option<String> = None;
    let mut updated_at: Option<String> = None;
    let mut cwd = None;
    let mut git_branch = None;
    let mut provider = None;
    let mut models = BTreeSet::new();
    let mut current_model = None;
    let mut events = Vec::new();
    let mut metadata_seen = false;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.contains("\"custom_tool_call_output\"")
            || line.contains("\"function_call_output\"")
            || line.contains("\"reasoning\"")
            || line.contains("\"token_count\"")
            || line.contains("\"world_state\"")
        {
            continue;
        }
        if !line.contains("\"session_meta\"")
            && !line.contains("\"turn_context\"")
            && !line.contains("\"response_item\"")
            && !line.contains("\"compacted\"")
        {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let row_type = row.get("type").and_then(Value::as_str).unwrap_or_default();
        let payload = row.get("payload").unwrap_or(&Value::Null);
        let timestamp = string(payload.get("timestamp")).or_else(|| string(row.get("timestamp")));
        update_dates(&mut started_at, &mut updated_at, timestamp.as_deref());
        match row_type {
            "session_meta" if !metadata_seen => {
                native_id = string(payload.get("id"))
                    .or_else(|| string(payload.get("session_id")))
                    .unwrap_or(native_id);
                cwd = string(payload.get("cwd"));
                provider = string(payload.get("model_provider"));
                if let Some(git) = payload.get("git") {
                    git_branch = string(git.get("branch"));
                }
                metadata_seen = true;
            }
            "turn_context" => {
                if let Some(model) = string(payload.get("model")) {
                    models.insert(model.clone());
                    current_model = Some(model);
                }
                cwd = string(payload.get("cwd")).or(cwd);
            }
            "compacted" => {
                if let Some(text) = string(payload.get("message")) {
                    events.push(text_event(timestamp, "summary", "system", text));
                }
            }
            "response_item" => {
                let payload_type = payload
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if payload_type == "message" {
                    let Some(role @ ("user" | "assistant")) =
                        payload.get("role").and_then(Value::as_str)
                    else {
                        continue;
                    };
                    let phase = string(payload.get("phase")).map(|value| {
                        if value == "final_answer" {
                            "final".to_owned()
                        } else {
                            value
                        }
                    });
                    for text in text_blocks(payload.get("content")) {
                        if role == "user" && title.is_none() {
                            title = Some(util::derive_title(&text, &native_id));
                        }
                        events.push(message_event(
                            timestamp.clone(),
                            role,
                            text,
                            phase.clone(),
                            provider.clone(),
                            current_model.clone(),
                        ));
                    }
                } else if matches!(payload_type, "custom_tool_call" | "function_call") {
                    if let Some(name) = string(payload.get("name")) {
                        events.push(tool_event(timestamp, name));
                    }
                } else if matches!(
                    payload_type,
                    "web_search_call" | "local_shell_call" | "mcp_tool_call"
                ) {
                    let name = string(payload.get("name"))
                        .unwrap_or_else(|| payload_type.trim_end_matches("_call").to_owned());
                    events.push(tool_event(timestamp, name));
                }
            }
            _ => {}
        }
    }
    if events.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        Session {
            schema_version: 1,
            logical_id: logical_id("codex", &native_id),
            source: "codex".to_owned(),
            native_id: native_id.clone(),
            title: title.unwrap_or_else(|| native_id.clone()),
            parent_session_id: None,
            started_at,
            updated_at,
            cwd,
            git_branch,
            provider,
            models,
            events,
        }
        .finish(),
    ))
}

fn parse_claude(paths: &Paths, path: &Path) -> Result<Option<Session>> {
    let mut native_id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("unknown")
        .to_owned();
    let mut title = None;
    let mut started_at = None;
    let mut updated_at = None;
    let mut cwd = None;
    let mut git_branch = None;
    let mut models = BTreeSet::new();
    let mut events = Vec::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.contains("\"file-history-snapshot\"") || line.contains("\"attachment\"") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let row_type = row.get("type").and_then(Value::as_str).unwrap_or_default();
        if matches!(row_type, "ai-title" | "custom-title") {
            title = string(row.get("aiTitle"))
                .or_else(|| string(row.get("customTitle")))
                .or(title);
            continue;
        }
        let timestamp = string(row.get("timestamp"));
        update_dates(&mut started_at, &mut updated_at, timestamp.as_deref());
        native_id = string(row.get("sessionId"))
            .or_else(|| string(row.get("session_id")))
            .unwrap_or(native_id);
        cwd = string(row.get("cwd")).or(cwd);
        git_branch = string(row.get("gitBranch")).or(git_branch);
        if row_type == "system"
            && matches!(
                row.get("subtype").and_then(Value::as_str),
                Some("compact_boundary" | "away_summary")
            )
        {
            if let Some(text) = string(row.get("content")) {
                events.push(text_event(timestamp, "summary", "system", text));
            }
            continue;
        }
        let Some(role @ ("user" | "assistant")) =
            Some(row_type).filter(|role| matches!(*role, "user" | "assistant"))
        else {
            continue;
        };
        if row.get("isMeta").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let message = row.get("message").unwrap_or(&Value::Null);
        let model = string(message.get("model"));
        if let Some(model) = &model {
            models.insert(model.clone());
        }
        let (texts, tools, has_tool_result) = claude_blocks(message.get("content"));
        if role == "user"
            && (row
                .get("toolUseResult")
                .is_some_and(|value| !value.is_null())
                || (has_tool_result && texts.is_empty()))
        {
            continue;
        }
        for text in texts {
            if role == "user" && title.is_none() {
                title = Some(util::derive_title(&text, &native_id));
            }
            events.push(message_event(
                timestamp.clone(),
                role,
                text,
                None,
                model.as_deref().and_then(infer_provider),
                model.clone(),
            ));
        }
        for tool in tools {
            events.push(tool_event(timestamp.clone(), tool));
        }
    }
    if events.is_empty() {
        return Ok(None);
    }
    let mut parent_session_id = None;
    if path
        .components()
        .any(|component| component.as_os_str() == "subagents")
    {
        let parent = native_id.clone();
        let relative = path
            .strip_prefix(paths.home.join(".claude/projects"))
            .unwrap_or(path)
            .with_extension("")
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        native_id = format!("{parent}:subagent:{relative}");
        parent_session_id = Some(parent);
    }
    mark_last_final(&mut events);
    Ok(Some(
        Session {
            schema_version: 1,
            logical_id: logical_id("claude", &native_id),
            source: "claude".to_owned(),
            native_id: native_id.clone(),
            title: title.unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
            parent_session_id,
            started_at,
            updated_at,
            cwd,
            git_branch,
            provider: models.iter().next().and_then(|model| infer_provider(model)),
            models,
            events,
        }
        .finish(),
    ))
}

fn parse_claude_memory(_paths: &Paths, path: &Path) -> Result<Session> {
    let project = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .unwrap_or("unknown");
    let native_id = format!(
        "memory:{project}:{}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let timestamp = file_timestamp(path);
    Ok(Session {
        schema_version: 1,
        logical_id: logical_id("claude", &native_id),
        source: "claude".to_owned(),
        native_id,
        title: format!(
            "Memory: {}",
            path.file_stem().unwrap_or_default().to_string_lossy()
        ),
        parent_session_id: None,
        started_at: timestamp.clone(),
        updated_at: timestamp.clone(),
        cwd: Some(project.to_owned()),
        git_branch: None,
        provider: None,
        models: BTreeSet::new(),
        events: vec![text_event(
            timestamp,
            "memory",
            "system",
            fs::read_to_string(path)?,
        )],
    })
}

fn parse_opencode(path: &Path) -> Result<Vec<Session>> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut session_statement = connection.prepare(
        "SELECT id, title, directory, time_created, time_updated FROM session ORDER BY time_created, id",
    )?;
    let sessions = session_statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<i64>>(4)?,
        ))
    })?;
    let mut result = Vec::new();
    for session in sessions {
        let (native_id, title, directory, created, updated) = session?;
        let mut events = Vec::new();
        let mut models = BTreeSet::new();
        let mut provider = None;
        let mut statement = connection.prepare(
            "SELECT p.time_created, m.data, p.data FROM part p JOIN message m ON m.id = p.message_id \
             WHERE p.session_id = ?1 ORDER BY p.time_created, p.id",
        )?;
        let rows = statement.query_map([&native_id], |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (timestamp, message_json, part_json) = row?;
            let message: Value = serde_json::from_str(&message_json).unwrap_or(Value::Null);
            let part: Value = serde_json::from_str(&part_json).unwrap_or(Value::Null);
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let model = string(message.get("modelID"))
                .or_else(|| string(message.pointer("/model/modelID")));
            let event_provider = string(message.get("providerID"))
                .or_else(|| string(message.pointer("/model/providerID")));
            if let Some(model) = &model {
                models.insert(model.clone());
            }
            provider = event_provider.clone().or(provider);
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = string(part.get("text")) {
                        events.push(message_event(
                            millis(timestamp),
                            role,
                            text,
                            (role == "assistant"
                                && message.get("finish").and_then(Value::as_str) == Some("stop"))
                            .then(|| "final".to_owned()),
                            event_provider,
                            model,
                        ));
                    }
                }
                Some("tool") => {
                    if let Some(name) = string(part.get("tool")) {
                        events.push(tool_event(millis(timestamp), name));
                    }
                }
                _ => {}
            }
        }
        mark_last_final(&mut events);
        result.push(
            Session {
                schema_version: 1,
                logical_id: logical_id("opencode", &native_id),
                source: "opencode".to_owned(),
                native_id: native_id.clone(),
                title: title.unwrap_or_else(|| native_id.clone()),
                parent_session_id: None,
                started_at: millis(created),
                updated_at: millis(updated),
                cwd: directory,
                git_branch: None,
                provider,
                models,
                events,
            }
            .finish(),
        );
    }
    Ok(result)
}

fn parse_grok(path: &Path) -> Result<Option<Session>> {
    let directory = path
        .parent()
        .context("Grok history has no session directory")?;
    let native_id = directory
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("unknown")
        .to_owned();
    let summary: Value = fs::read_to_string(directory.join("summary.json"))
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or(Value::Null);
    let mut events = Vec::new();
    if let Some(text) = string(summary.get("session_summary")) {
        events.push(text_event(
            string(summary.get("updated_at")),
            "summary",
            "system",
            text,
        ));
    }
    let mut models = BTreeSet::new();
    let mut provider = None;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.contains("\"type\":\"tool_result\"")
            || line.contains("\"type\":\"reasoning\"")
            || line.contains("\"type\":\"system\"")
        {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let row_type = row.get("type").and_then(Value::as_str).unwrap_or_default();
        let timestamp = string(row.get("timestamp"));
        let model = string(row.get("model")).or_else(|| string(row.get("model_id")));
        let event_provider = string(row.get("provider"));
        if let Some(model) = &model {
            models.insert(model.clone());
        }
        provider = event_provider.clone().or(provider);
        match row_type {
            "user" => {
                for text in text_blocks(row.get("content")) {
                    let mut event = message_event(
                        timestamp.clone(),
                        "user",
                        text,
                        None,
                        event_provider.clone(),
                        model.clone(),
                    );
                    event.synthetic = row
                        .get("synthetic_reason")
                        .is_some_and(|value| !value.is_null())
                        .then_some(true);
                    events.push(event);
                }
            }
            "assistant" => {
                if let Some(text) = string(row.get("content")) {
                    let phase = (!row.get("tool_calls").is_some_and(|calls| {
                        calls.as_array().is_some_and(|calls| !calls.is_empty())
                    }))
                    .then(|| "final".to_owned());
                    events.push(message_event(
                        timestamp.clone(),
                        "assistant",
                        text,
                        phase,
                        event_provider,
                        model,
                    ));
                }
                if let Some(calls) = row.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        if let Some(name) = string(call.get("name"))
                            .or_else(|| string(call.pointer("/function/name")))
                        {
                            events.push(tool_event(timestamp.clone(), name));
                        }
                    }
                }
            }
            "backend_tool_call" => {
                if let Some(name) = string(row.pointer("/kind/tool_type")) {
                    events.push(tool_event(timestamp, name));
                }
            }
            _ => {}
        }
    }
    if events.is_empty() {
        return Ok(None);
    }
    let encoded_project = directory
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let decoded_project = percent_decode_str(encoded_project)
        .decode_utf8_lossy()
        .into_owned();
    Ok(Some(
        Session {
            schema_version: 1,
            logical_id: logical_id("grok", &native_id),
            source: "grok".to_owned(),
            native_id: native_id.clone(),
            title: string(summary.get("generated_title")).unwrap_or_else(|| native_id.clone()),
            parent_session_id: None,
            started_at: string(summary.get("created_at")),
            updated_at: string(summary.get("updated_at"))
                .or_else(|| string(summary.get("last_active_at"))),
            cwd: string(summary.get("git_root_dir")).or(Some(decoded_project)),
            git_branch: None,
            provider,
            models,
            events,
        }
        .finish(),
    ))
}

fn string(value: Option<&Value>) -> Option<String> {
    util::clean_text(value.and_then(Value::as_str))
}

fn text_blocks(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => util::clean_text(Some(text)).into_iter().collect(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| {
                matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("text" | "input_text" | "output_text")
                )
            })
            .filter_map(|block| string(block.get("text")))
            .collect(),
        _ => Vec::new(),
    }
}

fn claude_blocks(value: Option<&Value>) -> (Vec<String>, Vec<String>, bool) {
    let mut texts = Vec::new();
    let mut tools = Vec::new();
    let mut tool_result = false;
    match value {
        Some(Value::String(text)) => texts.extend(util::clean_text(Some(text))),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => texts.extend(string(block.get("text"))),
                    Some("tool_use") => tools.extend(string(block.get("name"))),
                    Some("tool_result") => tool_result = true,
                    _ => {}
                }
            }
        }
        _ => {}
    }
    (texts, tools, tool_result)
}

fn infer_provider(model: &str) -> Option<String> {
    if model.starts_with("claude") {
        Some("anthropic".to_owned())
    } else if model.starts_with("gpt") || model.starts_with('o') {
        Some("openai".to_owned())
    } else if model.starts_with("grok") {
        Some("xai".to_owned())
    } else {
        None
    }
}

fn update_dates(start: &mut Option<String>, end: &mut Option<String>, timestamp: Option<&str>) {
    let Some(timestamp) = timestamp else {
        return;
    };
    if start.as_deref().is_none_or(|current| timestamp < current) {
        *start = Some(timestamp.to_owned());
    }
    if end.as_deref().is_none_or(|current| timestamp > current) {
        *end = Some(timestamp.to_owned());
    }
}

fn mark_last_final(events: &mut [Event]) {
    if let Some(event) = events
        .iter_mut()
        .rev()
        .find(|event| event.kind == "message" && event.role.as_deref() == Some("assistant"))
    {
        event.phase.get_or_insert_with(|| "final".to_owned());
    }
}

fn millis(value: Option<i64>) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(value?)
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn file_timestamp(path: &Path) -> Option<String> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| {
            chrono::DateTime::from_timestamp(duration.as_secs() as i64, duration.subsec_nanos())
        })
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}
