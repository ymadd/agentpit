---
description: Check or trigger the login flow for a backend agent
argument-hint: <gemini|claude|codex> [--check]
---

Parse `$ARGUMENTS`:
- First token is the backend id.
- If `--check` is present, pass `check_only: true`.

Call `mcp__agentpit__login` with `{ backend, check_only? }`.

If `check_only` was not requested and the backend is unauthenticated,
agentpit opens a new macOS Terminal window running the backend's login command.
Tell the user to complete the OAuth flow there and rerun their original command.
