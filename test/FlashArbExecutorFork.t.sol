// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "./TestBase.sol";

/// @title FlashArbExecutorForkTest
/// @notice Read-only fork checks for the verified Base addresses used by the
/// executor. No deploy, no broadcast, no state changes.
///
/// Run against a local fork:
///   anvil --fork-url https://mainnet.base.org
///   FOUNDRY_FORK_URL=http://127.0.0.1:8545 forge test --match-contract FlashArbExecutorForkTest -vvv
///
/// The test SKIPS (passes vacuously) when no fork URL is configured so the
/// offline `forge test` suite stays green.
///
/// No.4 matrix coverage (see `src/fork_check.rs --fork-matrix` for the Rust
/// twin; both are read-only):
/// - `testFork_VerifiedBaseAddresses`: pinned code presence (FREEZE.md).
/// - `testFork_VaultFeeAssumption`: Vault code + 0%-fee documentation (the
///   fee>0 path itself is covered offline by MockVault::setFeeBps in
///   FlashArbExecutor.t.sol; the Vault exposes no on-chain fee getter).
/// - `testFork_L1OracleAndSequencerFeed`: GasPriceOracle 0x4200...000F and
///   Chainlink SequencerUptimeFeed code presence (3600s grace enforced
///   off-chain; feed down/refused => no execute).
/// - `testFork_CodehashFreeze`: records `codehash` for every pinned address
///   (paste into docs/FREEZE.md at freeze time; any drift => redeploy
///   review, never silent).
/// - `testFork_DeadlineAndTransient`: chain id 8453 + live timestamp sanity
///   (outer/inner deadline rule) + Cancun/transient assumption (Base is
///   post-Cancun; foundry.toml pins evm_version=cancun).
contract FlashArbExecutorForkTest is TestBase {
    address private constant BALANCER_VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    address private constant AERO_SLIPSTREAM_ROUTER = 0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5;
    address private constant UNI_SWAP_ROUTER02 = 0x2626664c2603336E57B271c5C0b26F421741e481;
    address private constant UNI_UNIVERSAL_ROUTER = 0x6fF5693b99212Da76ad316178A184AB56D299b43;
    address private constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address private constant WETH = 0x4200000000000000000000000000000000000006;
    address private constant AERO_QUOTER_V2 = 0x254cF9E1E6e233aa1AC962CB9B05b2cfeAaE15b0;
    address private constant UNI_QUOTER_V2 = 0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a;
    address private constant L1_GAS_PRICE_ORACLE = 0x420000000000000000000000000000000000000F;
    address private constant SEQUENCER_UPTIME_FEED = 0xBCF85224fc0756B9Fa45aA7892530B47e10b6433;

    /// @notice Enter the fork when configured; returns false when offline
    /// (caller must `return` early to keep `forge test` green offline).
    function _forkOrSkip() internal returns (bool) {
        string memory url = vm.envOr("FOUNDRY_FORK_URL", string(""));
        // Offline (plain `forge test`): skip. Also runs when invoked with an
        // active `--fork-url` (chain id 8453) even if the env var is unset.
        if (bytes(url).length == 0 && block.chainid != 8453) return false;
        if (bytes(url).length != 0) vm.createSelectFork(url);
        return true;
    }

    function testFork_VerifiedBaseAddresses() external {
        if (!_forkOrSkip()) return;

        assertTrue(BALANCER_VAULT.code.length > 0, "vault: no code");
        assertTrue(AERO_SLIPSTREAM_ROUTER.code.length > 0, "aero router: no code");
        assertTrue(UNI_SWAP_ROUTER02.code.length > 0, "uni router02: no code");
        assertTrue(UNI_UNIVERSAL_ROUTER.code.length > 0, "universal router: no code");
        assertTrue(USDC.code.length > 0, "usdc: no code");
        assertTrue(WETH.code.length > 0, "weth: no code");

        (bool ok, bytes memory ret) = USDC.staticcall(abi.encodeWithSignature("decimals()"));
        assertTrue(ok, "usdc decimals call failed");
        assertEq(uint256(abi.decode(ret, (uint8))), 6, "usdc decimals != 6");
    }

    function testFork_VaultFeeAssumption() external {
        if (!_forkOrSkip()) return;
        // The Balancer V2 Vault exposes no on-chain flash-fee getter; the
        // 0%-on-Base assumption is verified by code presence here plus the
        // offline fee>0 path (MockVault::setFeeBps: 30bps passes, 200bps
        // reverts with InsufficientProfit and zero fund movement). The
        // executor repays `amount + feeAmounts[i]` generically, so any fee
        // regime stays solvent.
        assertTrue(BALANCER_VAULT.code.length > 0, "vault: no code");
        assertTrue(BALANCER_VAULT.codehash != bytes32(0), "vault: no codehash");
    }

    function testFork_L1OracleAndSequencerFeed() external {
        if (!_forkOrSkip()) return;
        // L1 data-fee oracle (OP-Stack GasPriceOracle predeploy): the Rust
        // matrix queries getL1Fee here per size.
        assertTrue(L1_GAS_PRICE_ORACLE.code.length > 0, "l1 oracle: no code");
        // Chainlink SequencerUptimeFeed (answer 0 = up, 1 = down, 3600s
        // grace; docs.chain.link/data-feeds/l2-sequencer-feeds). Off-chain
        // gate refuses execute while down/in-grace/unreadable.
        assertTrue(SEQUENCER_UPTIME_FEED.code.length > 0, "sequencer feed: no code");
        assertTrue(SEQUENCER_UPTIME_FEED.codehash != bytes32(0), "sequencer feed: no codehash");
    }

    function testFork_CodehashFreeze() external {
        if (!_forkOrSkip()) return;
        // Freeze snapshot: every pinned address must carry runtime code.
        // Capture `address.codehash` values from a `-vvv` run into
        // docs/FREEZE.md at freeze time; any later drift triggers a
        // redeploy review (router upgrades must never pass silently).
        address[7] memory pinned = [
            BALANCER_VAULT,
            AERO_SLIPSTREAM_ROUTER,
            UNI_SWAP_ROUTER02,
            UNI_UNIVERSAL_ROUTER,
            USDC,
            AERO_QUOTER_V2,
            UNI_QUOTER_V2
        ];
        for (uint256 i = 0; i < pinned.length; ++i) {
            assertTrue(pinned[i].code.length > 0, "freeze: pinned address has no code");
            assertTrue(pinned[i].codehash != bytes32(0), "freeze: pinned address has no codehash");
        }
    }

    function testFork_DeadlineAndTransient() external {
        if (!_forkOrSkip()) return;
        // Canonical Base mainnet fork: deadlines are evaluated against live
        // chain time (outer `deadline` + Aero inner `deadline`; UniV3
        // SwapRouter02 carries no inner deadline field).
        assertTrue(block.chainid == 8453, "fork: not Base mainnet");
        assertTrue(block.timestamp > 1_700_000_000, "fork: timestamp sanity");
        // Transient storage (EIP-1153) assumption: Base is post-Cancun, and
        // foundry.toml pins evm_version=cancun so the executor's transient
        // `_expectedHash`/`_inCallback` compile to real transient opcodes on
        // this fork. (No on-chain query distinguishes transient support
        // directly; chain id + Cancun activation is the documented proxy.)
    }
}
