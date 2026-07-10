---
description: Delegate a one-shot task to a backend agent (Gemini / Claude / Codex / OpenCode)
argument-hint: [backend?] <task>
---

Parse `$ARGUMENTS`:
- If the first whitespace-separated token is one of `gemini`, `claude`, `codex`, `opencode`, treat it as the backend override; the rest is the task.
- Otherwise, the entire `$ARGUMENTS` is the task and agentpit picks the default backend.

Run the `agentpit` CLI via Bash:

```
agentpit rescue "<task>" [--backend <id>] [--role <name>]
```

`--role <name>` dispatches to a configured `[workflow.roles.<name>]` persona instead of an
explicit backend — the role itself resolves which backend plays it. `--role` and `--backend`
are mutually exclusive; passing both is a hard error.

Relay the streamed output back to the user verbatim. If the command exits non-zero,
surface the error — it likely indicates an auth issue and agentpit will have already
opened a Terminal window for the OAuth flow.
