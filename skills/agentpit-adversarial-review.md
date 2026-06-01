---
name: agentpit-adversarial-review
description: Run a multi-agent ADVERSARIAL review (challenges assumptions, demands evidence, assumes the code is broken until proven otherwise; defaults to Codex + Antigravity in parallel). Invoke when the user wants a hostile / red-team / "rip it apart" / "what am I missing" review, or asks for negative findings on a design or PR.
---

# agentpit:adversarial-review

Use this skill when the user wants a hostile, evidence-demanding review across multiple agents — the opposite of a polite second opinion.

## When to invoke

- "Rip this PR apart."
- "What am I missing in src/scheduler.rs?"
- "Adversarial review of the new event layer."
- "Stress-test this design — what breaks?"
- "Be brutal about this refactor."

Do NOT use when the user wants:
- A general quality review → `agentpit-review`.
- A security audit specifically → `agentpit-security-review`.

## How to invoke

```bash
agentpit adversarial-review "<target>" [--focus <topic>] [--members <a,b,c>] [--aggregator <id>]
```

Defaults:
- members: `codex, antigravity` (codex for adversarial scrutiny, antigravity for long-context tracing)
- aggregator: none (each backend's findings are shown side by side)

Useful `--focus` values: `concurrency`, `error-paths`, `resource-limits`, `api-contracts`, `tests`, `performance`, `naming-lies`.

## Output

Each backend reports findings categorized as **CRITICAL / HIGH / MEDIUM / LOW** with file:line citations and a concrete reproducer or execution trace (not "could potentially…"). Negative results are reported explicitly with the evidence behind them.

Relay the output verbatim. Surface every finding even if it looks like a false positive — adversarial reviewers are deliberately tuned to over-call rather than under-call, and the user decides what to dismiss.
