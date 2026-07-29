---
description: Multi-agent code review (defaults to antigravity + opencode in parallel)
argument-hint: <target> [--focus=<topic>]
---

Parse `$ARGUMENTS`:
- The first whitespace-separated token (or quoted string) is `target` (path, glob, or description).
- Look for a `--focus=<value>` flag.
- If the user asks for router/learned/profile-based member selection (e.g. "--routed", "router"), add `--routed` (optionally `--routed N`).

Run:

```
agentpit review "<target>" [--focus <focus>] [--routed [N]]
```

The CLI emits per-backend sections with `=== antigravity (transport=exec) ===` style headers.
Relay verbatim, and group CRITICAL / HIGH findings at the top if helpful.

Note: this is an ensemble fan-out — by default members come from `[ensemble]` config (or `--members`). `--routed [N]` instead picks the top-N by learned capability profile (the stderr line shows each pick's score and sample count). Aggregator grades feed `agentpit profile learn`.
