# Freeze: deployment inputs + contract sizes

> This file pins EVERYTHING a deploy must reproduce byte-for-byte. Any drift
> (different address, different codehash, different selector, different
> compiler setting) is a redeploy review, never a silent config edit.
> Nothing here is secret; nothing here authorizes a deploy.

## Chain

- Base mainnet, chain id **8453**. (Base Sepolia is 84532; see
  `docs/RUNBOOK_SEPOLIA.md`.)

## Pinned addresses (Base mainnet, verified)

| Role                                     | Address                                      | Note                                                                                         |
|------------------------------------------|----------------------------------------------|----------------------------------------------------------------------------------------------|
| Balancer V2 Vault (flash lender)         | `0xBA12222222228d8Ba445958a75a0704d566BF2C8` | flash fee 0% on Base; repay computed generically as `amount + fee`                           |
| Aerodrome Slipstream SwapRouter          | `0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5` | tickSpacing-based `exactInputSingle`                                                         |
| Uniswap SwapRouter02                     | `0x2626664c2603336E57B271c5C0b26F421741e481` | fee-based `exactInputSingle` (no deadline field)                                             |
| Uniswap UniversalRouter                  | `0x6fF5693b99212Da76ad316178A184AB56D299b43` | EXPLICIT-SELECTOR-ONLY: never auto-allowlisted (see ADR-001)                                 |
| USDC (native, flash asset + profit unit) | `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913` | 6 decimals; no oracle needed                                                                 |
| Aerodrome QuoterV2                       | `0x254cF9E1E6e233aa1AC962CB9B05b2cfeAaE15b0` | struct keyed by `tickSpacing`                                                                |
| Uniswap V3 QuoterV2                      | `0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a` | struct keyed by `fee`                                                                        |
| Chainlink SequencerUptimeFeed            | `0xBCF85224fc0756B9Fa45aA7892530B47e10b6433` | answer 0 = up, 1 = down; 3600 s grace; source: docs.chain.link/data-feeds/l2-sequencer-feeds |
| OP-Stack GasPriceOracle (`getL1Fee`)     | `0x420000000000000000000000000000000000000F` | L1 data-fee term of the cost model                                                           |

## Selectors (executor-enforced)

| Selector          | Shape                                                                                                                                                |
|-------------------|------------------------------------------------------------------------------------------------------------------------------------------------------|
| `0xa026383e`      | Aerodrome Slipstream `exactInputSingle((address,address,int24,address,uint256,uint256,uint256,uint160))` (has inner `deadline`, re-checked on-chain) |
| `0x04e45aaf`      | Uniswap SwapRouter02 `exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))` (no deadline field)                                |
| mock `0xd5bcb9b5` | `swap(address,address,uint256,uint256,address)` — TESTS ONLY, never allowlisted on production routers                                                |

## Codehashes

Capture at freeze time from a mainnet fork (read-only); re-capture with the
same commands before any deploy. Values below were captured 2026-10-03.
Any later mismatch against `routerCodehashPinned` reverts
every `execute` with `RouterCodeChanged` by design.

```bash
# Rust matrix (includes a `codehash` row per pinned address):
FORK_URL=https://mainnet.base.org cargo run -- --fork-matrix
# or per address:
cast code --rpc-url https://mainnet.base.org 0xBA12222222228d8Ba445958a75a0704d566BF2C8 | keccak
```

| Label            | Address suffix | Codehash (`address.codehash` on Base mainnet)                                                                                |
|------------------|----------------|------------------------------------------------------------------------------------------------------------------------------|
| vault            | `…F2C8`        | `0xf73f7f7f4d4fd56067273eade4d46efa681f36bfb51e1ac0ca25d24ed15c16dc`                                                         |
| aero-router      | `…18a5`        | `0x8ca2876cc2c509da1648ef2ad21a8849c261cf7316b675119b98ba298c8657e7`                                                         |
| uni-router02     | `…1481`        | `0x38bd640f47df62b2fd5a6755a63f4976ad847dc9b946ae0d145d21d16bb124e4`                                                         |
| universal-router | `…99b43`       | `0x27713951fb0660a1422b710122022d90723d883dc7b72949be79cb2957d234e0` (explicit-only; pin only if a selector is ever enabled) |
| usdc             | `…2913`        | `0xa6705a10bb756b5dea144591118be77d7af0c3eee3bf2dfe2583dcb0364fefab`                                                         |
| aero-quoter      | `…15b0`        | `0xfb0ab713266d089d5b6ac48d50455c4fadc9cd49a1efcc83a091e7c2e48dad0e`                                                         |
| uni-quoter       | `…B76a`        | `0xceb5b8bc35e09fc64b1c48c6b920e0d2e37d5710e4f3ef214e1a81f75acee51e`                                                         |

Measured 2026-10-03 via `cargo run -- --fork-matrix --fork-url
https://mainnet.base.org` (20/20 matrix rows PASS, 12/12 quoter rows within
50 bps) against chain 8453. Re-capture with the same command (or
`testFork_CodehashFreeze`) before any deploy; any drift triggers a redeploy
review, never a silent config edit.

Cross-checks that also pin the freeze: `forge test --match-contract
FlashArbExecutorForkTest` (`testFork_CodehashFreeze`) and the Rust
`--fork-matrix` `codehash` rows must agree with this table.

## Compiler (bytecode reproducibility)

- `solc_version = "0.8.37"`, `evm_version = "cancun"` (transient storage for
  `_expectedHash` / `_inCallback`), `optimizer = true`, `optimizer_runs = 200`
  (see `foundry.toml`).
- Toolchain at freeze: `forge 1.8.4` (commit `50af4efe…`, build 2026-10-01).

## `forge build --sizes` (2026-10-03, unmodified executor)

```text
| Contract                 | Runtime Size (B) | Initcode Size (B) | Runtime Margin (B) | Initcode Margin (B) |
| FlashArbExecutor         | 14,274           | 14,756            | 10,302             | 34,396              |
| MockAeroSlipstreamRouter | 1,669            | 1,697             | 22,907             | 47,455              |
| MockERC20                | 1,713            | 2,440             | 22,863             | 46,712              |
| MockRouter               | 1,466            | 1,494             | 23,110             | 47,658              |
| MockTaxToken             | 1,795            | 2,528             | 22,781             | 46,624              |
| MockUniSwapRouter02      | 1,590            | 1,618             | 22,986             | 47,534              |
| MockVault                | 2,476            | 2,504             | 22,100             | 46,648              |
```

Notes:

- Executor runtime is 14,274 / 24,576 bytes (10,302 margin): comfortably under
  the 24 KiB EIP-170 limit with headroom for the audited fix set, but NOT for
  feature growth — see ADR-001 (immutable monolith).
- Initcode 14,756 / 49,152 bytes (34,396 margin): no initcode pressure.
- Mock rows are test-only and never deployed; they are recorded here only so
  a surprising size delta in CI is attributable.
