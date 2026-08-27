use std::{
    fs,
    io::Write,
    path::Path,
    process::Command as StdCommand,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agents::sudo::{
    ensure_zshenv, remove_zshenv, require_linux, validate_machine_name, validate_user_name,
};
use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::{Connection, params};
use serde_json::json;
use tempfile::TempDir;
use tiny_http::{Header, Response, Server, StatusCode};
use walkdir::WalkDir;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

#[test]
fn package_versions_match() {
    assert_eq!(env!("CARGO_PKG_VERSION"), include_str!("../VERSION").trim());
}

fn ensure_stub_mcp_bins(home: &Path) {
    let bin = home.join(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    for name in ["agentprism-workflow", "1password-mcp"] {
        let path = bin.join(name);
        if path.exists() {
            continue;
        }
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

fn path_with_home_bin(home: &Path) -> String {
    let mut path = home.join(".local/bin").display().to_string();
    if let Ok(existing) = std::env::var("PATH") {
        path.push(':');
        path.push_str(&existing);
    }
    path
}

fn agents(home: &Path) -> Command {
    ensure_stub_mcp_bins(home);
    let mut command = Command::cargo_bin("agents").expect("agents binary");
    command
        .env("HOME", home)
        .env("AGENTS_HOME", home.join(".agents"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".state"))
        .env("PATH", path_with_home_bin(home))
        .env("GIT_AUTHOR_NAME", "Agents Test")
        .env("GIT_AUTHOR_EMAIL", "agents@example.test")
        .env("GIT_COMMITTER_NAME", "Agents Test")
        .env("GIT_COMMITTER_EMAIL", "agents@example.test");
    command
}

#[test]
fn edit_opens_agents_home_in_zed() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    let agents_home = home.join("custom-agents-home");
    let zed_log = home.join("zed.log");
    let zed = home.join(".local/bin/zed");
    fs::create_dir_all(&agents_home).unwrap();
    fs::create_dir_all(zed.parent().unwrap()).unwrap();
    fs::write(&zed, "#!/bin/sh\nprintf '%s\\n' \"$1\" > \"$ZED_LOG\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&zed, fs::Permissions::from_mode(0o755)).unwrap();
    }

    agents(home)
        .env("AGENTS_HOME", &agents_home)
        .env("ZED_LOG", &zed_log)
        .arg("edit")
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(zed_log).unwrap().trim(),
        agents_home.display().to_string()
    );
}

#[test]
fn sudo_arguments_are_clear_and_exclusive() {
    let temporary = TempDir::new().unwrap();
    agents(temporary.path())
        .args(["sudo", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[MACHINE]"))
        .stdout(predicate::str::contains("--status"))
        .stdout(predicate::str::contains("--revoke"))
        .stdout(predicate::str::contains("--remove"))
        .stdout(predicate::str::contains("--sudo-only").not());

    agents(temporary.path())
        .args(["sudo", "--status", "--revoke"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));

    agents(temporary.path())
        .args(["sudo", "remote", "--sudo-only"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--sudo-only cannot target another machine",
        ));
}

#[test]
fn sudo_resolves_a_named_machine_over_ssh() {
    let temporary = TempDir::new().unwrap();
    let ssh_log = temporary.path().join("ssh.log");
    let ssh = temporary.path().join(".local/bin/ssh");
    fs::create_dir_all(ssh.parent().unwrap()).unwrap();
    fs::write(
        &ssh,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SSH_LOG\"\ncase \"$*\" in\n  *\"uname -s\"*) printf '__AGENTS_OUTPUT_START__\\nLinux\\n1000\\n'; exit 0 ;;\nesac\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    }

    agents(temporary.path())
        .env("SSH_LOG", &ssh_log)
        .args(["sudo", "--status", "tombook-linux"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "one or more tickets are inactive on tombook-linux",
        ));

    let calls = fs::read_to_string(ssh_log).unwrap();
    assert_eq!(calls.lines().count(), 3);
    assert!(calls.lines().all(|call| call.starts_with("tombook-linux ")));
    assert!(calls.contains("agents sudo --sudo-only --status"));
}

#[test]
fn sudo_reports_missing_and_outdated_remote_agents() {
    let temporary = TempDir::new().unwrap();
    let ssh = temporary.path().join(".local/bin/ssh");
    fs::create_dir_all(ssh.parent().unwrap()).unwrap();
    fs::write(
        &ssh,
        "#!/bin/sh\ncase \"$*\" in\n  *\"uname -s\"*) printf '__AGENTS_OUTPUT_START__\\nLinux\\n1000\\n'; exit 0 ;;\n  *\"--status\"*) exit \"$REMOTE_AGENTS_EXIT\" ;;\nesac\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    }

    agents(temporary.path())
        .env("REMOTE_AGENTS_EXIT", "127")
        .args(["sudo", "--status", "tombook-linux"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("agents is not installed"))
        .stderr(predicate::str::contains("primer update"));

    agents(temporary.path())
        .env("REMOTE_AGENTS_EXIT", "2")
        .args(["sudo", "--status", "tombook-linux"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not support `agents sudo`"))
        .stderr(predicate::str::contains("primer update"));
}

#[test]
fn sudo_sends_the_remote_op_token_only_through_ssh_stdin() {
    const TOKEN: &str = "test-service-account-token";

    let temporary = TempDir::new().unwrap();
    let bin = temporary.path().join(".local/bin");
    let ssh = bin.join("ssh");
    let op = bin.join("op");
    let ssh_log = temporary.path().join("ssh.log");
    let op_log = temporary.path().join("op.log");
    let event_log = temporary.path().join("events.log");
    let ssh_stdin = temporary.path().join("ssh.stdin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        &ssh,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SSH_LOG\"\nprintf 'ssh %s\\n' \"$*\" >> \"$EVENT_LOG\"\ncase \"$*\" in\n  *\"uname -s\"*) printf 'startup noise\\n__AGENTS_OUTPUT_START__\\nLinux\\n1000\\n' ;;\n  *\"agents sudo --sudo-only\"*) exit 0 ;;\n  *\"op-ticket.XXXXXX\"*) cat > \"$SSH_STDIN\" ;;\n  *\"printf '__AGENTS_OUTPUT_START__\"*) printf 'startup noise\\n__AGENTS_OUTPUT_START__\\n' ;;\n  *\".zshenv.agents.XXXXXX\"*) cat > /dev/null ;;\n  *) exit 1 ;;\nesac\n",
    )
    .unwrap();
    fs::write(
        &op,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$OP_LOG\"\nprintf 'op %s\\n' \"$*\" >> \"$EVENT_LOG\"\nif [ -n \"${{OP_SERVICE_ACCOUNT_TOKEN:-}}\" ]; then exit 9; fi\ncase \"$1\" in\n  whoami) exit 0 ;;\n  service-account) printf '{TOKEN}\\n' ;;\n  *) exit 1 ;;\nesac\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&op, fs::Permissions::from_mode(0o755)).unwrap();
    }

    agents(temporary.path())
        .env("SSH_LOG", &ssh_log)
        .env("SSH_STDIN", &ssh_stdin)
        .env("OP_LOG", &op_log)
        .env("EVENT_LOG", &event_log)
        .env("OP_SERVICE_ACCOUNT_TOKEN", "expired-token")
        .args(["sudo", "tombook-linux"])
        .assert()
        .success()
        .stdout(predicate::str::contains(TOKEN).not())
        .stderr(predicate::str::contains(TOKEN).not());

    assert_eq!(fs::read_to_string(ssh_stdin).unwrap(), TOKEN);
    assert!(!fs::read_to_string(ssh_log).unwrap().contains(TOKEN));
    assert!(!fs::read_to_string(op_log).unwrap().contains(TOKEN));
    let events = fs::read_to_string(event_log).unwrap();
    assert!(!events.contains(TOKEN));
    let op_preflight = events.find("op whoami").unwrap();
    let sudo_leg = events.find("agents sudo --sudo-only").unwrap();
    assert!(op_preflight < sudo_leg);
}

#[test]
fn sudo_zshenv_block_is_idempotent_and_removable() {
    let temporary = TempDir::new().unwrap();
    let zshenv = temporary.path().join(".zshenv");
    let original = "export KEEP_ME=yes\n";
    fs::write(&zshenv, original).unwrap();

    ensure_zshenv(&zshenv).unwrap();
    let first = fs::read_to_string(&zshenv).unwrap();
    ensure_zshenv(&zshenv).unwrap();
    let second = fs::read_to_string(&zshenv).unwrap();

    assert_eq!(first, second);
    assert_eq!(first.matches("# >>> agents op-ticket").count(), 1);
    assert_eq!(first.matches("# <<< agents op-ticket <<<").count(), 1);
    assert!(first.contains("${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/op-ticket"));
    assert!(first.contains("export KEEP_ME=yes"));

    remove_zshenv(&zshenv).unwrap();
    assert_eq!(fs::read_to_string(zshenv).unwrap(), original);
}

#[test]
fn sudo_refuses_unsupported_targets_and_user_names() {
    assert_eq!(
        require_linux("Darwin").unwrap_err().to_string(),
        "Linux is required on the target machine"
    );
    for user in ["", "space user", "user:rule", "tøm"] {
        assert!(validate_user_name(user).is_err(), "accepted {user:?}");
    }
    for user in ["tom", "tom.agranate", "tom-agranate", "tom_agranate"] {
        validate_user_name(user).unwrap();
    }
    for machine in ["", "-oProxyCommand=bad", "machine name", "host@example"] {
        assert!(
            validate_machine_name(machine).is_err(),
            "accepted {machine:?}"
        );
    }
    for machine in ["tombook-linux", "tomputer.local", "machine_1"] {
        validate_machine_name(machine).unwrap();
    }
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

fn write_zip(path: &Path, files: &[(&str, serde_json::Value)]) {
    let mut archive = ZipWriter::new(fs::File::create(path).unwrap());
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, value) in files {
        archive.start_file(name, options).unwrap();
        archive
            .write_all(&serde_json::to_vec(value).unwrap())
            .unwrap();
    }
    archive.finish().unwrap();
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
    agents(temporary.path())
        .args(["settings", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("State: synced"));
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
        .arg("edit")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "agents home is not initialized; run agents init",
        ));
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
    agents(home)
        .arg("settings")
        .assert()
        .success()
        .stdout(predicate::str::contains("Agents home: not initialized"));
    assert!(!home.join(".state/agents/chat-archive.sqlite").exists());
    assert!(!home.join(".agents").exists());
}

#[test]
fn manages_native_settings_and_preserves_local_only_values() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();

    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(
        home.join(".claude/settings.json"),
        serde_json::to_vec_pretty(&json!({
            "permissions": {"defaultMode": "auto"},
            "effortLevel": "high",
            "env": {"ANTHROPIC_API_KEY": "local-secret"},
            "hooks": {"PreToolUse": [{"command": "/local/hook"}]}
        }))
        .unwrap(),
    )
    .unwrap();

    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        home.join(".codex/config.toml"),
        r#"approval_policy = "never"
sandbox_mode = "danger-full-access"

[features]
js_repl = true

[mcp_servers.private]
command = "/local/server"

[mcp_servers.private.env]
TOKEN = "local-secret"
"#,
    )
    .unwrap();

    fs::create_dir_all(home.join(".grok")).unwrap();
    fs::write(
        home.join(".grok/config.toml"),
        r#"[ui]
permission_mode = "auto"

[marketplace]
credential = "local-secret"
"#,
    )
    .unwrap();

    fs::create_dir_all(home.join(".config/opencode")).unwrap();
    fs::write(
        home.join(".config/opencode/opencode.jsonc"),
        r#"{
  // Portable permission policy.
  "permission": {"*": "ask", "edit": "allow"},
  "provider": {"private": {"apiKey": "local-secret"}},
  "localPreference": true,
  "instructions": ["old"]
}
"#,
    )
    .unwrap();

    agents(home).arg("init").assert().success();

    let claude_source: serde_json::Value = serde_json::from_slice(
        &fs::read(home.join(".agents/harnesses/claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(claude_source["permissions"]["defaultMode"], "auto");
    assert!(claude_source.get("env").is_none());
    assert!(claude_source.get("hooks").is_none());

    let claude_target: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join(".claude/settings.json")).unwrap()).unwrap();
    assert_eq!(claude_target["env"]["ANTHROPIC_API_KEY"], "local-secret");
    assert_eq!(
        claude_target["hooks"]["PreToolUse"][0]["command"],
        "/local/hook"
    );

    let codex_source =
        fs::read_to_string(home.join(".agents/harnesses/codex/config.toml")).unwrap();
    assert!(codex_source.contains("approval_policy = \"never\""));
    assert!(!codex_source.contains("local-secret"));
    assert!(!codex_source.contains("mcp_servers"));
    let codex_target = fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    assert!(codex_target.contains("TOKEN = \"local-secret\""));

    let grok_source = fs::read_to_string(home.join(".agents/harnesses/grok/config.toml")).unwrap();
    assert!(grok_source.contains("permission_mode = \"auto\""));
    assert!(!grok_source.contains("local-secret"));
    let grok_target = fs::read_to_string(home.join(".grok/config.toml")).unwrap();
    assert!(grok_target.contains("credential = \"local-secret\""));

    let opencode_source: serde_json::Value = serde_json::from_slice(
        &fs::read(home.join(".agents/harnesses/opencode/opencode.jsonc")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        opencode_source["$schema"],
        "https://opencode.ai/config.json"
    );
    assert_eq!(opencode_source["permission"]["edit"], "allow");
    assert!(opencode_source.get("provider").is_none());
    let opencode_target: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join(".config/opencode/opencode.jsonc")).unwrap())
            .unwrap();
    assert_eq!(
        opencode_target["provider"]["private"]["apiKey"],
        "local-secret"
    );
    assert_eq!(opencode_target["localPreference"], true);
    assert_eq!(
        opencode_target["instructions"][0],
        home.join(".agents/shared/AGENTS.md").display().to_string()
    );

    agents(home)
        .arg("settings")
        .assert()
        .success()
        .stdout(predicate::str::contains("Claude"))
        .stdout(predicate::str::contains("OpenCode"))
        .stdout(predicate::str::contains("synced"));

    assert!(home.join(".agents/shared/mcp.toml").is_file());
}

#[test]
fn applies_shared_mcp_without_removing_local_servers() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    agents(home).args(["init", "--no-apply"]).assert().success();

    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(
        home.join(".claude.json"),
        r#"{"machineID":"local-machine","mcpServers":{}}"#,
    )
    .unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        home.join(".codex/config.toml"),
        r#"[mcp_servers.private]
command = "/local/server"

[mcp_servers.private.env]
TOKEN = "local-secret"
"#,
    )
    .unwrap();
    fs::create_dir_all(home.join(".grok")).unwrap();
    fs::write(home.join(".grok/config.toml"), "[ui]\n").unwrap();
    fs::create_dir_all(home.join(".config/opencode")).unwrap();
    fs::write(
        home.join(".config/opencode/opencode.jsonc"),
        r#"{"$schema":"https://opencode.ai/config.json","permission":"allow"}"#,
    )
    .unwrap();

    agents(home)
        .args(["home", "advanced", "apply"])
        .assert()
        .success();

    let claude: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(claude["machineID"], "local-machine");
    assert_eq!(
        claude["mcpServers"]["cloudflare-api"]["url"],
        "https://mcp.cloudflare.com/mcp"
    );
    assert_eq!(claude["mcpServers"]["cloudflare-api"]["type"], "http");
    assert_eq!(
        claude["mcpServers"]["1password"]["command"],
        "1password-mcp"
    );
    assert_eq!(
        claude["mcpServers"]["agentprism-workflow"]["command"],
        "agentprism-workflow"
    );

    let codex = fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    assert!(codex.contains("TOKEN = \"local-secret\""));
    assert!(codex.contains("/local/server"));
    assert!(codex.contains("mcp.cloudflare.com"));
    assert!(codex.contains("1password-mcp"));
    let overlay = fs::read_to_string(home.join(".agents/harnesses/codex/config.toml")).unwrap();
    assert!(!overlay.contains("mcp_servers"));
    assert!(!overlay.contains("local-secret"));

    let grok = fs::read_to_string(home.join(".grok/config.toml")).unwrap();
    assert!(grok.contains("1password-mcp"));
    assert!(grok.contains("mcp.cloudflare.com"));

    let opencode: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join(".config/opencode/opencode.jsonc")).unwrap())
            .unwrap();
    assert_eq!(
        opencode["mcp"]["cloudflare-api"]["url"],
        "https://mcp.cloudflare.com/mcp"
    );
    assert_eq!(opencode["mcp"]["cloudflare-api"]["type"], "remote");
    assert_eq!(opencode["mcp"]["1password"]["type"], "local");
    assert_eq!(opencode["mcp"]["1password"]["command"][0], "1password-mcp");
    assert_eq!(
        opencode["mcp"]["agentprism-workflow"]["command"][0],
        "agentprism-workflow"
    );
}

#[test]
fn installs_npm_mcp_package_when_command_is_missing() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    agents(home).args(["init", "--no-apply"]).assert().success();
    fs::write(
        home.join(".agents/shared/mcp.toml"),
        r#"
[[servers]]
id = "installed-mcp"
command = "installed-mcp"
npm = "@example/installed-mcp"
"#,
    )
    .unwrap();
    let npm = home.join(".local/bin/npm");
    fs::write(
        &npm,
        r#"#!/bin/sh
echo "$@" >> "$HOME/npm.log"
if [ "$1" = "install" ]; then
  printf '#!/bin/sh\nexit 0\n' > "$HOME/.local/bin/installed-mcp"
  chmod +x "$HOME/.local/bin/installed-mcp"
fi
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&npm, fs::Permissions::from_mode(0o755)).unwrap();
    }

    agents(home)
        .args(["home", "advanced", "apply"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "installing @example/installed-mcp",
        ));

    let log = fs::read_to_string(home.join("npm.log")).unwrap();
    assert!(log.contains("install -g @example/installed-mcp"));
    assert!(home.join(".local/bin/installed-mcp").is_file());
}

