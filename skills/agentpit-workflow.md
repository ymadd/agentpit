---
name: agentpit-workflow
description: Run a model-driven multi-step workflow where a manager backend (claude or codex) decomposes a goal, dispatches sub-tasks to worker agents, and writes a final synthesis. Use for open-ended, multi-step tasks that benefit from dynamic orchestration rather than a single one-shot dispatch.
---

# agentpit:workflow

## When to invoke

- "Plan and execute this end-to-end across multiple agents."
- "Decompose this goal, farm the pieces out, and give me a synthesis."
- "Drive a multi-step task — let a manager model figure out the steps."

## How to invoke

```bash
agentpit workflow "<goal>" [--manager claude|codex] [--agents a,b,c] [--max-depth N] [--use-mcp]
```

- `--manager` selects the orchestrating backend (claude or codex). Defaults to
  `[workflow].manager_backend` or `[default].backend`.
- `--agents` lists the worker backends the manager may dispatch to. Defaults to all available
  backends minus the manager.
- `--max-depth` caps workflow recursion (default 3); the ceiling is enforced in Rust.
- `--use-mcp` (claude manager only) drives the manager through agentpit's MCP server instead of
  shelling out to the CLI: claude is launched with `--mcp-config` pointing at `agentpit mcp serve`
  and scoped to the `mcp__agentpit__*` tools (`dispatch_task` / `list_backends` / `run_ensemble`).
  A codex manager warns and falls back to CLI shell-out. Also settable via `[workflow].use_mcp`.

## Roles (optional casting)

`[workflow.roles.*]` fixes the CAST, not the SCRIPT: which backend plays a persona moves from
LLM whim into config, while the manager keeps improvising the decomposition. When at least one
worker role is configured, the manager's roster becomes `AVAILABLE ROLES` (name, resolved
backend, one-line persona summary) and its dispatch grammar switches to `rescue --role <name>`
(shell mode) / `dispatch_task {"role":"<name>"}` (MCP mode); `--agents` is then ignored with a
warning. With zero roles configured the legacy flat-backend roster applies and the manager
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

The reserved `manager` role configures the orchestrator itself and is never a worker dispatch
target (`--role manager` is a hard error). Manager resolution order is `--manager` >
`[workflow.roles.manager]` (first claude|codex in its list) > `[workflow].manager_backend` >
`[default].backend`; the manager role's `prompt`, when present, is injected into the
orchestrator prompt as a MANAGER PERSONA block regardless of where the backend came from.

For every other role, dispatch by name with `agentpit rescue --role <name> "<sub-task>"`
(`--role` and `--backend` are mutually exclusive — the role resolves the backend). Resolution
walks the role's `backends` preference list for the first currently-available entry (empty list
= any available backend, chosen deterministically); an unknown role name or a role with no
available backend is a hard error rather than a silent substitution. The MCP equivalent is
`mcp__agentpit__dispatch_task {"role":"<name>","task":"<sub-task>"}` (`role` and `backend` are
mutually exclusive there too), so a `--use-mcp` workflow targets roles by name the same way.

## MCP channel

`agentpit mcp serve` is a standalone stdio MCP server exposing `dispatch_task`, `list_backends`,
`run_ensemble`, and `run_workflow`. `--use-mcp` wires it into the manager automatically — you do
not run it by hand. A whole workflow CAN also be launched over MCP via the
`mcp__agentpit__run_workflow` tool (in addition to the CLI): the per-sub-task dispatch/ensemble
primitives remain available too, so an MCP client can either drive the steps itself or hand off
the entire workflow.

## Output

A leader line `[workflow manager=... depth=.../... agents=...]`, then the manager's streamed
output: its plan, per-sub-task dispatches and results, and a trailing SYNTHESIS section.

Relay verbatim.
