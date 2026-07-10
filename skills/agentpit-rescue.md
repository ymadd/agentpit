---
name: agentpit-rescue
description: Delegate a coding task to an alternative backend agent (Gemini, Claude, Codex, OpenCode) when the main session is stuck, lacks context, or would benefit from a second opinion. Invoke when the user asks to "ask gemini" / "ask another agent" / "get a second opinion" / "I'm stuck" etc.
---

# agentpit:rescue

Use this skill to hand a one-shot task off to another coding agent without leaving the current session.

## When to invoke

- The user explicitly names a backend ("ask gemini to...", "let codex try...").
- The current session is stuck on a problem and a fresh perspective would help.
- The task needs a different model's strength (e.g. Gemini's long context, Claude's careful refactoring).

## How to invoke

Run the CLI:

```bash
agentpit rescue "<task description>" [--backend gemini|claude|codex|opencode]
```

Omit `--backend` to let agentpit pick via its routing config (long-context heuristics, review keywords, etc.).

To dispatch to a configured persona instead of an explicit backend, use `--role <name>`
(resolves against `[workflow.roles.<name>]`; the role itself picks which backend plays it):

```bash
agentpit rescue "<task description>" --role reviewer
```

`--role` and `--backend` are mutually exclusive — passing both is a hard error.

## Output

The CLI prints a header line `[backend=... transport=... route=...]` then streams the agent's reply to stdout. When dispatched via `--role`, the route segment reads `route=role:<name>` instead of the router's reason string.

If the chosen backend is not authenticated, the CLI exits non-zero with an auth hint and (on macOS) opens a Terminal window for the OAuth flow. Surface that to the user and re-run after they log in.
