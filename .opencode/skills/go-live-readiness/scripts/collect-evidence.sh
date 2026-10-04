#!/usr/bin/env bash
# collect-evidence.sh - offline, read-only evidence collector for base-flash-arb.
#
# Modes (run from the repo root):
#   collect-evidence.sh [--run-gates] [--out DIR] [--chain-id N] [--ttl-days N]
#       Collect evidence into DIR (default artifacts/readiness/<UTC>). G0 only.
#   collect-evidence.sh --seal DIR
#       Recompute SHA256SUMS after the auditor updated manifest.json, print the arm hash.
#   collect-evidence.sh --verify DIR
#       Check SHA256SUMS, commit, file hashes and expiry. Exit 0 only if fresh and intact.
#
# Safety: never contacts the network on purpose (cargo runs with CARGO_NET_OFFLINE, RPC env vars are unset for
# gates), never prints secret values (secret scans list file names only), never writes outside the output dir,
# never signs, deploys, or sends transactions.

set -u -o pipefail

RUN_GATES=0
OUT=""
CHAIN_ID="0"
TTL_DAYS=14
MODE="collect"
TARGET=""

while [ $# -gt 0 ]; do
  case "$1" in
    --run-gates) RUN_GATES=1 ;;
    --out) OUT="${2:?--out needs a directory}"; shift ;;
    --chain-id) CHAIN_ID="${2:?--chain-id needs a number}"; shift ;;
    --ttl-days) TTL_DAYS="${2:?--ttl-days needs a number}"; shift ;;
    --seal) MODE="seal"; TARGET="${2:?--seal needs a directory}"; shift ;;
    --verify) MODE="verify"; TARGET="${2:?--verify needs a directory}"; shift ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

# ---------- portability helpers (macOS and Linux) ----------
sha() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}
hash_of() { [ -f "$1" ] && sha "$1" | awk '{print $1}' || echo "missing"; }
now_utc() { date -u +%Y-%m-%dT%H:%M:%SZ; }
plus_days() {
  date -u -d "+$1 days" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -v+"$1"d +%Y-%m-%dT%H:%M:%SZ
}
to_epoch() {
  date -u -d "$1" +%s 2>/dev/null || date -u -j -f %Y-%m-%dT%H:%M:%SZ "$1" +%s 2>/dev/null || echo 0
}
json_field() { # json_field FILE KEY  (flat string/number keys only; first match)
  sed -n "s/.*\"$2\"[[:space:]]*:[[:space:]]*\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p" "$1" | head -1
}

[ -d .git ] || [ -f .git ] || { echo "run from the repository root" >&2; exit 2; }

# ---------- seal ----------
write_sums() {
  ( cd "$1" && find . -type f ! -name SHA256SUMS | LC_ALL=C sort | while read -r f; do sha "$f"; done > SHA256SUMS )
}

if [ "$MODE" = "seal" ]; then
  [ -f "$TARGET/manifest.json" ] || { echo "no manifest.json in $TARGET" >&2; exit 2; }
  write_sums "$TARGET"
  echo "sealed: $TARGET"
  echo "LIVE_ARM=$(hash_of "$TARGET/manifest.json")"
  exit 0
fi

# ---------- verify ----------
if [ "$MODE" = "verify" ]; then
  M="$TARGET/manifest.json"; rc=0
  [ -f "$M" ] || { echo "FAIL no manifest.json"; exit 1; }
  if ( cd "$TARGET" && { command -v sha256sum >/dev/null 2>&1 && sha256sum -c SHA256SUMS --quiet || shasum -a 256 -c SHA256SUMS --quiet; } ) >/dev/null 2>&1; then
    echo "PASS bundle checksums"
  else echo "FAIL bundle checksums"; rc=1; fi
  head_now=$(git rev-parse HEAD 2>/dev/null || echo none)
  [ "$(json_field "$M" commit)" = "$head_now" ] && echo "PASS commit matches HEAD" || { echo "FAIL commit differs from HEAD (stale)"; rc=1; }
  [ "$(json_field "$M" freeze_md)" = "$(hash_of docs/FREEZE.md)" ] && echo "PASS docs/FREEZE.md hash" || { echo "FAIL docs/FREEZE.md changed"; rc=1; }
  [ "$(json_field "$M" cargo_lock)" = "$(hash_of Cargo.lock)" ] && echo "PASS Cargo.lock hash" || { echo "FAIL Cargo.lock changed"; rc=1; }
  [ "$(json_field "$M" foundry_toml)" = "$(hash_of foundry.toml)" ] && echo "PASS foundry.toml hash" || { echo "FAIL foundry.toml changed"; rc=1; }
  exp=$(json_field "$M" expires_at)
  if [ "$(to_epoch "$exp")" -gt "$(date -u +%s)" ]; then echo "PASS not expired ($exp)"; else echo "FAIL expired or unreadable expiry"; rc=1; fi
  echo "stage_ready=$(json_field "$M" stage_ready) chain_id=$(json_field "$M" chain_id)"
  [ $rc -eq 0 ] && echo "LIVE_ARM=$(hash_of "$M")"
  exit $rc
