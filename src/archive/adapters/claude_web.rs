use std::{collections::BTreeSet, path::Path};

use anyhow::{Result, bail};
use serde_json::Value;

use super::{logical_id, mark_last_final, read_export_json, string};
use crate::{
    archive::model::{Session, message_event, text_event, tool_event},
    util,
};

pub fn parse(path: &Path) -> Result<Vec<Session>> {
    let documents = read_export_json(path, |name| {
        Path::new(name)
            .file_name()
            .is_some_and(|file_name| file_name == "conversations.json")
    })?;
    let mut sessions = Vec::new();
    for document in documents {
        let conversations = document
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Claude conversations.json must contain an array"))?;
        for conversation in conversations {
            if let Some(session) = parse_conversation(conversation)? {
                sessions.push(session);
            }
        }
    }
    Ok(sessions)
}

fn parse_conversation(conversation: &Value) -> Result<Option<Session>> {
    let native_id = string(conversation.get("uuid"))
        .ok_or_else(|| anyhow::anyhow!("Claude conversation has no UUID"))?;
    let messages = conversation
        .get("chat_messages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Claude conversation has no chat_messages array"))?;
    let known_ids: BTreeSet<String> = messages
        .iter()
        .filter_map(|message| string(message.get("uuid")))
        .collect();
    let mut events = Vec::new();
    if let Some(summary) = string(conversation.get("summary")) {
        events.push(text_event(
            string(conversation.get("updated_at")),
            "summary",
            "system",
            summary,
        ));
    }
    for message in messages {
        let Some(role) =
            message
                .get("sender")
                .and_then(Value::as_str)
                .and_then(|role| match role {
                    "human" => Some("user"),
                    "assistant" => Some("assistant"),
                    _ => None,
                })
        else {
            continue;
        };
        let timestamp = string(message.get("created_at"));
        let message_id = string(message.get("uuid"));
        let parent_id =
            string(message.get("parent_message_uuid")).filter(|parent| known_ids.contains(parent));
        let blocks = message
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let text = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter(|block| block.get("hidden_in_chat").and_then(Value::as_bool) != Some(true))
            .filter_map(|block| string(block.get("text")))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut identity_emitted = false;
        if let Some(text) = util::clean_text(Some(&text)) {
            let mut event = message_event(
                timestamp.clone(),
                role,
                text,
                None,
                Some("anthropic".to_owned()),
                None,
            );
            event.native_id = message_id.clone();
            event.parent_native_id = parent_id.clone();
            events.push(event);
            identity_emitted = true;
        }
        for block in blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        {
            let Some(name) = string(block.get("name")) else {
                continue;
            };
            let mut event = tool_event(timestamp.clone(), name);
            if !identity_emitted {
                event.native_id = message_id.clone();
                event.parent_native_id = parent_id.clone();
                identity_emitted = true;
            }
            events.push(event);
        }
    }
    if events.is_empty() {
        return Ok(None);
    }
    mark_last_final(&mut events);
    let title = string(conversation.get("name"))
        .or_else(|| {
            events
                .iter()
                .find(|event| event.role.as_deref() == Some("user"))
                .and_then(|event| event.text.as_deref())
                .map(|text| util::derive_title(text, &native_id))
        })
        .unwrap_or_else(|| native_id.clone());
    if native_id.is_empty() {
        bail!("Claude conversation UUID is empty");
    }
    Ok(Some(
        Session {
            schema_version: 1,
            logical_id: logical_id("claude-web", &native_id),
            source: "claude-web".to_owned(),
            native_id,
            title,
            parent_session_id: None,
            parent_event_id: None,
            started_at: string(conversation.get("created_at")),
            updated_at: string(conversation.get("updated_at")),
            cwd: None,
            git_branch: None,
            provider: Some("anthropic".to_owned()),
            models: BTreeSet::new(),
            events,
        }
        .finish(),
    ))
}
