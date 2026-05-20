---
description: Fan a prompt out to multiple backends and (optionally) synthesize the answers
argument-hint: <prompt> [--members=gemini,opencode,claude] [--aggregator=claude]
---

Parse `$ARGUMENTS`:
- Everything that is not a `--flag=value` is the prompt.
- `--members=<comma-separated>` overrides the default panel.
- `--aggregator=<backend>` adds a synthesis pass on top.

Call `mcp__agentpit__ensemble` with `{ prompt, members?, aggregator?, cwd }`.

Each backend streams its own chunks via `notifications/progress` with `_meta.source` set to the backend id (or `aggregator`).
Present the final result verbatim; the tool already formats per-source sections.
