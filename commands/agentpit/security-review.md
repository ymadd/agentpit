---
description: Multi-agent SECURITY review with OWASP-style checklist (defaults to claude + codex)
argument-hint: <target> [--focus=<topic>]
---

Parse `$ARGUMENTS`:
- The first whitespace-separated token (or quoted string) is `target` (path, glob, diff reference, or description).
- Look for a `--focus=<value>` flag (e.g. `--focus=auth`, `--focus=secrets`).

Run:

```
agentpit security-review "<target>" [--focus <focus>]
```

The CLI emits per-backend sections with `=== <backend> (transport=exec) ===` headers and findings categorized as CRITICAL / HIGH / MEDIUM / LOW with file:line citations.

Relay the output verbatim. Group CRITICAL / HIGH findings at the top if it helps the user. Do not silently drop a finding even if it looks like a false positive — surface it so the user can decide.

Note: this is an ensemble fan-out — the learned router is not consulted; members come from `[ensemble]` config (or `--members`). Aggregator grades feed `agentpit profile learn`.
