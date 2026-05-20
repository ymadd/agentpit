---
description: Run a code review via a backend agent (default route prefers Claude)
argument-hint: <target> [--focus=<topic>]
---

Parse `$ARGUMENTS`:
- The first whitespace-separated token (or quoted string) is `target` (path, glob, or description).
- Look for a `--focus=<value>` flag and pass it as `focus`.

Call `mcp__agentpit__review` with `{ target, focus?, cwd }`. Do not pass `backend`
unless the user explicitly typed `--backend=<id>`.

When the tool returns, print findings grouped by severity (CRITICAL / HIGH / MEDIUM / LOW).
If the backend says it cannot access files, surface that directly.
