---
name: agentpit-explain
description: Explain code or a topic via another backend agent. Useful when Gemini's long context is needed, or when the user explicitly asks for an outside explanation.
---

# agentpit:explain

## When to invoke

- "Explain how this module works using gemini"
- The target is too large to comfortably hold in the current context.

## How to invoke

```bash
agentpit explain "<target>" [--deep] [--backend <id>]
```

`--deep` requests a fuller walk-through (design rationale, control flow, edge cases).
Without it, the agent keeps the explanation under 200 words.

## Output

Plain text on stdout. Relay verbatim.
