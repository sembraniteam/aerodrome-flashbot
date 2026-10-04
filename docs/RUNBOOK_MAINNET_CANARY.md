# Runbook: Base mainnet canary (G5)

> Scope: Base mainnet (chain id **8453**) canary ONLY, G5 caps ($50 max
> flash, $10 daily loss, 2 consecutive failures, operator float gas-only ≤
> $20 equiv, daily sweep, 7 days + 20 attempts). The USER runs every step
> below. Agents never deploy, fund, broadcast, or touch keys. If any step
> asks for a real key, a non-8453 chain, or a broadcast you did not intend
> — STOP.

## 0. Principles (in force from the first armed run)

- **Dry-run first.** No canary step runs before `/readiness mainnet`
  reports G4 READY (fresh, ≤ 24 h old) on the EXACT commit being armed.
- **No secrets in the repo.** The operator key (bot host), owner key
  (multisig/hardware — never on the bot host), pauser key, and RPC URLs
  with keys live in YOUR environment or secret manager, never in `config/`,
  never committed. The trading binary holds the operator key only
  (`src/live/signer.rs` custody guard refuses owner/pauser keys); the
  Discord transport holds the pauser key only.
- **Ceilings, never targets.** `config/mainnet-canary.toml` G5 values are
  maxima. Raising one is a code change + new FREEZE pin and returns the bot
  to G2 — never via Discord, never by hand-editing the deployed profile
  past its `hard_max_*`.
- **Fail closed.** Missing, stale, or unverifiable evidence yields NOT
  READY, never READY. The kill switch (flag file AND on-chain pause) stops
  everything; an unreadable flag means stopped. Resume is owner-only with a
  written reason in the ledger.

Conventions (USER-ACTION: replace each before running; no fake values are
pre-filled anywhere in this runbook):

- `USER-ACTION: RPC endpoint` — your mainnet RPC (env only).
- `USER-ACTION: $EXE` — the deployed canary executor address (§2).
- `USER-ACTION: owner / operator / pauser addresses` — the three SEPARATE
  canary addresses (owner = multisig/hardware).
- `USER-ACTION: KMS handle` — the mainnet remote-signer/KMS wiring the live
  binary reads INSTEAD of any env key (env keys never touch mainnet; the
  binary refuses `SignerKind::Env` on 8453 — see `src/live/signer.rs`).
- `USER-ACTION: oracle price feed` — the production replacement for the
  `--eth-price-cents/--eth-price-asof` attestation (P5; until then every
  arm needs a FRESH (< 300 s) operator attestation, and staleness refuses).
- `USER-ACTION: onchain.json` — `artifacts/readiness/mainnet-canary/onchain.json`
  (template in §8, git-ignored).

## 1. C0 pre-flight (day 0, no capital moves)

1. Gate the chain (every session starts here; scripts and hands alike):
   ```bash
   cast chain-id --rpc-url "$RPC_URL"   # expect 8453; else STOP
   ```
2. Confirm `/readiness mainnet` G4 READY on this commit (bundle ≤ 24 h
   old): role read-back equals the intended matrix, codehash equals
   `docs/FREEZE.md`, `eth_call` simulations of the full flow at the
   current head produce expected outcomes, alert test acknowledged by a
   human.
3. Build the live binary from the pinned commit (`cargo build --bins
   --features live`) and compute the manifest + `LIVE_ARM` YOURSELF from
   the readiness bundle (the agent never handles the arm value):
   ```bash
   sha256sum artifacts/readiness/<UTC-timestamp>/manifest.json   # = LIVE_ARM
   ```
4. Record the go-decision (who, when, caps) in `docs/READINESS.md`
   (USER-ACTION: owner writes this line).

## 2. Deploy + verify executor (owner, 8453)

1. `forge build` green locally first.
2. Deploy `contracts/FlashArbExecutor.sol:FlashArbExecutor` from the OWNER
   (multisig/hardware) with YOUR reviewed command:
   ```bash
   cast chain-id --rpc-url "$RPC_URL"   # 8453, again, before any broadcast
   forge create contracts/FlashArbExecutor.sol:FlashArbExecutor \
     --rpc-url "$RPC_URL" \
     --account <OWNER_CANARY_ACCOUNT> \
     --constructor-args <BALANCER_VAULT_8453> <USDC_NATIVE_8453>
   ```
   Vault/USDC are the pinned `docs/FREEZE.md` values — re-verify them
   on-chain before use, never trust memory.
3. Verify source on the explorer; confirm the deployed codehash equals the
   FREEZE pin:
   ```bash
   cast code <EXE> --rpc-url "$RPC_URL" | cast keccak   # compare FREEZE.md
   ```
4. Record the address (USER-ACTION: replace the `0x11..`/`0x22..`
   placeholders in YOUR OFF-REPO copy of `config/mainnet-canary.toml` —
   never commit real addresses as "verified" without the read-back in §3).

## 3. Configure caps / allowlist / roles (owner-signed, canary values)

1. `setOperator(<operator>)`, `setPauser(<pauser>)` — three distinct
   addresses (startup refuses otherwise).
2. `setTokenAllowed` for the three canary tokens; `setRouterAllowed` for
   the two pinned routers (UniversalRouter is NEVER allowlisted).
3. `setMaxFlashUSDC` to the G5 canary value ($50 = 50000000 base units)
   and update `onchain_max_flash_usdc` in YOUR profile copy to match
   (mismatch refuses startup — L4).
4. Optional: `setRouterCodehashPinned` per router from `cast code <router>
   | cast keccak` (any later drift reverts every `execute` with
   `RouterCodeChanged` by design).
