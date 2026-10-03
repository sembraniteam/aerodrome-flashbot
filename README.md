# base-flash-arb

Flash-loan cross-DEX arbitrage on Base (chain id 8453): Aerodrome Slipstream
vs Uniswap V3, flash execution via the Balancer V2 Vault (0% flash fee on
Base), built on `alloy` — with a Discord control plane (alerts + pause only).

Two Rust binaries share one library crate (`src/lib.rs`): `base-flash-arb`
(paper-trading harness, simulate-only) and `discord-bot` (ops CLI holding the
pauser key only). On-chain settlement lives in
`contracts/FlashArbExecutor.sol` (atomic flash-loan executor, immutable
monolith — see `docs/ADR-001-immutable-monolith.md`).

> **Safety first:** dry-run is the default and only implemented mode. This
> repo never handles real private keys, never signs, never broadcasts, and
> never touches mainnet state. Live execution paths do not exist yet; adding
> one requires an explicit user-owned config flag plus a separate, reviewed
> code path.

## Workspace layout

| Path                  | What                                                                                                                      |
|-----------------------|---------------------------------------------------------------------------------------------------------------------------|
| `src/main.rs`         | `base-flash-arb` binary: simulate-only harness, metrics to stdout/CSV                                                     |
| `src/bin/discord-bot.rs` | `discord-bot` binary: ops CLI (status/alerts/pause only, pauser key; never resume)                                     |
| `src/lib.rs`          | Shared library crate used by both binaries                                                                                |
| `src/config.rs`       | `BotConfig` + verified Base addresses + allowlisted pairs                                                                 |
| `src/chain.rs`        | `BlockFeed` trait (live canonical `newHeads` vs Flashblocks stub), WS provider, L1 fee oracle binding, URL redactor       |
| `src/pools.rs`        | CL pool state, Slipstream/UniV3 fee schedules, `sol!` bindings (pool / Balancer Vault), integer-only estimator            |
| `src/profit.rs`       | Two-direction quoter harness, full cost model (incl. injectable live L1 fee), break-even + optimal size                   |
| `src/risk.rs`         | Hard limits (`size <= max <= hard_max`), circuit breaker, kill switch (fail closed)                                        |
| `src/sim.rs`          | Dry-run `eth_call` decision logic, off-chain vs on-chain verification                                                     |
| `src/metrics.rs`      | Hit-rate, revert rate, p95 latency, summary + per-pair/direction CSV export                                               |
| `src/alerts.rs`       | Discord webhook alerts (amounts only, rate-limited, drop-not-block)                                                        |
| `src/sequencer.rs`    | SequencerUptimeFeed gate (fail closed, 3600 s grace)                                                                      |
| `src/fork_check.rs`   | `--fork-check` / `--fork-matrix` read-only fork verification                                                              |
| `src/discord.rs`      | Control-plane stub: role-gated, read-only by default, audited                                                             |
| `contracts/`          | `FlashArbExecutor.sol`: atomic flash-loan executor (owner/operator/pauser roles, allowlists, codehash monitor)            |
| `test/`               | Forge unit + fork tests (incl. `mocks/` for offline runs)                                                                 |
| `script/`             | `deploy-mocks-sepolia.sh`: mock-stack drill deploy (Base Sepolia only, chain-id gated)                                    |
| `docs/`               | Runbook, freeze manifest, ADR, Discord setup (see below)                                                                  |
| `config/default.toml` | Testnet-safe defaults (`dryRun=true`, $500 max flash, $5 min profit)                                                      |

Key defaults: `dry_run=true`, `max_flash_usdc=$500`, `min_net_profit_usdc=$5`,
`max_slippage_bps=50`, `daily_loss_cap_usdc=$100`,
`max_consecutive_failures=3`, `feed_mode="canonical"` (post-Denim default;
see `config/default.toml`), `head_staleness_blocks=5`. Allowlist:
`WETH/USDC`, `AERO/USDC`, `AERO/WETH` (+ a disabled `cbETH/ETH`
placeholder pending a verified address). The paper loop polls the canonical
`newHeads` feed once per run for the risk head and falls back to a logged
fixture head when offline; `--l1-fee` injects a measured `getL1Fee` value
into the cost model (fixture default otherwise, source logged).

## How to run the dry-run

```bash
cargo run -- --config config/default.toml --csv results.csv
```

No private key needed. The binary forces dry-run even if the config says
otherwise, redacts RPC URLs in logs, and exits after printing metrics, e.g.:

```text
seen=3 sim=3 won=3 hit_rate=1.000 revert_rate=0.000 ...
p95_evaluate_us=... hit_rate=1.000 revert_rate=0.000
```

Override the endpoint label for a fork run without editing the config:

```bash
cargo run -- --config config/default.toml --fork-url ws://127.0.0.1:8545
```

## Fork test (paper mode against real Base state)

The paper binary needs no key; `eth_call` reads work against a local fork:

1. Start anvil against a **public** Base RPC (ask before starting it; never
   paste a URL containing a secret — put secrets in the environment, never
   in `config/`):
   ```bash
   anvil --fork-url "$BASE_RPC_URL" --port 8545
   ```
2. Copy `config/default.toml` to `config/local.toml` (gitignored) and set
   `rpc_ws_url = "ws://127.0.0.1:8545"`.
3. Run the harness against the fork:
   ```bash
   cargo run -- --config config/local.toml --csv results.csv
   ```
4. Expected: exit 0, `results.csv` with a summary row plus a
   `pair,direction,...` per-pair/direction breakdown section, stdout
   metrics. Current fork wiring status: the WS probe in `main` verifies
   connectivity; pool-state loading (`slot0`/`liquidity`/`fee()` via
   multicall) and quoter `eth_call` comparison via `sim::verify_quote` are
   the next fork-data milestone — the paper run still uses deterministic
   fixtures until that lands (see “What still needs fork data”).

