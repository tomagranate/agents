use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use toml_edit::DocumentMut;

use crate::{config::Paths, util};

const HARNESSES: [&str; 4] = ["claude", "codex", "grok", "opencode"];

const CLAUDE_KEYS: &[&str] = &[
    "$schema",
    "permissions",
    "autoMode",
    "model",
    "availableModels",
    "effortLevel",
    "theme",
    "attribution",
    "includeCoAuthoredBy",
    "cleanupPeriodDays",
    "autoMemoryEnabled",
    "respectGitignore",
    "outputStyle",
    "language",
    "autoUpdatesChannel",
    "agentPushNotifEnabled",
    "preferredNotifChannel",
    "terminalProgressBarEnabled",
    "skipWorkflowUsageWarning",
    "spinnerTipsEnabled",
    "spinnerVerbs",
    "autoScrollEnabled",
    "showTurnDuration",
    "syntaxHighlightingDisabled",
    "fastModePerSessionOptIn",
    "enabledPlugins",
    "disableAllHooks",
    "alwaysThinkingEnabled",
    "teammateMode",
    "includeGitInstructions",
    "companyAnnouncements",
    "sandbox",
    "worktree",
];

const CODEX_KEYS: &[&str] = &[
    "approvals_reviewer",
    "sandbox_mode",
    "approval_policy",
    "model",
    "model_reasoning_effort",
    "model_verbosity",
    "plan_mode_reasoning_effort",
    "review_model",
    "service_tier",
    "personality",
    "web_search",
    "disable_response_storage",
    "allow_login_shell",
    "features",
    "plugins",
];

const GROK_KEYS: &[&str] = &[
    "ui",
    "permission",
    "terminal",
    "model",
    "reasoning_effort",
    "sandbox",
    "memory",
    "plugins",
];

const OPENCODE_KEYS: &[&str] = &[
    "$schema",
    "model",
    "small_model",
    "default_agent",
    "permission",
    "agent",
    "autoupdate",
    "share",
    "plugin",
    "compaction",
    "watcher",
    "snapshot",
    "experimental",
    "disabled_providers",
    "enabled_providers",
    "username",
];

#[derive(Clone, Copy)]
enum Format {
    Json,
    Toml,
}

struct Spec {
    harness: &'static str,
    source: PathBuf,
    target: PathBuf,
    state: PathBuf,
    format: Format,
    keys: &'static [&'static str],
}

fn spec(paths: &Paths, harness: &str) -> Result<Spec> {
    let (harness, source_name, target, format, keys) = match harness {
        "claude" => (
            "claude",
            "settings.json",
            paths.claude_settings.clone(),
            Format::Json,
            CLAUDE_KEYS,
        ),
        "codex" => (
            "codex",
            "config.toml",
            paths.codex_settings.clone(),
            Format::Toml,
            CODEX_KEYS,
        ),
        "grok" => (
            "grok",
            "config.toml",
            paths.grok_settings.clone(),
            Format::Toml,
            GROK_KEYS,
        ),
        "opencode" => (
            "opencode",
            "opencode.jsonc",
            paths.opencode_jsonc.clone(),
            Format::Json,
            OPENCODE_KEYS,
        ),
        _ => bail!("unknown harness: {harness} (claude|codex|grok|opencode)"),
    };
    let extension = match format {
        Format::Json => "json",
        Format::Toml => "toml",
    };
    Ok(Spec {
        harness,
        source: paths.harness_dir(harness).join(source_name),
        target,
        state: paths
            .state_dir
            .join("settings")
            .join(format!("{harness}.{extension}")),
        format,
        keys,
    })
}

pub fn initialize(paths: &Paths) -> Result<()> {
    for harness in HARNESSES {
        initialize_one(&spec(paths, harness)?)?;
    }
    Ok(())
}

fn initialize_one(spec: &Spec) -> Result<()> {
    if spec.source.is_file() {
        validate_source(spec)?;
        return Ok(());
    }
    match spec.format {
        Format::Json => {
            let target = read_json(&spec.target)?;
            let mut source = filter_json(&target, spec.keys);
            if spec.harness == "opencode" && !source.contains_key("$schema") {
                source.insert(
                    "$schema".to_owned(),
                    Value::String("https://opencode.ai/config.json".to_owned()),
                );
            }
            write_json(&spec.source, &source)?;
        }
        Format::Toml => {
            let target = read_toml(&spec.target)?;
            write_toml(&spec.source, &filter_toml(&target, spec.keys))?;
        }
    }
    println!(
        "  captured {} settings into {}",
        spec.harness,
        spec.source.display()
    );
    Ok(())
}

