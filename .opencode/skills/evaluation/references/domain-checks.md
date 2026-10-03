# Domain Checks for base-flash-arb

Checklists and heuristics for the `evaluation` skill. Commands are **heuristics**: every hit must be read in context
before it becomes a finding, and an empty result is not proof. Run them from the repo root.

Secret-related commands use `-l` (file names only) on purpose, so values never reach the transcript.

## 1. Safety Invariants (hard gate)

### S1: Dry-run only (no keys, signing, broadcasting, mainnet writes)

```bash
grep -rnE "PrivateKeySigner|LocalSigner|EthereumWallet|send_transaction|send_raw_transaction|eth_sendRawTransaction|\.sign_" src/
grep -rnEi "wallet|signer|broadcast" src/ | head -40
```

Pass when there are no live execution paths. Adding one requires an explicit user-owned config flag and a separate,
reviewed code path. Check that new `eth_call` usage stays read-only and that nothing in the
diff submits a transaction.

### S2: Paper binary forces dry-run

Read `src/main.rs`. The binary must force dry-run even if `config/default.toml` or a local config says
`dry_run = false`. Verify the override still exists and that `config/default.toml` still has `dry_run = true`.

### S3: Discord control plane is pause-only

```bash
grep -rn "discord_exposed" src/
grep -rnEi "resume|unpause|sweep|allowlist|set_limit|max_flash|owner|operator" src/discord.rs src/bin/discord-bot.rs
grep -rnE "PAUSER_KEY|OWNER_KEY|OPERATOR_KEY|DRILL_OWNER_KEY" src/
```

Pass when `discord_exposed(Resume)` is still `false`, no new Discord command can resume, unpause, sweep, change an
allowlist, or change a limit, and `src/bin/discord-bot.rs` reads only `PAUSER_KEY`. Any owner or operator key referenced
from the Discord path is a violation. Alerts stay amounts-only, rate-limited, and drop-not-block.

### S4: Executor selectors and routers

```bash
grep -rnE "0xd5bcb9b5|0xa026383e|0x04e45aaf" contracts/ src/ test/ script/ docs/
```

- Production selectors stay `0xa026383e` (Slipstream) and `0x04e45aaf` (Uniswap V3).
- Mock `SWAP_SELECTOR` (`0xd5bcb9b5`) appears only in mocks, tests, and the Sepolia mock drill, and is never enabled on
  a production router.
- UniversalRouter stays explicit-selector-only and never becomes a production path
  (`docs/ADR-001-immutable-monolith.md`).

### S5: Secrets and artifacts

```bash
git ls-files | grep -E '(^|/)(\.env(\..*)?|config/local\.toml|discord-audit\.csv|results\.csv)$'
git grep -lEi 'discord(app)?\.com/api/webhooks/[0-9]+'
git grep -lEi '(alchemy|infura|quiknode|ankr|blastapi)[^[:space:]"'"'"']*[A-Za-z0-9_-]{20,}'
git grep -lE '0x[0-9a-fA-F]{64}'
```

- The first command must print nothing (an `.env.example` template is acceptable).
- Known false positives for the last command: codehashes and selectors in `docs/FREEZE.md`, hashes in tests. Inspect any
  other file, and redact before quoting.
- Secrets live in env only (`DISCORD_WEBHOOK_URL`, `PAUSER_KEY`, `DRILL_OWNER_KEY`, `*_RPC_URL`). Logs must go through
  the URL redactor (`src/chain.rs`); new log lines must not print URLs, headers, or key material.

### S6: Layout and toolchain rules

```bash
grep -rn "forge-std" contracts test foundry.toml remappings.txt
test -d lib && echo "VIOLATION: lib/ exists"
test -s remappings.txt && echo "VIOLATION: remappings.txt not empty"
find src -name '*.sol'
find script -type f ! -name '*.sh'
grep -rn '#\[path' src/
```

All must be empty or silent. Tests inline a minimal `Vm` interface in `test/TestBase.sol`. `contracts/` and `test/` hold
Solidity, `src/` holds Rust, and `script/` holds bash only.

### S7: Tests need no network

```bash
grep -rnE "FOUNDRY_FORK_URL|createSelectFork|createFork" test/
grep -rnE "https?://|wss?://" src/ test/ | head -30
```

- Solidity fork tests must skip vacuously when no RPC URL is present.
- Rust fork modes must print `SKIPPED` and exit 0 when no RPC is reachable.
- A new test whose pass or fail depends on a live endpoint is a violation.

### S8: Sepolia safety

```bash
grep -n "84532" script/deploy-mocks-sepolia.sh
```

The deploy script must abort on any other chain id. Mainnet Vault, router, and token addresses must never be reused on
Base Sepolia (84532), because they have no code there.

### S9: Risk limits and fail-closed gates

Read `src/risk.rs`, `src/sequencer.rs`, and `config/default.toml`. Verify these are not loosened without explicit
user approval:

- Hard limits `size <= max <= hard_max`; circuit breaker; kill switch that fails closed
- Defaults: `max_flash_usdc=$500`, `min_net_profit_usdc=$5`, `max_slippage_bps=50`, `daily_loss_cap_usdc=$100`,
  `max_consecutive_failures=3`, `head_staleness_blocks=5`
