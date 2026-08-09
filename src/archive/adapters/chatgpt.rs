use std::{
    collections::{BTreeSet, HashSet},
    path::Path,
};

use anyhow::Result;
use serde_json::{Map, Value};

use super::{is_chatgpt_conversations_name, logical_id, mark_last_final, read_export_json, string};
use crate::{
    archive::model::{Session, message_event},
    util,
};

pub fn parse(path: &Path) -> Result<Vec<Session>> {
    let documents = read_export_json(path, is_chatgpt_conversations_name)?;
    let mut sessions = Vec::new();
    for document in documents {
        let conversations = document
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("ChatGPT conversations JSON must contain an array"))?;
        for conversation in conversations {
            if let Some(session) = parse_conversation(conversation)? {
                sessions.push(session);
            }
        }
    }
    Ok(sessions)
}

fn parse_conversation(conversation: &Value) -> Result<Option<Session>> {
    let native_id = string(conversation.get("conversation_id"))
        .or_else(|| string(conversation.get("id")))
        .ok_or_else(|| anyhow::anyhow!("ChatGPT conversation has no ID"))?;
    let mapping = conversation
        .get("mapping")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("ChatGPT conversation has no mapping object"))?;
    let ordered = selected_node_ids(mapping, string(conversation.get("current_node")).as_deref());
    let conversation_model = string(conversation.get("default_model_slug"));
    let mut models = BTreeSet::new();
    if let Some(model) = &conversation_model {
        models.insert(model.clone());
    }
    let mut events = Vec::new();

    for node_id in ordered {
        let Some(node) = mapping.get(&node_id) else {
            continue;
        };
        let Some(message) = node.get("message").filter(|message| !message.is_null()) else {
            continue;
        };
        let role = message.pointer("/author/role").and_then(Value::as_str);
        let timestamp = unix_seconds(message.get("create_time"));
        let parent_id = string(node.get("parent")).filter(|parent| mapping.contains_key(parent));
        if message
            .pointer("/metadata/is_visually_hidden_from_conversation")
            .and_then(Value::as_bool)
            == Some(true)
        {
            continue;
        }
        let model = string(message.pointer("/metadata/model_slug"))
            .or_else(|| string(message.pointer("/metadata/default_model_slug")))
            .or_else(|| conversation_model.clone());
        if let Some(model) = &model {
            models.insert(model.clone());
        }

        if let Some(role @ ("user" | "assistant")) = role {
            let text = message_text(message);
            let Some(text) = util::clean_text(Some(&text)) else {
                continue;
            };
            let mut event = message_event(
                timestamp,
                role,
                text,
                None,
                Some("openai".to_owned()),
                model,
            );
            event.native_id = Some(node_id);
            event.parent_native_id = parent_id;
            event.active_branch = Some(true);
            events.push(event);
        }
    }
    if events.is_empty() {
        return Ok(None);
    }
    mark_last_final(&mut events);
    let title = string(conversation.get("title"))
        .or_else(|| {
            events
                .iter()
                .find(|event| event.role.as_deref() == Some("user"))
                .and_then(|event| event.text.as_deref())
                .map(|text| util::derive_title(text, &native_id))
        })
        .unwrap_or_else(|| native_id.clone());
    Ok(Some(
        Session {
            schema_version: 1,
            logical_id: logical_id("chatgpt", &native_id),
            source: "chatgpt".to_owned(),
            native_id,
            title,
            parent_session_id: None,
            parent_event_id: None,
            started_at: unix_seconds(conversation.get("create_time")),
            updated_at: unix_seconds(conversation.get("update_time")),
            cwd: None,
            git_branch: None,
            provider: Some("openai".to_owned()),
            models,
            events,
        }
        .finish(),
    ))
}

fn selected_node_ids(mapping: &Map<String, Value>, current_node: Option<&str>) -> Vec<String> {
    let leaf = current_node
        .filter(|id| mapping.contains_key(*id))
        .map(str::to_owned)
        .or_else(|| fallback_leaf(mapping));
    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    let mut cursor = leaf;
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            break;
        }
        selected.push(id.clone());
        cursor = mapping
            .get(&id)
            .and_then(|node| string(node.get("parent")))
            .filter(|parent| mapping.contains_key(parent));
    }
    selected.reverse();
    selected
}

fn fallback_leaf(mapping: &Map<String, Value>) -> Option<String> {
    let mut leaves: Vec<_> = mapping
        .iter()
        .filter(|(_, node)| {
            node.get("children")
                .and_then(Value::as_array)
                .is_none_or(|children| {
                    children
                        .iter()
                        .filter_map(Value::as_str)
                        .all(|child| !mapping.contains_key(child))
                })
        })
        .map(|(id, node)| {
            (
                node.pointer("/message/create_time")
                    .and_then(Value::as_f64)
                    .filter(|time| time.is_finite())
                    .unwrap_or(f64::NEG_INFINITY),
                id.clone(),
            )
        })
        .collect();
    leaves.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
    });
    leaves.pop().map(|(_, id)| id)
}

fn message_text(message: &Value) -> String {
    let Some(content) = message.get("content") else {
        return String::new();
    };
    let mut texts = Vec::new();
    if matches!(
        content.get("content_type").and_then(Value::as_str),
        Some("text" | "multimodal_text")
    ) && let Some(parts) = content.get("parts").and_then(Value::as_array)
    {
        for part in parts {
            if let Value::String(text) = part {
                texts.extend(util::clean_text(Some(text)));
            }
        }
    }
    texts.join("\n\n")
}

fn unix_seconds(value: Option<&Value>) -> Option<String> {
    let seconds = value?.as_f64()?;
    if !seconds.is_finite() {
        return None;
    }
    let mut whole = seconds.floor() as i64;
    let mut nanos = ((seconds - whole as f64) * 1_000_000_000.0).round() as u32;
    if nanos == 1_000_000_000 {
        whole = whole.checked_add(1)?;
        nanos = 0;
    }
    chrono::DateTime::from_timestamp(whole, nanos)
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}
