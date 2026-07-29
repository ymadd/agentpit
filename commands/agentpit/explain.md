---
description: Explain code via a backend agent (auto-routed; useful for targets too large for the current context)
argument-hint: <target> [--deep]
---

Parse `$ARGUMENTS`:
- The leading argument is `target`.
- If `--deep` is present, pass `--deep`.

Run:

```
agentpit explain "<target>" [--deep]
```

Relay the explanation directly. If the user asks a follow-up,
re-run `agentpit explain` rather than answering yourself.
