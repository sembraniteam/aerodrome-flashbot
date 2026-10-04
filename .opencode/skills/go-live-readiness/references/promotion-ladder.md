# Promotion Ladder (G0 to G6)

Each rung has **entry criteria**, **activities**, **exit criteria** (what the evidence must show), and **abort
triggers** (what sends the bot back one rung). Numbers marked *default* are starting points; the user may tighten them
and may loosen them only with a written reason in `docs/READINESS.md`. They are never loosened by an agent.

Chain ids: Base Sepolia `84532`, Base mainnet `8453`. Mainnet addresses are never reused on Sepolia (S8).

## G0: Offline verified

- **Activities**: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`,
  `cargo build --bins`, `forge fmt --check`, `forge build`, `forge test`; S1 to S10 audit; secret scan by file name.
- **Exit**: every gate PASS with no network; clean tree at a recorded commit; `docs/FREEZE.md` matches
  `forge build --sizes` and codehashes.
- **Abort**: any gate fails or any invariant is violated.

## G1: Shadow run on live data (dry-run, mainnet read-only)

- **Entry**: G0 READY. Read-only RPC only; no signer loaded; paper binary.
- **Activities**: run continuously against Base mainnet data. Record every opportunity with predicted gross, each cost
  component, predicted net, quoter verification result, and the gate that rejected it.
- **Exit** (*default*): at least 72 h continuous with at least 95% uptime; zero panics; zero gate-open-on-error
  events; estimator-versus-quoter mismatch inside the configured tolerance for at least 99% of verified quotes; the
  cost model uses a measured L1 fee source (logged); at least one induced fault (RPC outage, stale head, sequencer
  down) observed to fail closed with a logged reason; predicted-net distribution published with sample size.
- **Abort**: systematic estimator bias, any fail-open, unexplained gaps in the ledger.
- **Note**: predicted profit here is a hypothesis. Do not report it as earnings.

## G2: Fork verification

- **Entry**: G1 READY (or user waives G1 for testnet-only work; mainnet stages still require it).
- **Activities**: on a pinned block, run `--fork-check` and `--fork-matrix`, and the Solidity fork tests with a URL, so
  the executor is exercised against the real Vault, routers, pools, and quoters. User must authorize `anvil` and fork
  mode in the session.
- **Exit**: production selectors (`0xa026383e`, `0x04e45aaf`) accepted by the real routers; both swap directions pass
  for every allowlisted pair; unprofitable path reverts atomically with the Vault unpaid-protection intact; fee>0
  path computes `repay = amount + fee`; fork block number and RPC provider class recorded (never the URL).
- **Abort**: any selector or pool mismatch, any non-atomic path.

## G3: Base Sepolia drill (84532)

- **Entry**: G2 READY (or G0 for mock-only drill). Operator key and owner key are **different throwaway Sepolia
  keys** held by the user. Deploy script is chain-id gated to 84532.
- **Activities** (user executes, agents write the runbook): deploy mocks and executor; set roles and allowlists; then
  run the drill matrix:

  | Case | Expected on-chain result                                                 |
  |------|--------------------------------------------------------------------------|
  | D1   | Profitable cycle succeeds, profit retained in executor                   |
  | D2   | Unprofitable cycle reverts, only gas spent                               |
  | D3   | Slippage breach reverts (min-out on both legs)                           |
  | D4   | Expired deadline reverts                                                 |
  | D5   | Non-allowlisted router, selector, token, or pool reverts                 |
  | D6   | Callback from a non-Vault caller reverts                                 |
  | D7   | Operator cannot unpause, sweep, or change allowlists or limits           |
  | D8   | Pauser pauses; trades revert while paused; only owner unpauses           |
  | D9   | Owner sweeps profit to the owner address                                 |
  | D10  | Fee>0 flash path repays `amount + fee`                                   |
  | D11  | Bot run end to end: opportunity, simulation, submit, receipt, ledger row |
  | D12  | Kill switch via file and via Discord pause stops the bot within 1 block  |

- **Exit**: every case has a transaction hash (or a documented revert) that the auditor verified read-only:
  correct chain id, status, `to` address, emitted events, and executor codehash equal to `docs/FREEZE.md`. Ledger
  rows reconcile with on-chain balances to the wei. Evidence stored as `onchain.json` (schema in
  `evidence-schema.md`).
- **Abort**: any case behaves differently from expected, any role escalation, any unreconciled balance.
- **Result**: unlocks "ready for testnet" claim. It says nothing about mainnet profitability: Sepolia has mock pools.

## G4: Mainnet pre-flight (no trading capital)

- **Entry**: G3 READY and G1 READY on the current commit; user has a multisig or hardware-held owner; hot operator key
  is separate, holds gas only.
- **Activities** (user executes): deploy executor on 8453; verify source on the explorer; confirm codehash equals
  `docs/FREEZE.md`; set owner (multisig), operator, pauser; set allowlists to the three pairs; set mainnet caps to the
  canary values; run `eth_call` simulations of the full flow at the current head; pause and unpause drill with zero
  funds; confirm alerting reaches the on-call channel; fund the operator with gas only.
- **Exit**: all of the above evidenced by transaction hashes and read-only calls; role matrix read back from chain
  equals the intended matrix; simulations produce expected outcomes; alert test acknowledged by a human.
- **Abort**: codehash mismatch, role mismatch, unexpected storage values.
- **Expiry**: 24 h. Re-run the read-back checks before G5 if older.

## G5: Mainnet canary (smallest real exposure)

- **Entry**: G4 READY and fresh. Written stop-loss and the owner's explicit go decision recorded in
  `docs/READINESS.md` (who, when, caps).
- **Caps** (*default*): `max_flash_usdc` 50, `daily_loss_cap_usdc` 10, `max_consecutive_failures` 2,
  `max_in_flight` 1, operator gas float at most the equivalent of 20 USD, profit swept daily. These are compiled
  `hard_max` ceilings for the canary profile, so a config typo cannot exceed them.
- **Exit** (*default*): at least 7 days and at least 20 attempted live transactions (or a written reason fewer
  opportunities existed); zero invariant breaches; zero unexplained reverts; reverted-attempt rate within the
  predicted band; realized net per trade versus predicted net within the configured drift band; ledger versus
  on-chain reconciliation exact on every day; kill-switch drill repeated once on mainnet; no alert left unacknowledged.
- **Abort** (automatic, any one): daily loss cap hit, consecutive failures reached, realized-vs-predicted drift beyond
  band, reconciliation mismatch, sequencer gate closed for longer than the grace policy, codehash drift. Action: pause,
  alert, require owner to review before resume.

## G6: Controlled ramp

- **Entry**: G5 READY.
- **Policy**: raise `max_flash_usdc` and `daily_loss_cap_usdc` in steps of at most 2x. Each step needs at least 7 days
  and 20 attempts at the previous cap, a fresh `/readiness mainnet` run, and an owner transaction or reviewed config
  change. No automatic ramp. Never through Discord (S3).
- **Ceiling**: the compiled `hard_max` and the executor's on-chain limit. Raising either is a code change plus a new
  FREEZE pin and returns the bot to G2.
- **Abort**: same triggers as G5, plus any drawdown beyond the written stop-loss. Return to the previous cap, not to
  zero, only if the cause is understood; otherwise return to G4.
