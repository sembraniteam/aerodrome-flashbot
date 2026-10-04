# Evidence Bundle Schema

A bundle is a directory `artifacts/readiness/<UTC timestamp>/` (git-ignored, S5). It contains hashes, addresses,
transaction hashes, and counts, and never secrets or URLs.

## Layout

```
manifest.json          the signed-off summary the Live Lock reads
SHA256SUMS             hashes of every file in the bundle (sha256sum -c must pass)
env.txt                tool versions (rustc, cargo, forge), OS, UTC time
git.txt                commit, branch, clean/dirty, diffstat
gates/                 one .log per command with PASS/FAIL line first
scans/                 file-name-only results of secret and layout scans
invariants/            S1-S10 and L1-L10 scan outputs: path, line number, and line hash only (never line content);
                       allowlist-used.txt is a copy of the adjudications in force
runs/                  --emit-evidence summaries from shadow, canary, ramp runs (user supplied)
onchain.json           user-supplied tx hashes and addresses per drill case (G3 to G5)
report.md              the readiness report
```

## manifest.json

```json
{
  "schema": 1,
  "created_at": "2026-10-04T12:00:00Z",
  "expires_at": "2026-10-18T12:00:00Z",
  "commit": "<40-hex>",
  "tree_clean": true,
  "chain_id": 84532,
  "stage_ready": "G3",
  "evidence_level": "E3",
  "hashes": {
    "freeze_md": "<sha256>",
    "cargo_lock": "<sha256>",
    "foundry_toml": "<sha256>",
    "profile": "<sha256 of the chain profile used>",
    "allowlist": "<sha256 of docs/readiness-allowlist.txt, or 'missing'>",
    "executor_codehash": "<0x... from chain or forge inspect>"
  },
  "gates": {
    "cargo_fmt": "PASS", "cargo_clippy": "PASS", "cargo_test": "PASS", "cargo_build_bins": "PASS",
    "forge_fmt": "PASS", "forge_build": "PASS", "forge_test": "PASS"
  },
  "invariants": { "S1": "PASS", "L1": "PASS" },
  "stages": {
    "G0": { "verdict": "READY", "evidence": ["gates/", "invariants/"] },
    "G1": { "verdict": "INCONCLUSIVE", "missing": ["runs/shadow-72h.json"] }
  }
}
```

`stage_ready` is the highest stage whose verdict is READY and for which all earlier stages are READY. The Live Lock
accepts a manifest only when this value, the commit, the chain id, the hashes, and the expiry all check out.

## Lock-subset manifest: attempt vs completion (2026-10-04)

The file the live binary actually reads via `--manifest` (and the bytes `LIVE_ARM` covers) is the lock subset:
`commit`, `chain_id`, `stage_ready`, `attempt_stage`, `waived`, `expires_at`, `hashes`
(`freeze_md`, `profile`, `cargo_lock`). Attempt-authorization is separated from completion:

- `attempt_stage` MUST equal the profile's `stage_required` (84532: G3; 8453 canary: G4).
- `stage_ready` MUST be a valid `G0`..=`G6` label strictly below `attempt_stage` — never equal, never above.
  The attempt is uncompleted work; completion is proven only by stage-exit evidence, never by the manifest.
- Every rung strictly between `stage_ready` and `attempt_stage` MUST appear in `waived` as
  `{"stage": "<rung>", "reason": "<non-empty justification>"}`. A missing rung, an empty/whitespace-only
  reason, or a waiver for a rung outside the open interval `(stage_ready, attempt_stage)` refuses.
  Duplicate entries for the same rung are tolerated (coverage is set-based).
- Unknown stage labels (in `attempt_stage`, `stage_ready`, any waiver, or the profile's `stage_required`)
  refuse with `BadStage`.

Drill example (Sepolia mock drill: honestly ready=G0, attempting G3):

```json
{
  "commit": "<40-hex of the armed commit>",
  "chain_id": 84532,
  "stage_ready": "G0",
  "attempt_stage": "G3",
  "waived": [
    {"stage": "G1", "reason": "mock-only drill entry: shadow run deferred, G3 exit rests on D1-D12"},
    {"stage": "G2", "reason": "mock-only drill entry: fork matrix deferred, G3 exit rests on D1-D12"}
  ],
  "expires_at": "2026-10-18T12:00:00Z",
  "hashes": {
    "freeze_md": "<sha256 of docs/FREEZE.md>",
    "profile": "<sha256 of the chain profile used>",
    "cargo_lock": "<sha256 of Cargo.lock>"
  }
}
```

Bump note: lock-subset manifests written before `attempt_stage`/`waived` existed carry no version marker and
are refused at parse (serde missing-field error, before the lock runs) — fail closed. `LIVE_ARM`
(`sha256sum` of the exact manifest bytes, unchanged mechanism) automatically covers the new fields, so an arm
computed over an old manifest can never validate a new one and vice versa. The bundle `manifest.json`
`schema` above stays `1` (auditor summary shape unchanged).

`LIVE_ARM` is `sha256sum manifest.json`. The script `scripts/collect-evidence.sh` prints it after verification; the
user exports it into the environment of the live process themselves.

## onchain.json

```json
{
  "chain_id": 84532,
  "executor": "0x...",
  "roles": { "owner": "0x...", "operator": "0x...", "pauser": "0x..." },
  "cases": {
    "D1": { "tx": "0x...", "expect": "success" },
    "D2": { "tx": "0x...", "expect": "revert" },
    "D8": { "tx": "0x...", "expect": "success", "note": "pause by pauser" }
  }
}
```

Verification, read-only and only with the user's go-ahead for network use (RPC from env, never printed):

```bash
cast chain-id --rpc-url "$RPC_URL"
cast receipt <tx> --rpc-url "$RPC_URL"            # status, to, logs, block
cast tx <tx> --rpc-url "$RPC_URL"                 # from, to, input selector
cast code <executor> --rpc-url "$RPC_URL" | cast keccak   # compare with FREEZE.md codehash
cast call <executor> "owner()(address)" --rpc-url "$RPC_URL"   # role read-back
```

Check per case: `chain_id` matches, `to` is the executor (or the Vault for callback tests), status matches `expect`,
the expected event is present, the sender matches the intended role, and the block is after deployment. A case without
a verifiable hash is INCONCLUSIVE.

## Evidence levels

| Level | Meaning                                                               | Enough for                      |
|-------|-----------------------------------------------------------------------|---------------------------------|
| E0    | Claims only (README, comments)                                        | Nothing                         |
| E1    | Offline gates and invariants verified on the current commit           | G0                              |
| E2    | Live-data shadow run and fork checks verified                         | G1, G2                          |
| E3    | Testnet drill verified on-chain by read-only calls                    | G3, G4 entry                    |
| E4    | Mainnet canary window verified: ledger chain, reconciliation, drift   | G5 exit, G6 steps               |

## Staleness

Recompute and compare: current `git rev-parse HEAD` against `commit`; hashes against the files; `expires_at` against
now. Any difference means the bundle is stale and every stage that depended on it is INCONCLUSIVE until re-collected.