- `max_in_flight` pinned to 1; `deadline_secs` default 30 within range 1 to 300
- Sequencer uptime gate fails closed on error and honors the 3600 s grace period
- Allowlist stays `WETH/USDC`, `AERO/USDC`, `AERO/WETH`; the `cbETH/ETH` placeholder stays disabled until a verified
  address exists

### S10: Language

Code, comments, and docs are English. Spot-check the diff.

## 2. Rust Off-chain Correctness

**Money math** (`src/pools.rs`, `src/profit.rs`, `src/sim.rs`, `src/risk.rs`):

```bash
grep -nE "\bf(32|64)\b" src/pools.rs src/profit.rs src/sim.rs src/risk.rs
grep -nE "unwrap\(\)|expect\(|as u(64|128)|as i(64|128)" src/pools.rs src/profit.rs src/sim.rs
```

- The estimator is integer-only; every float in a money path needs a justification.
- No lossy casts on `U256`; overflow is handled explicitly (checked or saturating on purpose). A past bug saturated the
  token0 to token1 leg at 18dp/6dp scales.
- Decimals are correct per pair: USDC is 6dp, WETH and AERO are 18dp. `AERO/WETH` is sized in WETH, not USDC notionals.
- Both directions are handled, and token0/token1 ordering is not assumed.
- Quoter binding field order is correct. A past bug swapped `amountIn` and `tickSpacing` in the Aero quoter binding.
  Slipstream pools are keyed by `tickSpacing`, Uniswap V3 pools by fee tier.

**Cost model** (`src/profit.rs`): the net profit includes flash fee (0% on the Balancer V2 Vault on Base, but the fee>0
path must still compute `repay = amount + fee`), swap fees from the live schedule, gas, L1 data fee (injectable via
`--l1-fee`, with the source logged), and slippage. Break-even and optimal size must be consistent with the cost model
and honor `min_net_profit_usdc`.

**Fail-closed behavior:** every error path (RPC failure, stale head, quoter mismatch beyond tolerance, sequencer down,
parse error) results in no execution and a logged reason. Look for `unwrap_or(true)`-style defaults that open a gate on
error.

**Verification:** `sim::verify_quote` stays tolerance-banded, and a systematic estimator versus quoter mismatch is
treated as a bug.

**Logging and metrics:** no URLs or secrets in logs; metrics (hit rate, revert rate, p95 latency, CSV export) stay
consistent with their existing format unless the behavior intentionally changes.

## 3. Solidity Executor (`contracts/FlashArbExecutor.sol`)

Read the whole contract, not only the diff. Verify:

- **Atomicity:** the transaction reverts when the trade is unprofitable. The post-trade balance must cover
  `amount + feeAmounts[i]` plus the minimum profit, with no path that leaves the Vault unpaid.
- **Callback authentication:** the flash-loan callback accepts only the Vault as `msg.sender` and only for a loan this
  contract initiated. No `tx.origin`.
- **Roles:** owner, operator, and pauser stay separate. The pauser can only pause. Unpause, sweep, allowlist changes,
  and limit changes are owner-only, and no new function widens the operator's powers.
- **Allowlists:** routers, selectors, tokens, and pools are checked before any external call. No arbitrary `call` or
  `delegatecall` with caller-controlled target or calldata.
- **Slippage and deadline:** minimum-out is enforced on both legs, and the outer plus Aero-inner deadlines are checked
  against block time.
- **Approvals:** exact amounts, no lingering unlimited approvals to non-allowlisted spenders.
- **Immutability:** no upgrade proxy, `selfdestruct`, or admin-settable core addresses
  (`docs/ADR-001-immutable-monolith.md`). USDC-only, no oracle dependency.
- **Codehash monitor** remains intact, and events or custom errors cover new state changes and failure modes.
- **Reentrancy:** no external call before state updates, and a guard where a callback is possible.

## 4. Tests

- Every new behavior has a test, including the negative path (revert, rejection, fail-closed).
- Solidity tests use mocks in `test/mocks/` (for example `MockVault::setFeeBps` for the fee>0 path) and inline `Vm` from
  `test/TestBase.sol`.
- Rust tests are deterministic and offline. They use fixtures, not live endpoints.
- A passing test suite proves nothing about profitability. Do not credit measured-profit claims backed only by fixtures
  or compilation.
- Check that a test actually asserts the intended behavior, not merely that the code runs.

## 5. Docs and Pinned Data

Update these when the related code changes:

- `docs/FREEZE.md`: pinned addresses, selectors, chain id, `forge build --sizes`, codehashes
- `docs/ADR-001-immutable-monolith.md`: any architectural decision that changes
- `docs/RUNBOOK_SEPOLIA.md`: drill steps affected by deploy-script or executor changes
- `docs/DISCORD_SETUP.md`: Discord control-plane changes
- `README.md`: layout table, defaults, flags, and measured tallies. Measured numbers must be re-measured, never carried
  over after the scope changes (the README already notes its 12/12 tally predates the `AERO/WETH` rows).

Docs that contradict the code are a finding, even when the code is correct.

## 6. Severity Guide

| Severity | Examples                                                                                                      |
|----------|---------------------------------------------------------------------------------------------------------------|
| Critical | Safety invariant violated; secret committed; live execution path added; executor can lose funds or be drained |
| High     | Failing gate; fail-open error path; wrong decimals or direction in money math                                 |
| Medium   | Missing negative-path test; docs or pinned data out of date                                                   |
| Low      | Naming, comment, or formatting nits that pass the gates                                                       |
