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
agentpit review "<target>" [--focus <topic>] [--members <a,b,c> | --routed [N]] [--aggregator <id>]
```

Defaults:
- members: `antigravity, opencode`
- aggregator: none (members run in parallel and each section is shown separately)

## Routing note

This is an **ensemble** (parallel fan-out): by default the learned router is **not
consulted** — members come from the config's `[ensemble]` lists unless overridden.
Member selection, strongest override first:

- `--members <a,b,c>` — explicit list, always wins.
- `--routed [N]` — pick the top-N members from the learned capability profiles
  (review-category scores; suspended backends excluded). N defaults to the config
  list's size. **Use this whenever the user asks for the router / learned routing /
  profile-based member selection** (e.g. "router で選んで", `--backend router` intent).
- neither — the config's `[ensemble]` list.

The learning loop: when an aggregator is set, its per-member grades become training
labels for `agentpit profile learn`, which improves both the routed commands
(`rescue` / `explain` / `refactor`) and `--routed` member selection here.

## Output

Per-backend sections with `=== <backend> (transport=...) ===` headers. If `--aggregator` is set,
a trailing `=== aggregator [<id>] ===` section is also produced.

Relay the output verbatim. Group `CRITICAL` / `HIGH` findings at the top if it helps the user.
