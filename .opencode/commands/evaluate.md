---
description: Evaluate current implementation (base-flash-arb)
agent: evaluator
subtask: true
---

Evaluate whether the current implementation is acceptable by auditing it against this repo's dry-run safety invariants and verification gates.

Load and apply the `evaluation` skill to guide the evaluation process.

This command is **read-only**. Do not edit, stage, commit, or push anything. Run offline verification only: do not start `anvil` or run any fork mode (`--fork-check`, `--fork-matrix`, fork tests with a URL) unless the user explicitly asks for it in this session. Never print secrets, RPC URLs, or webhook URLs.

Steps to follow:

1. Load the `evaluation` skill for the evaluation framework, hard gates, and scoring rubric
2. Inspect the current implementation (`git status`, `git log`, `git diff`, plus changed files) and gather evidence (`path:line`)
3. Run the offline verification gates for the toolchains that changed (Rust and/or Solidity), in the order defined by the skill
4. Audit the safety invariants (hard gate) and review correctness, tests, fail-closed behavior, and docs/pinned-data sync using `references/domain-checks.md` from the skill. For each area the diff touches, delegate a read-only review in parallel to the matching subagent (`rust-analyst`, `solidity-auditor`, `base-mev-analyst`, `security-expert`), then verify its Critical/High findings yourself before reporting. Never dispatch engineer agents.
5. Produce a structured evaluation report with:
  - **Verdict**: ACCEPTABLE, NOT ACCEPTABLE, or INCONCLUSIVE (when required evidence needs a fork or network run that was not authorized)
  - **Score**: N/100 with the per-dimension breakdown
  - **Gates**: safety invariants and verification commands, each PASS / FAIL / NOT RUN
  - **Gaps / issues**: specific deviations, missing pieces, or quality concerns, ordered by severity
  - **Recommendations**: concrete next steps if the implementation is not acceptable