pub fn capture(paths: &Paths) -> Result<()> {
    if !paths.agents_home.join(".git").is_dir() {
        bail!("agents home is not configured; run agents init, then connect its Git remote")
    }
    let mut changed = 0;
    for harness in HARNESSES {
        let spec = spec(paths, harness)?;
        if !spec.source.is_file() {
            initialize_one(&spec)?;
            changed += 1;
            continue;
        }
        validate_source(&spec)?;
        if !spec.state.is_file() {
            continue;
        }
        changed += match spec.format {
            Format::Json => capture_json(&spec)?,
            Format::Toml => capture_toml(&spec)?,
        };
    }
    if changed == 0 {
        println!("Harness settings are unchanged.");
    } else {
        println!("Captured settings changes for {changed} harness(es).");
    }
    Ok(())
}

fn capture_json(spec: &Spec) -> Result<usize> {
    let current = filter_json(&read_json(&spec.target)?, spec.keys);
    let base = filter_json(&read_json(&spec.state)?, spec.keys);
    let mut source = read_json(&spec.source)?;
    let before = source.clone();
    for key in spec.keys {
        let local = current.get(*key);
        let previous = base.get(*key);
        let repository = source.get(*key);
        let local_changed = local != previous;
        let repository_changed = repository != previous;
        if local_changed && repository_changed && local != repository {
            bail!(
                "{} setting {key} changed both locally and in agents-home; reconcile {}",
                spec.harness,
                spec.source.display()
            )
        }
        if local_changed {
            if let Some(value) = local {
                source.insert((*key).to_owned(), value.clone());
            } else {
                source.remove(*key);
            }
        }
    }
    if source == before {
        return Ok(0);
    }
    write_json(&spec.source, &source)?;
    Ok(1)
}

fn capture_toml(spec: &Spec) -> Result<usize> {
    let current = filter_toml(&read_toml(&spec.target)?, spec.keys);
    let base = filter_toml(&read_toml(&spec.state)?, spec.keys);
    let mut source = read_toml(&spec.source)?;
    let current_values = toml_values(&current)?;
    let base_values = toml_values(&base)?;
    let source_values = toml_values(&source)?;
    let mut changed = false;
    for key in spec.keys {
        let local = current_values.get(*key);
        let previous = base_values.get(*key);
        let repository = source_values.get(*key);
        let local_changed = local != previous;
        let repository_changed = repository != previous;
        if local_changed && repository_changed && local != repository {
            bail!(
                "{} setting {key} changed both locally and in agents-home; reconcile {}",
                spec.harness,
                spec.source.display()
            )
        }
        if !local_changed {
            continue;
        }
        changed = true;
        if let Some(item) = current.get(key) {
            source.insert(key, item.clone());
        } else {
            source.remove(key);
        }
    }
    if !changed {
        return Ok(0);
    }
    write_toml(&spec.source, &source)?;
    Ok(1)
}

pub fn apply(paths: &Paths) -> Result<()> {
    initialize(paths)?;
    println!("Applying harness settings...");
    for harness in HARNESSES {
        let spec = spec(paths, harness)?;
        validate_source(&spec)?;
        match spec.format {
            Format::Json => apply_json(paths, &spec)?,
            Format::Toml => apply_toml(&spec)?,
        }
        println!("  merged {}", spec.target.display());
    }
    println!("Harness settings applied.");
    Ok(())
}

fn apply_json(paths: &Paths, spec: &Spec) -> Result<()> {
    let source = read_json(&spec.source)?;
    let mut target = read_json(&spec.target)?;
    for key in spec.keys {
        target.remove(*key);
    }
    for (key, value) in source {
        target.insert(key, value);
    }
    if spec.harness == "opencode" {
        target.insert(
            "instructions".to_owned(),
            Value::Array(vec![
                Value::String(paths.shared_md.display().to_string()),
                Value::String(paths.harness_md("opencode").display().to_string()),
            ]),
        );
    }
    write_json(&spec.target, &target)?;
    write_json(&spec.state, &filter_json(&target, spec.keys))
}

fn apply_toml(spec: &Spec) -> Result<()> {
    let source = read_toml(&spec.source)?;
    let mut target = read_toml(&spec.target)?;
    for key in spec.keys {
        target.remove(key);
    }
    for key in spec.keys {
        if let Some(item) = source.get(key) {
            target.insert(key, item.clone());
        }
    }
    write_toml(&spec.target, &target)?;
    write_toml(&spec.state, &filter_toml(&target, spec.keys))
}

