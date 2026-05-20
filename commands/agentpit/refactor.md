---
description: Plan a refactor via a backend agent (default route prefers Claude)
argument-hint: <path> <goal>
---

Parse `$ARGUMENTS`:
- First whitespace-separated token (or quoted string) is `path`.
- The remainder is `goal`.

Call `mcp__agentpit__refactor` with `{ path, goal, cwd }`.

Present the backend's plan + diff verbatim. Do NOT apply the diff;
ask the user to confirm before applying. If they confirm, apply with the
standard Edit / Write tools, not via the backend.
