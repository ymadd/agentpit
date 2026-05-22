# agentpit

Single-binary CLI that routes coding tasks across **Gemini**, **Antigravity (agy)**, **Claude Code**, **Codex**, and **OpenCode** — pick the best agent per task, fan out to several in parallel, or let `agentpit` choose for you.

![demo](assets/demo.gif)

## Why

Different coding agents are good at different things: long-context reads on Gemini, refactors on Claude, adversarial review on Codex, free local models on OpenCode. `agentpit` is the dispatcher in front of them so you never have to remember which CLI to open.

- **One binary, many backends** — Rust, no runtime
- **One-shot dispatch** — `agentpit rescue "task"` picks a backend by your routing rules
- **Ensembles** — `agentpit review <target>` runs Gemini + OpenCode in parallel (configurable) and optionally synthesizes
- **Auth-aware** — checks each backend's credentials before dispatching; `agentpit login <backend>` triggers the right login flow
- **Self-updating** — `agentpit update` pulls the latest release from GitHub
- **Discoverable** — running `agentpit` with no args opens an interactive menu

## Install

### From GitHub Releases (recommended)

```bash
# macOS arm64 example — adjust target for your platform
curl -L https://github.com/ymadd/agentpit/releases/latest/download/agentpit-aarch64-apple-darwin.gz \
  | gunzip > agentpit
chmod +x agentpit
mv agentpit ~/.local/bin/
agentpit --version
```

Targets published: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`.

> Releases ship plain gzipped binaries (not `.tar.gz`) because `self_update`'s tar entry-name matching is fragile across BSD/GNU tar — plain `.gz` lets the library stream decompress straight to the target path. `agentpit update` from v0.1.5 onward works against this format with no changes.

### From source

```bash
cargo install --git https://github.com/ymadd/agentpit
```

### Install Claude Code commands + skills

```bash
agentpit init           # interactive picker: project (./.claude/) or user (~/.claude/)
agentpit init --scope user
```

## Backends

| Backend | CLI on PATH | Default transport | Auth check |
|---|---|---|---|
| `gemini` | `gemini` | exec | `~/.gemini/oauth_creds.json` |
| `antigravity` (alias `agy`) | `agy` | exec | `~/.gemini/oauth_creds.json` (shared with Gemini CLI) |
| `claude` | `claude` | exec | `~/.claude.json` |
| `codex` | `codex` | exec | `codex login status` |
| `opencode` | `~/.opencode/bin/opencode` | acp | binary present |

Each backend's transport (`exec` per-request or `acp` persistent session) can be overridden in `config.toml`.

### Installing the backends

- **Gemini CLI** — `npm i -g @google/gemini-cli` (deprecated June 18 2026)
- **Antigravity CLI** — `curl -fsSL https://antigravity.google/cli/install.sh | bash` (Gemini CLI's successor; Go binary)
- **Claude Code** — install per [Anthropic docs](https://docs.claude.com/claude-code)
- **Codex** — `npm i -g @openai/codex`
- **OpenCode** — `curl -fsSL https://opencode.ai/install | bash`

## Usage

```bash
agentpit                              # interactive menu

agentpit rescue "list files in src/" # one-shot
agentpit rescue "..." --backend agy   # force a backend

agentpit review src/lib.rs            # multi-agent review (defaults: gemini + opencode)
agentpit review src/ --members gemini,antigravity,claude --aggregator claude

agentpit explain src/router.rs        # gemini-first by default
agentpit refactor src/big.rs "split into modules"

agentpit ensemble "design X" --members gemini,antigravity,claude

agentpit status                       # config + per-backend auth state
agentpit login antigravity            # opens `agy auth login` in a terminal
agentpit update                       # check + self-replace from GitHub releases
```

## Configuration

`agentpit` reads `~/.config/agentpit/config.toml` (XDG-aware). Run `agentpit config` for an interactive editor, or edit the file directly.

```toml
[default]
backend = "gemini"
auto_route = true

[routes]
rescue   = "gemini"
review   = "claude"
explain  = "gemini"
refactor = "claude"

[auto_route]
long_context_threshold = 100000
long_context_backend   = "gemini"
review_keywords        = ["review", "audit", "critique", "security"]
review_backend         = "claude"

[ensemble]
default_members = ["gemini", "claude", "opencode"]
# aggregator    = "claude"

review_members  = ["gemini", "opencode"]
# review_aggregator = "claude"

# Per-tool ensembles (split prompts across multiple backends)
# rescue_members   = ["gemini", "antigravity"]
# refactor_members = ["claude", "antigravity"]

# Per-backend transport override
# [backends.antigravity]
# transport = "exec"
```

Environment variables in string values are expanded as `${VAR}`.

### CLI config helpers

```bash
agentpit config show
agentpit config init [--force]
agentpit config backend antigravity          # interactive transport picker
agentpit config route review --backend agy   # set per-tool default
agentpit config ensemble review              # edit members + aggregator
```

## Antigravity (agy) — Gemini CLI's successor

Google announced Antigravity 2.0 at I/O 2026; **Gemini CLI free / Pro / Ultra tiers stop serving requests on 2026-06-18** and migrate to `agy`. `agentpit` ships first-class support for `agy` as a backend so you can roll forward without changing your slash commands.

Pass either name on the CLI: `--backend antigravity` or `--backend agy` (both resolve to `BackendId::Antigravity`).

```bash
# Migrate Gemini CLI plugins into Antigravity
agy plugin import gemini

# Route Gemini-shaped tasks to agy globally
agentpit config route rescue --backend antigravity
agentpit config route explain --backend antigravity
```

Notes:

- Exec spec defaults to `agy --dangerously-skip-permissions --print <task>`. **`--dangerously-skip-permissions` skips per-action approval prompts**, so `agy` can edit / delete files without confirmation. Same stance as the Gemini exec spec — drop the flag in `src/exec/antigravity.rs` if you want explicit confirmation.
- If `agy`'s non-interactive flag changes, edit the same file.
- ACP transport is **not yet wired** for `agy` — once Google publishes an `--acp` mode equivalent we'll add it.
- Auth is OAuth on first run of `agy`. For headless boxes use `agy auth login`.

## How auto-routing works

When `default.auto_route = true` (the default), `agentpit`:

1. Honors an explicit `--backend` flag
2. Else uses the per-tool `[routes]` entry
3. Else, if the prompt is huge (`> long_context_threshold` chars), sends it to `long_context_backend`
4. Else, if the prompt contains a review keyword, sends it to `review_backend`
5. Else, falls back to `default.backend`

## License

Apache-2.0 — see `Cargo.toml`.
