# agents

`agents` manages AI agent rules, skills, harness settings, normalized chat archives, and the plans archive.

It supports these harnesses:

- Claude Code
- OpenAI Codex
- Grok
- OpenCode

The CLI is one Rust binary. It does not require Python, Node.js, or a separate SQLite installation.

## Install

### Standalone

```sh
curl -fsSL https://raw.githubusercontent.com/tomagranate/agents/master/install.sh | sh
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
agents sudo
agents sudo tombook-linux
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

## Machine tickets

Use `agents sudo` to grant 12-hour sudo and 1Password tickets on a Linux machine.
The 1Password ticket can read items from the `Agents` and `Dev` vaults.

Run the command without a machine name to grant tickets on the current machine:

```sh
agents sudo
```

Pass a Tailscale SSH machine name to grant tickets remotely:

```sh
agents sudo tombook-linux
```

Use `--status` to check both tickets. Use `--revoke` to revoke both tickets.
Use `--remove` to also remove the managed sudoers rule and `.zshenv` block.

```sh
agents sudo --status tombook-linux
agents sudo --revoke tombook-linux
agents sudo --remove tombook-linux
```

The 1Password service account remains active until its 12-hour expiry.
Use the 1Password app to revoke it immediately.

## Agents archive

The archive uses a separate private Git repository. Do not store chat data in `agents-home`.

Initialize a local repository:

```sh
agents archive init --path ~/.local/share/agents/agents-archive
```

Connect an existing private remote:

```sh
agents archive init \
  --path ~/.local/share/agents/agents-archive \
  --remote git@github.com:OWNER/agents-archive.git
```

Remote archives use thin clones by default. The clone downloads metadata, but not session objects.

Use `--full` to download all objects during initialization:

```sh
agents archive init \
  --full \
  --path ~/.local/share/agents/agents-archive \
  --remote git@github.com:OWNER/agents-archive.git
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
agents-archive/
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

These local index and cache names stay unchanged for compatibility with existing machines.

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

## Plans archive

Use `agents plans` to manage implementation plans in the plans archive.

```sh
agents plans upload plan.html
agents plans ls
agents plans show 01PLAN
agents plans mv 01PLAN other-project
agents plans rm 01PLAN
agents plans search "upload limits"
agents plans open 01PLAN
agents plans projects
```

Uploads take one HTML file or one directory. Directory uploads use `index.html` as the entry file.
Use `--entry` to name a different entry file. Use `--replace <id>` to overwrite the content of one plan.

The command infers the project from the Git remote, or from the current directory name.
Use `--project <slug>` to name the project. Use `--no-project` to store the plan without one.

Use `agents media` to manage public media in the same archive.

```sh
agents media put shot.png
agents media ls
agents media rm 01MEDIA
```

The default endpoint is `https://plans.tomagranate.com`. Caddy on the tailnet adds the service credential.

Set a different endpoint in `~/.config/agents/plans.toml`:

```toml
endpoint = "https://plans.example.com"
```

Use `--endpoint` to override the configured endpoint for one command.
Set `AGENTS_PLANS_TOKEN` to send a bearer token. This is only needed to reach the service without Caddy.

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
