---
description: Multi-agent ADVERSARIAL review — challenges assumptions, demands evidence (defaults to codex + antigravity)
argument-hint: <target> [--focus=<topic>]
---

Parse `$ARGUMENTS`:
- The first whitespace-separated token (or quoted string) is `target` (path, glob, diff reference, or description).
- Look for a `--focus=<value>` flag (e.g. `--focus=concurrency`, `--focus=error-paths`).
- If the user asks for router/learned/profile-based member selection (e.g. "--routed", "router"), add `--routed` (optionally `--routed N`).

Run:

```
agentpit adversarial-review "<target>" [--focus <focus>] [--routed [N]]
```

The CLI emits per-backend sections with `=== <backend> (transport=exec) ===` headers and findings categorized as CRITICAL / HIGH / MEDIUM / LOW with file:line citations and concrete reproducers or execution traces.

Relay the output verbatim. Group CRITICAL / HIGH findings at the top if it helps the user. Do not silently drop a finding even if it looks like a false positive — adversarial reviewers are deliberately tuned to over-call, and the user decides what to dismiss.

Note: this is an ensemble fan-out — by default members come from `[ensemble]` config (or `--members`). `--routed [N]` instead picks the top-N by learned capability profile (the stderr line shows each pick's score and sample count). Aggregator grades feed `agentpit profile learn`.
