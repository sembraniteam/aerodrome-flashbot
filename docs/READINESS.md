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

## Stage-gating circularity for mock drills (found 2026-10-04, pre-G3) — FIXED by the attempt/waiver lock change described below

`LiveProfile::validate` (`src/live/profile.rs:173-185`) forces
`stage_required >= G3` on Sepolia, and the L1 lock
(`src/live/manifest.rs:167-176`) forces
`manifest.stage_ready >= stage_required`. No honest pre-drill label
reaches G3 (G3 exit needs D11 itself), so a mock-only drill cannot
ARM without either a stage overstatement or a code change.
For the mock-only drill (ladder entry: G0), the drill manifest asserts
`stage_ready = "G3"` strictly as drill-entry authorization, OFF-repo
and single-use (`artifacts/readiness/sepolia-drill/manifest-drill.json`
+ `.NOTE.txt`), never as exit evidence. G3 verdict rests solely on
D1–D12 on-chain evidence. Proper fix before any non-mock run:
separate attempt-authorization from completion in the lock policy.

FIX (2026-10-04): the lock now enforces `attempt_stage ==
profile.stage_required` with `stage_ready` strictly below it and an
explicit reasoned `waived` entry per rung in between
(`src/live/manifest.rs`: `AttemptMismatch` / `ReadyNotBelowAttempt` /
`MissingWaiver` / `EmptyWaiverReason` / `ExtraWaiver`; pre-fix manifests
fail closed at parse). The drill manifest now says ready=G0 attempt=G3
waived=[G1,G2] with reasons — no overstatement. This entry's
`stage_ready = "G3"` drill-entry manifest is superseded; it remains
documented here for history but must NOT be reused (it fails closed
under the new lock).

## G3 drill evidence carries across the lockfix (2026-10-04)

`git diff d8fadab..7474529` touches only `src/live/` (manifest/lock/
profile stage rule + tests), `tests/live_l10.rs`, docs, skills, and
`script/deploy-mocks-sepolia.sh`. `contracts/` is byte-identical, so
the deployed executor (`0x41319bbA5DAa5537a59C3C01B4377D04Ba2479C3`,
codehash `0x230c8f7c…`) and every D1–D12 on-chain outcome are
unaffected: the lockfix changes off-chain startup gating only, and no
drill transaction passes through the gated binary (all submits are the
user's `cast send`). The new binary path at the new commit is proven
separately by the honest re-arm (`manifest-drill-v2.json` ready=G0
attempt=G3, ARMED, `evidence-d11b`). Therefore D1–D12 hashes stand as
G3 exit evidence without re-running the drill.

## G3 drill evidence carries across the lockfile refresh (2026-10-10)

`git diff 0fcb7cb..f6b3963` touches only `Cargo.lock` (registry churn:
syn 3.0.6→3.0.7, cc 1.6.0→1.7.0, written by `cargo build --bins
--features live`). `src/`, `contracts/`, and `tests/` are byte-identical,
so the deployed executor
(`0xbF29E9802e246d770f2662d76b0726D5B252515a`) and every D1–D12
on-chain outcome are unaffected: dependency metadata changes no
runtime behavior, and no drill transaction passes through the gated
binary (all submits are the user's `cast send`). The drill manifest
is re-sealed at `f6b3963` (same profile hash, refreshed `cargo_lock`
hash); the new binary path is proven separately by the honest re-arm
at the new commit plus `--emit-evidence`. Therefore D1–D12 hashes
stand as G3 exit evidence without re-running the drill.
