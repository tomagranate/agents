use std::{
    fs,
    path::Path,
    process::Command as StdCommand,
    time::{SystemTime, UNIX_EPOCH},
};

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::{Connection, params};
use serde_json::json;
use tempfile::TempDir;
use walkdir::WalkDir;

#[test]
fn package_versions_match() {
    assert_eq!(env!("CARGO_PKG_VERSION"), include_str!("../VERSION").trim());
}

fn agents(home: &Path) -> Command {
    let mut command = Command::cargo_bin("agents").expect("agents binary");
    command
        .env("HOME", home)
        .env("AGENTS_HOME", home.join(".agents"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".state"))
        .env("GIT_AUTHOR_NAME", "Agents Test")
        .env("GIT_AUTHOR_EMAIL", "agents@example.test")
        .env("GIT_COMMITTER_NAME", "Agents Test")
        .env("GIT_COMMITTER_EMAIL", "agents@example.test");
    command
}

fn write_jsonl(path: &Path, rows: &[serde_json::Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let contents = rows
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    fs::write(path, format!("{contents}\n")).unwrap();
}

#[test]
fn supports_scoped_content_and_public_commands() {
    let temporary = TempDir::new().unwrap();
    agents(temporary.path())
        .args(["init", "--no-apply"])
        .assert()
        .success()
        .stdout(predicate::str::contains("embedded templates"));
    assert!(temporary.path().join(".agents/shared/AGENTS.md").is_file());
    assert!(
        temporary
            .path()
            .join(".agents/harnesses/codex/AGENTS.md")
            .is_file()
    );
    fs::create_dir_all(
        temporary
            .path()
            .join(".agents/harnesses/codex/skills/codex-only"),
    )
    .unwrap();
    fs::create_dir_all(temporary.path().join(".agents/shared/skills/codex-only")).unwrap();
    fs::write(
        temporary
            .path()
            .join(".agents/shared/skills/codex-only/SKILL.md"),
        "# Shared version\n",
    )
    .unwrap();
    fs::write(
        temporary
            .path()
            .join(".agents/harnesses/codex/skills/codex-only/SKILL.md"),
        "# Codex only\n",
    )
    .unwrap();
    agents(temporary.path()).arg("sync").assert().success();
    assert!(temporary.path().join(".claude/CLAUDE.md").is_file());
    assert!(temporary.path().join(".codex/AGENTS.md").is_file());
    assert_eq!(
        fs::read_link(temporary.path().join(".codex/skills/codex-only")).unwrap(),
        temporary
            .path()
            .join(".agents/harnesses/codex/skills/codex-only")
    );
    assert_eq!(
        fs::read_link(temporary.path().join(".claude/skills/codex-only")).unwrap(),
        temporary.path().join(".agents/shared/skills/codex-only")
    );
    agents(temporary.path()).arg("status").assert().success();
    agents(temporary.path())
        .args(["skills", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("codex-only"))
        .stdout(predicate::str::contains("harness"));
    agents(temporary.path())
        .args(["md", "codex"])
        .assert()
        .success();
    agents(temporary.path()).arg("version").assert().success();
    agents(temporary.path())
        .args(["upgrade", "--help"])
        .assert()
        .success();
}

#[test]
fn commands_are_safe_before_initialization() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    agents(home)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Agents home: not configured"));
    agents(home)
        .args(["archive", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Archive: not configured"));
    agents(home)
        .arg("sync")
        .assert()
        .failure()
        .stderr(predicate::str::contains("agents home is not configured"));
    agents(home)
        .args(["archive", "search", "anything"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("archive is not configured"));
    agents(home)
        .arg("skills")
        .assert()
        .success()
        .stdout(predicate::str::contains("Agents home: not initialized"));
    assert!(!home.join(".state/agents/chat-archive.sqlite").exists());
    assert!(!home.join(".agents").exists());
}

#[test]
fn shell_check_uses_cached_state_without_waiting_for_network() {
    let temporary = TempDir::new().unwrap();
    let state = temporary.path().join(".state/agents");
    fs::create_dir_all(&state).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::write(
        state.join("update-check.json"),
        serde_json::to_vec(&json!({
            "checked_at": now,
            "latest_cli": "9.0.0",
            "agents_home_behind": 1,
            "archive_behind": 2
        }))
        .unwrap(),
    )
    .unwrap();
    agents(temporary.path())
        .arg("_shell-check")
        .assert()
        .success()
        .stderr(predicate::str::contains("CLI 9.0.0 is available"))
        .stderr(predicate::str::contains("agents-home has remote changes"))
        .stderr(predicate::str::contains("chat archive has remote changes"));
}

#[test]
fn home_sync_rebases_local_content_and_pushes_it() {
    let temporary = TempDir::new().unwrap();
    let remote = temporary.path().join("agents-home.git");
    assert!(
        StdCommand::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&remote)
            .status()
            .unwrap()
            .success()
    );
    let machine_a = temporary.path().join("home-a");
    let machine_b = temporary.path().join("home-b");
    fs::create_dir_all(&machine_a).unwrap();
    fs::create_dir_all(&machine_b).unwrap();

    agents(&machine_a)
        .args(["init", "--no-apply"])
        .assert()
        .success();
    assert!(
        StdCommand::new("git")
            .args(["remote", "add", "origin"])
            .arg(&remote)
            .current_dir(machine_a.join(".agents"))
            .status()
            .unwrap()
            .success()
    );
    agents(&machine_a).arg("sync").assert().success();
    assert!(
        StdCommand::new("git")
            .arg("clone")
            .arg(&remote)
            .arg(machine_b.join(".agents"))
            .status()
            .unwrap()
            .success()
    );

    fs::write(
        machine_a.join(".agents/shared/AGENTS.md"),
        "# Shared from A\n",
    )
    .unwrap();
    agents(&machine_a).arg("sync").assert().success();
    fs::write(
        machine_b.join(".agents/harnesses/codex/AGENTS.md"),
        "# Codex from B\n",
    )
    .unwrap();
    agents(&machine_b).arg("sync").assert().success();

    let verification = temporary.path().join("verification");
    assert!(
        StdCommand::new("git")
            .arg("clone")
            .arg(&remote)
            .arg(&verification)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        fs::read_to_string(verification.join("shared/AGENTS.md")).unwrap(),
        "# Shared from A\n"
    );
    assert_eq!(
        fs::read_to_string(verification.join("harnesses/codex/AGENTS.md")).unwrap(),
        "# Codex from B\n"
    );
}

#[test]
fn archives_all_four_sources_incrementally() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    let archive = home.join("archive");

    agents(home)
        .args([
            "archive",
            "init",
            "--path",
            archive.to_str().unwrap(),
            "--machine",
            "test-machine",
        ])
        .assert()
        .success();

    write_jsonl(
        &home.join(".codex/sessions/2026/01/session.jsonl"),
        &[
            json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":"codex-1","cwd":"/project","model_provider":"openai"}}),
            json!({"type":"turn_context","timestamp":"2026-01-01T00:00:01Z","payload":{"model":"gpt-test"}}),
            json!({"type":"response_item","timestamp":"2026-01-01T00:00:02Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"codex alpha question"}]}}),
            json!({"type":"response_item","timestamp":"2026-01-01T00:00:03Z","payload":{"type":"function_call","name":"read_file","arguments":"secret-input"}}),
            json!({"type":"response_item","timestamp":"2026-01-01T00:00:04Z","payload":{"type":"function_call_output","output":"secret-tool-output"}}),
            json!({"type":"response_item","timestamp":"2026-01-01T00:00:05Z","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"codex answer"}]}}),
        ],
    );

    write_jsonl(
        &home.join(".claude/projects/project/claude-1.jsonl"),
        &[
            json!({"type":"user","sessionId":"claude-1","timestamp":"2026-01-02T00:00:00Z","cwd":"/project","message":{"role":"user","content":"claude question"}}),
            json!({"type":"assistant","sessionId":"claude-1","timestamp":"2026-01-02T00:00:01Z","message":{"role":"assistant","model":"claude-test","content":[{"type":"text","text":"claude answer"},{"type":"tool_use","name":"Bash","input":{"command":"secret-input"}}]}}),
            json!({"type":"user","sessionId":"claude-1","timestamp":"2026-01-02T00:00:02Z","toolUseResult":{"stdout":"secret-tool-output"},"message":{"role":"user","content":[{"type":"tool_result","content":"secret-tool-output"}]}}),
        ],
    );

    let grok_dir = home.join(".grok/sessions/%2Fproject/grok-1");
    fs::create_dir_all(&grok_dir).unwrap();
    fs::write(
        grok_dir.join("summary.json"),
        serde_json::to_vec(&json!({
            "generated_title":"Grok title",
            "created_at":"2026-01-03T00:00:00Z",
            "updated_at":"2026-01-03T00:00:02Z"
        }))
        .unwrap(),
    )
    .unwrap();
    write_jsonl(
        &grok_dir.join("chat_history.jsonl"),
        &[
            json!({"type":"system","content":"secret-system"}),
            json!({"type":"user","model":"grok-test","content":[{"type":"text","text":"grok question"}]}),
            json!({"type":"assistant","model":"grok-test","content":"grok answer"}),
            json!({"type":"tool_result","content":"secret-tool-output"}),
        ],
    );

    create_opencode(&home.join(".local/share/opencode/opencode.db"));

    agents(home)
        .args(["archive", "advanced", "ingest"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Normalized 4 sessions"))
        .stderr(predicate::str::contains("Scanning local chat history"))
        .stderr(predicate::str::contains("Normalizing 4 changed sources"))
        .stderr(predicate::str::contains("Archive update complete"))
        .stderr(predicate::str::contains("\x1b").not());
    agents(home)
        .args(["archive", "advanced", "verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Available objects verified: 4"))
        .stdout(predicate::str::contains("References verified: 4"));
    agents(home)
        .args(["archive", "search", "alpha"])
        .assert()
        .success()
        .stdout(predicate::str::contains("codex alpha question"));
    agents(home)
        .args(["archive", "advanced", "ingest"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 changed"));

    let objects = WalkDir::new(archive.join("objects"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(objects.contains("gpt-test"));
    assert!(objects.contains("claude-test"));
    assert!(objects.contains("open-model"));
    assert!(objects.contains("grok-test"));
    assert!(!objects.contains("secret-tool-output"));
    assert!(!objects.contains("secret-input"));
    assert!(!objects.contains("secret-system"));
}

#[test]
fn syncs_machine_owned_refs_through_one_remote() {
    let temporary = TempDir::new().unwrap();
    let remote = temporary.path().join("archive.git");
    assert!(
        StdCommand::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&remote)
            .status()
            .unwrap()
            .success()
    );
    let machine_a = temporary.path().join("machine-a");
    let machine_b = temporary.path().join("machine-b");
    fs::create_dir_all(&machine_a).unwrap();
    fs::create_dir_all(&machine_b).unwrap();

    agents(&machine_a)
        .args([
            "archive",
            "init",
            "--path",
            machine_a.join("archive").to_str().unwrap(),
            "--remote",
            remote.to_str().unwrap(),
            "--machine",
            "machine-a",
        ])
        .assert()
        .success();
    write_jsonl(
        &machine_a.join(".codex/sessions/2026/01/session.jsonl"),
        &[
            json!({"type":"session_meta","payload":{"id":"shared-codex","cwd":"/a"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"from machine a"}]}}),
        ],
    );
    agents(&machine_a)
        .args(["archive", "sync"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Pushed the unified archive"));

    agents(&machine_b)
        .args([
            "archive",
            "init",
            "--path",
            machine_b.join("archive").to_str().unwrap(),
            "--remote",
            remote.to_str().unwrap(),
            "--machine",
            "machine-b",
        ])
        .assert()
        .success();
    agents(&machine_b)
        .args(["archive", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Storage: thin"))
        .stdout(predicate::str::contains("Session content cached: 0 of 1"));
    let remote_reference = WalkDir::new(machine_b.join("archive/refs"))
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|value| value == "json")
        })
        .unwrap();
    let reference: serde_json::Value =
        serde_json::from_slice(&fs::read(remote_reference.path()).unwrap()).unwrap();
    assert_eq!(reference["title"], "from machine a");
    let index = Connection::open(machine_b.join(".state/agents/chat-archive.sqlite")).unwrap();
    assert_eq!(
        index
            .query_row(
                "SELECT count(*) FROM sessions_fts WHERE sessions_fts MATCH 'machine'",
                [],
                |row| row.get::<_, usize>(0),
            )
            .unwrap(),
        1
    );
    agents(&machine_b)
        .args(["archive", "search", "machine-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("metadata:"));
    let logical_id = reference["logical_id"].as_str().unwrap();
    agents(&machine_b)
        .args(["archive", "show", &logical_id[..12]])
        .assert()
        .success()
        .stdout(predicate::str::contains("from machine a"));
    let grok_dir = machine_b.join(".grok/sessions/%2Fb/grok-b");
    fs::create_dir_all(&grok_dir).unwrap();
    write_jsonl(
        &grok_dir.join("chat_history.jsonl"),
        &[json!({"type":"user","content":[{"type":"text","text":"from machine b"}]})],
    );
    agents(&machine_b)
        .args(["archive", "advanced", "ingest"])
        .assert()
        .success();
    agents(&machine_b)
        .args(["archive", "cache", "clear"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("archive has uncommitted changes"));
    agents(&machine_b)
        .args(["archive", "sync"])
        .assert()
        .success();
    assert!(
        StdCommand::new("git")
            .args([
                "-c",
                "user.name=Agents Test",
                "-c",
                "user.email=agents@example.test",
                "commit",
                "--allow-empty",
                "-m",
                "Local archive commit",
            ])
            .current_dir(machine_b.join("archive"))
            .status()
            .unwrap()
            .success()
    );
    agents(&machine_b)
        .args(["archive", "cache", "clear"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("archive has unpushed commits"));
    agents(&machine_b)
        .args(["archive", "sync"])
        .assert()
        .success();
    agents(&machine_b)
        .args(["archive", "cache", "clear"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Local session cache cleared"));
    assert!(
        !machine_b
            .join(".state/agents/chat-archive-objects")
            .exists()
    );
    assert_eq!(
        WalkDir::new(machine_b.join("archive/objects"))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .count(),
        0
    );
    agents(&machine_b)
        .args(["archive", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Storage: thin"))
        .stdout(predicate::str::contains("Session content cached: 0 of 2"));
    agents(&machine_b)
        .args(["archive", "advanced", "verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Available objects verified: 0"))
        .stdout(predicate::str::contains("Remote objects: 2"));
    agents(&machine_b)
        .args(["archive", "search", "machine-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("metadata:"));
    agents(&machine_b)
        .args(["archive", "show", &logical_id[..12]])
        .assert()
        .success()
        .stdout(predicate::str::contains("from machine a"));
    agents(&machine_a)
        .args(["archive", "sync"])
        .assert()
        .success();
    agents(&machine_a)
        .args(["archive", "advanced", "verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Available objects verified: 1"))
        .stdout(predicate::str::contains("References verified: 2"))
        .stdout(predicate::str::contains("Remote objects: 1"));
    agents(&machine_a)
        .args(["archive", "cache", "hydrate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Fetched session objects: 1"));
    agents(&machine_a)
        .args(["archive", "search", "machine b"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from [machine] [b]"));
    agents(&machine_a)
        .args(["archive", "advanced", "verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Available objects verified: 2"))
        .stdout(predicate::str::contains("References verified: 2"))
        .stdout(predicate::str::contains("Remote objects: 0"));
}

fn create_opencode(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY, title TEXT, directory TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO session VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "open-1",
                "OpenCode title",
                "/project",
                1_767_225_600_000_i64,
                1_767_225_601_000_i64
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
            params![
                "message-1",
                "open-1",
                1_767_225_600_000_i64,
                json!({"role":"user","model":{"providerID":"openai","modelID":"open-model"}})
                    .to_string()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "part-1",
                "message-1",
                "open-1",
                1_767_225_600_000_i64,
                json!({"type":"text","text":"opencode question"}).to_string()
            ],
        )
        .unwrap();
}
