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
