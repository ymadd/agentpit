# agentpit

Single-binary CLI that routes coding tasks across **Gemini**, **Antigravity (agy)**, **Claude Code**, **Codex**, and **OpenCode** — pick the best agent per task, fan out to several in parallel, or let `agentpit` choose for you.

![agentpit Workflow Studio — cast each agent into a role and orchestrate a model-driven workflow](assets/dashboard-studio.png)

<p align="center"><em>The desktop dashboard's <strong>Workflow Studio</strong>: cast each agent CLI into a role, design a model-driven workflow, and pin a model per agent.</em></p>

## Why

Different coding agents are good at different things: long-context reads on Gemini, refactors on Claude, adversarial review on Codex, free local models on OpenCode. `agentpit` is the dispatcher in front of them so you never have to remember which CLI to open.

- **One binary, many backends** — Rust, no runtime
- **One-shot dispatch** — `agentpit rescue "task"` picks a backend by your routing rules
- **Ensembles** — `agentpit review <target>` runs Gemini + OpenCode in parallel (configurable) and optionally synthesizes
- **Model-driven workflows** — a manager backend decomposes a goal and dispatches sub-tasks to configured **roles** (the cast), then synthesizes
- **Named workflows** — `agentpit workflow <type> "goal"` runs a saved preset; `workflow list` shows what's configured; `workflow new "<description>"` generates one for you
- **Per-agent models** — pin a model per role or backend (`--model`, `[workflow.roles.<name>].model`, `[backends.<id>].model`), flowing through both one-shot and workflows
- **Desktop dashboard** — a decision cockpit (one supervised window) plus a visual **Workflow Studio** to cast roles and design workflows
- **Auth-aware** — checks each backend's credentials before dispatching; `agentpit login <backend>` triggers the right login flow
- **Self-updating** — `agentpit update` updates the CLI and an installed desktop dashboard together
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

The same release also publishes `agentpit-dashboard-<target>.gz`. Install the
decompressed `agentpit-dashboard` binary next to `agentpit` (or anywhere on `PATH`) to
enable `agentpit dashboard` and the Agent CLI version manager.

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

agentpit review src/lib.rs            # multi-agent review (defaults: antigravity + opencode)
agentpit review src/ --members antigravity,claude,codex --aggregator claude

agentpit security-review src/auth.rs  # OWASP-style review (defaults: claude + codex)
agentpit adversarial-review src/      # hostile, evidence-demanding review (defaults: codex + antigravity)

agentpit explain src/router.rs        # antigravity-first by default
agentpit refactor src/big.rs "split into modules"

agentpit ensemble "design X" --members antigravity,claude,codex

agentpit workflow "fix the auth flow"            # model-driven workflow: a manager orchestrates roles
agentpit workflow review "check the diff"        # run a named [workflow.types.review] preset
agentpit workflow list                           # show the base workflow + every configured type
agentpit workflow new "a strict PR review flow"  # generate a workflow from a description
agentpit rescue "..." --role reviewer --model opus  # dispatch to a role, pinned to a model

agentpit status                       # config + per-backend auth state
agentpit login antigravity            # opens `agy auth login` in a terminal
agentpit dashboard                    # launch the desktop dashboard (needs the dashboard binary)
agentpit update                       # update the CLI + an installed dashboard together
```

## Desktop dashboard

`agentpit dashboard` launches a desktop app (install the `agentpit-dashboard` binary next to
`agentpit`). It has two faces:

**The decision cockpit** — one supervised window. A manager backend runs the swarm; the cockpit
surfaces *exactly one* thing that needs a human at a time, and says so when nothing does (inbox
zero). Reversible work never stops while you decide.

![The decision cockpit — one manager, one decision at a time](assets/dashboard-cockpit.png)

**The Workflow Studio** (⚙ → the node-graph above) — cast each agent CLI into a **role**, design a
model-driven workflow visually, and pin a **model per agent**. Save named workflow *types* (presets
over a shared cast) or hit **✨ 生成** to have an agent draft one from a description — everything
writes back to `~/.config/agentpit/config.toml`.

| Named workflows (types) | Per-agent roles & models |
|---|---|
| ![A named workflow type: brief, role selection, and its `agentpit workflow review` invocation](assets/dashboard-workflow-type.png) | ![A role's backend preference order and a pinned model](assets/dashboard-role-model.png) |

## Configuration

`agentpit` reads `~/.config/agentpit/config.toml` (XDG-aware). Run `agentpit config` for an interactive editor, or edit the file directly.

```toml
[default]
backend = "antigravity"
auto_route = true

[routes]
rescue   = "antigravity"
review   = "claude"
explain  = "antigravity"
refactor = "claude"

[auto_route]
long_context_threshold = 100000
long_context_backend   = "antigravity"
review_keywords        = ["review", "audit", "critique", "security"]
review_backend         = "claude"

[ensemble]
default_members = ["antigravity", "claude", "opencode"]
# aggregator    = "claude"

review_members  = ["antigravity", "opencode"]
# review_aggregator = "claude"

# Per-tool ensembles (split prompts across multiple backends)
# rescue_members   = ["antigravity", "gemini"]
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

## Workspace

The CLI, shared event schema, and Tauri desktop app share one Cargo workspace and root
`Cargo.lock`:

```bash
cargo build -p agentpit --release
cargo run -p agentpit-dashboard
```

The workspace's default members remain the CLI and shared crate, so a plain `cargo build`
does not pull the desktop/Tauri dependency graph.

## License

Apache-2.0 — see `Cargo.toml`.
