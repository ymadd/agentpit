---
description: Fan a prompt to multiple backends and optionally synthesize results
argument-hint: <prompt> [--members=antigravity,opencode,claude] [--aggregator=claude]
---

Parse `$ARGUMENTS`:
- Everything that is not a `--flag=value` is the prompt.
- `--members=<comma-separated>` overrides the default panel.
- `--aggregator=<backend>` adds a synthesis pass.

Run:

```
agentpit ensemble "<prompt>" [--members <a,b,c>] [--aggregator <id>]
```

The CLI already prints per-source sections (and a trailing aggregator section if requested).
Relay verbatim.
