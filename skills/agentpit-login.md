---
name: agentpit-login
description: Check or launch a backend's login flow. On macOS, agentpit opens a new Terminal window running the backend's auth command.
---

# agentpit:login

## How to invoke

```bash
agentpit login <claude|codex|antigravity|opencode> [--check]
```

- `--check` only reports status; it does not open Terminal.
- Without `--check`, agentpit launches the backend's login command in a fresh Terminal window
  (macOS only). The user completes the OAuth flow there, then re-runs whatever command originally
  hit the auth wall.

## Output

Status lines on stdout. If a Terminal window was opened, tell the user to complete the OAuth flow
and then retry their previous command.
