---
description: Run a model-driven workflow: a manager backend orchestrates sub-agents
argument-hint: <goal> [--manager=claude|codex] [--agents=gemini,opencode] [--max-depth=3] [--use-mcp]
---

Parse `$ARGUMENTS`:
- Everything that is not a `--flag` / `--flag=value` is the goal.
- `--manager=<claude|codex>` overrides the orchestrating manager backend.
- `--agents=<comma-separated>` overrides the worker roster the manager may dispatch to.
- `--max-depth=<N>` sets the recursion-depth ceiling.
- `--use-mcp` routes the manager through agentpit's MCP server (claude manager only) instead of
  shelling out: claude is launched with `--mcp-config` pointing at `agentpit mcp serve` and scoped
  to the `mcp__agentpit__*` tools (`dispatch_task` / `list_backends` / `run_ensemble`). With a codex
  manager the flag warns and falls back to CLI shell-out.

Run:

```
agentpit workflow "<goal>" [--manager claude|codex] [--agents a,b,c] [--max-depth N] [--use-mcp]
```

The manager decomposes the goal, dispatches sub-tasks to worker backends, and ends with a
labelled SYNTHESIS section. The CLI streams the manager's output to stdout.
Relay verbatim.

## Roles (optional casting)

`[workflow.roles.*]` fixes which backend plays which persona (config, not LLM whim), while the
manager keeps improvising the decomposition. When at least one worker role is configured, the
`workflow` manager's roster becomes `AVAILABLE ROLES` and it dispatches by role name (`--agents`
is then ignored with a warning); the same roles are also dispatchable directly via `rescue
--role`. With zero roles configured the legacy flat `--agents` roster applies and the manager
prompt is byte-identical to before roles existed.

```toml
[workflow.roles.manager]
backends = ["claude"]              # first SUPPORTED manager (claude|codex) in the list wins
prompt   = "Prefer small, verifiable steps."

[workflow.roles.implementer]
backends = ["claude", "codex"]     # preference order; first AVAILABLE backend wins
prompt   = "You are the implementer. Write the smallest correct change with tests."

[workflow.roles.reviewer]
backends = ["codex", "antigravity"]
prompt   = "You are a strict reviewer. Critique only; do not rewrite."
```

Dispatch grammar for a configured worker role: `agentpit rescue --role <name> "<sub-task>"`
(`--role` and `--backend` are mutually exclusive), or over MCP
`mcp__agentpit__dispatch_task {"role":"<name>","task":"<sub-task>"}` (`role`/`backend` exclusive
there too). With at least one worker role configured, the workflow manager's roster becomes
AVAILABLE ROLES and it dispatches by role name; `--agents` is ignored with a warning. The
reserved `manager` role is never a dispatch target (`--role manager` is a hard error); manager
resolution is `--manager` > `[workflow.roles.manager]` (first claude|codex) >
`[workflow].manager_backend` > `[default].backend`, and its `prompt` is injected into the
orchestrator prompt as a MANAGER PERSONA block.
