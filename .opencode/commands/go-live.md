---
description: Build the live-capable path (testnet and mainnet) in reviewed phases with user checkpoints
agent: release-manager
subtask: true
---

Build or continue the live-capable path of base-flash-arb, in phases, without weakening dry-run safety.

Scope or phase (`P1` to `P5`, `all`, or free text; default is the next incomplete phase): $ARGUMENTS

Current tree: !`git status --short` !`git log --oneline -5`

Load and apply the `go-live-readiness` skill (Workflow B) and build to `references/live-contract.md`.

Hard rules:

- Dry-run remains the default; the paper binary still forces dry-run. Live code is behind the `live` feature, its own
  binary, and the four-part Live Lock (build, config, arm, manifest).
- No agent signs, deploys, funds, or broadcasts, and none asks for keys, RPC URLs, or webhook URLs. Live steps become
  numbered runbooks for the user, who hands back transaction hashes as evidence.
- Never loosen a default, cap, or invariant. Caps are raised only by the user, one rung at a time.
- Every behavior change ships with offline tests (refusal and fail-closed paths included) and updated docs.

Steps to follow:

1. Determine which phases already exist by reading the repo (`src/bin/live.rs`, `feature = "live"`, `config/*.toml`,
   `docs/ADR-002-live-path.md`, runbooks). State what is in place and what is missing.
2. Do the requested phase only. Brief the delegates from the skill's phase table with exact files, L-invariants, tests,
   and verification commands.
3. After implementation, run `/evaluate`-style offline checks through the engineers, and dispatch the matching
   read-only reviewers in parallel. Verify Critical and High findings before acting on them.
4. Update README, `docs/FREEZE.md`, ADRs, and runbooks in the same phase as the code.
5. Stop at the checkpoint: summarize what changed, what was verified, what remains, and the single decision needed. Do
   not start the next phase without the user's confirmation.
6. When P5 is reached, recommend `/evaluate` then `/readiness testnet`, and list the runbook steps only the user can run.