## Fork quoter check (read-only: real QuoterV2 vs off-chain estimator)

`--fork-check` compares the on-chain `quoteExactInputSingle` result against
the integer-only `estimate_amount_out` (`src/pools.rs`) for the allowlisted
pairs `WETH/USDC` and `AERO/USDC` at $100 / $500 / $1000 (USDC → token
direction, amounts in 6dp USDC base units) plus `AERO/WETH` at 0.05 / 0.2 /
0.5 WETH (WETH → AERO direction, amounts in 18dp WETH base units — both
tokens have 18 decimals, so USDC notionals do not apply), on both venues:

- Aerodrome Slipstream QuoterV2 (`0x254c...15b0`, pool keyed by `tickSpacing`,
  live `fee()` read for the estimator),
- Uniswap V3 QuoterV2 (`0x3d4e...B76a`, pool keyed by fee tier).

Pools are discovered via factory `getPool` across Slipstream spacings
`[1, 50, 100, 200, 2000]` and UniV3 tiers `[100, 500, 3000, 10000]`; the
deepest-liquidity pool wins per venue. No keys, no signing, no broadcast —
pure `eth_call`s. Prints `pair, venue, pool, size, amount_in, fee_used,
quoter_out, estimator_out, diff_bps, PASS/FAIL` vs `--tolerance-bps`
(default 50). Exits 0 and prints `SKIPPED` when no RPC is reachable, so
offline CI stays green.

```bash
anvil --fork-url https://mainnet.base.org --port 8545
FORK_URL=http://127.0.0.1:8545 cargo run -- --fork-check
# flag form + tolerance override:
cargo run -- --fork-check --fork-url http://127.0.0.1:8545 --tolerance-bps 50
# direct public endpoint works too (rate-limited; local fork preferred):
FORK_URL=https://mainnet.base.org cargo run -- --fork-check
```

No secrets needed. Measured 2026-10-03 against `https://mainnet.base.org`:
**12/12 rows within 50 bps** (WETH/USDC both venues 0 bps; AERO/USDC aero
0 bps; AERO/USDC uni 3/10/18 bps, growing with size as the single-tick
estimator ignores cross-tick price impact). Note: that run predates the
`AERO/WETH` rows (3 pairs × 2 venues × 3 sizes = 18 rows now); re-measure
before quoting a new tally. The check also caught two real
bugs before that run: a swapped `amountIn`/`tickSpacing` field order in the
Aero quoter binding (reverted on-chain) and a `U256` overflow in the
token0→token1 estimator leg (saturated to a constant at 18dp/6dp scales).

## Fork matrix (read-only: quoter + Vault/sandwich/deadline/L1/codehash/sequencer)

`--fork-matrix` runs the quoter table above plus the No.4 matrix (20 rows):
Vault fee>0 path (code presence + generic `repay = amount + fee` math; the
fee>0 execution path itself is covered offline by `MockVault::setFeeBps`),
sandwich drill (live $500 WETH/USDC aero quote shocked 10/25/50 bps — the
gate must say REVERT, not lose), outer + Aero-inner deadlines against live
chain time, Cancun/transient note, L1 `getL1Fee` per size via
`0x4200…000F`, a `codehash` row per pinned address (feeds `docs/FREEZE.md`),
and the Chainlink SequencerUptimeFeed gate (refuse execute while down or
in the 3600 s grace period; fail-closed on error). Same skip-gracefully
contract as `--fork-check`:

```bash
FORK_URL=https://mainnet.base.org cargo run -- --fork-matrix
```

Config additions: `private_rpc_url` (optional private-relay endpoint,
redacted in logs, absent in paper mode), `deadline_secs` (default 30,
range 1–300), `max_in_flight` (pinned to 1). The sequencer gate lives in
`src/sequencer.rs` (Base feed `0xBCF8…6433`, verified against
`docs.chain.link/data-feeds/l2-sequencer-feeds`); the Discord transport
holds the pauser key only — unpause/allowlist/sweep are never Discord
commands (`src/discord.rs`: `discord_exposed`).

Docs: `docs/RUNBOOK_SEPOLIA.md` (pause→sweep→redeploy drill on Base Sepolia
with dust, no secrets), `docs/FREEZE.md` (`forge build --sizes` + pinned
addresses/selectors/chain id + codehash capture), `docs/ADR-001-immutable-monolith.md`
(immutable monolith, no UR in production, USDC-only no oracle,
operator/pauser vs multisig-timelock).

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --bins
forge build
forge test
```

Off-chain math vs on-chain quote: enforced at runtime by
`sim::verify_quote` (tolerance-banded) and measured by `--fork-check`
(12/12 within 50 bps on mainnet state, 2026-10-03) — any systematic
mismatch is treated as a bug, not noise.

## What still needs fork data

- L1 data-fee oracle read (`getL1Fee`) auto-fetch: `--l1-fee` already injects
  a measured value into `CostModel::l1_data_fee` (fixture default otherwise,
  source logged); a live oracle poll per attempt is still a follow-up.
- Continuous WebSocket head streaming: `CanonicalFeed::next_head` holds a
  live `newHeads` subscription with reconnect + gap/reorg detection, and the
  paper loop polls it once per run; a long-lived streaming runner is still a
  follow-up. Re-verify `pending`-tag behavior against `docs.base.org`
  (Denim upgrade may have replaced Flashblocks with canonical 200ms blocks
  — select via `feed_mode`, now defaulting to `canonical`).
- Measured (non-fixture) profitability evidence: only dry-run/fork numbers
  count, never compilation.