#[test]
fn rejects_missing_mcp_command_without_npm() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    agents(home).args(["init", "--no-apply"]).assert().success();
    fs::write(
        home.join(".agents/shared/mcp.toml"),
        r#"
[[servers]]
id = "missing-bin"
command = "definitely-not-an-mcp-command"
"#,
    )
    .unwrap();
    agents(home)
        .args(["home", "advanced", "apply"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not on PATH"));
}

#[test]
fn rejects_secrets_in_shared_mcp_catalog() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    agents(home).args(["init", "--no-apply"]).assert().success();
    fs::write(
        home.join(".agents/shared/mcp.toml"),
        r#"
[[servers]]
id = "leaky"
command = "npx"
args = ["-y", "pkg"]
env = { TOKEN = "nope" }
"#,
    )
    .unwrap();
    agents(home)
        .args(["home", "advanced", "apply"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown field"));
}

#[test]
fn syncs_managed_settings_between_machines() {
    let temporary = TempDir::new().unwrap();
    let remote = temporary.path().join("agents-home.git");
    assert!(
        StdCommand::new("git")
            .args(["init", "--bare", "-b", "master"])
            .arg(&remote)
            .status()
            .unwrap()
            .success()
    );
    let machine_a = temporary.path().join("home-a");
    let machine_b = temporary.path().join("home-b");
    fs::create_dir_all(machine_a.join(".claude")).unwrap();
    fs::create_dir_all(&machine_b).unwrap();
    fs::write(
        machine_a.join(".claude/settings.json"),
        serde_json::to_vec_pretty(&json!({
            "permissions": {"defaultMode": "auto"}
        }))
        .unwrap(),
    )
    .unwrap();

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
    agents(&machine_b)
        .args(["home", "advanced", "apply"])
        .assert()
        .success();

    fs::write(
        machine_a.join(".claude/settings.json"),
        serde_json::to_vec_pretty(&json!({
            "permissions": {"defaultMode": "acceptEdits"}
        }))
        .unwrap(),
    )
    .unwrap();
    agents(&machine_a).arg("sync").assert().success();
    agents(&machine_b).arg("sync").assert().success();

    let installed: serde_json::Value =
        serde_json::from_slice(&fs::read(machine_b.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(installed["permissions"]["defaultMode"], "acceptEdits");
}

#[test]
fn captures_native_settings_changes_and_reports_conflicts() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(
        home.join(".claude/settings.json"),
        serde_json::to_vec_pretty(&json!({
            "permissions": {"defaultMode": "auto"}
        }))
        .unwrap(),
    )
    .unwrap();
    agents(home).arg("init").assert().success();

    fs::write(
        home.join(".claude/settings.json"),
        serde_json::to_vec_pretty(&json!({
            "permissions": {"defaultMode": "acceptEdits"}
        }))
        .unwrap(),
    )
    .unwrap();
    agents(home)
        .args(["home", "advanced", "capture"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Captured settings changes"));
    let captured: serde_json::Value = serde_json::from_slice(
        &fs::read(home.join(".agents/harnesses/claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(captured["permissions"]["defaultMode"], "acceptEdits");

    fs::write(
        home.join(".agents/harnesses/claude/settings.json"),
        serde_json::to_vec_pretty(&json!({
            "permissions": {"defaultMode": "plan"}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        home.join(".claude/settings.json"),
        serde_json::to_vec_pretty(&json!({
            "permissions": {"defaultMode": "bypassPermissions"}
        }))
        .unwrap(),
    )
    .unwrap();
    agents(home)
        .args(["home", "advanced", "capture"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "changed both locally and in agents-home",
        ));
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
        .stderr(predicate::str::contains(
            "agents archive has remote changes",
        ));
}

#[test]
fn home_sync_rebases_local_content_and_pushes_it() {
    let temporary = TempDir::new().unwrap();
    let remote = temporary.path().join("agents-home.git");
    assert!(
        StdCommand::new("git")
            .args(["init", "--bare", "-b", "master"])
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
fn imports_chatgpt_claude_and_t3chat_exports() {
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
            "import-test",
        ])
        .assert()
        .success();

    let chatgpt_export = home.join("chatgpt-export.zip");
    let chatgpt_conversations = json!([{
        "id": "chatgpt-1",
        "title": "ChatGPT title",
        "create_time": 1767225600.25,
        "update_time": 1767225602.75,
        "current_node": "assistant-active",
        "default_model_slug": "gpt-default",
        "mapping": {
            "root": {"id":"root", "parent":null, "children":["system"]},
            "system": {"id":"system", "parent":"root", "children":["user"], "message":{
                "author":{"role":"system"}, "content":{"content_type":"text", "parts":["secret-chatgpt-system"]}
            }},
            "user": {"id":"user", "parent":"system", "children":["hidden", "assistant-abandoned"], "message":{
                "author":{"role":"user"}, "create_time":1767225601.0,
                "content":{"content_type":"multimodal_text", "parts":["chatgpt question", {"asset_pointer":"secret-chatgpt-asset"}]}
            }},
            "hidden": {"id":"hidden", "parent":"user", "children":["tool"], "message":{
                "author":{"role":"assistant"}, "metadata":{"is_visually_hidden_from_conversation":true},
                "content":{"content_type":"text", "parts":["secret-chatgpt-hidden"]}
            }},
            "tool": {"id":"tool", "parent":"hidden", "children":["assistant-active"], "message":{
                "author":{"role":"tool"}, "content":{"content_type":"text", "parts":["secret-chatgpt-tool"]}
            }},
            "assistant-active": {"id":"assistant-active", "parent":"tool", "children":[], "message":{
                "author":{"role":"assistant"}, "create_time":1767225602.5,
                "metadata":{"model_slug":"gpt-active"},
                "content":{"content_type":"text", "parts":["chatgpt answer"]}
            }},
            "assistant-abandoned": {"id":"assistant-abandoned", "parent":"user", "children":[], "message":{
                "author":{"role":"assistant"}, "create_time":1767225603.0,
                "content":{"content_type":"text", "parts":["secret-chatgpt-abandoned"]}
            }}
        }
    }]);
    write_zip(
        &chatgpt_export,
        &[
            ("nested/chat.html", json!({})),
            ("nested/conversations.json", chatgpt_conversations),
        ],
    );
    agents(home)
        .args(["archive", "import", chatgpt_export.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Normalized 1 sessions and 2 events",
        ));

    let claude_export = home.join("claude-export.zip");
    let claude_conversations = json!([{
        "uuid":"claude-web-1", "name":"Claude title", "summary":"claude summary",
        "created_at":"2026-01-02T00:00:00Z", "updated_at":"2026-01-02T00:00:03Z",
        "chat_messages":[
            {"uuid":"claude-user", "parent_message_uuid":"root", "sender":"human", "created_at":"2026-01-02T00:00:01Z",
             "text":"secret-claude-flattened", "attachments":[{"name":"secret-claude-attachment"}],
             "content":[{"type":"text", "text":"claude web question"}, {"type":"text", "hidden_in_chat":true, "text":"secret-claude-hidden"}]},
            {"uuid":"claude-assistant", "parent_message_uuid":"claude-user", "sender":"assistant", "created_at":"2026-01-02T00:00:02Z",
             "text":"secret-claude-tool-result", "content":[
                {"type":"text", "text":"claude web answer"},
                {"type":"tool_use", "name":"web_search", "input":{"query":"secret-claude-input"}},
                {"type":"tool_result", "content":"secret-claude-result"}
             ]}
        ]
    }]);
    write_zip(
        &claude_export,
        &[
            ("nested/users.json", json!([])),
            ("nested/conversations.json", claude_conversations),
        ],
    );
    agents(home)
        .args(["archive", "import", claude_export.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Normalized 1 sessions and 4 events",
        ));

    let t3_export = home.join("threads-export.json");
    fs::write(
        &t3_export,
        serde_json::to_vec(&json!({
            "version":"test",
            "threads":[
                {"_id":"internal-parent", "threadId":"t3-parent", "title":"T3 parent", "created_at":1767398400000_i64, "updated_at":1767398402000_i64},
                {"_id":"internal-branch", "threadId":"t3-branch", "title":"T3 branch", "created_at":1767398403000_i64,
                 "branchParentThreadId":"internal-parent", "branchParentPublicMessageId":"t3-answer"}
            ],
            "messages":[
                {"threadId":"t3-parent", "messageId":"t3-user", "role":"user", "created_at":1767398400000_i64,
                 "content":"t3 question", "model":"claude-test", "providerMetadata":{"anthropic":{"secret":"secret-t3-provider"}}},
                {"threadId":"t3-parent", "messageId":"t3-answer", "role":"assistant", "created_at":1767398401000_i64,
                 "content":"t3 answer", "model":"claude-test", "parts":[
                    {"type":"reasoning", "text":"secret-t3-reasoning"},
                    {"type":"tool_call", "toolName":"search", "args":{"query":"secret-t3-args"}, "result":"secret-t3-result"}
                 ]},
                {"threadId":"t3-branch", "messageId":"t3-branch-user", "role":"user", "created_at":1767398403000_i64,
                 "content":"t3 branch question", "model":"gpt-test"}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    agents(home)
        .args(["archive", "import", t3_export.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Normalized 2 sessions and 4 events",
        ));
    agents(home)
        .args(["archive", "import", t3_export.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 sources; 0 changed"));

    agents(home)
        .args(["archive", "advanced", "verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Available objects verified: 4"))
        .stdout(predicate::str::contains("References verified: 4"));
    let machine_refs = fs::read_dir(archive.join("refs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    for source in ["chatgpt", "claude-web", "t3chat"] {
        assert!(machine_refs.join(source).is_dir());
    }
    let references = WalkDir::new(&machine_refs)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|value| value == "json")
        })
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!references.contains(home.to_string_lossy().as_ref()));
    assert!(references.contains("account-export:chatgpt"));
    assert!(references.contains("account-export:claude-web"));
    assert!(references.contains("account-export:t3chat"));
    let objects = WalkDir::new(archive.join("objects"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|value| value == "jsonl")
        })
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "chatgpt question",
        "chatgpt answer",
        "gpt-active",
        "claude web question",
        "claude web answer",
        "web_search",
        "t3 branch question",
        "t3-parent",
        "t3-answer",
    ] {
        assert!(objects.contains(expected), "missing {expected}");
    }
    for secret in [
        "secret-chatgpt-system",
        "secret-chatgpt-asset",
        "secret-chatgpt-hidden",
        "secret-chatgpt-tool",
        "secret-chatgpt-abandoned",
        "secret-claude-flattened",
        "secret-claude-attachment",
        "secret-claude-hidden",
        "secret-claude-input",
        "secret-claude-result",
        "secret-t3-provider",
        "secret-t3-reasoning",
        "secret-t3-args",
        "secret-t3-result",
    ] {
        assert!(!objects.contains(secret), "retained {secret}");
    }
}

#[test]
fn syncs_machine_owned_refs_through_one_remote() {
    let temporary = TempDir::new().unwrap();
    let remote = temporary.path().join("archive.git");
    assert!(
        StdCommand::new("git")
            .args(["init", "--bare", "-b", "master"])
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

struct RequestData {
    method: String,
    path: String,
    body: Vec<u8>,
}

struct MockServer {
    endpoint: String,
    request: Receiver<RequestData>,
}

/// Answers one request, then records it for assertions.
fn mock_server(status: u16, body: &'static str, headers: &[(&str, &str)]) -> MockServer {
    let server = Server::http("127.0.0.1:0").expect("start mock server");
    let endpoint = format!("http://{}", server.server_addr());
    let (sender, receiver) = mpsc::channel();
    let headers = headers
        .iter()
        .map(|(name, value)| {
            Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid response header")
        })
        .collect::<Vec<_>>();

    thread::spawn(move || {
        let Some(mut request) = server
            .recv_timeout(Duration::from_secs(10))
            .expect("receive mock request")
        else {
            return;
        };
        let method = request.method().to_string();
        let path = request.url().to_owned();
        let mut request_body = Vec::new();
        request
            .as_reader()
            .read_to_end(&mut request_body)
            .expect("read mock request body");

        let response = headers.iter().cloned().fold(
            Response::from_string(body).with_status_code(StatusCode(status)),
            |response, header| response.with_header(header),
        );
        request.respond(response).expect("send mock response");
        sender
            .send(RequestData {
                method,
                path,
                body: request_body,
            })
            .expect("record mock request");
    });

    MockServer {
        endpoint,
        request: receiver,
    }
}

#[test]
fn plans_upload_posts_multipart_and_prints_the_plan_url() {
    let home = TempDir::new().unwrap();
    let project_dir = home.path().join("agents");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("plan.html"), "<h1>Ship it</h1>").unwrap();
    let server = mock_server(
        201,
        r#"{"plan":{"id":"01PLAN","title":"Ship it"}}"#,
        &[("Content-Type", "application/json")],
    );

    agents(home.path())
        .current_dir(&project_dir)
        .args([
            "plans",
            "upload",
            "plan.html",
            "--endpoint",
            &server.endpoint,
        ])
        .assert()
        .success()
        .stdout(format!("{}/plans/01PLAN\n", server.endpoint));

    let request = server.request.recv().unwrap();
    let body = String::from_utf8_lossy(&request.body);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/plans");
    assert!(body.contains("name=\"files\""));
    assert!(body.contains("filename=\"plan.html\""));
    assert!(body.contains("<h1>Ship it</h1>"));
    assert!(body.contains("name=\"project\""));
    assert!(body.contains("agents"));
}

#[test]
fn plans_upload_uses_the_api_location_when_present() {
    let home = TempDir::new().unwrap();
    let file = home.path().join("plan.html");
    fs::write(&file, "<h1>Plan</h1>").unwrap();
    let server = mock_server(
        201,
        r#"{"plan":{"id":"01PLAN"}}"#,
        &[
            ("Content-Type", "application/json"),
            ("Location", "/custom/01PLAN"),
        ],
    );

    agents(home.path())
        .args([
            "plans",
            "--endpoint",
            &server.endpoint,
            "upload",
            file.to_str().unwrap(),
            "--no-project",
        ])
        .assert()
        .success()
        .stdout(format!("{}/custom/01PLAN\n", server.endpoint));
}

#[test]
fn plans_upload_rejects_a_missing_entry_and_oversize_input() {
    let home = TempDir::new().unwrap();
    let incomplete = home.path().join("incomplete");
    fs::create_dir(&incomplete).unwrap();
    fs::write(incomplete.join("other.html"), "<p>Other</p>").unwrap();

    agents(home.path())
        .args([
            "plans",
            "upload",
            incomplete.to_str().unwrap(),
            "--no-project",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "entry file is missing: index.html",
        ));

    let oversize = home.path().join("oversize");
    fs::create_dir(&oversize).unwrap();
    let file = fs::File::create(oversize.join("index.html")).unwrap();
    file.set_len(10 * 1024 * 1024 + 1).unwrap();

    agents(home.path())
        .args([
            "plans",
            "upload",
            oversize.to_str().unwrap(),
            "--no-project",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("upload exceeds the 10 MB limit"));
}

#[test]
fn plans_upload_replace_keeps_membership_without_project_flags() {
    let home = TempDir::new().unwrap();
    let project_dir = home.path().join("some-other-repo");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("plan.html"), "<p>Plan v2</p>").unwrap();
    let server = mock_server(
        200,
        r#"{"plan":{"id":"01PLAN"}}"#,
        &[("Content-Type", "application/json")],
    );

    agents(home.path())
        .current_dir(&project_dir)
        .args([
            "plans",
            "upload",
            "plan.html",
            "--replace",
            "01PLAN",
            "--endpoint",
            &server.endpoint,
        ])
        .assert()
        .success();

    let request = server.request.recv().unwrap();
    let body = String::from_utf8_lossy(&request.body);
    assert!(body.contains("name=\"replace\""));
    assert!(!body.contains("name=\"project\""));
    assert!(!body.contains("name=\"no_project\""));
}

#[test]
fn plans_upload_no_project_overrides_project_inference() {
    let home = TempDir::new().unwrap();
    let project_dir = home.path().join("inferred-project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("plan.html"), "<p>Plan</p>").unwrap();
    let server = mock_server(
        201,
        r#"{"plan":{"id":"01PLAN"}}"#,
        &[("Content-Type", "application/json")],
    );

    agents(home.path())
        .current_dir(&project_dir)
        .args([
            "plans",
            "upload",
            "plan.html",
            "--no-project",
            "--endpoint",
            &server.endpoint,
        ])
        .assert()
        .success();

    let request = server.request.recv().unwrap();
    let body = String::from_utf8_lossy(&request.body);
    assert!(body.contains("name=\"no_project\""));
    assert!(!body.contains("name=\"project\""));
}

#[test]
fn plans_ls_renders_plans() {
    let home = TempDir::new().unwrap();
    let server = mock_server(
        200,
        r#"{"plans":[{"id":"01PLAN","title":"Improve uploads","project":{"slug":"agents"},"updated_at":"2026-08-23T00:00:00Z"}]}"#,
        &[("Content-Type", "application/json")],
    );

    agents(home.path())
        .args(["plans", "ls", "--endpoint", &server.endpoint])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "01PLAN\tagents\tImprove uploads\t2026-08-23T00:00:00Z",
        ));

    let request = server.request.recv().unwrap();
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/api/plans");
}

#[test]
fn plans_endpoint_flag_overrides_the_configured_endpoint() {
    let home = TempDir::new().unwrap();
    let config_dir = home.path().join(".config/agents");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("plans.toml"), "invalid = [").unwrap();
    let server = mock_server(
        200,
        r#"{"plans":[]}"#,
        &[("Content-Type", "application/json")],
    );

    agents(home.path())
        .args([
            "plans",
            "ls",
            "--since",
            "7d",
            "--endpoint",
            &server.endpoint,
        ])
        .assert()
        .success();

    let request = server.request.recv().unwrap();
    assert_eq!(request.path, "/api/plans?since=7d");
}

#[test]
fn plans_reports_an_invalid_configured_endpoint() {
    let home = TempDir::new().unwrap();
    let config_dir = home.path().join(".config/agents");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("plans.toml"), "invalid = [").unwrap();

    agents(home.path())
        .args(["plans", "ls"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("plans.toml is invalid"));
}

#[test]
fn media_put_prints_only_the_url_on_stdout() {
    let home = TempDir::new().unwrap();
    let file = home.path().join("shot.png");
    fs::write(&file, b"png bytes").unwrap();
    let server = mock_server(
        201,
        r#"{"url":"https://media.example/01MEDIA.png","id":"01MEDIA","key":"01MEDIA.png"}"#,
        &[("Content-Type", "application/json")],
    );

    agents(home.path())
        .args([
            "media",
            "put",
            file.to_str().unwrap(),
            "--endpoint",
            &server.endpoint,
        ])
        .assert()
        .success()
        .stdout("https://media.example/01MEDIA.png\n")
        .stderr(
            predicate::str::contains("id: 01MEDIA").and(predicate::str::contains("size: 9 bytes")),
        );

    let request = server.request.recv().unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/media");
}

#[test]
fn media_ls_renders_media() {
    let home = TempDir::new().unwrap();
    let server = mock_server(
        200,
        r#"{"media":[{"id":"01MEDIA","key":"01MEDIA.png","url":"https://media.example/01MEDIA.png","byte_size":9}]}"#,
        &[("Content-Type", "application/json")],
    );

    agents(home.path())
        .args(["media", "ls", "--endpoint", &server.endpoint])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "01MEDIA\t01MEDIA.png\t9\thttps://media.example/01MEDIA.png",
        ));

    let request = server.request.recv().unwrap();
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/api/media");
}
