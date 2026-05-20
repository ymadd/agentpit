---
name: agentpit-refactor
description: Have another backend agent plan a refactor (changes + unified diff). The plan is for human review — do not apply automatically.
---

# agentpit:refactor

## When to invoke

- "Plan a refactor of src/foo.ts to extract the bar() helper"
- The user wants a second opinion on how to restructure code before they apply changes.

## How to invoke

```bash
agentpit refactor "<path>" "<goal>" [--backend <id>]
```

## Output

The agent produces a written plan followed (where possible) by a unified diff. **Do not apply the diff automatically.**
Ask the user to confirm. If they confirm, apply via the standard Edit / Write tools — not via the backend.
