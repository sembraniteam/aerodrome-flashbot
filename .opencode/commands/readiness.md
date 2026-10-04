---
description: Prove readiness for Base Sepolia, mainnet canary, or mainnet ramp with an evidence bundle
agent: readiness-auditor
subtask: true
---

Assess whether base-flash-arb is ready for the stage given in the argument, and produce a hash-linked evidence bundle.

Target stage (`testnet`, `canary`, or `mainnet`; ask once if empty): $ARGUMENTS

Current tree: !`git status --short` !`git rev-parse --short HEAD`

Load and apply the `go-live-readiness` skill (Workflow A). This command never edits source, config, or docs and never
signs, deploys, or sends anything. It writes only inside `artifacts/readiness/`. Do not start `anvil` or any fork mode
unless the user explicitly asks in this session. Network use (read-only `cast` calls against an RPC from the
environment) needs the user's go-ahead in this session. Never print secrets, RPC URLs, or webhook URLs.

Steps to follow:

1. Load the `go-live-readiness` skill and its `references/promotion-ladder.md`.
2. Run `.opencode/skills/go-live-readiness/scripts/collect-evidence.sh --run-gates` (add `--chain-id 84532` for
   testnet, `--chain-id 8453` for canary and mainnet). If a bundle for this commit exists, run `--verify <dir>` first.
3. Audit the hard gates: S1 to S10 from the `evaluation` skill, and L1 to L10 from `references/live-contract.md` when a
   live path exists. If it does not exist, report NOT READY at G0 and list the missing capability and the `/go-live`
   phase that builds it.
4. Check each stage's exit criteria up to the target against the bundle, `runs/*.json`, and `onchain.json`. Verify
   on-chain items read-only when authorized; otherwise list them as Unverified with the exact commands.
5. Delegate read-only reviews in parallel for the areas the target stage depends on (`solidity-auditor`,
   `rust-analyst`, `base-mev-analyst`, `security-expert`), then verify their Critical and High findings yourself.
6. Update the bundle's `manifest.json` and `report.md`, run `collect-evidence.sh --seal <dir>`, and report:
  - **Verdict** for the target: READY, NOT READY, or INCONCLUSIVE, with the evidence level (E0 to E4)
  - **Ladder status**: G0 to G6 with verdict, evidence, and blocking items
  - **Hard gates**: S1 to S10, L1 to L10, cargo, forge
  - **Findings** by severity with `path:line`
  - **Needs user action**: exact ordered steps, who runs them, which hashes to hand back
  - **Bundle**: path, manifest sha256 (the `LIVE_ARM` value), and expiry
