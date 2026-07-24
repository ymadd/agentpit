# agentpit desktop

The primary agentpit desktop application: a live decision cockpit, full CLI configuration,
automatic updates, and the visual Workflow Studio. Release bundles contain the matching
`agentpit` CLI as a Tauri sidecar, so users install one desktop package.

![The decision cockpit — one manager, one decision at a time](../assets/dashboard-cockpit.png)

## How it works

`agentpit` appends one JSON object per event to an append-only log:

```
$XDG_STATE_HOME/agentpit/events.jsonl   (default: ~/.local/state/agentpit/events.jsonl)
```

Every dispatch emits `run_started → member_started → member_finished → run_finished`.
The dashboard ([Tauri](https://tauri.app), WKWebView on macOS) watches that file with
`notify`, rebuilds run state on each change, and pushes a snapshot to the UI.

The footer's **CLI バージョン** panel also inventories the exact agent executables on
`PATH` (with common GUI-launch fallback paths), shows their installed versions, and runs
the fixed self-update command supplied by each CLI. The frontend cannot provide a shell
command or arguments. Older Gemini builds that do not advertise `gemini update` stay
read-only so `update` cannot be mistaken for an interactive prompt.

A run is shown as **LIVE** while it has no `run_finished` event *and* its process is
still alive (checked via `kill(pid, 0)`); if the process dies mid-run it drops to
**Recent** marked `interrupted`, so nothing hangs in the live list forever.

Disable event emission entirely with `AGENTPIT_NO_EVENTS=1`.

## Run it from source

Portable installs can still place `agentpit-dashboard` next to `agentpit` and launch it with:

```bash
agentpit dashboard
```

Normal releases also publish native desktop installers. Their application bundle already contains
the CLI; `tauri.bundle.conf.json` adds the target-suffixed sidecar during the release build.

The desktop checks for a paired release after startup. With automatic updates enabled it invokes
the bundled CLI updater in the background, then offers to restart into the new desktop binary.

`agentpit dashboard` finds the binary via `AGENTPIT_DASHBOARD_BIN`, then next to the
`agentpit` executable, then `PATH` — and spawns it detached.

The repository is one Cargo workspace containing `agentpit`, `agentpit-events`, and this
desktop app. The dashboard frontend is a Vite/React bundle embedded by `tauri-build`; build it
before a package-targeted `cargo run` in a clean checkout (no `tauri-cli` is required):

```bash
npm --prefix dashboard/frontend ci
npm --prefix dashboard/frontend test
npm --prefix dashboard/frontend run build
cargo run -p agentpit-dashboard
# or, after the same frontend build: cargo build -p agentpit-dashboard --release
```

Then, in another terminal (or from Claude Code), drive agentpit and watch it update live:

```bash
agentpit ensemble "design a cache" --members gemini,claude,codex
agentpit review src/
```

## Layout

```
dashboard/
  frontend/           Vite app (React settings shell + Workflow Studio islands +
                      legacy cockpit in public/app.js); builds to frontend/dist
  src-tauri/
    src/main.rs        Tauri app: file watcher + pid liveness + `get_snapshot`
    src/state.rs       JSONL → run/member snapshot (mirrors the event wire format)
    tauri.conf.json    desktop identity/window/bundle defaults
    tauri.bundle.conf.json  release overlay that embeds the agentpit CLI sidecar
```

The dashboard deliberately mirrors the event schema as strings rather than depending on
the `agentpit` crate, so it stays a lightweight, independent consumer of the log.
