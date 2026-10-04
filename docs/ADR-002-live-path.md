# ADR-002: Live execution path (testnet → mainnet canary)

- Status: proposed (P1 design record; no code in this phase)
- Date: 2026-10-04
- Scope: off-chain live path (`src/live/`, `src/bin/live.rs`, `live` feature, chain profiles) and its on-chain seams
- Builds to: `references/live-contract.md` (L1–L10); preserves S1–S10 with dry-run the default
- Companion: ADR-001 (immutable monolith), `docs/FREEZE.md`, `docs/RUNBOOK_SEPOLIA.md`, promotion ladder (G0–G6), risk playbook
- P3 implementation note (2026-10-04): the §1–§11 design is implemented as specified — `live` feature + `src/live/`
  (gated `mod live`) + `src/bin/live.rs` (`required-features`), `config/sepolia.toml` + `config/mainnet-canary.toml`
  (`[live]` per §2), L1 lock (build/config/arm/manifest, constant-time `LIVE_ARM`, strict load order with signer last),
  L3 (`EnvKeySigner` Sepolia-only + `RemoteSigner` mainnet-only stub, trading custody guard, `redact_secret`/zeroize/
  custom `Debug`), L4 role read-back, L5 pipeline wiring, L6 nonce sender + reconciliation, L7 breaker + kill switch,
  L8 hash-chained JSONL ledger + `ledger verify`, L9 `--emit-evidence` on both binaries, full L10 offline suite
  (`cargo test` 115 + `cargo test --features live` 171 green; `forge test` 71 green, contract untouched).
  `docs/FREEZE.md` untouched (no contract change in P2, none in P3). Runbooks and the trading loop land in P4.

## Context

The repo today is a dry-run harness plus an immutable executor:

- Paper binary (`src/main.rs`) forces `dry_run = true` by assignment even if config says `false` (S2; `src/main.rs:126`).
- `config/default.toml` keeps `dry_run = true`; `BotConfig::validate` enforces chain id ∈ {8453, 84532}, `max_flash_usdc ≤ HARD_MAX_FLASH_USDC` (`src/risk.rs:14`, currently 5_000_000_000 = $5000), `max_in_flight == 1`, `deadline_secs` 1..=300.
- `contracts/FlashArbExecutor.sol` is immutable (no proxy), USDC-only flash, operator-may-execute-only, pauser-may-pause-only, owner-only everything else, two-step ownership, `MAX_FLASH_USDC_HARD_CAP = 10_000_000_000_000` ($10M), per-trade `maxFlashUSDC` (default $5000), events for every allowlist/cap/role/pause/sweep change, `RouterCodeChanged` enforcement when pinned.
- `docs/FREEZE.md` pins mainnet (8453) addresses, selectors (`0xa026383e`, `0x04e45aaf`; UR explicit-selector-only), codehashes, solc 0.8.37 cancun, executor 14,274 / 24,576 B.
- No live path exists: no `src/live/`, no `src/bin/live.rs`, no `live` feature, no `config/sepolia.toml` / `config/mainnet-canary.toml`, no ADR-002. This record is the design P2/P3 will implement.

## Decision

Build the live path exactly as drawn in the live contract, in three P3 pieces behind one P1 lock design:

```text
paper binary (default build)      live binary (feature "live", off by default)
  forces dry_run = true             requires the Live Lock (§1)
  no signer, no sender              Signer trait -> operator key only
                                    Sender (nonce, gas caps, receipts)
        \                                   /
         shared: feeds, estimator, risk, sequencer gate, sim::verify_quote, ledger
```

Strategy code depends on traits (`Feed`, `Simulator`, `Executor`, `Clock`, `Notifier`, `Ledger`). Dry-run wires a `NoopExecutor`; live wires `OnchainExecutor`. Dry-run can never reach `OnchainExecutor` (compile-time wiring, plus an L10 test asserting it).

### 1. L1 — Live Lock (all four hold at startup, else refuse)

