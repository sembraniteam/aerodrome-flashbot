# Live-Path Design Contract

What the code must satisfy before `/readiness` can report any stage above G2. Implementers build to this contract;
auditors check it as invariants **L1 to L10**. The contract keeps S1 to S10 intact: dry-run remains the default and the
paper binary never changes behavior.

## Architecture

```
paper binary (default build)      live binary (feature "live", off by default)
  forces dry_run = true             requires the Live Lock (below)
  no signer, no sender              Signer trait -> operator key only
                                    Sender (nonce, gas caps, receipts)
        \                                   /
         shared: feeds, estimator, risk, sequencer gate, sim::verify_quote, ledger
```

- Live code sits behind `#[cfg(feature = "live")]` in its own modules and its own binary (`src/bin/live.rs`).
  `cargo build --bins` without the feature must contain no signer or sender symbols.
- The strategy pipeline depends on traits (`Feed`, `Simulator`, `Executor`, `Clock`, `Notifier`, `Ledger`). Dry-run
  wires a `NoopExecutor`; live wires `OnchainExecutor`. Dry-run can never reach `OnchainExecutor`.

## L1: Live Lock (all four must hold, checked at startup, refuse to start otherwise)

1. **Build lock**: binary built with `--features live`.
2. **Config lock**: the chosen profile sets `[live] enabled = true`. `config/default.toml` keeps `dry_run = true` and
   has no live section enabled.
3. **Arm lock**: env `LIVE_ARM` equals the SHA-256 of the current readiness manifest. A missing or mismatched value
   means refuse.
4. **Manifest lock**: the manifest (see `evidence-schema.md`) names a `commit` equal to the commit embedded at build
   time, `chain_id` equal to the profile and to the RPC's `eth_chainId`, a `stage_ready` at least the stage required
   by the profile (84532 needs G3 drill-ready inputs, 8453 canary needs G4, mainnet ramp needs G5), and `expires_at`
   in the future.

The lock reads the manifest; it does not trust it blindly. It recomputes the hashes of `docs/FREEZE.md`, the profile,
and `Cargo.lock` and compares them with the manifest values.

## L2: Chain profiles

- `config/sepolia.toml` (84532) and `config/mainnet-canary.toml` (8453). Each pins `chain_id`, executor address,
  Vault, routers, tokens, and the compiled `hard_max` set it may not exceed.
- Mainnet addresses never appear in the Sepolia profile and vice versa (S8). A test asserts disjointness.
- Deploy and drill scripts abort on a chain id other than the one they are written for.

## L3: Signer isolation

- A `Signer` trait with implementations for an env-held key (testnet only) and a remote signer or KMS (mainnet).
  Mainnet profile rejects the env-key implementation.
- The bot loads the **operator** key only. Owner and pauser keys are never read by the trading binary. The Discord bot
  reads only `PAUSER_KEY` (S3).
- Keys never appear in logs, errors, metrics, or the ledger. Errors pass through the existing URL and secret redactor.
- Operator balance is gas only. A startup check refuses to run if the operator holds more than the profile's float cap.

## L4: Executor roles and caps (on-chain)

- Owner, operator, pauser stay separate. Operator may only call the trade entry point. Pauser may only pause.
  Unpause, sweep, allowlist, and limit changes are owner-only. Sweep goes to the owner address only.
- The executor enforces its own on-chain per-trade and size limits; the off-chain `hard_max` is lower than or equal to
  the on-chain limit. Limits can only be changed by the owner and are covered by events.
- Role matrix read back from chain at startup and compared with the profile; mismatch means refuse.

## L5: Pre-submit pipeline (every step fail-closed)

1. Sequencer uptime gate and grace period.
2. Head freshness and provider agreement (two providers).
3. Opportunity estimate with integer math and measured L1 fee; net must exceed `min_net_profit_usdc` plus safety margin.
4. `sim::verify_quote` against the quoter within tolerance.
5. Risk gate: size limits, daily loss cap, consecutive failures, `max_in_flight = 1`.
6. Final `eth_call` of the exact calldata at the latest head, then `estimateGas`; gas limit equals estimate plus margin,
   under the per-attempt gas cap; max fee and priority fee under caps; L1 fee under cap.
7. Persist the intent to the ledger **before** submit. Only then sign and send.

Any `Err`, timeout, or ambiguity rejects the opportunity with a logged reason. No `unwrap_or(true)` style defaults.

## L6: Sender and reconciliation

- One nonce owner with recovery after dropped or replaced transactions; no blind retries; a retry re-runs L5 first.
- Every transaction is tracked to a terminal state (confirmed, reverted, dropped) and booked only after the configured
  confirmation depth.
- After each transaction and at least daily, reconcile ledger totals against on-chain balances (executor and operator).
  Any mismatch trips the circuit breaker.
- On startup, reconcile any non-terminal intent before allowing new trades.

## L7: Circuit breaker and kill switch

- Breaker trips on: daily loss cap, `max_consecutive_failures`, realized-versus-predicted drift beyond band,
  reconciliation mismatch, codehash drift, sequencer or provider faults beyond policy.
- Kill switch: a file flag and the on-chain pause; both checked before every submit; unreadable flag means stopped.
- Breaker state persists across restarts. Reset is manual, by the owner, with a reason written to the ledger.
- The control plane (Discord) is separate from the trading path, pause-only, and its outage never affects the kill
  switch (S3).

## L8: Append-only ledger (the raw material of evidence)

- JSON Lines, one record per opportunity, intent, submission, receipt, reconciliation, breaker event, and operator
  action. Each record includes `prev_hash` and `hash = sha256(prev_hash || canonical_json)` so tampering is detectable.
- Fields cover predicted gross, each cost component, predicted net, realized net, tx hash, block number, gas used,
  L1 fee paid, gate that rejected, and the commit hash.
- The ledger contains no secrets or URLs. Files live under `artifacts/` and are git-ignored (S5).
- A `ledger verify` subcommand re-computes the chain and prints PASS or the first broken index.

## L9: Evidence emission

- `live` and paper binaries support `--emit-evidence <dir>`: writes a run summary (window, counts, rejection reasons,
  predicted versus realized, breaker events, ledger head hash) as JSON. The auditor consumes this; it never edits it.
- Summaries state sample sizes and never present predicted numbers as realized.

## L10: Tests (all offline, deterministic)

- Lock refuses on each missing or mismatched element (build, config, arm, manifest commit, chain id, stage, expiry).
- Paper binary still forces dry-run with a config that says `dry_run = false`.
- Each L5 step has a rejection test; failed simulation means no submission; dry-run never reaches the executor.
- Breaker trips and persists; kill switch unreadable means stopped; reconciliation mismatch trips.
- Ledger hash chain detects tampering; `ledger verify` fails on an edited record.
- Role-matrix mismatch refuses startup; operator over float cap refuses startup.
- Solidity: roles, caps, atomic revert, fee>0 path, callback authentication, pause behavior (existing mocks).
