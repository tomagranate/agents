# agents

`agents` manages AI agent rules, skills, harness settings, and normalized chat archives.

It supports these harnesses:

- Claude Code
- OpenAI Codex
- Grok
- OpenCode

The CLI is one Rust binary. It does not require Python, Node.js, or a separate SQLite installation.

## Install

### Standalone

```sh
curl -fsSL https://raw.githubusercontent.com/tomagranate/agents/main/install.sh | sh
```

Set `INSTALL_DIR` to change the destination. The default is `~/.local/bin`.

Set `SKIP_INIT=1` to skip initial configuration.

### Homebrew

```sh
brew install tomagranate/tap/agents
agents init
```

### From source

```sh
git clone https://github.com/tomagranate/agents.git
cd agents
cargo install --path .
agents init
```

## Shared configuration

Use a private Git repository at `~/.agents` for personal rules and shared skills.

```text
~/.agents/
  shared/
    AGENTS.md
    skills/<name>/SKILL.md
  harnesses/
    claude/
      AGENTS.md
      settings.json
      skills/<name>/SKILL.md
    codex/config.toml
    grok/config.toml
    opencode/opencode.jsonc
```

Common commands:

```sh
agents status
agents init
agents sync
agents skills
agents skills codex
agents md
agents md codex
agents settings
agents settings codex
```

`agents status` and `agents archive status` fetch current remote state. Use `--offline` to use cached Git state. Use `--verbose` to show local paths.

`agents sync` fetches remote content, preserves local changes, rebases, applies the effective content, and pushes it.

Shared content applies to every harness. Harness content applies only to its named harness. A harness-specific skill replaces a shared skill with the same name.

Settings remain native to each harness. `agents` does not translate settings between harnesses.

Each settings adapter manages a safe set of portable keys. It preserves unmanaged keys in the installed configuration. Authentication, secret environment values, MCP credentials, project trust, and machine state remain local.

Portable MCP servers live in `shared/mcp.toml`. `agents sync` installs each `npm` package, checks that each command is on PATH, then upserts each server id into Claude, Codex, Grok, and OpenCode. Local-only servers and local `env` values stay in place. The catalog may contain HTTPS URLs or a bare command. It must not contain tokens, headers, or absolute paths.

The adapters manage these categories:

- Claude: permissions, auto mode, models, effort, interface, and plugin preferences.
- Codex: approvals, sandboxing, models, reasoning, features, and plugin preferences.
- Grok: permissions, models, reasoning, interface, terminal, and plugin preferences.
- OpenCode: permissions, agents, models, updates, sharing, plugins, and runtime behavior.

`agents sync` detects installed changes to managed keys. It stores those changes in agents-home before the Git update. A three-way comparison detects changes made both remotely and on the current machine.

Use `agents settings` to show settings state for every harness. Use a harness name to print its managed overlay.

Individual operations are available under `agents home advanced`:

```sh
agents home advanced pull
agents home advanced apply
agents home advanced capture
agents home advanced push
```

## Chat archive

The archive uses a separate private Git repository. Do not store chat data in `agents-home`.

Initialize a local repository:

```sh
agents archive init --path ~/.local/share/agents/chat-archive
```

Connect an existing private remote:

```sh
agents archive init \
  --path ~/.local/share/agents/chat-archive \
  --remote git@github.com:OWNER/chat-archive.git
```

Remote archives use thin clones by default. The clone downloads metadata, but not session objects.

Use `--full` to download all objects during initialization:

```sh
agents archive init \
  --full \
  --path ~/.local/share/agents/chat-archive \
  --remote git@github.com:OWNER/chat-archive.git
```

Synchronize local history:

```sh
agents archive sync
agents archive status
agents archive search "liquid glass"
```

Import account exports from ChatGPT, Claude, or T3 Chat:

```sh
agents archive import ~/Downloads/chatgpt-export.zip
agents archive import ~/Downloads/claude-export.zip
agents archive import ~/Downloads/threads-export.json
agents archive sync
```

The command detects the provider from the export content. It does not copy the raw export.

ChatGPT imports the active branch from each conversation. Claude imports message branch links.

T3 Chat imports thread branches. Reimport the same file after it changes.

Metadata search covers every archived session. Message search covers locally available sessions.

`show` fetches one missing session and keeps it in the local cache:

```sh
agents archive show <session-id>
agents archive cache fetch <session-id>
```

Remove downloaded sessions and restore the thin clone:

```sh
agents archive cache clear
```

The command keeps archive metadata and the local search index. It refuses uncommitted or unpushed archive changes.

Download and index every session when full-text search needs the complete archive:

```sh
agents archive cache hydrate
agents archive advanced verify --full
```

Individual archive operations are available under `advanced`:

```sh
agents archive advanced pull
agents archive advanced ingest
agents archive advanced reindex
agents archive advanced verify
```

### Archive layout

```text
chat-archive/
  objects/sha256/<prefix>/<hash>.jsonl
  refs/<machine-id>/<source>/<session-id>.json
  machines/<machine-id>.json
  schema/v1/
```

Session objects are immutable and content-addressed. Identical normalized sessions share one object.

Updates prune unreferenced working-tree objects. Git retains objects from committed revisions.

Each machine writes only its own references. This structure prevents most Git conflicts.

Thin clones use Git partial clone and sparse checkout. Git fetches a session object when the command requests it.

The command stores its searchable SQLite index outside the repository:

```text
~/.local/state/agents/chat-archive.sqlite
~/.local/state/agents/chat-archive-objects/
```

The first path contains the SQLite FTS5 index. The second path caches fetched session objects.

The binary includes SQLite.

The default policy retains:

- User and assistant messages.
- Readable summaries and memory.
- Model and provider identifiers.
- Tool names without tool payloads.
- Session titles, dates, projects, and branches.

The default policy excludes:

- Tool inputs and outputs.
- Terminal logs and file dumps.
- Images, audio, and binary artifacts.
- Hidden or encrypted reasoning.
- System and developer prompts.
- Authentication and token data.

Updates scan source fingerprints first. Rust workers parse changed sources in parallel.

Long archive operations show an animated terminal spinner. Redirected output uses plain progress lines.

Normalizer versions are part of each fingerprint. Schema changes can reprocess old sources safely.

Missing local histories do not delete archived sessions.

## Update the command

Use either command:

```sh
agents update
agents upgrade
```

Primer can run the hidden shell update check at startup. It reads cached state immediately and refreshes stale state in a detached process. The check reports CLI, agents-home, and archive updates without delaying the shell.

For Homebrew installations, the command uses `brew upgrade agents`.

For standalone installations, it downloads the matching release asset. It verifies the SHA-256 checksum before replacement.

Use `agents update --check` to check without installing.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
```

Release tags build binaries for macOS and Linux. GitHub Actions publishes archives and checksums.

## License

MIT
