---
description: Delegate a one-shot task to a backend agent (Gemini or Claude)
argument-hint: [backend?] <task>
---

Parse `$ARGUMENTS`:
- If the first whitespace-separated token is one of `gemini`, `claude`, `codex`, treat it as the backend override; the rest is the task.
- Otherwise, the entire `$ARGUMENTS` is the task and agentpit picks the default backend.

Call the MCP tool `mcp__agentpit__rescue` with:
- `task`: the parsed task string.
- `backend`: the parsed backend if any (else omit).
- `cwd`: the current working directory (absolute).

Stream chunks back to the user as they arrive via `notifications/progress`.
After the tool returns, summarize the result in two short sentences if helpful;
otherwise just relay the final text.