5. Read back and compare (any mismatch → STOP, do not arm):
   ```bash
   cast call <EXE> "owner()(address)" --rpc-url "$RPC_URL"
   cast call <EXE> "operator()(address)" --rpc-url "$RPC_URL"
   cast call <EXE> "pauser()(address)" --rpc-url "$RPC_URL"
   cast call <EXE> "maxFlashUSDC()(uint256)" --rpc-url "$RPC_URL"
   ```

## 4. Fund operator with gas ONLY (≤ $20 equiv)

1. Fund the operator address with gas dust ONLY (at most the $20-equiv
   float cap in `config/mainnet-canary.toml`). The startup float check,
   the pre-submit float gate, and L6 reconciliation all enforce it;
   over-cap refuses everywhere.
2. Confirm the owner/pauser keys are NOT on the bot host (the custody
   guard refuses startup if `OWNER_KEY` or `PAUSER_KEY` is visible there).

## 5. Arm the live binary (runbook command lines only)

```bash
export LIVE_ARM="<USER-ACTION: sha256 of YOUR manifest.json>"
printf OK > artifacts/live-canary.kill   # flag file: exactly OK, else stopped
./target/debug/live --config <YOUR-canary-profile-copy> \
  --manifest artifacts/readiness/<UTC-timestamp>/manifest.json \
  --eth-price-cents <USER-ACTION: fresh cents> \
  --eth-price-asof <USER-ACTION: now-secs> \
  --emit-evidence artifacts/readiness/mainnet-canary/evidence-arm
# expect: ARMED chain=8453 ... (no trading loop; runbooks execute)
```

Refusal is a PASS when the reason names the failed lock element
(build/config/arm/manifest/roles/float/signer): fix the cause, never
bypass. The binary never prints secret values (match/mismatch only).

## 6. Canary window: 7 days + 20 attempts, daily sweep + reconcile

1. Attempts are YOUR reviewed submits (operator key, dust-sized within the
   $50 cap), each: simulate exact calldata at latest head (`cast call`),
   check net ≥ min + margin with measured L1 fee, submit, wait for
   `confirmation_depth` (5) before booking, reconcile ledger vs on-chain
   (executor + operator) after EACH attempt.
2. Sweep profit to the owner address DAILY (owner-signed `sweep`); the
   executor must end each day at zero except in-flight dust.
3. Emit evidence per attempt batch (`--emit-evidence`): window, counts,
   rejection reasons, predicted-vs-realized, breaker events, ledger head.
   `realized_samples=0` until a settled receipt exists — never present a
   prediction as a fill.
4. Exit bar (ALL): ≥ 7 days AND ≥ 20 attempted live transactions (or a
   written reason fewer opportunities existed); zero invariant breaches;
   zero unexplained reverts; revert rate within the predicted band;
   realized-vs-predicted per trade within the drift band; ledger vs
   on-chain reconciliation EXACT every day; §7 drills done; no alert left
   unacknowledged.

## 7. Kill-switch + breaker drills (repeat once on mainnet)

1. File flag: replace the flag content (anything but `OK`) → next arm
   MUST refuse (`kill switch engaged`). Restore `OK` afterwards by hand.
2. On-chain pause: pauser `pause()` → any `execute` reverts `EnforcedPause`
   the same block; owner `unpause()` restores (pauser can never unpause —
   verify the revert once).
3. Breaker: if any §9 trigger fires during the window, confirm new
   submissions stop, the trip persists across a restart, and reset is
   owner-only with a ledger reason. (Do NOT trip the breaker artificially
   with real funds — the drill is the file-flag + pause pair above plus
   any genuine trip the window produces.)
4. Record pause/unpause hashes in onchain.json (§8).

## 8. onchain.json (USER-ACTION: fill as you go)

`artifacts/readiness/mainnet-canary/onchain.json` (git-ignored):

```json
{
  "chain_id": 8453,
  "executor": "USER-ACTION: deployed $EXE",
  "roles": {
    "owner": "USER-ACTION",
    "operator": "USER-ACTION",
    "pauser": "USER-ACTION"
  },
  "deploy_tx": "USER-ACTION",
  "config_txs": ["USER-ACTION"],
  "sweeps": ["USER-ACTION: one per day"],
  "pause_drill": { "pause_tx": "USER-ACTION", "unpause_tx": "USER-ACTION" },
  "attempts": ["USER-ACTION: one tx per attempt"]
}
```

Verify each read-only: `cast chain-id` == 8453; `cast receipt <tx>`
status/`to`/events; `cast tx <tx>` sender == intended role; codehash ==
FREEZE.md. A missing hash is INCONCLUSIVE, never READY.

## 9. Rollback / abort triggers (automatic — any one)

Daily loss cap hit ($10) · consecutive failures reached (2) ·
realized-vs-predicted drift beyond band · reconciliation mismatch (any
size) · sequencer gate closed past the grace policy · codehash drift ·
operator float over cap · kill switch engaged · manifest expired.

Action (in order): pause (pauser key is enough) → alert → owner `sweep`
to the owner address → stop and review. Resume needs an owner action with
a written ledger reason AND a fresh `/readiness mainnet`. There is no
"retry with more gas": re-simulate and re-check limits first. Return to
the previous cap only if the cause is understood; otherwise return to G4
(no capital) and re-enter from §1.

## 10. What P5 needs from this window

The 7-day evidence bundle (manifest + `gates/` + `runs/` summaries +
onchain.json + ledger chain verifying with `live ledger-verify`) is the
G5-exit input to `/readiness mainnet`. Known P4 placeholders the owner
must replace before any ramp (G6): KMS handle for the remote signer
(env keys stay Sepolia-only), oracle price feed replacing the
`--eth-price` attestation, and the production alert channel ACK receipts.
