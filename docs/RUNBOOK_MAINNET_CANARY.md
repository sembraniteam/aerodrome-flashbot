# Runbook: Base mainnet canary (STUB — full runbook lands in P4)

> Scope: Base mainnet (chain id **8453**) canary ONLY, G5 caps ($50 max
> flash, $10 daily loss, operator float gas-only <= $20 equiv). Nothing here
> is runnable yet: P3 delivered code + offline tests only. Do NOT fund,
> deploy, or broadcast from this stub — STOP and wait for the numbered P4
> runbook.

## 0. Principles

- **Dry-run first.** No canary step runs before `/readiness mainnet` gates
  pass on the exact commit being deployed.
- **No secrets in the repo.** `OPERATOR_KEY` (bot host), owner key
  (multisig/hardware), and RPC URLs with keys live in the environment or a
  secret manager on YOUR machine, never in `config/`, never committed. The
  trading binary holds the operator key only (`src/live/signer.rs`
  custody guard refuses owner/pauser keys).
- **Small canary only.** Caps in `config/mainnet-canary.toml` (§11 G5 row)
  are ceilings, never targets. Raising one is a code change + new FREEZE
  pin and returns the bot to G2 — never via Discord.
- **Fail closed.** Missing, stale, or unverifiable evidence yields NOT
  READY, never READY. The kill switch (flag file AND on-chain pause) stops
  everything; an unreadable flag means stopped.

## 1. What P4 will contain (numbered steps, not yet written)

1. D0 pre-flight: `/readiness canary` bundle, manifest hash, `LIVE_ARM`
   export by the operator (agents never touch keys).
2. Deploy + verify executor from the owner address; record the address and
   replace the `0x11..`/`0x22..` placeholders in
   `config/mainnet-canary.toml` (role read-back refuses until then).
3. Configure caps/allowlist/pauser/operator (owner-signed, dust first).
4. Arm the live binary from the runbook command lines only.
5. Canary window: 7 days + 20 attempts, daily sweep + reconciliation review.
6. Kill-switch + pause drills, then `/readiness mainnet` per ramp step.

## 2. Stop conditions (in force from the first armed run)

Daily loss cap, `max_consecutive_failures = 2`, realized-vs-predicted drift
beyond band, reconciliation mismatch, codehash drift, sequencer/provider
faults beyond policy, operator float over cap. Reset is manual, owner-only,
with a reason written to the ledger.
