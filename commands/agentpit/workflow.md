---
description: Run a model-driven workflow (optionally a named type), list the types, or generate one
argument-hint: [type|new|list] <goal> [--manager=claude|codex] [--model=<m>] [--agents=gemini,opencode] [--max-depth=3] [--use-mcp]
---

Parse `$ARGUMENTS`:
- The FIRST non-flag positional is a workflow TYPE when a SECOND positional (the goal) follows;
  with a single positional it IS the goal. So `agentpit workflow "<goal>"` runs the base
  `[workflow]`, and `agentpit workflow <type> "<goal>"` runs the `[workflow.types.<type>]` preset.
- The literal first positionals `new` (the generator) and `list` (the catalog) are reserved,
  never types.
- `--manager=<claude|codex>` overrides the orchestrating manager backend.
- `--agents=<comma-separated>` overrides the worker roster the manager may dispatch to.
- `--max-depth=<N>` sets the recursion-depth ceiling.
- `--use-mcp` routes the manager through agentpit's MCP server (claude manager only) instead of
  shelling out: claude is launched with `--mcp-config` pointing at `agentpit mcp serve` and scoped
  to the `mcp__agentpit__*` tools (`dispatch_task` / `list_backends` / `run_ensemble`). With a codex
  manager the flag warns and falls back to CLI shell-out.

Run:

```
agentpit workflow [type] "<goal>" [--manager claude|codex] [--agents a,b,c] [--max-depth N] [--use-mcp]
```

The manager decomposes the goal, dispatches sub-tasks to worker backends, and ends with a
labelled SYNTHESIS section. The CLI streams the manager's output to stdout.
Relay verbatim.

## Named workflows (`[workflow.types.<name>]`)

A workflow TYPE is a PRESET over the base `[workflow]` and the shared `[workflow.roles.*]` cast:
it picks which roles to dispatch to, gives the manager a BRIEF, and may override knobs. The cast is
never duplicated — a type only *selects* shared roles. Unset per-type fields inherit the base.

```toml
[workflow.types.review]
title    = "Strict code review"
prompt   = "Run a strict review: spec violations, boundary bugs, security."   # the manager BRIEF
roles    = ["reviewer", "security"]   # subset of the shared cast; empty/omitted = all worker roles
manager_backend = "claude"            # optional per-type override
enable_ask_human = true               # optional per-type knob override
```

`agentpit workflow review "check the auth refactor"` runs that preset. The type's `prompt` is
injected as a `WORKFLOW BRIEF` block, and only its `roles` become the manager's `AVAILABLE ROLES`.

## List configured workflows (`agentpit workflow list`)

`agentpit workflow list` prints the base `[workflow]` plus every configured type with its
EFFECTIVE values (manager, knobs, roster, brief) and a copy-pasteable invocation; roles a type
names that are missing from the shared cast are flagged, and an unsupported manager (anything
but claude|codex) is called out instead of failing at run time. `--json` emits the same summary
machine-readable. Takes no goal.

## Generate a workflow (`agentpit workflow new "<description>"`)

Turns a natural-language description into a workflow proposal (a type + the roles it needs) via a
one-shot manager call:

```
agentpit workflow new "<description>" [--json] [--write]
```

- default: prints the proposal as human-readable TOML to paste into `config.toml`.
- `--json`: emits the structured proposal (the dashboard's ✨ generate button shells out to this).
- `--write`: appends the generated `[workflow.types.*]` (+ any new `[workflow.roles.*]`) to
  `config.toml` (refuses if the type name already exists). Review before running with `--write`.

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

## Models

Each agent can run on a specific model. Sources, highest precedence first: an explicit
`--model <m>` on the command, the role's `[workflow.roles.<name>].model`, then the backend's
`[backends.<id>].model` default. Unset everywhere = the backend CLI's own default (no `--model`
flag is emitted). `--model` on `workflow` pins the manager; a worker role dispatched via `rescue
--role` carries its own `model`, so a workflow's cast can mix models per role. Over MCP,
`dispatch_task` / `run_ensemble` / `run_workflow` each accept an optional `model`.

Dispatch grammar for a configured worker role: `agentpit rescue --role <name> "<sub-task>"`
(`--role` and `--backend` are mutually exclusive), or over MCP
`mcp__agentpit__dispatch_task {"role":"<name>","task":"<sub-task>"}` (`role`/`backend` exclusive
there too). With at least one worker role configured, the workflow manager's roster becomes
AVAILABLE ROLES and it dispatches by role name; `--agents` is ignored with a warning. The
reserved `manager` role is never a dispatch target (`--role manager` is a hard error); manager
resolution is `--manager` > the type's `manager_backend` > `[workflow.roles.manager]` (first
claude|codex) > `[workflow].manager_backend` > `[default].backend`, and its `prompt` is injected
into the orchestrator prompt as a MANAGER PERSONA block.
