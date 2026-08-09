---
description: Check or trigger the login flow for a backend agent
argument-hint: <claude|codex|antigravity|opencode|prime-agent> [--check]
---

Parse `$ARGUMENTS`:
- First token is the backend id.
- If `--check` is present, pass `--check` (status only, no Terminal launch).

Run:

```
agentpit login <backend> [--check]
```

If `--check` was not passed and the backend is unauthenticated, agentpit opens a new
macOS Terminal window running the backend's login command. Tell the user to complete
the OAuth flow there and rerun their original command.
