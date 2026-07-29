---
description: Multi-agent code review (defaults to antigravity + opencode in parallel)
argument-hint: <target> [--focus=<topic>]
---

Parse `$ARGUMENTS`:
- The first whitespace-separated token (or quoted string) is `target` (path, glob, or description).
- Look for a `--focus=<value>` flag.

Run:

```
agentpit review "<target>" [--focus <focus>]
```

The CLI emits per-backend sections with `=== antigravity (transport=exec) ===` style headers.
Relay verbatim, and group CRITICAL / HIGH findings at the top if helpful.

Note: this is an ensemble fan-out — the learned router is not consulted; members come from `[ensemble]` config (or `--members`). Aggregator grades feed `agentpit profile learn`.
