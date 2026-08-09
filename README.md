# agents

`agents` manages shared AI agent rules, skills, and normalized chat archives.

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
  AGENTS.md
  harness/
    claude.md
    codex.md
    grok.md
    opencode.md
  skills/<name>/SKILL.md
```

Common commands:

```sh
agents status
agents init
agents sync
agents skills
agents md
agents pull
agents push -m "Update shared rules"
```

`agents sync` wires shared rules into each installed harness.

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

Ingest local history:

```sh
agents archive update
agents archive status
agents archive search "liquid glass"
agents archive verify
```

Metadata search covers every archived session. Message search covers locally available sessions.

`show` fetches one missing session and keeps it in the local cache:

```sh
agents archive show <session-id>
agents archive fetch <session-id>
```

Remove downloaded sessions and restore the thin clone:

```sh
agents archive cache clear
```

The command keeps archive metadata and the local search index. It refuses uncommitted or unpushed archive changes.

Download and index every session when full-text search needs the complete archive:

```sh
agents archive hydrate
agents archive verify --full
```

Pull, ingest, commit, and push in one operation:

```sh
agents archive sync
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
