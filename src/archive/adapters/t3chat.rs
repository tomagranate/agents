use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
};

use anyhow::Result;
use serde_json::Value;

use super::{logical_id, mark_last_final, read_export_json, string};
use crate::{
    archive::model::{Session, message_event, tool_event},
    util,
};

pub fn parse(path: &Path) -> Result<Vec<Session>> {
    let document = read_export_json(path, |_| true)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("T3 Chat export is empty"))?;
    let threads = document
        .get("threads")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("T3 Chat export has no threads array"))?;
    let messages = document
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("T3 Chat export has no messages array"))?;

    let internal_thread_ids: HashMap<String, String> = threads
        .iter()
        .filter_map(|thread| {
            Some((
                string(thread.get("_id"))?,
                string(thread.get("threadId")).or_else(|| string(thread.get("id")))?,
            ))
        })
        .collect();
    let mut messages_by_thread: HashMap<String, Vec<&Value>> = HashMap::new();
    for message in messages {
        if let Some(thread_id) = string(message.get("threadId")) {
            messages_by_thread
                .entry(thread_id)
                .or_default()
                .push(message);
        }
    }

    let mut sessions = Vec::new();
    for thread in threads {
        let Some(native_id) = string(thread.get("threadId")).or_else(|| string(thread.get("id")))
        else {
            continue;
        };
        let mut thread_messages = messages_by_thread.remove(&native_id).unwrap_or_default();
        thread_messages.sort_by(|left, right| {
            number(left.get("created_at"))
                .partial_cmp(&number(right.get("created_at")))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| string(left.get("messageId")).cmp(&string(right.get("messageId"))))
        });
        let mut events = Vec::new();
        let mut models = BTreeSet::new();
        let mut providers = BTreeSet::new();
        for message in thread_messages {
            let Some(role @ ("user" | "assistant")) = message.get("role").and_then(Value::as_str)
            else {
                continue;
            };
            let timestamp = millis(number(message.get("created_at")));
            let model = string(message.get("model"));
            if let Some(model) = &model {
                models.insert(model.clone());
            }
            let provider = message_provider(message, model.as_deref());
            if let Some(provider) = &provider {
                providers.insert(provider.clone());
            }
            let message_id = string(message.get("messageId"));
            let mut identity_emitted = false;
            if let Some(text) = string(message.get("content")) {
                let mut event = message_event(timestamp.clone(), role, text, None, provider, model);
                event.native_id = message_id.clone();
                events.push(event);
                identity_emitted = true;
            }
            if let Some(parts) = message.get("parts").and_then(Value::as_array) {
                for part in parts
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("tool_call"))
                {
                    let Some(name) = string(part.get("toolName")) else {
                        continue;
                    };
                    let mut event = tool_event(timestamp.clone(), name);
                    if !identity_emitted {
                        event.native_id = message_id.clone();
                        identity_emitted = true;
                    }
                    events.push(event);
                }
            }
        }
        if events.is_empty() {
            continue;
        }
        mark_last_final(&mut events);
        let parent_session_id = string(thread.get("branchParentThreadId"))
            .and_then(|internal| internal_thread_ids.get(&internal).cloned());
        let title = string(thread.get("title"))
            .or_else(|| {
                events
                    .iter()
                    .find(|event| event.role.as_deref() == Some("user"))
                    .and_then(|event| event.text.as_deref())
                    .map(|text| util::derive_title(text, &native_id))
            })
            .unwrap_or_else(|| native_id.clone());
        sessions.push(
            Session {
                schema_version: 1,
                logical_id: logical_id("t3chat", &native_id),
                source: "t3chat".to_owned(),
                native_id,
                title,
                parent_session_id,
                parent_event_id: string(thread.get("branchParentPublicMessageId")),
                started_at: millis(
                    number(thread.get("created_at")).or(number(thread.get("createdAt"))),
                ),
                updated_at: millis(
                    number(thread.get("updated_at"))
                        .or(number(thread.get("updatedAt")))
                        .or(number(thread.get("last_message_at")))
                        .or(number(thread.get("lastMessageAt"))),
                ),
                cwd: None,
                git_branch: None,
                provider: if providers.len() == 1 {
                    providers.into_iter().next()
                } else {
                    None
                },
                models,
                events,
            }
            .finish(),
        );
    }
    Ok(sessions)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64)
}

fn millis(value: Option<f64>) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(value? as i64)
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn message_provider(message: &Value, model: Option<&str>) -> Option<String> {
    let metadata = message.get("providerMetadata").and_then(Value::as_object);
    for provider in ["anthropic", "google", "openai", "openrouter"] {
        if metadata.is_some_and(|metadata| metadata.contains_key(provider)) {
            return Some(provider.to_owned());
        }
    }
    let model = model?;
    if model.starts_with("claude") {
        Some("anthropic".to_owned())
    } else if model.starts_with("gpt") || model.starts_with('o') {
        Some("openai".to_owned())
    } else if model.starts_with("gemini") {
        Some("google".to_owned())
    } else if model.starts_with("grok") {
        Some("xai".to_owned())
    } else {
        None
    }
}
