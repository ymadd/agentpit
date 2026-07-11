---
description: Fan a prompt to multiple backends and optionally synthesize results
argument-hint: <prompt> [--members=antigravity,opencode,claude] [--aggregator=claude] [--model=<m>]
---

Parse `$ARGUMENTS`:
- Everything that is not a `--flag=value` is the prompt.
- `--members=<comma-separated>` overrides the default panel.
- `--aggregator=<backend>` adds a synthesis pass.
- `--model=<m>` pins that model for every member + the aggregator. Omitted = each backend's own
  `[backends.<id>].model` default, else the backend CLI's default.

Run:

```
agentpit ensemble "<prompt>" [--members <a,b,c>] [--aggregator <id>] [--model <m>]
```

The CLI already prints per-source sections (and a trailing aggregator section if requested).
Relay verbatim.
