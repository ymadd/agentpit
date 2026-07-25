---
name: agentpit-status
description: Show which backends agentpit can reach and whether each is authenticated. Useful as a diagnostic when a backend fails or before kicking off a multi-agent flow.
---

# agentpit:status

Run:

```bash
agentpit status
```

Output includes the active config file (or `defaults`), the default backend, and per-backend transport + auth state.
A backend whose last recorded dispatch failed also gets a `note:` line with when it failed and why — credentials
can be valid while an exhausted quota or a retired client makes every dispatch fail, so treat that note as the
stronger signal when picking a backend.
Relay verbatim.