Checked in this order; the first failure stops startup with a logged reason naming the failed element (never the secret value):

1. **Build lock**: binary built with `--features live`. The `live` binary target requires the feature; the paper binary never sets it.
2. **Config lock**: the chosen profile sets `[live] enabled = true`. `config/default.toml` keeps `dry_run = true` and gains no live section.
3. **Arm lock**: env `LIVE_ARM` equals the SHA-256 of the current readiness manifest. Missing or mismatched → refuse. The value is a hash, safe to compare in logs by match/mismatch only.
4. **Manifest lock**: the manifest names `commit` == build-embedded commit, `chain_id` == profile and == RPC `eth_chainId`, `stage_ready` ≥ the profile's required stage (84532 needs G3 drill-ready inputs; 8453 canary needs G4; mainnet ramp needs G5), and `expires_at` in the future. The lock recomputes hashes of `docs/FREEZE.md`, the profile file, and `Cargo.lock` and compares them to manifest values — it reads the manifest, it does not trust it blindly.

Strict startup order in `src/bin/live.rs`: L1 lock → role read-back (L4) → float-cap check → load `Signer` last. No key or KMS handle is loaded before L1 passes; each L1 failure asserts the signer constructor was never called (L10 test). `LIVE_ARM` comparison uses constant-time equality and logs match/mismatch only, never values.

### 2. L2 — Chain profiles

- `config/sepolia.toml` (84532) and `config/mainnet-canary.toml` (8453). Each pins `chain_id`, executor address, Vault, routers, tokens, quoters, sequencer feed, and the compiled `hard_max` set it may not exceed.
- Mainnet addresses never appear in the Sepolia profile and vice versa (S8); an offline test asserts disjointness.
- Deploy and drill scripts abort on any chain id other than the one they are written for (`RUNBOOK_SEPOLIA.md` scripts already gate to 84532; the canary runbook in P4 gates to 8453).
- Profile keys (new `[live]` section; everything else reuses `BotConfig` validation, which already enforces `size ≤ max ≤ hard_max`, `max_in_flight == 1`, deadline range):
  `[live] enabled, stage_required, executor, vault, routers[], tokens[], quoters[], sequencer_feed, hard_max_flash_usdc, hard_daily_loss_cap_usdc, hard_max_consecutive_failures, operator_float_cap_usdc, confirmation_depth, kill_switch_file, ledger_dir`.

### 3. L3 — Signer isolation

- `Signer` trait with two concrete types: `EnvKeySigner::new` asserts `chain_id == 84532` else refuses (Sepolia only); mainnet (8453) accepts `RemoteSigner`/KMS only, enforced at startup by `ensure!(signer_kind == remote || chain_id != 8453)` with an L10 refusal test per combination.
- The trading binary loads the **operator** key only and mirrors the Discord custody guard: `guard_trading_custody()` refuses startup when `OWNER_KEY` or `PAUSER_KEY` is set; `Signer::from_env` reads only `OPERATOR_KEY`. Owner and pauser keys are never read by it. The Discord bot reads only `PAUSER_KEY` (S3, unchanged).
- Keys never appear in logs, errors, metrics, or the ledger. `src/chain.rs::redact_url` covers URLs only, so P3 adds a separate `redact_secret()` for hex keys and secret-shaped values, sanitizes `anyhow` signer errors through it, gives signers a custom `Debug` printing address/label only (never key material), never logs free-form `Debug` into ledger/evidence, and `zeroize`s key buffers. L10 test asserts a fake key string never appears in logs or ledger.
- Operator balance is gas only: checked at startup, before every submit, and in L6 reconciliation (mismatch trips the breaker). The USD-equivalent conversion is fail-closed (`Err`/stale price → refuse). Canary cap: ≤ $20 equiv.
### 4. L4 — Executor roles and caps (on-chain; gap verdict)

Role matrix (current contract verdict in brackets):

