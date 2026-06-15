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
