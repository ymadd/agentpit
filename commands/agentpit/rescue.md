---
description: Delegate a one-shot task to a backend agent (Claude / Codex / Antigravity / OpenCode)
argument-hint: [backend?] <task> [--model <m>]
---

Parse `$ARGUMENTS`:
- If the first whitespace-separated token is one of `claude`, `codex`, `antigravity` (or `agy`), `opencode`, treat it as the backend override; the rest is the task.
- Otherwise, the entire `$ARGUMENTS` is the task and agentpit auto-routes (`[routes]` pin → capability profile → similarity → long-context/keyword heuristics → default).

Run the `agentpit` CLI via Bash:

```
agentpit rescue "<task>" [--backend <id>] [--role <name>] [--model <m>] [--cascade]
```

`--role <name>` dispatches to a configured `[workflow.roles.<name>]` persona instead of an
explicit backend — the role itself resolves which backend plays it. `--role` and `--backend`
are mutually exclusive; passing both is a hard error.

`--model <m>` pins the model (e.g. `opus`, `gpt-5-codex`) for this dispatch. Precedence:
`--model` > the role's `[workflow.roles.<name>].model` > the backend's `[backends.<id>].model`
default > the CLI's own default. Omitting it (and configuring no model) is byte-identical to
before — no `--model` flag is passed to the backend CLI.

`--cascade` runs a cost-ladder cascade: dispatch to the cheapest qualifying backend and escalate
on failure (`[cascade]` config; `[default].cascade = true` makes it the default). Mutually
exclusive with `--role`/`--backend`.

Relay the streamed output back to the user verbatim. If the command exits non-zero,
surface the error — it likely indicates an auth issue and agentpit will have already
opened a Terminal window for the OAuth flow.
