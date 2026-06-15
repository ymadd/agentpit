---
description: Register agentpit as an MCP server so a client's model can call its tools directly
argument-hint: [register|tools]
---

`agentpit mcp serve` runs a stdio MCP server exposing agentpit's dispatch / ensemble / workflow
tools. Register it in an MCP client so the client's own model can orchestrate agentpit backends
through structured tool calls instead of shelling out.

Register it:

```
# Claude Code
claude mcp add agentpit -- agentpit mcp serve
```

Or add a generic `.mcp.json`:

```json
{ "mcpServers": { "agentpit": { "command": "agentpit", "args": ["mcp", "serve"] } } }
```

Exposed tools:

- `mcp__agentpit__list_backends` — list backends + their transport and auth state.
- `mcp__agentpit__dispatch_task` — run ONE backend on a task and return its output.
- `mcp__agentpit__run_ensemble` — fan a prompt to several backends in parallel, optional aggregator.
- `mcp__agentpit__run_workflow` — launch a whole model-driven workflow (manager decomposes the goal,
  dispatches sub-tasks to workers, returns a synthesis).

SECURITY: `dispatch_task`, `run_ensemble`, and `run_workflow` launch backend agents with full
autonomy in the server's working directory, and `run_workflow` launches a full-autonomy manager.
Only register the server in trusted clients.

Relay verbatim.