| Action                                                  | Owner          | Operator        | Pauser          |
|---------------------------------------------------------|----------------|-----------------|-----------------|
| `execute`                                               | yes            | yes (only this) | no              |
| `pause`                                                 | yes            | no              | yes (only this) |
| `unpause`, `sweep`, allowlist, limit changes            | yes (only)     | no              | no              |
| `transferOwnership`/`acceptOwnership`/`cancelOwnership` | yes (two-step) | no              | no              |

- Verdict: the contract **already implements** the L4 matrix — `onlyOperatorOrOwner` on `execute`, `onlyPauserOrOwner` on `pause`, `onlyOwner` on `unpause`/`sweep`/allowlist/cap setters, two-step ownership, pause gates `execute` only while `sweep` stays live, events on every change (`OperatorUpdated`, `PauserUpdated`, `RouterAllowlistUpdated`, `RouterSelectorUpdated`, `RouterCodehashPinnedUpdated`, `TokenAllowlistUpdated`, `AllowFeeOnTransferUpdated`, `MaxFlashUSDCUpdated`, `Paused`/`Unpaused`, `Swept`). **No P2 contract change is required for L4.**
- On-chain caps already satisfy "off-chain `hard_max` ≤ on-chain limit": constructor `maxFlashUSDC` = $5000 mirrors `HARD_MAX_FLASH_USDC`; `setMaxFlashUSDC` is capped by `MAX_FLASH_USDC_HARD_CAP` ($10M) with `ExceedsHardCap` (owner may raise or lower within the cap, 0 = halt; exceeding the constant is a redeploy).
- Known non-blocking observation (P2 decision item, NOT a gap): `sweep(token, to, amount)` takes an owner-chosen `to` rather than hard-coding the owner address, while the live contract phrases sweep as "to the owner address only". Against owner-key compromise, `to == owner` enforcement adds nothing (whoever holds the owner key *is* the owner); it only guards owner fat-finger. Options for P2: (a) keep the contract as-is and enforce owner-recipient by runbook + G4 role-matrix review (recommended: zero size cost, zero redeploy risk); (b) hard-code `to == owner` (costs runtime bytes from the 10,302 B margin and a fresh audit + FREEZE re-pin). Default is (a) unless the user directs otherwise.
- Startup role read-back: the live binary reads `owner`/`operator`/`pauser`/`maxFlashUSDC` from chain and compares to the profile; any mismatch → refuse (L10 test).
- Size budget: 14,274 / 24,576 B runtime leaves 10,302 B margin — headroom exists for audited fixes, not scope growth (ADR-001 stands: no proxy, redeploy is the upgrade path).

### 5. L5 — Pre-submit pipeline (every step fail-closed)

Fixed order; any `Err`, timeout, or ambiguity rejects the opportunity with a logged gate reason. No `unwrap_or(true)`-style defaults:

1. Sequencer uptime gate + 3600 s grace (existing `src/sequencer.rs`, honors fail-closed on error).
2. Head freshness (`head_staleness_blocks`, default 5) and two-provider agreement (`eth_blockNumber` + key reads compared).
3. Opportunity estimate in integer USDC base units with measured L1 fee (`getL1Fee`); net must exceed `min_net_profit_usdc` + safety margin.
4. `sim::verify_quote` against the quoter within tolerance.
5. Risk gate: size limits, daily loss cap, consecutive failures, `max_in_flight = 1`.
6. Final `eth_call` of the exact calldata at latest head, then `estimateGas`; gas limit = estimate + margin, under the per-attempt gas cap; max fee, priority fee, and L1 fee each under caps.
7. Persist the intent to the ledger **before** submit. Only then sign and send.

### 6. L6 — Sender and reconciliation

- One nonce owner; recovery after dropped/replaced transactions; no blind retries — a retry re-runs L5 from step 1.
- Every transaction tracked to a terminal state (confirmed, reverted, dropped) and booked only after `confirmation_depth`.
- After each transaction and at least daily, reconcile ledger totals against on-chain balances (executor + operator); any mismatch trips the breaker.
- On startup, reconcile non-terminal intents to terminal state before any new trade is allowed.

