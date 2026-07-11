---
name: agentpit-ensemble
description: Fan a prompt out to multiple backend agents in parallel, optionally synthesizing the results. Use for cross-agent comparisons, RFC-style multi-opinion gathering, or when robustness matters more than speed.
---

# agentpit:ensemble

## When to invoke

- "Ask three agents and merge the answers."
- "What does each model say about this approach?"
- "Cross-check with Gemini and OpenCode before I commit to a direction."

## How to invoke

```bash
agentpit ensemble "<prompt>" [--members <a,b,c>] [--aggregator <id>] [--model <m>]
```

Defaults come from `~/.config/agentpit/config.toml` — typically `antigravity, claude, opencode` with no aggregator.

`--model <m>` pins that model for every member and the aggregator (each backend maps it to its
own CLI flag). Omitted = each backend's `[backends.<id>].model` default, else the backend CLI's
own default — mixed-model panels come from those per-backend defaults, not from the flag.

## Output

Per-source sections with `=== <backend> (transport=...) ===` headers. If `--aggregator` is set,
a trailing `=== aggregator [<id>] ===` synthesis section follows.

Relay verbatim.
