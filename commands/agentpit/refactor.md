---
description: Plan a refactor via a backend agent (Claude-first by default)
argument-hint: <path> <goal>
---

Parse `$ARGUMENTS`:
- First whitespace-separated token (or quoted string) is `path`.
- The remainder is `goal`.

Run:

```
agentpit refactor "<path>" "<goal>"
```

Present the backend's plan + diff verbatim. Do NOT apply the diff automatically;
ask the user to confirm. If they confirm, apply via the standard Edit / Write tools.
