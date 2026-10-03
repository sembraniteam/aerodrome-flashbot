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
2. Deploy from the OWNER drill address with a manual, reviewed command. There
   is no `script/` dir and no broadcast helper in this repo on purpose —
   every mainnet-class action is typed by a human:
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