### 7. L7 — Circuit breaker and kill switch

- Breaker trips on: daily loss cap, `max_consecutive_failures`, realized-vs-predicted drift beyond band, reconciliation mismatch, codehash drift, sequencer/provider faults beyond policy.
- Kill switch = file flag AND on-chain pause, both checked before every submit; unreadable flag file means stopped.
- Breaker state persists across restarts (ledger-backed). Reset is manual, owner-only, with a reason written to the ledger.
- Discord control plane stays pause-only and separate; its outage never affects the kill switch (S3).

### 8. L8 — Append-only ledger (raw material of evidence)

- JSON Lines under `artifacts/` (git-ignored, S5): one record per opportunity, intent, submission, receipt, reconciliation, breaker event, operator action. Each record carries `prev_hash` and `hash = sha256(prev_hash || canonical_json)`.
- Fields: predicted gross, each cost component, predicted net, realized net, tx hash, block number, gas used, L1 fee paid, rejecting gate, commit hash. No secrets or URLs.
- `ledger verify` subcommand recomputes the chain and prints PASS or the first broken index (L10 tamper test).

### 9. L9 — Evidence emission

- Both binaries support `--emit-evidence <dir>`: run summary JSON (window, counts, rejection reasons, predicted-vs-realized, breaker events, ledger head hash). The auditor consumes it; it never edits it.
- Summaries state sample sizes and never present predicted numbers as realized (no profit promises: stages are "safe to attempt").

### 10. L10 — Offline test plan (P3 acceptance)

All offline and deterministic: lock refuses on each missing/mismatched element (build, config, arm, manifest commit, chain id, stage, expiry); paper binary still forces dry-run with `dry_run = false` on disk; each L5 step has a rejection test; failed simulation ⇒ no submission; dry-run never reaches `OnchainExecutor` (wiring test); breaker trips and persists; unreadable kill-switch file means stopped; reconciliation mismatch trips; role-matrix mismatch refuses; operator over float cap refuses; ledger tamper detected by `ledger verify`; profile address-disjointness (S8); Solidity suite keeps passing (roles, caps, atomic revert, fee>0, callback auth, pause) plus any P2 additions.

### 11. Stage caps (defaults; user may tighten; loosening is a user-only, one-rung, written-reason act)

| Stage            | `max_flash_usdc` | `daily_loss_cap_usdc` | `max_consecutive_failures` | `max_in_flight` | operator float | notes                                                     |
|------------------|------------------|-----------------------|----------------------------|-----------------|----------------|-----------------------------------------------------------|
| G3 Sepolia drill | dust (few $)     | dust                  | 3                          | 1               | gas dust       | mocks; D1–D12                                             |
| G4 pre-flight    | 0 (no capital)   | 0                     | —                          | 1               | gas only       | `eth_call` sims, pause drill                              |
| G5 canary        | 50_000_000 ($50) | 10_000_000 ($10)      | 2                          | 1               | ≤ $20 equiv    | daily sweep; 7 d + 20 attempts                            |
| G6 ramp          | ≤2× per step     | ≤2× per step          | 2                          | 1               | ≤ $20 equiv    | each step: 7 d + 20 attempts + fresh `/readiness mainnet` |

Raising a compiled `hard_max` or an on-chain limit is a code change + new FREEZE pin and returns the bot to G2. Never via Discord.

## Non-goals for the live path

- No proxy/upgrades (ADR-001), no multi-asset flash, no oracle-priced profit (USDC balance delta only), no UniversalRouter auto-allowlist, no automatic ramp, no profit forecasting as earnings.

## What P2 / P3 implement from this record

