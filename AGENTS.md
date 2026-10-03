# AGENTS.md — base-flash-arb

Flash-loan cross-DEX arbitrage on Base (Aerodrome Slipstream vs Uniswap V3).
**Dry-run only**: no live execution path exists. Never add signing keys,
broadcasts, or mainnet writes without an explicit user request.

## Layout (two toolchains, one repo)

- `src/` — Rust (Cargo). Binaries: `src/main.rs` (`base-flash-arb`, paper
  harness) + `src/bin/discord-bot.rs` (ops CLI). Shared code is a `[lib]`
  (`src/lib.rs`); do NOT re-add `#[path]` includes.
- `contracts/` + `test/` — Solidity (Foundry). `foundry.toml` sets
  `src = "contracts"`, `test = "test"`. Never put `.sol` files in `src/`.
- `script/` — bash only (`deploy-mocks-sepolia.sh`). No Forge `.s.sol`
  scripts (see "No forge-std" below).
- `docs/` — `RUNBOOK_SEPOLIA.md` (drill), `FREEZE.md` (pinned addresses /
  selectors / codehashes), `ADR-001-immutable-monolith.md` (architecture
  decisions), `DISCORD_SETUP.md`. Trust these + code over memory.
- `.opencode/agents/` — subagent definitions for the Task tool.

## Commands

Rust (all must pass; run in this order):
`cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test`, `cargo build --bins`. Single test: `cargo test <name>`.

Solidity:
`forge fmt --check`, `forge build`,
`forge test`. Single test:
`forge test --match-test <name>`.

## Quirks that will bite you

- **No forge-std, ever.** `remappings.txt` is empty and there is no `lib/`
  (offline-first). Tests inline a minimal `Vm` cheatcode interface in
  `test/TestBase.sol` — do not add `import "forge-std/..."`.
- **Tests must pass with no network.** Fork tests skip vacuously without an
  RPC URL (`FOUNDRY_FORK_URL` / `--fork-url`); Rust fork modes print
  `SKIPPED` and exit 0. Never add a test that requires network.
- **`cast`/`forge create --private-key` fails to decode keys in this env.**
  On a local fork use `--unlocked --from <anvil-acct> --broadcast` instead.
- **Mainnet addresses have no code on Base Sepolia (84532).** Verified by
  `cast code` returning `0x`. Never reuse mainnet Vault/router/token
  addresses on Sepolia — deploy the mock stack with
  `script/deploy-mocks-sepolia.sh` (it chain-id-gates 84532 and aborts
  otherwise).
- **Release profile is slow to build** (`lto = true`, `codegen-units = 1`).
  `panic = "abort"` is intentionally absent so `cargo test --release` works.
- `foundry.toml` header comment claiming "No `script/` dir" is stale;
  `script/` exists but holds bash, not Forge scripts.

## Secrets (hard rule)

Never commit or log: private keys, RPC URLs with tokens, webhook URLs,
`.env`, `config/local.toml` (both gitignored). Secrets live in env only:
`DISCORD_WEBHOOK_URL`, `PAUSER_KEY`, `DRILL_OWNER_KEY`, `*_RPC_URL`.
Delete drill artifacts (`discord-audit.csv`, `results.csv`) before committing.

## Safety invariants (do not weaken)

- Paper binary forces dry-run; Discord transport holds the **pauser key
  only** (never owner/operator). `discord_exposed(Resume) == false`:
  no `resume` / `unpause` / `sweep` / allowlist / limit changes via Discord.
- Executor (`contracts/FlashArbExecutor.sol`): UniversalRouter is
  explicit-selector-only (never a production path); mock `SWAP_SELECTOR`
  (`0xd5bcb9b5`) must never be enabled on production routers; production
  selectors are `0xa026383e` (Slipstream) / `0x04e45aaf` (UniV3).
- Repo-wide language is English (code, comments, docs).

## Local fork drill

```bash
anvil --fork-url https://mainnet.base.org --port 8545
# deploy: forge create <contract> --rpc-url http://127.0.0.1:8545 \
#   --unlocked --from <anvil-acct> --broadcast --constructor-args ...
```

No CI exists; no `opencode.json` at root.
