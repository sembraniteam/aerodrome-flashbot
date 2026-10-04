# Runbook: Base Sepolia pause → sweep → redeploy drill

> Scope: Base Sepolia (chain id **84532**) ONLY, with small dust amounts.
> Nothing here touches mainnet, moves real funds, or belongs in the repo as a
> secret. If any step asks for a real key, a mainnet address, or a broadcast
> you did not intend — STOP.

## 0. Principles

- **Dry-run first.** Every step below is rehearsed with `eth_call` /
  `forge test` before any Sepolia broadcast.
- **No secrets in the repo.** RPC URLs with keys, private keys, and mnemonics
  live in the environment (or a secret manager) on YOUR machine, never in
  `config/`, never committed. `config/local.toml` and `.env` are gitignored.
- **Small dust only.** Fund the drill with faucet Sepolia ETH plus a few
  cents of test USDC. The drill passes with dust; size proves nothing.
- **Two humans, two keys.** Owner key stays on an offline/air-gapped signer.
  The Discord/ops transport holds the PAUSER key only — never owner, never
  operator (see `src/discord.rs`: `discord_exposed`).

## 1. Prerequisites

1. Sepolia RPC endpoint in the environment (ask before starting anything that
   dials it; the public endpoint `https://sepolia.base.org` is rate-limited —
   prefer your own fork for reads):
   ```bash
   export BASE_SEPOLIA_RPC_URL="https://sepolia.base.org"  # or your provider URL from env
   ```
2. Throwaway drill keys generated in-memory (example with cast; the printed
   key NEVER leaves your terminal):
   ```bash
   cast wallet new   # OWNER drill key (air-gapped signer in production)
   cast wallet new   # PAUSER drill key (ops transport holds ONLY this one)
   cast wallet new   # OPERATOR drill key (bot submitter)
   ```
3. Faucet dust: a few Sepolia ETH per drill address (public Base/Sepolia
   faucets). Verify balances with a read-only call before proceeding.
4. Verify the CURRENT Sepolia addresses you depend on (routers, Vault, USDC
   test token) on Sepolia Basescan. Do NOT assume mainnet addresses (`docs/FREEZE.md`) exist on Sepolia. Record what you
   used in your drill
   notes (off-repo).

## 2. Deploy (owner, Sepolia, dust)

1. `forge build` green locally first.
2. Deploy from the OWNER drill address with a manual, reviewed command. Sepolia
   mock deploys use `script/deploy-mocks-sepolia.sh` (chain-id gated to 84532);
   any other mainnet-class action is typed by a human, never by a broadcast helper:
   ```bash
   forge create contracts/FlashArbExecutor.sol:FlashArbExecutor \
     --rpc-url "$BASE_SEPOLIA_RPC_URL" \
     --account <OWNER_DRILL_ACCOUNT> \
     --constructor-args <SEPOLIA_VAULT> <SEPOLIA_USDC_TEST_TOKEN>
   ```
3. Record the deployed address. Confirm `owner`, `VAULT`, `USDC`,
   `maxFlashUSDC` with read-only `cast call`s.

## 3. Configure (owner)

1. `setTokenAllowed` for the Sepolia test tokens (non-tax only).
2. `setRouterAllowed` for the Sepolia router (s) under test.
3. `setOperator(<OPERATOR_DRILL>)`, `setPauser(<PAUSER_DRILL>)`.
4. `setMaxFlashUSDC` to a DUST cap (e.g. the equivalent of a few dollars).
5. Optional: `setRouterCodehashPinned` for each router from
   `cast code --rpc-url "$BASE_SEPOLIA_RPC_URL" <router> | keccak`.

## 4. Pause drill (pauser key only)

1. As the OPERATOR drill key, attempt a dust `execute` dry-run first (`eth_call` / fork simulation — must show PASS with
   profit ≥ min).
2. As the PAUSER drill key, call `pause()`.
   Expected: `paused == true`; any `execute` now reverts `EnforcedPause`.
