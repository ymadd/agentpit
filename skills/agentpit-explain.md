---
name: agentpit-explain
description: Explain code or a topic via another backend agent. Useful when the target is too large for the current context, or when the user explicitly asks for an outside explanation.
---

# agentpit:explain

## When to invoke

- "Explain how this module works" (agentpit picks the backend)
- The target is too large to comfortably hold in the current context.

## How to invoke

Prefer the bare form — omitting `--backend` lets agentpit auto-route by learned capability
(chain, first hit wins: `[routes]` pin → capability profile → similarity → long-context /
keyword heuristics → default; backends whose last dispatch failed durably are skipped):

```bash
agentpit explain "<target>" [--deep]
```

Pass `--backend <id>` only when the user names one explicitly — an explicit backend
bypasses the learned routing entirely.

`--deep` requests a fuller walk-through (design rationale, control flow, edge cases).
Without it, the agent keeps the explanation under 200 words.

## Output

Plain text on stdout. Relay verbatim.
