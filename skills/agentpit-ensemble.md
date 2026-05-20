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
agentpit ensemble "<prompt>" [--members <a,b,c>] [--aggregator <id>]
```

Defaults come from `~/.config/agentpit/config.toml` — typically `gemini, claude, opencode` with no aggregator.

## Output

Per-source sections with `=== <backend> (transport=...) ===` headers. If `--aggregator` is set,
a trailing `=== aggregator [<id>] ===` synthesis section follows.

Relay verbatim.
