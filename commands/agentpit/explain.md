---
description: Explain code via a backend agent (default route prefers Gemini for large context)
argument-hint: <target> [--deep]
---

Parse `$ARGUMENTS`:
- The leading argument is `target`.
- If `--deep` is present, pass `depth: "deep"`, else default to `brief`.

Call `mcp__agentpit__explain` with `{ target, depth, cwd }`.

Relay the explanation directly. If the user asked a follow-up,
re-run `mcp__agentpit__explain` rather than answering yourself.
