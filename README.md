# agents

Sync and inspect **global** `AGENTS.md` rules and skills across AI coding harnesses:

- [Claude Code](https://docs.anthropic.com/en/docs/claude-code)
- [OpenAI Codex](https://github.com/openai/codex)
- [Grok Build](https://grok.x.ai/)
- [OpenCode](https://opencode.ai/)

One shared rules file. Optional per-harness extras. One CLI to see what is installed and what each harness actually loads.

## Install

### One-liner

```sh
curl -fsSL https://raw.githubusercontent.com/tomagranate/agents/main/install.sh | sh
```

Options via env:

| Variable | Default | Meaning |
|----------|---------|---------|
| `INSTALL_DIR` | `~/.local/bin` | Where the `agents` binary goes |
| `SHARE_DIR` | `~/.local/share/agents` | Templates / package data |
| `VERSION` | latest release, else `main` | Tag without `v`, or `main` |
| `SKIP_INIT` | unset | Set `1` to skip scaffolding |
| `FORCE_INIT` | unset | Set `1` to overwrite scaffold files |

```sh
# install CLI only, keep existing ~/.agents as-is
SKIP_INIT=1 curl -fsSL https://raw.githubusercontent.com/tomagranate/agents/main/install.sh | sh
```

### Homebrew

```sh
brew install tomagranate/tap/agents
agents init    # first time only
```

### From source

```sh
git clone https://github.com/tomagranate/agents.git
cd agents
install -m 755 bin/agents ~/.local/bin/agents
mkdir -p ~/.local/share/agents
cp -R share/templates ~/.local/share/agents/
export AGENTS_SHARE=~/.local/share/agents
agents init
```

## Quick start

```sh
# Personal multi-machine content (recommended)
git clone git@github.com:tomagranate/agents-home.git ~/.agents
agents sync

# Or scaffold empty templates on a fresh machine
agents init

agents status    # overview
agents skills    # skill matrix
agents md        # full resolved rules per harness
agents pull      # git pull + sync (when using agents-home)
agents push -m "msg"  # commit + push agents-home
```

## Layout

```
~/.agents/
  AGENTS.md                 # shared rules (edit for all harnesses)
  harness/
    claude.md               # Claude Code only
    codex.md                # Codex only
    grok.md                 # Grok only
    opencode.md             # OpenCode only
  skills/<name>/SKILL.md    # shared skills → agents sync links into Claude/Codex
```

| Goal | Action |
|------|--------|
| Rule for every harness | Edit `~/.agents/AGENTS.md` |
| Rule for one harness | Edit `~/.agents/harness/<name>.md`, then `agents sync` |
| Shared skill | Add `~/.agents/skills/<name>/SKILL.md`, then `agents sync` |
| Harness-only skill | Put it only under that harness’s skills dir |

### Harness skill directories

| Scope | Path |
|-------|------|
| Shared | `~/.agents/skills/` |
| Claude | `~/.claude/skills/` |
| Codex | `~/.codex/skills/` |
| Grok | `~/.grok/skills/` |
| OpenCode | `~/.config/opencode/skills/` |

Grok and OpenCode also discover `~/.agents/skills` natively. Claude and Codex get symlinks from `agents sync`.

## How each harness is wired

| Harness | Mechanism |
|---------|-----------|
| **Claude** | `~/.claude/CLAUDE.md` imports `@~/.agents/AGENTS.md` and harness file |
| **Codex** | `~/.codex/AGENTS.md` composed on `sync` (shared + harness) |
| **Grok** | `~/.grok/rules/*.md` symlinks to shared + harness |
| **OpenCode** | `~/.config/opencode/opencode.jsonc` `instructions` list |

## Multi-machine content

Put personal rules/skills in a private git repo that **is** `~/.agents`
(see [agents-home](https://github.com/tomagranate/agents-home) for the intended layout).

```sh
agents pull              # git -C ~/.agents pull --ff-only && agents sync
agents push -m "Update"  # commit all + push
```

## Commands

```
agents              Status overview
agents init         Scaffold ~/.agents from templates, then sync
agents pull         Pull agents-home + sync
agents push [-m]    Push agents-home (optional commit)
agents skills       Where each skill lives
agents md [name]    Full resolved AGENTS text
agents sync         Wire entrypoints + link shared skills
agents paths        Print paths
agents version      Print version
agents help         Help
```

## Development

```sh
./bin/agents help
./bin/agents status
```

Bump `VERSION` and the `AGENTS_VERSION` string in `bin/agents` together when cutting a release.

## License

MIT
