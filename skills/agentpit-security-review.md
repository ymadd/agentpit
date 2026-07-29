---
name: agentpit-security-review
description: Run a multi-agent SECURITY review (OWASP-style checklist; defaults to Claude + Codex in parallel). Invoke when the user asks for a security review, security audit, pen-test style read, or asks "is this safe / vulnerable / exploitable?"
---

# agentpit:security-review

Use this skill when the user wants a security-focused review across multiple agents.

## When to invoke

- "Security review of src/auth.ts"
- "Audit this PR for vulnerabilities"
- "Is this endpoint safe?"
- "Check secret handling in the deploy script"

Do NOT use for general code quality reviews — those go to `agentpit-review`.

## How to invoke

```bash
agentpit security-review "<target>" [--focus <topic>] [--members <a,b,c>] [--aggregator <id>]
```

Defaults:
- members: `claude, codex` (adversarial pair)
- aggregator: none (each backend's findings are shown side by side)

Useful `--focus` values: `auth`, `secrets`, `injection`, `crypto`, `supply-chain`, `deserialization`.

## Routing note

This is an **ensemble** (parallel fan-out), not a routed dispatch: the learned router
(capability profile / similarity / suspension) is **not consulted** — members come from the
config's `[ensemble]` lists unless `--members` overrides them. The trade runs the other way:
when an aggregator is set, its per-member grades become training labels for
`agentpit profile learn`, which improves the routed commands (`rescue` / `explain` /
`refactor`).

## Output

Each backend reports findings categorized as **CRITICAL / HIGH / MEDIUM / LOW** with file:line citations and one-line remediations. If `--aggregator` is set, a trailing synthesized section is added.

Relay the output verbatim. Surface every finding even if it looks like a false positive — the user decides what to dismiss.