3. Confirm the pauser key CANNOT do anything else: `unpause`, `sweep`,
   `setRouterAllowed`, `setMaxFlashUSDC` must all revert (`NotOwner`).
   This is the Discord safety property (`discord_exposed(Resume) == false`).

## 5. Sweep drill (owner key only)

1. While paused, send dust test tokens to the executor (a few cents).
2. As OWNER, call `sweep(<token>, <owner-recipient>, <amount>)`.
   Expected: balances move, `Swept` event, executor drained.
3. `sweepETH` only if forced ETH is present; normally expect zero.
4. Recovery (`sweep`) MUST stay live while `paused` — verify an `execute`
   still reverts while sweep succeeds.

## 6. Redeploy drill

1. Change nothing on the live drill instance. Deploy a FRESH instance (§2)
   with the same constructor args.
2. Re-apply §3 configuration; re-run the pause + sweep checks (§4–§5) on the
   fresh instance.
3. Old instance: leave paused, sweep it to zero, abandon it. Never reuse a
   drill instance for anything real — redeploy is the upgrade path (see `docs/ADR-001-immutable-monolith.md`: immutable
   monolith, no proxy).

## 7. Pass criteria (all must hold)

- [ ] Pause reverts `execute` with `EnforcedPause`.
- [ ] Pauser key cannot unpause / sweep / allowlist / change caps.
- [ ] Sweep recovers dust while paused; executor ends at zero.
- [ ] Fresh redeploy configures, pauses, and sweeps identically.
- [ ] No secret was pasted into a file, a URL, a log, or this repo (`git status` clean of `config/local.toml`, `.env`).

## 8. Rollback / abort

- Any unexpected revert, balance movement, or address mismatch → `pause()`
  immediately (pauser key is enough), then `sweep` to the owner recipient,
  then stop and review. There is no "retry with more gas" step in this
  runbook: re-simulate and re-verify first, per `src/sim.rs`.

## 9. D1–D12 drill matrix (P4 — the USER runs every step)

G3 exit needs one verifiable transaction hash (or documented revert) per
case below, read-only verified and recorded in
`artifacts/readiness/sepolia-drill/onchain.json` (git-ignored; schema in
`.opencode/skills/go-live-readiness/references/evidence-schema.md`). The
agent never runs these steps: no keys, RPC endpoints, or broadcasts leave
your machine.

Conventions used below (USER-ACTION: replace each before running):

- `USER-ACTION: $BASE_SEPOLIA_RPC_URL` — your Sepolia RPC endpoint (env
  only, never committed, never pasted into the repo).
- `USER-ACTION: $EXE, $MUSDC, $MWETH, $MVAULT, $MROUTER` — addresses from
  `script/deploy-mocks-sepolia.sh` output (saved OFF-repo after §2).
- `USER-ACTION: $OWNER_DRILL, $OPERATOR_DRILL, $PAUSER_DRILL` — the three
  throwaway drill addresses (three DIFFERENT `cast wallet new` outputs).
- `USER-ACTION: onchain.json` — create
  `artifacts/readiness/sepolia-drill/onchain.json` from the template in §10
  and fill one entry per case as you complete it.

### 9.0 Gate: chain id MUST be 84532 (every drill session)

Every command session starts here. The deploy script already aborts on any
other chain id; repeat the same gate by hand before any `cast send`:

```bash
cast chain-id --rpc-url "$BASE_SEPOLIA_RPC_URL"   # expect 84532; else STOP
```

### D1 — Profitable cycle succeeds, profit stays in executor

1. Dry-run first (must show profit ≥ min): `cast call --rpc-url
   "$BASE_SEPOLIA_RPC_URL" $EXE "execute(address,uint256,uint256,bytes[])"
   ...` with the drill amount (dust, e.g. a few dollars of mUSDC), or the
   equivalent `eth_call` at latest head.
