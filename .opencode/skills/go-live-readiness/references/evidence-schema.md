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
