# Risk Playbook

For each risk: **prevent** (design), **detect** (signal), **respond** (automatic action, then human action). Every
automatic action is "stop trading", never "try something cleverer". Resume is always an owner decision.

## 1. Keys and control plane

| Risk                                  | Prevent                                                                                                   | Detect                                       | Respond                                              |
|---------------------------------------|-----------------------------------------------------------------------------------------------------------|----------------------------------------------|------------------------------------------------------|
| Operator (hot) key compromised        | Operator can only call the trade function; holds gas only; no sweep, unpause, allowlist, or limit rights  | Unexpected tx from operator; balance drop    | Owner pauses and rotates operator on-chain           |
| Owner key compromised                 | Owner is a multisig or hardware wallet, never on the bot host; sweep destination is the owner only        | Owner tx alerts                              | Follow multisig incident plan; pause via pauser      |
| Pauser key leaked (Discord)           | Pauser can only pause (S3); `discord_exposed(Resume) == false`                                            | Pause events not initiated by a human        | Treat as nuisance DoS; rotate pauser                 |
| Secrets in logs, repo, or artifacts   | URL redactor, env-only secrets, evidence holds hashes only, `artifacts/` git-ignored                      | S5 scan by file name; pre-commit hook        | Rotate immediately, purge history                    |
| Wrong network or wrong profile        | Profile pins chain id; startup compares RPC `eth_chainId`; deploy scripts gated by chain id               | Startup refusal                              | Fix profile; never override                          |

## 2. Executor contract

| Risk                          | Prevent                                                                                                    | Detect                             | Respond                              |
|-------------------------------|------------------------------------------------------------------------------------------------------------|------------------------------------|--------------------------------------|
| Drain or fund loss            | Immutable, no arbitrary call or delegatecall, allowlisted routers/selectors/tokens/pools, USDC-only        | Balance monitor, codehash monitor  | Pause; owner sweep; post-mortem      |
| Unprofitable trade settles    | Post-trade balance check covers `amount + fee + minProfit`; revert otherwise                               | Revert rate                        | Circuit breaker                      |
| Callback abuse                | Only the Vault, only for a loan this contract initiated                                                    | Reverts at D6                      | None needed if tests hold            |
| Codehash drift or proxy swap  | Codehash monitor compares to the pinned value each block batch                                             | Monitor mismatch                   | Pause, alert, no resume until reviewed |
| Approval residue              | Exact approvals, reset after use                                                                           | Allowance read-back in G4          | Owner revoke                         |

## 3. Market and execution

| Risk                                   | Prevent                                                                                                                  | Detect                              | Respond                             |
|----------------------------------------|--------------------------------------------------------------------------------------------------------------------------|-------------------------------------|-------------------------------------|
| Stale quote at inclusion               | Final `eth_call` at latest head, `min-out` both legs, short deadline (default 30 s, range 1 to 300), head-staleness gate | Slippage reverts                    | Count toward consecutive failures   |
| Reverts burn gas                       | Atomic profit check, gas-limit from simulation with margin, per-attempt gas cap, failed-attempt allowance in cost model  | Revert rate vs prediction           | Raise margin or stop; never retry blindly |
| Competition (searchers win first)      | Cost model includes failed attempts; priority fee cap; no escalation wars                                                | Win rate, realized vs predicted     | Stop if net after failures is negative |
| L1 data fee spike                      | Read the fee oracle each decision; cap; source logged                                                                    | L1 fee metric                       | Gate closes above cap               |
| Gas or priority fee spike              | Hard cap on max fee and priority fee                                                                                     | Gas metric                          | Gate closes above cap               |
| Pool manipulation, JIT liquidity       | Quoter cross-check inside tolerance; skip pools with abnormal liquidity change; size capped                              | Estimator vs quoter mismatch        | Reject, count, alert on streak      |
| Token edge cases (fee-on-transfer, blacklist, rebase) | Allowlist only WETH, USDC, AERO; balance-delta accounting instead of trusting amounts                  | Reconciliation mismatch             | Pause; remove pair                  |
| Flash loan unavailable or fee changes  | Read fee at decision time; fee>0 path tested (D10)                                                                       | Vault fee read                      | Recompute or skip                   |

## 4. Chain and infrastructure

| Risk                                    | Prevent                                                                                                           | Detect                                  | Respond                         |
|-----------------------------------------|-------------------------------------------------------------------------------------------------------------------|-----------------------------------------|---------------------------------|
| Sequencer down or in grace period       | Uptime gate fails closed on error; honors the 3600 s grace after recovery                                         | Uptime feed                             | No trading; resume automatically only when gate opens and breaker is clear |
| Reorg or unsafe head                    | Act on a configured head tag; confirm receipts at a defined depth before booking profit                            | Head vs receipt mismatch                | Re-simulate; never double-submit |
| RPC lies, lags, or diverges             | Two independent providers; compare `eth_blockNumber` and key reads; head staleness limit (default 5 blocks)        | Divergence metric                       | Gate closes on disagreement     |
| Feed mode change (Flashblocks, Denim)   | Feed behind a trait selected by config; verify current mode in `docs.base.org` before each stage                   | Feed health check                       | Fall back to canonical blocks   |
| Nonce stuck or dropped tx               | Single nonce owner; recovery path; one in flight                                                                   | Pending age                             | Cancel or replace with same nonce, owner-approved |
| Host crash or restart mid-trade         | Persist intent before submit; on start, reconcile to terminal state before any new trade                           | Startup reconciliation                  | Block trading until reconciled  |
| Clock skew                              | Deadlines from chain time, not host time                                                                           | Skew metric                             | Gate closes                     |

## 5. Operations and human error

| Risk                                   | Prevent                                                                                                | Detect                           | Respond                               |
|----------------------------------------|--------------------------------------------------------------------------------------------------------|----------------------------------|---------------------------------------|
| Config typo raises limits              | `size <= max <= hard_max` enforced at load; `hard_max` compiled per profile                            | Startup validation               | Refuse to start                       |
| Stale readiness evidence               | Startup verifies manifest commit, stage, chain id, and expiry                                          | Startup refusal                  | Re-run `/readiness`                   |
| Alert fatigue                          | Amounts-only, rate-limited, drop-not-block alerts; severity tiers; on-call owner named                 | Unacknowledged alerts            | Escalate; pause after N unacked criticals |
| Dependency or supply-chain compromise  | `cargo audit`, `cargo deny`, pinned lockfile, no new dependency without review                         | Audit output                     | Block release                         |
| Silent drift in profit model           | Realized-vs-predicted tracked per trade and per day; band enforced                                     | Drift metric                     | Breaker trips                         |
| Regulatory or ToS exposure             | Strategy limited to atomic arbitrage between public pools; no harm to a specific user's transaction    | Review                           | User seeks legal advice if unsure     |

## 6. Stop conditions (all fail closed)

The bot stops and alerts when any of these occurs. Resume needs an owner action and a written reason.

1. Daily loss cap reached, or `max_consecutive_failures` reached.
2. Realized-versus-predicted net drift beyond the band, or reconciliation mismatch of any size.
3. Codehash monitor mismatch, role read-back mismatch, or unexpected owner or operator transaction.
4. Sequencer gate closed, head stale, RPC providers disagree, or chain id mismatch.
5. Kill switch (file or on-chain pause) engaged, or readiness manifest expired.
6. Any panic, unhandled error in the trading path, or ledger write failure.
