---
name: agentpit-rescue
description: Delegate a coding task to an alternative backend agent (Claude, Codex, Antigravity, OpenCode) when the main session is stuck, lacks context, or would benefit from a second opinion. Invoke when the user asks to "ask codex" / "ask another agent" / "get a second opinion" / "I'm stuck" etc.
---

# agentpit:rescue

Use this skill to hand a one-shot task off to another coding agent without leaving the current session.

## When to invoke

- The user explicitly names a backend ("ask antigravity to...", "let codex try...").
- The current session is stuck on a problem and a fresh perspective would help.
- The task needs a different model's strength (e.g. Antigravity's long-context tracing, Claude's careful refactoring).

## How to invoke

Run the CLI:

```bash
agentpit rescue "<task description>" [--backend claude|codex|antigravity|opencode]
```

Omit `--backend` to let agentpit auto-route. The chain, first hit wins: a `[routes]` hard pin
(opt-in), the capability-profile route (task diagnosis → category → highest-scoring backend,
with a cost tiebreak), the kNN similarity route (only in `--features similarity` builds), then
the long-context / review-keyword heuristics, then `[default].backend`. Backends whose last
dispatch failed durably (quota, retired client) are skipped.

Add `--cascade` for a cost-ladder cascade: dispatch to the cheapest qualifying backend and
escalate up the ladder on failure (knobs in `[cascade]`; `[default].cascade = true` makes it
the default). Mutually exclusive with `--role`/`--backend`.

To dispatch to a configured persona instead of an explicit backend, use `--role <name>`
(resolves against `[workflow.roles.<name>]`; the role itself picks which backend plays it):

```bash
agentpit rescue "<task description>" --role reviewer
```

`--role` and `--backend` are mutually exclusive — passing both is a hard error.

Add `--model <m>` (e.g. `opus`, `gpt-5-codex`) to pin the model. Precedence: `--model` > the
role's `[workflow.roles.<name>].model` > the backend's `[backends.<id>].model` default > the
CLI's own default. Omitting it emits no `--model` flag (identical to the pre-model behaviour).

## Output

The CLI prints a header line `[backend=... transport=... route=...]` then streams the agent's reply to stdout. When dispatched via `--role`, the route segment reads `route=role:<name>` instead of the router's reason string.

If the chosen backend is not authenticated, the CLI exits non-zero with an auth hint and (on macOS) opens a Terminal window for the OAuth flow. Surface that to the user and re-run after they log in.
