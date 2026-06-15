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
