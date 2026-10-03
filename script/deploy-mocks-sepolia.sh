#!/usr/bin/env bash
# Deploy mock arb stack for drills on Base Sepolia ONLY (chain 84532).
# Real DEX addresses do NOT exist on Sepolia, so this deploys:
#   mock USDC (6 dec) + mock WETH (18 dec), MockVault (0 bps fee),
#   MockRouter (~2% profitable round-trip), FlashArbExecutor wired to them.
#
# Safety:
#   - Refuses to run unless chain-id == 84532 (never mainnet).
#   - Throwaway drill keys only. Key lives in env (DRILL_OWNER_KEY), never in
#     repo, never logged. Unset after use.
#   - Dust amounts only. This script moves no real funds.
#
# Usage:
#   export BASE_SEPOLIA_RPC_URL="https://sepolia.base.org"  # or your provider
#   export OPERATOR="0x..."   # bot submitter drill address
#   export PAUSER="0x..."     # ops/Discord drill address (pauser key ONLY)
#   export DRILL_OWNER_KEY="0x..."  # throwaway owner key (cast wallet new)
#   export MAX_FLASH_USDC_BASE="10000000"  # optional, default $10 dust
#   ./script/deploy-mocks-sepolia.sh
#
# Prereqs: forge + cast (foundry), faucet Sepolia ETH on owner address.
set -euo pipefail

# Fallback for default foundry install location when not on PATH.
if ! command -v cast >/dev/null 2>&1 && [ -x "$HOME/.foundry/bin/cast" ]; then
  export PATH="$HOME/.foundry/bin:$PATH"
fi

: "${BASE_SEPOLIA_RPC_URL:?Set BASE_SEPOLIA_RPC_URL first}"
: "${OPERATOR:?Set OPERATOR drill address first}"
: "${PAUSER:?Set PAUSER drill address first}"
: "${DRILL_OWNER_KEY:?Set DRILL_OWNER_KEY (throwaway drill key) first}"
MAX_FLASH_USDC_BASE="${MAX_FLASH_USDC_BASE:-10000000}"
RPC="$BASE_SEPOLIA_RPC_URL"
SWAP_SELECTOR="0xd5bcb9b5"  # MockRouter.swap(address,address,uint256,uint256,address) — test-only shape

echo "== 0. Safety gate: chain-id must be 84532 (Base Sepolia) =="
CHAIN_ID="$(cast chain-id --rpc-url "$RPC")"
echo "chain-id: $CHAIN_ID"
if [ "$CHAIN_ID" != "84532" ]; then
  echo "ABORT: not Base Sepolia (got $CHAIN_ID). Refusing to deploy." >&2
  exit 1
fi

OWNER_ADDR="$(cast wallet address --private-key "$DRILL_OWNER_KEY")"
echo "owner drill address: $OWNER_ADDR"
echo "operator: $OPERATOR"
echo "pauser:   $PAUSER"
echo "maxFlash (base units): $MAX_FLASH_USDC_BASE"
echo "Faucet check (owner Sepolia ETH):"
cast balance --rpc-url "$RPC" "$OWNER_ADDR"

echo "== 1. Build =="
forge build

deploy() { # $1 = create-args... ; prints deployed address
  forge create "$@" --rpc-url "$RPC" --private-key "$DRILL_OWNER_KEY" --broadcast \
    | grep -E "Deployed to:" | awk '{print $3}'
}

send() { # $1+ = cast send args (contract + sig + params)
  cast send --rpc-url "$RPC" --private-key "$DRILL_OWNER_KEY" "$@" > /dev/null
  echo "  ok: $2"
}

echo "== 2. Deploy mock tokens =="
MUSDC="$(deploy test/mocks/MockERC20.sol:MockERC20 --constructor-args "Mock USDC" "mUSDC" 6)"
echo "mUSDC: $MUSDC"
MWETH="$(deploy test/mocks/MockERC20.sol:MockERC20 --constructor-args "Mock WETH" "mWETH" 18)"
echo "mWETH: $MWETH"

echo "== 3. Deploy MockVault (0 bps) + MockRouter =="
MVAULT="$(deploy test/mocks/MockVault.sol:MockVault)"
echo "MockVault: $MVAULT"
MROUTER="$(deploy test/mocks/MockRouter.sol:MockRouter)"
echo "MockRouter: $MROUTER"

echo "== 4. Rates (~2%: 1 mUSDC -> 4e14 mWETH -> ~1.02e6 mUSDC) =="
send "$MROUTER" "setRate(address,address,uint256,uint256)" "$MUSDC" "$MWETH" 400000000 1
send "$MROUTER" "setRate(address,address,uint256,uint256)" "$MWETH" "$MUSDC" 255 100000000000

echo "== 5. Fund vault + router (dust) =="
send "$MUSDC" "mint(address,uint256)" "$MVAULT" 1000000000000
send "$MWETH" "mint(address,uint256)" "$MROUTER" 1000000000000000000000
send "$MUSDC" "mint(address,uint256)" "$MROUTER" 1000000000000

echo "== 6. Deploy executor =="
EXE="$(deploy contracts/FlashArbExecutor.sol:FlashArbExecutor --constructor-args "$MVAULT" "$MUSDC")"
echo "Executor: $EXE"

echo "== 7. Configure (RUNBOOK section 3, dust caps) =="
send "$EXE" "setTokenAllowed(address,bool,bool)" "$MUSDC" true false
send "$EXE" "setTokenAllowed(address,bool,bool)" "$MWETH" true false
send "$EXE" "setRouterAllowed(address,bool)" "$MROUTER" true
# Mock selector is NEVER auto-enabled: explicit opt-in (test-only shape).
send "$EXE" "setRouterSelectorAllowed(address,bytes4,bool)" "$MROUTER" "$SWAP_SELECTOR" true
send "$EXE" "setOperator(address)" "$OPERATOR"
send "$EXE" "setPauser(address)" "$PAUSER"
send "$EXE" "setMaxFlashUSDC(uint256)" "$MAX_FLASH_USDC_BASE"

echo ""
echo "== DONE. Drill addresses (save OFF-repo) =="
echo "mUSDC:      $MUSDC"
echo "mWETH:      $MWETH"
echo "MockVault:  $MVAULT"
echo "MockRouter: $MROUTER"
echo "Executor:   $EXE"
echo ""
echo "Next (RUNBOOK sections 4-5), with the PAUSER key only:"
echo "  cast send --rpc-url \"\$BASE_SEPOLIA_RPC_URL\" --interactive $EXE \"pause()\""
echo "  cast call  --rpc-url \"\$BASE_SEPOLIA_RPC_URL\" $EXE \"paused()\"   # expect true"
echo "Unset your drill key now: unset DRILL_OWNER_KEY"