pub fn show(paths: &Paths, harness: Option<&str>) -> Result<()> {
    if let Some(harness) = harness {
        let spec = spec(paths, harness)?;
        println!("{} settings", display_name(spec.harness));
        println!("Source: {}", spec.source.display());
        println!("Installed: {}", spec.target.display());
        if !spec.source.is_file() {
            println!("State: not managed");
            println!("Run: agents init");
            return Ok(());
        }
        validate_source(&spec)?;
        println!("State: {}", state(&spec)?);
        println!();
        print!("{}", fs::read_to_string(&spec.source)?);
        return Ok(());
    }

    if !paths.agents_home.join(".git").is_dir() {
        println!("Agents home: not initialized. Run: agents init");
    }
    println!("{:<12} {:<14} {:<8} SOURCE", "HARNESS", "STATE", "SETTINGS");
    for harness in HARNESSES {
        let spec = spec(paths, harness)?;
        if !spec.source.is_file() {
            println!(
                "{:<12} {:<14} {:<8} {}",
                display_name(harness),
                "not managed",
                0,
                spec.source.display()
            );
            continue;
        }
        validate_source(&spec)?;
        println!(
            "{:<12} {:<14} {:<8} {}",
            display_name(harness),
            state(&spec)?,
            managed_count(&spec)?,
            spec.source.display()
        );
    }
    Ok(())
}

pub fn status_line(paths: &Paths, harness: &str) -> Result<String> {
    let spec = spec(paths, harness)?;
    if !spec.source.is_file() {
        return Ok("not managed".to_owned());
    }
    Ok(format!(
        "{} ({} settings)",
        state(&spec)?,
        managed_count(&spec)?
    ))
}

fn state(spec: &Spec) -> Result<&'static str> {
    if !spec.target.is_file() {
        return Ok("not installed");
    }
    let matches = match spec.format {
        Format::Json => {
            read_json(&spec.source)? == filter_json(&read_json(&spec.target)?, spec.keys)
        }
        Format::Toml => {
            toml_values(&read_toml(&spec.source)?)?
                == toml_values(&filter_toml(&read_toml(&spec.target)?, spec.keys))?
        }
    };
    Ok(if matches { "synced" } else { "drifted" })
}

fn managed_count(spec: &Spec) -> Result<usize> {
    match spec.format {
        Format::Json => Ok(read_json(&spec.source)?.len()),
        Format::Toml => Ok(read_toml(&spec.source)?.iter().count()),
    }
}

fn validate_source(spec: &Spec) -> Result<()> {
    let allowed = spec.keys.iter().copied().collect::<BTreeSet<_>>();
    let keys = match spec.format {
        Format::Json => read_json(&spec.source)?.keys().cloned().collect::<Vec<_>>(),
        Format::Toml => read_toml(&spec.source)?
            .iter()
            .map(|(key, _)| key.to_owned())
            .collect::<Vec<_>>(),
    };
    for key in keys {
        if !allowed.contains(key.as_str()) {
            bail!(
                "{} settings contain unmanaged key {key}; remove it from {} or add adapter support",
                spec.harness,
                spec.source.display()
            )
        }
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Map<String, Value>> {
    if !path.is_file() {
        return Ok(Map::new());
    }
    let text = fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = json5::from_str(&text)
        .with_context(|| format!("invalid JSON settings in {}", path.display()))?;
    value
        .as_object()
        .cloned()
        .with_context(|| format!("settings root must be an object in {}", path.display()))
}

fn write_json(path: &Path, value: &Map<String, Value>) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    util::atomic_write(path, text.as_bytes())
}

fn filter_json(value: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|key| {
            value
                .get(*key)
                .cloned()
                .map(|value| ((*key).to_owned(), value))
        })
        .collect()
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
        .with_context(|| format!("invalid TOML settings in {}", path.display()))
}

fn write_toml(path: &Path, value: &DocumentMut) -> Result<()> {
    util::atomic_write(path, value.to_string().as_bytes())
}

fn filter_toml(value: &DocumentMut, keys: &[&str]) -> DocumentMut {
    let mut filtered = DocumentMut::new();
    for key in keys {
        if let Some(item) = value.get(key) {
            filtered.insert(key, item.clone());
        }
    }
    filtered
}

fn toml_values(value: &DocumentMut) -> Result<toml::Table> {
    if value.is_empty() {
        return Ok(toml::Table::new());
    }
    toml::from_str(&value.to_string()).context("could not normalize TOML settings")
}

fn display_name(harness: &str) -> &'static str {
    match harness {
        "claude" => "Claude",
        "codex" => "Codex",
        "grok" => "Grok",
        "opencode" => "OpenCode",
        _ => "Unknown",
    }
}
