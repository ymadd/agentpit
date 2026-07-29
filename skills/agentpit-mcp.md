---
name: agentpit-mcp
description: Register agentpit as an MCP server in a client (Claude Code or any .mcp.json client) so the client's own model can call agentpit's backend-dispatch, ensemble, and workflow tools directly instead of shelling out. Use when you want a model to orchestrate agentpit backends through structured MCP tool calls.
---

# agentpit:mcp

## When to invoke

- "Let Claude call agentpit's backends as tools instead of shelling out."
- "Register agentpit as an MCP server so the model can dispatch sub-tasks directly."
- "Wire agentpit into my MCP client."

## How to register

`agentpit mcp serve` is a standalone stdio MCP server. Register it in a client so the client's
model can call agentpit's tools:

```bash
# Claude Code
claude mcp add agentpit -- agentpit mcp serve
```

Or a generic `.mcp.json`:

```json
{ "mcpServers": { "agentpit": { "command": "agentpit", "args": ["mcp", "serve"] } } }
```

## Exposed tools

- `mcp__agentpit__list_backends` — backends + their transport and auth state.
- `mcp__agentpit__dispatch_task` — run ONE backend on a task; returns its output. Address it by
  backend id (`{"backend":"<id>"}`) or by a configured workflow role (`{"role":"<name>"}` — the
  role resolves its backend and prepends its persona); exactly one of the two.
- `mcp__agentpit__run_ensemble` — fan a prompt to several backends in parallel, optional aggregator.
- `mcp__agentpit__run_workflow` — launch a whole model-driven workflow: a manager backend
  decomposes the goal, dispatches sub-tasks to workers, and returns a final synthesis.
- `mcp__agentpit__ask_human` — ask the supervising human and block for an answer (workflow-manager
  tool; returns `HUMAN_UNAVAILABLE` on timeout).
- `mcp__agentpit__post_note` — record a durable handoff / shared-board note on the workflow
  transcript (requires a workflow run context).
- `mcp__agentpit__refute` — one critique→defense pass over a stuck candidate, returned for the
  manager to adjudicate.

## Security

`dispatch_task`, `run_ensemble`, and `run_workflow` launch backend agents with full autonomy in the
server's working directory, and `run_workflow` launches a full-autonomy manager. Only register the
server in trusted clients.

Relay verbatim.