fi

# ---------- collect ----------
TS=$(date -u +%Y%m%dT%H%M%SZ)
OUT="${OUT:-artifacts/readiness/$TS}"
mkdir -p "$OUT/gates" "$OUT/scans" "$OUT/invariants" "$OUT/runs"

COMMIT=$(git rev-parse HEAD 2>/dev/null || echo none)
if [ -z "$(git status --porcelain 2>/dev/null)" ]; then CLEAN=true; else CLEAN=false; fi

{
  echo "utc=$(now_utc)"
  echo "os=$(uname -sm)"
  for t in "rustc --version" "cargo --version" "forge --version"; do
    $t 2>/dev/null | head -1 || echo "${t%% *}=not installed"
  done
} > "$OUT/env.txt"

{ echo "commit=$COMMIT"; echo "branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)"; echo "clean=$CLEAN"
  git diff --stat HEAD 2>/dev/null; } > "$OUT/git.txt"

# ---- gates (offline) ----
declare -a GATE_NAMES=(cargo_fmt cargo_clippy cargo_test cargo_build_bins forge_fmt forge_build forge_test)
declare -a GATE_CMDS=(
  "cargo fmt --all -- --check"
  "cargo clippy --all-targets -- -D warnings"
  "cargo test"
  "cargo build --bins"
  "forge fmt --check"
  "forge build"
  "forge test"
)
declare -a GATE_RESULTS=()

for i in "${!GATE_NAMES[@]}"; do
  name="${GATE_NAMES[$i]}"; cmd="${GATE_CMDS[$i]}"; tool="${cmd%% *}"; log="$OUT/gates/$name.log"
  if [ "$RUN_GATES" -ne 1 ]; then res="NOT RUN"; echo "NOT RUN (--run-gates not given)" > "$log"
  elif ! command -v "$tool" >/dev/null 2>&1; then res="NOT RUN"; echo "NOT RUN ($tool not installed)" > "$log"
  else
    # S7: gates must pass with no network and no RPC endpoints in the environment.
    if env -u FOUNDRY_FORK_URL -u BASE_RPC_URL -u BASE_SEPOLIA_RPC_URL -u RPC_URL -u MAINNET_RPC_URL \
         -u DISCORD_WEBHOOK_URL -u PAUSER_KEY -u DRILL_OWNER_KEY -u OPERATOR_KEY -u OWNER_KEY \
         CARGO_NET_OFFLINE=true \
         $cmd > "$log.tmp" 2>&1; then res="PASS"; else res="FAIL"; fi
    { echo "$res: $cmd"; tail -n 200 "$log.tmp"; } > "$log"; rm -f "$log.tmp"
  fi
  GATE_RESULTS+=("$res")
done

# ---- scans: file names and counts only, never values ----
{
  echo "# tracked secret-like files (must be empty; .env.example is acceptable)"
  git ls-files | grep -E '(^|/)(\.env(\..*)?|config/local\.toml|discord-audit\.csv|results\.csv)$' | grep -v '\.env\.example$'
} > "$OUT/scans/tracked-secret-files.txt" 2>/dev/null
{
  echo "# files matching webhook, provider-key, or 32-byte hex patterns (names only; review before quoting)"
  git grep -lEi 'discord(app)?\.com/api/webhooks/[0-9]+' 2>/dev/null
  git grep -lEi '(alchemy|infura|quiknode|ankr|blastapi)[^[:space:]"'"'"']*[A-Za-z0-9_-]{20,}' 2>/dev/null
  git grep -lE '0x[0-9a-fA-F]{64}' 2>/dev/null
} > "$OUT/scans/secret-patterns-files.txt"
TRACKED_SECRETS=$(grep -vc '^#' "$OUT/scans/tracked-secret-files.txt" 2>/dev/null || true)

# ---- invariants (heuristic, counts and path:line only) ----
count() { grep -c . "$1" 2>/dev/null || echo 0; }
S1_HITS="$OUT/invariants/S1-signing-in-src.txt"
grep -rnE "PrivateKeySigner|LocalSigner|EthereumWallet|send_transaction|send_raw_transaction|eth_sendRawTransaction|\.sign_" src/ 2>/dev/null \
  | grep -v 'cfg(feature = "live")' > "$S1_HITS"
S2_DRY="$OUT/invariants/S2-default-dry-run.txt"
grep -nE "^[[:space:]]*dry_run[[:space:]]*=" config/default.toml > "$S2_DRY" 2>/dev/null
S3_HITS="$OUT/invariants/S3-discord-keys.txt"
grep -rnE "OWNER_KEY|OPERATOR_KEY|DRILL_OWNER_KEY" src/discord.rs src/bin/discord-bot.rs 2>/dev/null > "$S3_HITS"
S6_FILE="$OUT/invariants/S6-layout.txt"
{
  grep -rn "forge-std" contracts test foundry.toml remappings.txt 2>/dev/null
  test -d lib && echo "VIOLATION: lib/ exists"
  test -s remappings.txt && echo "VIOLATION: remappings.txt not empty"
  find src -name '*.sol' 2>/dev/null
  grep -rn '#\[path' src/ 2>/dev/null
} > "$S6_FILE"
S8_FILE="$OUT/invariants/S8-sepolia-gate.txt"
grep -n "84532" script/deploy-mocks-sepolia.sh > "$S8_FILE" 2>/dev/null
L1_FILE="$OUT/invariants/L-live-path-present.txt"
{ grep -rn 'feature = "live"' src/ 2>/dev/null; ls src/bin/live.rs 2>/dev/null; grep -rn "LIVE_ARM" src/ 2>/dev/null; } > "$L1_FILE"

