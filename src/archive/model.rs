use std::{collections::BTreeSet, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub schema_version: u32,
    pub logical_id: String,
    pub source: String,
    pub native_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub models: BTreeSet<String>,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub sequence: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synthetic: Option<bool>,
}

impl Session {
    pub fn finish(mut self) -> Self {
        for (sequence, event) in self.events.iter_mut().enumerate() {
            event.sequence = sequence;
        }
        self
    }
}

#[derive(Debug, Clone)]
pub struct Artifact {
    pub source: &'static str,
    pub path: PathBuf,
    pub kind: ArtifactKind,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Copy)]
pub enum ArtifactKind {
    Codex,
    Claude,
    ClaudeMemory,
    OpenCode,
    Grok,
}

#[derive(Debug)]
pub struct ParsedArtifact {
    pub artifact: Artifact,
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveRef {
    pub schema_version: u32,
    pub machine_id: String,
    pub machine_name: String,
    pub source: String,
    pub native_id: String,
    pub logical_id: String,
    pub object_sha256: String,
    pub source_path: String,
    pub source_fingerprint: String,
    pub observed_at: String,
}

pub fn message_event(
    timestamp: Option<String>,
    role: &str,
    text: String,
    phase: Option<String>,
    provider: Option<String>,
    model: Option<String>,
) -> Event {
    Event {
        sequence: 0,
        timestamp,
        kind: "message".to_owned(),
        role: Some(role.to_owned()),
        phase,
        text: Some(text),
        tool_name: None,
        provider,
        model,
        synthetic: None,
    }
}

pub fn text_event(timestamp: Option<String>, kind: &str, role: &str, text: String) -> Event {
    Event {
        sequence: 0,
        timestamp,
        kind: kind.to_owned(),
        role: Some(role.to_owned()),
        phase: None,
        text: Some(text),
        tool_name: None,
        provider: None,
        model: None,
        synthetic: None,
    }
}

pub fn tool_event(timestamp: Option<String>, name: String) -> Event {
    Event {
        sequence: 0,
        timestamp,
        kind: "tool".to_owned(),
        role: Some("assistant".to_owned()),
        phase: None,
        text: None,
        tool_name: Some(name),
        provider: None,
        model: None,
        synthetic: None,
    }
}
