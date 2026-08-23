use std::{collections::BTreeSet, fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use crate::{config::Paths, util};

const MCP_TEMPLATE: &str = include_str!("../share/templates/mcp.toml");

#[derive(Debug, Deserialize)]
struct Catalog {
    #[serde(default)]
    servers: Vec<Server>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Server {
    id: String,
    url: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
}

pub fn ensure_catalog(paths: &Paths) -> Result<()> {
    if paths.shared_mcp.is_file() {
        return Ok(());
    }
    util::atomic_write(&paths.shared_mcp, MCP_TEMPLATE.as_bytes())?;
    println!("  wrote {}", paths.shared_mcp.display());
    Ok(())
}

pub fn apply(paths: &Paths) -> Result<()> {
    ensure_catalog(paths)?;
    let servers = load_catalog(&paths.shared_mcp)?;
    if servers.is_empty() {
        return Ok(());
    }
    apply_claude(paths, &servers)?;
    apply_toml_harness(&paths.codex_settings, &servers)?;
    apply_toml_harness(&paths.grok_settings, &servers)?;
    apply_opencode(paths, &servers)?;
    println!("  merged {} shared MCP server(s)", servers.len());
    Ok(())
}

fn load_catalog(path: &Path) -> Result<Vec<Server>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let catalog: Catalog = toml::from_str(&text)
        .with_context(|| format!("invalid MCP catalog in {}", path.display()))?;
    let mut seen = BTreeSet::new();
    for server in &catalog.servers {
        validate_server(server)?;
        if !seen.insert(server.id.clone()) {
            bail!("duplicate MCP server id '{}'", server.id);
        }
    }
    Ok(catalog.servers)
}

fn validate_server(server: &Server) -> Result<()> {
    if !is_server_id(&server.id) {
        bail!(
            "MCP server id '{}' must be 2-64 characters of lowercase letters, digits, and hyphens",
            server.id
        );
    }
    let has_url = server.url.as_ref().is_some_and(|value| !value.is_empty());
    let has_command = server
        .command
        .as_ref()
        .is_some_and(|value| !value.is_empty());
    if has_url == has_command {
        bail!(
            "MCP server '{}' must set exactly one of url or command",
            server.id
        );
    }
    if let Some(url) = &server.url {
        if !url.starts_with("https://") {
            bail!("MCP server '{}' url must start with https://", server.id);
        }
        if !server.args.is_empty() {
            bail!("MCP server '{}' url entries cannot set args", server.id);
        }
    }
    if let Some(command) = &server.command
        && (command.contains('/') || command.contains('\\'))
    {
        bail!(
            "MCP server '{}' command must be a bare executable name",
            server.id
        );
    }
    for arg in &server.args {
        if looks_like_secret(arg) {
            bail!(
                "MCP server '{}' args cannot contain connection strings or secrets",
                server.id
            );
        }
    }
    Ok(())
}

fn is_server_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (2..=64).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn looks_like_secret(arg: &str) -> bool {
    arg.contains("://") || arg.contains("TOKEN=") || arg.contains("SECRET=")
}

fn apply_claude(paths: &Paths, servers: &[Server]) -> Result<()> {
    let mut root = read_json_object(&paths.claude_json)?;
    let mcp = root
        .entry("mcpServers".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let map = mcp.as_object_mut().with_context(|| {
        format!(
            "{} mcpServers must be an object",
            paths.claude_json.display()
        )
    })?;
    for server in servers {
        upsert_json_server(map, server, false);
    }
    write_json(&paths.claude_json, &root)
}

fn apply_opencode(paths: &Paths, servers: &[Server]) -> Result<()> {
    let mut root = read_json_object(&paths.opencode_jsonc)?;
    let mcp = root
        .entry("mcp".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let map = mcp
        .as_object_mut()
        .with_context(|| format!("{} mcp must be an object", paths.opencode_jsonc.display()))?;
    for server in servers {
        upsert_json_server(map, server, true);
    }
    write_json(&paths.opencode_jsonc, &root)
}

fn upsert_json_server(map: &mut Map<String, Value>, server: &Server, opencode: bool) {
    let entry = map
        .entry(server.id.clone())
        .or_insert_with(|| Value::Object(Map::new()));
    let object = match entry.as_object_mut() {
        Some(object) => object,
        None => {
            *entry = Value::Object(Map::new());
            entry.as_object_mut().expect("object")
        }
    };
    if let Some(url) = &server.url {
        object.remove("command");
        object.remove("args");
        if opencode {
            object.insert("type".to_owned(), Value::String("remote".to_owned()));
        }
        object.insert("url".to_owned(), Value::String(url.clone()));
        return;
    }
    object.remove("url");
    let command = server.command.as_deref().unwrap_or("");
    if opencode {
        object.insert("type".to_owned(), Value::String("local".to_owned()));
        let mut command_parts = vec![Value::String(command.to_owned())];
        command_parts.extend(server.args.iter().cloned().map(Value::String));
        object.insert("command".to_owned(), Value::Array(command_parts));
        object.remove("args");
        return;
    }
    object.insert("command".to_owned(), Value::String(command.to_owned()));
    object.insert(
        "args".to_owned(),
        Value::Array(server.args.iter().cloned().map(Value::String).collect()),
    );
}

fn apply_toml_harness(path: &Path, servers: &[Server]) -> Result<()> {
    let mut document = read_toml(path)?;
    let root = document.as_table_mut();
    let mcp = match root.entry("mcp_servers") {
        toml_edit::Entry::Occupied(entry) => entry.into_mut(),
        toml_edit::Entry::Vacant(entry) => entry.insert(Item::Table(Table::new())),
    };
    let table = mcp
        .as_table_mut()
        .with_context(|| format!("{} mcp_servers must be a table", path.display()))?;
    table.set_implicit(true);
    for server in servers {
        let item = table.entry(&server.id).or_insert(Item::Table(Table::new()));
        let entry = item.as_table_mut().with_context(|| {
            format!(
                "{} mcp_servers.{} must be a table",
                path.display(),
                server.id
            )
        })?;
        if let Some(url) = &server.url {
            entry.remove("command");
            entry.remove("args");
            entry["url"] = value(url.as_str());
            continue;
        }
        entry.remove("url");
        entry["command"] = value(server.command.as_deref().unwrap_or(""));
        let mut args = Array::new();
        for arg in &server.args {
            args.push(arg.as_str());
        }
        entry["args"] = value(args);
    }
    write_toml(path, &document)
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>> {
    if !path.is_file() {
        return Ok(Map::new());
    }
    let text = fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value =
        json5::from_str(&text).with_context(|| format!("invalid JSON in {}", path.display()))?;
    value
        .as_object()
        .cloned()
        .with_context(|| format!("JSON root must be an object in {}", path.display()))
}

fn write_json(path: &Path, value: &Map<String, Value>) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    util::atomic_write(path, text.as_bytes())
}

fn read_toml(path: &Path) -> Result<DocumentMut> {
    if !path.is_file() {
        return Ok(DocumentMut::new());
    }
    let text = fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(DocumentMut::new());
    }
    text.parse::<DocumentMut>()
        .with_context(|| format!("invalid TOML in {}", path.display()))
}

fn write_toml(path: &Path, value: &DocumentMut) -> Result<()> {
    util::atomic_write(path, value.to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(id: &str, url: Option<&str>, command: Option<&str>, args: &[&str]) -> Server {
        Server {
            id: id.to_owned(),
            url: url.map(str::to_owned),
            command: command.map(str::to_owned),
            args: args.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    #[test]
    fn rejects_env_like_args_and_paths() {
        assert!(validate_server(&server("ok", Some("https://mcp.example/mcp"), None, &[])).is_ok());
        assert!(validate_server(&server("ok", None, Some("npx"), &["-y", "pkg"])).is_ok());
        assert!(validate_server(&server("ok", None, Some("/usr/bin/npx"), &[])).is_err());
        assert!(
            validate_server(&server(
                "ok",
                None,
                Some("npx"),
                &["postgresql://user:pass@localhost/db"]
            ))
            .is_err()
        );
        assert!(
            validate_server(&server("ok", Some("http://insecure.example"), None, &[])).is_err()
        );
    }
}