verdict_empty() { [ ! -s "$1" ] && echo PASS || echo FAIL; }
S1=$(verdict_empty "$S1_HITS")
S2=$(grep -qE 'dry_run[[:space:]]*=[[:space:]]*true' "$S2_DRY" 2>/dev/null && echo PASS || echo FAIL)
S3=$(verdict_empty "$S3_HITS")
S5=$([ "${TRACKED_SECRETS:-0}" = "0" ] && echo PASS || echo FAIL)
S6=$(verdict_empty "$S6_FILE")
S8=$([ -s "$S8_FILE" ] && echo PASS || echo FAIL)
LIVE_PRESENT=$([ -s "$L1_FILE" ] && echo yes || echo no)

# ---- G0 verdict ----
G0=READY
for r in "${GATE_RESULTS[@]}"; do [ "$r" = "PASS" ] || G0=INCONCLUSIVE; done
for r in "${GATE_RESULTS[@]}"; do [ "$r" = "FAIL" ] && G0="NOT READY"; done
for r in "$S1" "$S2" "$S3" "$S5" "$S6" "$S8"; do [ "$r" = "FAIL" ] && G0="NOT READY"; done
[ "$CLEAN" = "true" ] || { [ "$G0" = "READY" ] && G0=INCONCLUSIVE; }
STAGE_READY=$([ "$G0" = "READY" ] && echo G0 || echo NONE)
LEVEL=$([ "$G0" = "READY" ] && echo E1 || echo E0)

{
cat <<JSON
{
  "schema": 1,
  "created_at": "$(now_utc)",
  "expires_at": "$(plus_days "$TTL_DAYS")",
  "commit": "$COMMIT",
  "tree_clean": $CLEAN,
  "chain_id": $CHAIN_ID,
  "stage_ready": "$STAGE_READY",
  "evidence_level": "$LEVEL",
  "live_path_present": "$LIVE_PRESENT",
  "hashes": {
    "freeze_md": "$(hash_of docs/FREEZE.md)",
    "cargo_lock": "$(hash_of Cargo.lock)",
    "foundry_toml": "$(hash_of foundry.toml)",
    "default_toml": "$(hash_of config/default.toml)"
  },
  "gates": {
JSON
last=$(( ${#GATE_NAMES[@]} - 1 ))
for i in "${!GATE_NAMES[@]}"; do
  sep=","; [ "$i" -eq "$last" ] && sep=""
  echo "    \"${GATE_NAMES[$i]}\": \"${GATE_RESULTS[$i]}\"$sep"
done
cat <<JSON
  },
  "invariants": { "S1": "$S1", "S2": "$S2", "S3": "$S3", "S5": "$S5", "S6": "$S6", "S8": "$S8" },
  "stages": {
    "G0": { "verdict": "$G0" },
    "G1": { "verdict": "INCONCLUSIVE", "needs": "runs/shadow-*.json from --emit-evidence" },
    "G2": { "verdict": "INCONCLUSIVE", "needs": "fork-check and fork-matrix output at a pinned block" },
    "G3": { "verdict": "INCONCLUSIVE", "needs": "onchain.json with drill cases D1-D12 on chain 84532" },
    "G4": { "verdict": "INCONCLUSIVE", "needs": "onchain.json pre-flight read-backs on chain 8453" },
    "G5": { "verdict": "INCONCLUSIVE", "needs": "canary runs/*.json and ledger verify output" },
    "G6": { "verdict": "INCONCLUSIVE", "needs": "previous stage plus fresh readiness run" }
  },
  "note": "G1-G6 verdicts are set by the readiness auditor after reviewing supplied evidence; this script only establishes G0."
}
JSON
} > "$OUT/manifest.json"

write_sums "$OUT"

echo "evidence: $OUT"
echo "commit: $COMMIT (clean=$CLEAN)"
for i in "${!GATE_NAMES[@]}"; do printf '  %-18s %s\n' "${GATE_NAMES[$i]}" "${GATE_RESULTS[$i]}"; done
echo "  invariants: S1=$S1 S2=$S2 S3=$S3 S5=$S5 S6=$S6 S8=$S8 (heuristic; auditor must read in context)"
echo "  live path present: $LIVE_PRESENT"
echo "G0: $G0   stage_ready: $STAGE_READY   evidence_level: $LEVEL"
[ "$RUN_GATES" -eq 1 ] || echo "note: gates were not run; re-run with --run-gates"
