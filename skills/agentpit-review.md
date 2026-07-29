---
name: agentpit-review
description: Run a multi-agent code review (Antigravity + OpenCode in parallel by default). Invoke when the user asks for a code review, audit, or second/third-opinion review of files, diffs, or designs.
---

# agentpit:review

Use this skill when the user asks for a code review across multiple agents.

## When to invoke

- "Review src/foo.ts"
- "Get a panel to audit this PR"
- "Two opinions on this design"

## How to invoke

```bash
agentpit review "<target>" [--focus <topic>] [--members <a,b,c>] [--aggregator <id>]
```

Defaults:
- members: `antigravity, opencode`
- aggregator: none (members run in parallel and each section is shown separately)

## Routing note

This is an **ensemble** (parallel fan-out), not a routed dispatch: the learned router
(capability profile / similarity / suspension) is **not consulted** — members come from the
config's `[ensemble]` lists unless `--members` overrides them. The trade runs the other way:
when an aggregator is set, its per-member grades become training labels for
`agentpit profile learn`, which improves the routed commands (`rescue` / `explain` /
`refactor`).

## Output

Per-backend sections with `=== <backend> (transport=...) ===` headers. If `--aggregator` is set,
a trailing `=== aggregator [<id>] ===` section is also produced.

Relay the output verbatim. Group `CRITICAL` / `HIGH` findings at the top if it helps the user.
