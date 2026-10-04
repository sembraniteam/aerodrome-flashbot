# base-flash-arb

Flash-loan arb on Base (8453): Aerodrome Slipstream vs Uniswap V3 via Balancer V2 Vault (0% fee on Base). Dry-run only —
no private key, signing, or broadcast. Stack: Rust `alloy`, Foundry, Discord pause-only.

> Live execution path exists behind a triple lock (P3, ADR-002) but performs no trades: the `live` binary
> (`--features live`) verifies the L1 lock, role read-back, float cap, ledger, and evidence emission, then exits.
> The trading loop lands in P4 with numbered runbooks. Dry-run stays the default everywhere; the paper binary
> still forces `dry_run=true`.

## Features

- Integer-only two-direction estimator, per-venue fee schedule (Slipstream `tickSpacing`, UniV3 `fee tier`)
- Full cost model: flash fee, swap fee, gas, L1 `getL1Fee` (`0x4200…000F`), slippage; `breakeven` + `optimal size`
- Fail-closed risk: `size <= max <= hard_max`, circuit breaker, kill switch, Chainlink sequencer gate `0xBCF8…6433`
  (3600s grace), `head_staleness_blocks`
- Atomic executor `contracts/FlashArbExecutor.sol`: split owner/operator/pauser, router/selector/token/pool allowlist,
  `codehash` monitor, dual-layer deadline
- Discord: rate-limited alerts (amounts only) + pause only; `discord-bot` holds `PAUSER_KEY` only
- Read-only fork verification: `--fork-check` (quoter vs estimator) and `--fork-matrix`
  (Vault/sandwich/deadline/L1/codehash/sequencer)
- Metrics: hit-rate, revert-rate, p95 latency, CSV export

## Prerequisites

- Rust 1.99+, `cargo`
- Foundry (`forge`, `cast`, `anvil`)
- Base RPC for fork (env `BASE_RPC_URL`, never in config)

## Setup

```bash
git clone https://github.com/sembraniteam/aerodrome-flashbot && cd aerodrome-flashbot
chmod +x .githooks/* && git config core.hooksPath .githooks
cp config/default.toml config/local.toml  # edit rpc_ws_url if needed, never commit
```

## Running

Dry-run (no key, forces `dry_run=true`, redacts URLs):

```bash
cargo run -- --config config/default.toml --csv results.csv
cargo run -- --config config/default.toml --fork-url ws://127.0.0.1:8545  # label override
cargo run -- --l1-fee 1200000  # inject measured getL1Fee (fixture default)
cargo run -- --kill           # kill-switch demo (rejects all trades)
```

Local fork (read-only `eth_call`):

```bash
anvil --fork-url "$BASE_RPC_URL" --port 8545
# other terminal, set config/local.toml: rpc_ws_url = "ws://127.0.0.1:8545"
cargo run -- --config config/local.toml --csv results.csv
```

Fork check / matrix (exit 0 + `SKIPPED` offline, no key):

```bash
FORK_URL=http://127.0.0.1:8545 cargo run -- --fork-check --tolerance-bps 50
FORK_URL=https://mainnet.base.org cargo run -- --fork-matrix
```

Live path (P3: lock + verify only, no trades, no keys touched without a runbook):

```bash
cargo build --bins --features live
./target/debug/live --help
./target/debug/live ledger-verify --ledger-dir artifacts/ledger-sepolia
./target/debug/live --config config/sepolia.toml  # refuses without --manifest (L1 lock)
```

Evidence (predicted-only from paper; auditor input, never realized):

```bash
cargo run -- --emit-evidence /tmp/aero-evidence --csv results.csv
```

## Configuration

`config/default.toml` (testnet-safe):

| Key                        | Default              | Notes                            |
|----------------------------|----------------------|----------------------------------|
| `dry_run`                  | `true`               | forced `true` by paper binary    |
| `max_flash_usdc`           | `5000000000` ($5000) | `hard_max` in `src/risk.rs`      |
| `min_net_profit_usdc`      | `5_000_000` ($5)     | `optimal_size` filter            |
| `max_slippage_bps`         | `50`                 |                                  |
| `daily_loss_cap_usdc`      | `100_000_000`        |                                  |
| `max_consecutive_failures` | `3`                  | circuit breaker                  |
| `head_staleness_blocks`    | `5`                  |                                  |
| `deadline_secs`            | `30` (1..300)        |                                  |
| `max_in_flight`            | `1` (pinned)         |                                  |
| `feed_mode`                | `canonical`          | `flashblocks` = pending-tag stub |

Allowlist: `WETH/USDC`, `AERO/USDC`, `AERO/WETH` enabled; `cbETH/ETH` disabled placeholder. Executor is USDC-flash-only
(`DirectionMismatch` unless one leg inputs USDC), so the paper loop skips non-USDC-quoted pairs (`AERO/WETH` stays
allowlisted for `--fork-check` estimator accuracy, never for execution). Secrets via env only:
`DISCORD_WEBHOOK_URL`, `PAUSER_KEY`, `DRILL_OWNER_KEY`, `*_RPC_URL`.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --bins
forge fmt --check
forge build
forge test
```

`cargo test` and `forge test` must pass offline. Fork tests skip without `FOUNDRY_FORK_URL`; `fork-check`/`fork-matrix`
print `SKIPPED` offline.

## Layout

| Path                             | Contents                                                                                                                       |
|----------------------------------|--------------------------------------------------------------------------------------------------------------------------------|
| `src/main.rs`                    | `paper` binary (dry-run harness, forces `dry_run=true`)                                                        |
| `src/bin/discord-bot.rs`         | `discord-bot` binary (ops CLI)                                                                                 |
| `src/bin/live.rs`                | `live` binary (triple-locked; P3 arms/verifies only, no trading loop)                                          |
| `src/live/`                      | live path (`live` feature only): lock, profiles, signer, pipeline, sender, breaker, ledger                      |
| `src/evidence.rs`                | keyless `--emit-evidence` summaries (both binaries; predicted never presented as realized)                     |
| `config/sepolia.toml`            | Base Sepolia profile (84532, dust, `[live]` section)                                                           |
| `config/mainnet-canary.toml`     | Base mainnet canary profile (8453, G5 caps, `[live]` section)                                                   |
| `src/lib.rs`                     | lib shared by both binaries                                                                                                    |
| `contracts/FlashArbExecutor.sol` | atomic executor                                                                                                                |
| `test/`                          | Forge tests + `mocks/` + `TestBase.sol` (minimal Vm, no `forge-std`)                                                           |
| `script/deploy-mocks-sepolia.sh` | Sepolia mock deploy (chain-id gate 84532, dust, `DRILL_OWNER_KEY` env)                                                         |
| `config/default.toml`            | testnet-safe defaults                                                                                                          |
| `docs/`                          | `FREEZE.md` (pinned addresses/selectors/codehashes), `RUNBOOK_SEPOLIA.md`, `ADR-001-immutable-monolith.md`, `DISCORD_SETUP.md` |

## Docs

- `docs/FREEZE.md` — Base mainnet addresses/selectors/codehashes + `forge build --sizes`
- `docs/RUNBOOK_SEPOLIA.md` — pause→sweep→redeploy drill on Base Sepolia
- `docs/RUNBOOK_MAINNET_CANARY.md` — canary stub (full runbook lands in P4)
- `docs/ADR-001-immutable-monolith.md` — immutable monolith, UR explicit-selector-only, USDC-only
- `docs/ADR-002-live-path.md` — live-path design + P3 implementation note
- `docs/DISCORD_SETUP.md` — Discord control-plane setup
