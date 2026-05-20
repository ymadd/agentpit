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
Relay verbatim.