2. Broadcast from the OPERATOR drill key only:
   `cast send --rpc-url "$BASE_SEPOLIA_RPC_URL" --private-key
   "$OPERATOR_DRILL_KEY" $EXE "execute(...)" ...` (USER-ACTION: key from
   YOUR env; the agent never sees it).
3. Verify read-only: `cast receipt <tx> --rpc-url "$BASE_SEPOLIA_RPC_URL"`
   (status 1, `to` == `$EXE`), `cast call ... "maxFlashUSDC()"`, and the
   mUSDC balance of `$EXE` increased.
4. Record `D1: {tx, expect: success}` in onchain.json.

### D2 — Unprofitable cycle reverts, only gas spent

1. Pick an amount the estimator says clears BELOW min-net (or a flat
   leg): `cast call` must show revert / below-min.
2. Broadcast the same calldata from the OPERATOR drill key; expect revert.
3. Verify: `cast receipt <tx>` shows status 0; executor mUSDC balance
   UNCHANGED (atomic revert); only Sepolia ETH gas spent.
4. Record `D2: {tx, expect: revert}`.

### D3 — Slippage breach reverts (min-out both legs)

1. Set the leg min-out ABOVE the quoter return (forced breach).
2. `cast send` from OPERATOR; expect revert.
3. Verify receipt status 0 + unchanged balances; record `D3: {tx, expect:
   revert}`.

### D4 — Expired deadline reverts

1. Submit with a deadline in the past (dual-layer deadline: executor
   re-checks on-chain).
2. Expect revert; verify receipt status 0; record `D4: {tx, expect:
   revert}`.

### D5 — Non-allowlisted router, selector, token, or pool reverts

1. Attempt `execute` routing through an address NEVER allowlisted in §3
   (do NOT enable it first — the point is the revert).
2. Expect revert (`NotAllowed`-class); verify receipt status 0; record
   `D5: {tx, expect: revert}`.

### D6 — Callback from a non-Vault caller reverts

1. Call the executor callback entrypoint directly (not via `$MVAULT`).
2. Expect revert (callback auth); verify receipt status 0; record `D6:
   {tx, expect: revert}`.

### D7 — Operator cannot unpause, sweep, or change allowlists/limits

1. From the OPERATOR drill key, attempt each of: `unpause()`,
   `sweep(...)`, `setRouterAllowed(...)`, `setMaxFlashUSDC(...)`.
2. Every one must revert (`NotOwner`); verify each receipt status 0.
3. Record `D7: {txs: [...], expect: revert}` (one hash per attempt).

### D8 — Pauser pauses; trades revert while paused; only owner unpauses

1. From the PAUSER drill key: `cast send ... $EXE "pause()"`.
2. Verify `cast call ... "paused()"` returns true; a dust `execute` now
   reverts `EnforcedPause` (receipt status 0).
3. Confirm the pauser CANNOT `unpause` (reverts); unpause from OWNER.
4. Record `D8: {tx, expect: success, note: "pause by pauser"}` plus the
   revert hash of the paused `execute`.

### D9 — Owner sweeps profit to the owner address

1. While paused (§4 flow), OWNER calls `sweep($MUSDC, $OWNER_DRILL,
   <amount>)` (USER-ACTION: recipient is the OWNER drill address —
   runbook-enforced per ADR-002 §4 option (a)).
2. Verify `Swept` event in `cast receipt`, executor mUSDC balance drained
   to zero.
3. Record `D9: {tx, expect: success}`.

### D10 — Fee>0 flash path repays amount + fee

1. If `$MVAULT` exposes a fee setter (mock): set a NONZERO fee, run a
   profitable cycle, verify repay == amount + fee and the drill still
   profits (or reverts atomically when it cannot).
2. Reset the mock fee to 0 afterwards (or redeploy per §6).
3. Record `D10: {tx, expect: success-or-revert, note: fee used}`.

### D11 — Bot run end to end (user-driven; bot arms, you submit)

