use std::{fs, path::Path, process::Command as StdCommand};

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
fn preserves_legacy_commands_and_embedded_templates() {
    let temporary = TempDir::new().unwrap();
    agents(temporary.path())
        .args(["init", "--no-sync"])
        .assert()
        .success()
        .stdout(predicate::str::contains("embedded templates"));
    assert!(temporary.path().join(".agents/AGENTS.md").is_file());
    agents(temporary.path()).arg("sync").assert().success();
    assert!(temporary.path().join(".claude/CLAUDE.md").is_file());
    assert!(temporary.path().join(".codex/AGENTS.md").is_file());
    agents(temporary.path()).arg("status").assert().success();
    agents(temporary.path()).arg("skills").assert().success();
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
        .args(["archive", "update"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Normalized 4 sessions"));
    agents(home)
        .args(["archive", "verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Verified 4 objects and 4 references",
        ));
    agents(home)
        .args(["archive", "search", "alpha"])
        .assert()
        .success()
        .stdout(predicate::str::contains("codex alpha question"));
    agents(home)
        .args(["archive", "update"])
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
    let grok_dir = machine_b.join(".grok/sessions/%2Fb/grok-b");
    fs::create_dir_all(&grok_dir).unwrap();
    write_jsonl(
        &grok_dir.join("chat_history.jsonl"),
        &[json!({"type":"user","content":[{"type":"text","text":"from machine b"}]})],
    );
    agents(&machine_b)
        .args(["archive", "sync"])
        .assert()
        .success();
    agents(&machine_a)
        .args(["archive", "sync"])
        .assert()
        .success();
    agents(&machine_a)
        .args(["archive", "verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Verified 2 objects and 2 references",
        ));
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
