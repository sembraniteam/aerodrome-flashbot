# Readiness decisions

Human adjudications the evidence collector cannot make on its own.
Binding evidence lives in sealed bundles under `artifacts/readiness/`.

## S6 layout FAIL (2026-10-04) — confirmed false positive

Bundle `artifacts/readiness/20261004T041540Z`, `invariants/S6-layout.txt`.
All 4 hits are comments, zero `VIOLATION` lines:

- `contracts/FlashArbExecutor.sol:6` — "forge-std/OZ are not vendored"
- `test/TestBase.sol:6` — "works offline without forge-std"
- `test/TestBase.sol:19` — "replacing forge-std for offline use"
- `src/lib.rs:5` — "no `#[path]` duplication"

No `lib/`, no `src/*.sol`, no vendored `forge-std`.
Decision: no code change. The S6 scan stays dumb-and-loud on
purpose; the auditor carries this as a known-heuristic verdict.
Re-check these lines if the collector output ever changes.

## Sweep `to` vs L4 text (2026-10-04) — recorded deviation, no gap

The live contract phrases sweep as "to the owner address only";
`sweep(token, to, amount)` (`contracts/FlashArbExecutor.sol:745-751`)
takes an owner-chosen `to` (bounded by `onlyOwner`).
Decision (P2, kept since): option (a) — contract as-is, owner
recipient enforced by runbook + G4 review; see
`docs/ADR-002-live-path.md` §4 for the full reasoning
(`to == owner` adds nothing against owner-key compromise).
Enforcement: `docs/RUNBOOK_SEPOLIA.md` D9 sweeps profit to the
owner address; canary runbook sweeps to owner daily.
Revisit only if sweeps become frequent/automated.