The P4 live binary arms and verifies but does NOT broadcast: the submit
below is YOUR `cast send` (throwaway operator key), and the
submit→book→reconcile state machine it exercises is the offline-tested
`sender` path (move-enforced intent, `bookable()` gating, breaker on
mismatch).

The lock arms an *attempt* of the profile's required stage, never a
completion claim: your drill manifest MUST say `attempt_stage = "G3"`
(equal to the Sepolia profile's `stage_required`), `stage_ready = "G0"`
(the honestly completed stage), and one reasoned waiver per rung in
between (`G1`, `G2`). Anything else refuses — see the attempt/waiver
semantics in
`.opencode/skills/go-live-readiness/references/evidence-schema.md`.

1. Arm the bot (proves L1 lock + roles + float against YOUR drill
   values):
   1. Build with `--features live` on the drill commit:
      ```bash
      cargo build --bins --features live
      COMMIT="$(git rev-parse HEAD)"  # must be the drill commit; tree clean
      ```
   2. Copy the profile OFF-repo and fill YOUR drill addresses (never edit
      the committed `config/sepolia.toml` placeholders in place):
      ```bash
      mkdir -p artifacts/readiness/sepolia-drill   # git-ignored
      cp config/sepolia.toml artifacts/readiness/sepolia-drill/profile-drill.toml
      # USER-ACTION: edit profile-drill.toml — executor, vault, routers,
      # tokens, quoters, owner/operator/pauser, onchain_max_flash_usdc.
      ```
   3. Write the drill manifest (USER-ACTION: replace the three `<...>`
      hashes with YOUR computed values from step 4 — nothing below is
      pre-filled with real hashes):
      ```bash
      cat > artifacts/readiness/sepolia-drill/manifest-drill.json <<EOF
      {
        "commit": "$COMMIT",
        "chain_id": 84532,
        "stage_ready": "G0",
        "attempt_stage": "G3",
        "waived": [
          {"stage": "G1", "reason": "mock-only drill entry: shadow run deferred, G3 exit rests on D1-D12"},
          {"stage": "G2", "reason": "mock-only drill entry: fork matrix deferred, G3 exit rests on D1-D12"}
        ],
        "expires_at": "USER-ACTION: e.g. 14 days out, strict UTC YYYY-MM-DDTHH:MM:SSZ",
        "hashes": {
          "freeze_md": "USER-ACTION: sha256 of docs/FREEZE.md",
          "profile": "USER-ACTION: sha256 of YOUR profile-drill.toml",
          "cargo_lock": "USER-ACTION: sha256 of Cargo.lock"
        }
      }
      EOF
      ```
   4. Fill the hashes from YOUR files, then verify each one matches:
      ```bash
      sha256sum docs/FREEZE.md artifacts/readiness/sepolia-drill/profile-drill.toml Cargo.lock
      # paste the three digests into manifest-drill.json, then re-check:
      grep -c USER-ACTION artifacts/readiness/sepolia-drill/manifest-drill.json  # expect 0
      ```
   5. Arm (the arm covers the EXACT manifest bytes — any later edit
      invalidates it, recompute if you touch the file):
      ```bash
      export LIVE_ARM="$(sha256sum artifacts/readiness/sepolia-drill/manifest-drill.json | awk '{print $1}')"
      printf OK > artifacts/live-sepolia.kill   # flag file: exactly OK, else stopped
      # (matches kill_switch_file in your profile-drill.toml copy; run from the repo root)
      ./target/debug/live --config artifacts/readiness/sepolia-drill/profile-drill.toml \
        --manifest artifacts/readiness/sepolia-drill/manifest-drill.json \
        --eth-price-cents <USER-ACTION: fresh cents> \
        --eth-price-asof <USER-ACTION: now-secs>
      ```
      Expect `ARMED ... (no trading loop; runbooks execute)`. Refusal is a
      PASS when the reason names the failed lock element
      (build/config/arm/manifest/roles/float/signer): fix the cause, never
      bypass. The binary never prints secret values (match/mismatch only).
2. Simulate the exact calldata at latest head (`cast call`); confirm
   profit ≥ min + margin.
3. Submit via `cast send` (operator drill key); capture `<tx>`.
4. Receipt: `cast receipt <tx>` — status 1; confirmations ≥ the profile
   `confirmation_depth` before booking (reorg safety).
5. Reconcile read-only: executor + operator balances vs the ledger intent
   row; any mismatch → pause immediately (§8), do NOT proceed.
6. Emit evidence: re-run the arm with `--emit-evidence
   artifacts/readiness/sepolia-drill/evidence-d11` and confirm
   `summary.json` carries `realized_samples=0` (predictions only — the
   realized fill is YOUR receipt, verified above, never auto-claimed).
7. Record `D11: {tx, expect: success}`.

### D12 — Kill switch (file AND Discord pause) stops the bot < 1 block

1. With the bot armed (D11 step 1 state), delete the flag file content
   (write anything but `OK`) — the next arm MUST refuse (`kill switch
   engaged`), proving file-flag gating.
2. Separately: pauser `pause()` on-chain — any `execute` reverts
   `EnforcedPause` (same block the pause mines).
3. Record `D12: {pause_tx, expect: success}` plus the refused-arm log line
   (match/mismatch only — redact the manifest hash hunk if you paste it
   anywhere).

## 10. onchain.json template (USER-ACTION: fill as you finish cases)

Create `artifacts/readiness/sepolia-drill/onchain.json` (git-ignored —
never commit hashes as "proof" without the read-only checks in §9):

```json
{
  "chain_id": 84532,
  "executor": "USER-ACTION: deployed $EXE",
  "roles": {
    "owner": "USER-ACTION: $OWNER_DRILL",
    "operator": "USER-ACTION: $OPERATOR_DRILL",
    "pauser": "USER-ACTION: $PAUSER_DRILL"
  },
  "tokens": {
    "mUSDC": "USER-ACTION: deployed $MUSDC",
    "mWETH": "USER-ACTION: deployed $MWETH"
  },
  "cases": {
    "D1": { "tx": "USER-ACTION", "expect": "success" },
    "D2": { "tx": "USER-ACTION", "expect": "revert" },
    "D3": { "tx": "USER-ACTION", "expect": "revert" },
    "D4": { "tx": "USER-ACTION", "expect": "revert" },
    "D5": { "tx": "USER-ACTION", "expect": "revert" },
    "D6": { "tx": "USER-ACTION", "expect": "revert" },
    "D7": { "txs": ["USER-ACTION"], "expect": "revert" },
    "D8": { "tx": "USER-ACTION", "expect": "success", "note": "pause by pauser" },
    "D9": { "tx": "USER-ACTION", "expect": "success" },
    "D10": { "tx": "USER-ACTION", "expect": "success-or-revert" },
    "D11": { "tx": "USER-ACTION", "expect": "success" },
    "D12": { "pause_tx": "USER-ACTION", "expect": "success" }
  }
}
```

Per-case auditor verification (read-only, RPC from YOUR env, never
printed): `cast chain-id` matches 84532; `cast receipt <tx>` status matches
`expect` and `to` is `$EXE` (or `$MVAULT` for D6); `cast tx <tx>` sender
matches the intended role; expected events present (`Swept`, `Paused`);
executor codehash equals the drill deploy (`cast code $EXE | cast keccak`).
A case without a verifiable hash is INCONCLUSIVE (G3 exit needs all 12).

## 11. G3 exit checklist (all must hold)

- [ ] D1–D12 each have a verifiable hash in onchain.json (auditor
  re-ran the §10 checks read-only).
- [ ] Ledger rows (arming events + D11 evidence) reconcile with on-chain
  balances to the wei.
- [ ] `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test`, `cargo test --features live`, `forge test`
  green on the drill commit.
- [ ] No secret pasted into a file, URL, log, or this repo (`git status`
  clean of `config/local.toml`, `.env`, profile copies, onchain.json).
