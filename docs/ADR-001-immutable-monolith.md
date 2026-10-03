# ADR-001: Immutable monolith executor

- Status: accepted
- Date: 2026-10-03
- Scope: `contracts/FlashArbExecutor.sol` on Base (8453)

## Context

The executor holds funds, borrows flash liquidity, and approves routers. Every
upgrade mechanism (proxy, diamond, migration function) is itself an attack
surface: storage collisions, selector clashes, and a privileged upgrader key
that can silently change the trading rules. The Rust bot already quotes
off-chain and simulates before submitting, so the contract's job is narrow:
atomically enforce allowlists, minimums, deadlines, and profit — then revert
otherwise.

## Decision

1. **Immutable monolith, no proxy.** The executor is deployed once with
   `VAULT` / `USDC` immutables and NO upgrade path. Behaviour changes (new
   router, new selector, new cap above the hard ceiling) require a fresh
   deploy plus the pause → sweep → redeploy drill (`docs/RUNBOOK_SEPOLIA.md`). Redeploy is cheap; trust is expensive.
2. **No UniversalRouter in production.** The UniversalRouter address (`0x6fF5…99b43`) is known to the contract ONLY as
   an explicit-selector
   constant: `setRouterAllowed` auto-enables NOTHING for it, and any selector
   needs a separate owner transaction. Rationale: UR multiplexes many
   command types behind one entrypoint, so calldata review is harder and a
   single allowlist entry covers more behaviour than intended. Production legs
   stay on the two single-purpose shapes (Slipstream `0xa026383e`,
   SwapRouter02 `0x04e45aaf`) with full on-chain recipient/path/amount pinning.
3. **USDC-only, no oracle.** The flash asset is ALWAYS USDC and profit is the
   post-repay USDC balance delta, so `minProfitUSDC` needs no price feed.
   WETH appears strictly as an ERC20 middle token; there is no wrap/unwrap
   path and no `receive()`/`fallback()`. A price oracle would add a feed
   dependency (staleness, sequencer downtime) to the one number that must
   never lie. L1/L2 fee terms live in the OFF-CHAIN cost model (`CostModel`
    + `getL1Fee` oracle read), which can only make the bot quote MORE
      conservatively, never settle a loss (on-chain minimums are the backstop).
4. **Operator/pauser split, NOT a multisig-timelock (yet).**
    - `operator`: may call `execute` only (hot bot key, easily rotated).
    - `pauser`: may call `pause` only (monitor/Discord key; can halt but
      cannot resume, move funds, or change risk).
    - `owner`: everything else (allowlist, caps, sweep, unpause, redeploy).
      Owner actions SHOULD be fronted by a 24–48 h timelock in production so
      users can exit before risk parameters change; the contract keeps
      two-step ownership transfer so the timelock contract can BE the owner.
    - A multisig-timelock owner is RECOMMENDED for any real deployment but is
      an operational choice, not a contract dependency: the contract cannot
      distinguish a timelock owner from an EOA owner, and that is intentional (smaller trusted surface).

## Consequences

- Upgrades are redeploys: liquidity must be swept and re-seeded, router
  codehash pins re-captured (`docs/FREEZE.md`), and the fork matrix
  re-run. The runbook makes this a drill, not an incident.
- The 24 KiB runtime ceiling is a FEATURE: at 14,274 bytes the contract has
  room for audited fixes but not for scope creep. New venues integrate
  off-chain first (quoter + simulation); on-chain support means a new
  audited deploy.
- If a timelock/multisig is adopted later, no contract change is needed:
  transfer ownership to it with the two-step flow and keep the pauser key
  where it is (separate, always able to halt).

## Alternatives considered

- **Upgradeable proxy:** rejected — upgrader key can rewrite trading rules
  atomically; contradicts the codehash-pinning and freeze discipline.
- **Multi-asset flash (WETH legs):** rejected — needs an oracle for profit
  accounting and a wrap/unwrap path; USDC-only covers the allowlisted pairs.
- **UniversalRouter-first routing:** rejected — see (2); generality here
  trades reviewability for convenience.
