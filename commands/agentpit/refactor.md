---
description: Plan a refactor via a backend agent (auto-routed by learned capability)
argument-hint: <path> <goal>
---

Parse `$ARGUMENTS`:
- First whitespace-separated token (or quoted string) is `path`.
- The remainder is `goal`.

Run (bare — no `--backend` — so agentpit auto-routes by learned capability; only add
`--backend <id>` when the user names one, which bypasses routing):

```
agentpit refactor "<path>" "<goal>"
```

Present the backend's plan + diff verbatim. Do NOT apply the diff automatically;
ask the user to confirm. If they confirm, apply via the standard Edit / Write tools.