- **P2** (`@solidity-engineer` → `@solidity-auditor`): confirm the §4 verdict with tests — role matrix (operator cannot unpause/sweep/allowlist/change limits; pauser cannot unpause/sweep/allowlist/change limits), caps (`ExceedsMaxFlash`, `ExceedsHardCap`), atomic revert, fee>0 repay, callback auth (non-Vault reverts), pause behavior; decide the sweep-`to` item (default option (a)); re-pin `docs/FREEZE.md` if and only if the contract changes.
> P2 verdict (2026-10-04): contract unchanged — L4 matrix confirmed against `contracts/FlashArbExecutor.sol`, sweep-`to` stays option (a) (runbook + G4 review, no Critical flaw found); gap-fill tests in `test/FlashArbExecutorP2.t.sol` (5 tests); `docs/FREEZE.md` untouched.
- **P3** (`@base-mev-engineer` + `@rust-engineer` → `@rust-analyst`, `@security-expert`): `live` feature + `src/bin/live.rs`, profiles, `Signer` trait + sender, L5 pipeline wiring, ledger + breaker + kill switch, `--emit-evidence`, full L10 suite; `cargo build --bins` without the feature must contain no signer/sender symbols.
- **P4**: evidence emission wiring, `RUNBOOK_SEPOLIA.md` D1–D12 extension, `RUNBOOK_MAINNET_CANARY.md`, `READINESS.md`.
- **P5**: `/evaluate` then `/readiness testnet`.
> P4 implementation note (2026-10-04): P3 Info items closed without
> loosening any default/cap/invariant — (a) the live binary funnels every
> startup error through `sanitize_error` with the operator
> `SecretBlocklist` (single boundary in `src/bin/live.rs`; test-enforced
> twins cover both paste forms); (b) production `Ledger::open` takes that
> blocklist, the append boundary refuses blocklisted values naming the
> field only, and `Ledger`'s `Debug` is count-only (no blocklist leak);
> (c) bare-64hex stays heuristic-exempt by documented policy (exact
> blocklist is the enforced mechanism); `EnvKeySigner` stays non-`Clone`
> with zeroized decode buffers on every path (full `ZeroizeOnDrop` of the
> upstream credential deferred: mainnet uses the keyless `RemoteSigner`).
> Post-lock call sites wired with no new live execution: `submit` consumes
> `ApprovedIntent` by move (retry = fresh L5 run), `book_receipt` gates on
> `bookable()`, `reconcile_and_trip` trips the breaker and returns the
> `TripReason` (per-tx + daily call sites, one helper). Evidence is schema
> 2 (`window_start`/`window_end`, ordered-window + stale-schema gates,
> `live_arming` constructor; schema-1 files still parse, never re-emit).
> Live binary still refuses without the full arm and performs no broadcast.
> Runbooks are user-run (D1–D12 + canary); `docs/FREEZE.md` untouched (no
> contract change). USER-ACTION placeholders: real Sepolia addresses (drill
> profile copy), KMS handle (mainnet remote signer), oracle feed replacing
> the `--eth-price` attestation (P5).

## Consequences

- Paper behavior is untouched: default build has no signer/sender code paths reachable, and the S1 audit scan excludes exactly `src/live/` + `src/bin/live.rs` (plus the existing Discord pause exception). Signing `Signer` implementations live strictly inside `src/live/` (`#[cfg(feature = "live")]`); a shared trait definition, if any, is keyless (no env constructors).
- Every new behavior ships with refusal/fail-closed tests and same-phase doc updates (`FREEZE.md`, runbooks, README) when code changes. This phase changes no code, so `FREEZE.md` and runbooks are untouched — recorded here deliberately, not by omission.
- Live steps (deploy, fund, drill transactions) are user-run from numbered runbooks; hashes come back as evidence.

## Alternatives considered

- **One binary with a `--live` flag**: rejected — a flag is one typo from live; a separate feature-gated binary plus the four-part lock makes accidental live execution a multi-failure event.
- **Owner key on the bot host**: rejected — operator-only key on host, owner on multisig/hardware; limits blast radius of host compromise (risk playbook §1).
- **Env-key signer for mainnet**: rejected — remote signer/KMS only; env keys stay on Sepolia throwaways.
- **On-chain sweep `to == owner` hard-code now**: deferred to P2 (see §4) — policy + review covers it at zero redeploy risk.
