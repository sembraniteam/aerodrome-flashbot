# AGENTS.md — base-flash-arb

Dry-run flash-loan arb on Base (Aerodrome Slipstream vs Uniswap V3, Balancer V2 Vault 0% fee, `alloy`). Two Rust
binaries share `src/lib.rs`; Solidity executor in `contracts/FlashArbExecutor.sol`. Live execution paths do not exist —
never add keys, signing, or broadcasts without explicit user request.

## Layout

- `src/` Rust Cargo lib (`src/lib.rs`) + bins `src/main.rs` (`base-flash-arb` paper harness) and
  `src/bin/discord-bot.rs` (pauser-key-only CLI). Never use `#[path]` includes; never put `.sol` in `src/`.
- `contracts/` + `test/` Solidity (Foundry: `src = "contracts"`, `test = "test"`). `test/TestBase.sol` inlines minimal
  `Vm`; `remappings.txt` empty, no `lib/`, no `forge-std`.
- `script/` bash only (`deploy-mocks-sepolia.sh`). No Forge `.s.sol` scripts.
- `config/default.toml` testnet-safe defaults; `config/local.toml` gitignored local override. `docs/` → `FREEZE.md`,
  `RUNBOOK_SEPOLIA.md`, `ADR-001-immutable-monolith.md`, `DISCORD_SETUP.md`.

## Commands (run in order, no `--release`)

Rust: `cargo fmt --all -- --check` → `cargo clippy --all-targets -- -D warnings` → `cargo test` → `cargo build --bins`
(single: `cargo test <name>`). Release profile `lto=true` `codegen-units=1` is slow — never `--release`.

Solidity: `forge fmt --check` → `forge build` → `forge test` (single: `forge test --match-test <name>`). Commit hooks:
`bash .githooks/commit-message <file>` (75-char Conventional Commits) and `bash .githooks/pre-commit` (secret scan).
Setup once: `chmod +x .githooks/* && git config core.hooksPath .githooks`.

## Run

Paper (no key, forces dry-run even if config says false, redacts URLs):
`cargo run -- --config config/default.toml --csv results.csv`
`cargo run -- --config config/default.toml --fork-url ws://127.0.0.1:8545` (endpoint label override)

Fork paper:

```
anvil --fork-url "$BASE_RPC_URL" --port 8545
# copy config/default.toml → config/local.toml, set rpc_ws_url = "ws://127.0.0.1:8545"
cargo run -- --config config/local.toml --csv results.csv
```

Read-only checks (exit 0 + `SKIPPED` offline, never need keys):
`FORK_URL=http://127.0.0.1:8545 cargo run -- --fork-check --tolerance-bps 50`
`FORK_URL=https://mainnet.base.org cargo run -- --fork-matrix`
`cargo run -- --l1-fee <measured getL1Fee base units>` injects measured L1 fee (fixture default otherwise, source
logged).

## Quirks

- Tests must pass offline. Forge fork tests skip without `FOUNDRY_FORK_URL`; Rust `fork-check`/`fork-matrix` print
  `SKIPPED` offline. Never add a network-dependent test.
- `cast --private-key` fails in this env; on local fork use `--unlocked --from <anvil-acct> --broadcast`.
- Mainnet addresses have no code on Base Sepolia 84532 (`cast code` → `0x`). Never reuse them there; use
  `script/deploy-mocks-sepolia.sh` (chain-id gate 84532, dust only, throwaway `DRILL_OWNER_KEY` in env).
- `feed_mode = "canonical"` default post-Denim (200ms canonical blocks); `flashblocks` pending-tag stub yields None
  offline.
- `foundry.toml` header comment about "no `script/` dir" is stale — `script/` exists for bash.

## Secrets

Never commit or log: keys, RPC URLs with tokens, webhook URLs, `.env`, `config/local.toml`, `discord-audit.csv`,
`results.csv`. Env only: `DISCORD_WEBHOOK_URL`, `PAUSER_KEY`, `DRILL_OWNER_KEY`, `*_RPC_URL`. Logs go through
`src/chain.rs` URL redactor.

## Safety invariants (do not weaken)

- Paper binary forces `dry_run=true`; Discord holds **pauser key only** (`discord_exposed(Resume)==false` — no
  resume/unpause/sweep/allowlist/limit via Discord, `src/bin/discord-bot.rs` reads only `PAUSER_KEY`).
- Executor selectors: `0xa026383e` Slipstream / `0x04e45aaf` UniV3 production; mock `0xd5bcb9b5` never enabled on
  production; UniversalRouter explicit-selector-only (never production path per `ADR-001`).
- Risk: `size <= max <= hard_max`, `max_flash_usdc=$500` `min_net_profit_usdc=$5` `max_slippage_bps=50`
  `daily_loss_cap_usdc=$100` `max_consecutive_failures=3` `head_staleness_blocks=5` `deadline_secs=30` (1..300)
  `max_in_flight=1` pinned; circuit breaker + kill switch + sequencer gate (Chainlink `0xBCF8…6433`, 3600s grace) all
  fail-closed. Allowlist `WETH/USDC`, `AERO/USDC`, `AERO/WETH`; `cbETH/ETH` placeholder stays disabled.
- Repo language English everywhere. Verify money math integer-only, decimals per pair (USDC 6dp, WETH/AERO 18dp,
  `AERO/WETH` sized in WETH), both directions, checked overflow, `sim::verify_quote` tolerance-banded.

## Evaluation

Skill: `.opencode/skills/evaluation/SKILL.md` + `references/domain-checks.md` (S1-S10). Agent:
`.opencode/agents/evaluator.md` (read-only, offline). Command: `/evaluate` dispatches `rust-analyst`/`solidity-auditor`/
`base-mev-analyst`/`security-expert` in parallel for touched areas only; never `*-engineer`.
