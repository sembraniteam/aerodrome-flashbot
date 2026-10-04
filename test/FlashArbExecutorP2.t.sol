// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "../contracts/FlashArbExecutor.sol";
import "./TestBase.sol";
import "./mocks/MockERC20.sol";
import "./mocks/MockVault.sol";
import "./mocks/MockRouter.sol";

/// @title FlashArbExecutorP2Test
/// @notice P2 (G4 role-matrix review) gap-fill suite for ADR-002 §4 (L4).
/// @dev Offline and deterministic: mocks only, no fork, no RPC, no keys.
/// Complements `test/FlashArbExecutor.t.sol` — covers ONLY the P2 checklist
/// items missing there: ctor defaults + hard-cap constant, raise/lower
/// within cap (incl. 0 = halt and == hard cap), operator/pauser denial of
/// `sweepETH`/codehash-pin/`allowFeeOnTransfer`/role setters, events on cap/
/// sweep/pause mutations, and execute-gated-by-pause while sweep stays live.
/// The contract itself is unchanged (sweep-`to` stays owner-chosen per ADR-002
/// §4 option (a)); this file asserts that behavior, it does not alter it.
contract FlashArbExecutorP2Test is TestBase {
    event MaxFlashUSDCUpdated(uint256 maxFlashUSDC);
    event Swept(address indexed token, address indexed to, uint256 amount);
    event SweptETH(address indexed to, uint256 amount);
    event Paused(address indexed account);
    event Unpaused(address indexed account);

    address private constant ATTACKER = address(0xA71CE);
    address private constant OPERATOR = address(0x0BE970A);
    address private constant PAUSER = address(0x9A05EA);

    FlashArbExecutor private executor;
    MockERC20 private usdc;
    MockERC20 private weth;
    MockVault private vault;
    MockRouter private buyRouter;
    MockRouter private sellRouter;

    uint256 private constant FLASH = 500_000_000; // 500 USDC
    uint256 private constant BOUGHT = 250_000_000_000_000; // 0.00025 WETH
    uint256 private constant PROCEEDS = 510_000_000; // 510 USDC
    uint256 private constant MIN_PROFIT = 5_000_000; // $5

    // ADR-002 §4 / contract pins (USDC 6 decimals).
    uint256 private constant DEFAULT_MAX_FLASH_USDC = 5_000_000_000; // $5000
    uint256 private constant HARD_CAP = 10_000_000_000_000; // $10M

    function setUp() external {
        usdc = new MockERC20("USD Coin", "USDC", 6);
        weth = new MockERC20("Wrapped Ether", "WETH", 18);
        vault = new MockVault();
        buyRouter = new MockRouter();
        sellRouter = new MockRouter();
        executor = new FlashArbExecutor(address(vault), address(usdc));

        executor.setTokenAllowed(address(usdc), true, false);
        executor.setTokenAllowed(address(weth), true, false);
        executor.setRouterAllowed(address(buyRouter), true);
        executor.setRouterAllowed(address(sellRouter), true);
        executor.setRouterSelectorAllowed(address(buyRouter), executor.SWAP_SELECTOR(), true);
        executor.setRouterSelectorAllowed(address(sellRouter), executor.SWAP_SELECTOR(), true);

        usdc.mint(address(vault), 100_000_000_000);
        weth.mint(address(buyRouter), 1 ether);
        usdc.mint(address(sellRouter), 10_000_000_000);
        buyRouter.setRate(address(usdc), address(weth), 500_000_000_000, 1_000_000);
        sellRouter.setRate(address(weth), address(usdc), 2040, 1_000_000_000);
    }

    function _goodAtoB() internal view returns (FlashArbExecutor.ExecParams memory p) {
        bytes memory buyData = abi.encodeWithSelector(
            MockRouter.swap.selector, address(usdc), address(weth), FLASH, BOUGHT, address(executor)
        );
        bytes memory sellData = abi.encodeWithSelector(
            MockRouter.swap.selector, address(weth), address(usdc), BOUGHT, PROCEEDS, address(executor)
        );
        p = FlashArbExecutor.ExecParams({
            tokenA: address(usdc),
            tokenB: address(weth),
            direction: FlashArbExecutor.Direction.AtoB,
            flashAmount: FLASH,
            buyRouter: address(buyRouter),
            buyData: buyData,
            buyMinOut: BOUGHT,
            sellRouter: address(sellRouter),
            sellData: sellData,
            sellMinOut: PROCEEDS,
            minProfitUSDC: MIN_PROFIT,
            deadline: block.timestamp
        });
    }

    // ------------------------------------------------------------ P2: pins

    function test_P2_CtorDefaultsAndHardCap() external view {
        assertEq(executor.maxFlashUSDC(), DEFAULT_MAX_FLASH_USDC, "ctor maxFlashUSDC != $5000");
        assertEq(executor.MAX_FLASH_USDC_HARD_CAP(), HARD_CAP, "hard cap != $10M");
        assertEq(executor.owner(), address(this), "owner != deployer");
        assertEq(executor.operator(), address(0), "operator nonzero at ctor");
        assertEq(executor.pauser(), address(0), "pauser nonzero at ctor");
        assertTrue(!executor.paused(), "paused at ctor");
    }

    function test_P2_Caps_RaiseLowerWithinCap_ZeroHalt() external {
        // Lower within cap.
        vm.expectEmit(false, false, false, true);
        emit MaxFlashUSDCUpdated(1_000_000);
        executor.setMaxFlashUSDC(1_000_000);
        assertEq(executor.maxFlashUSDC(), 1_000_000, "lower failed");

        // Raise within cap.
        vm.expectEmit(false, false, false, true);
        emit MaxFlashUSDCUpdated(6_000_000_000);
        executor.setMaxFlashUSDC(6_000_000_000);
        assertEq(executor.maxFlashUSDC(), 6_000_000_000, "raise failed");

        // Exactly the hard cap is allowed (redeploy only beyond it).
        executor.setMaxFlashUSDC(HARD_CAP);
        assertEq(executor.maxFlashUSDC(), HARD_CAP, "hard-cap-exact failed");

        // One unit above the hard cap reverts.
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.ExceedsHardCap.selector, HARD_CAP + 1, HARD_CAP));
        executor.setMaxFlashUSDC(HARD_CAP + 1);

        // 0 = halt: any flash amount then exceeds the cap.
        executor.setMaxFlashUSDC(0);
        assertEq(executor.maxFlashUSDC(), 0, "zero halt failed");
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.ExceedsMaxFlash.selector, FLASH, 0));
        executor.execute(_goodAtoB());
    }

    // ------------------------------------------------------------ P2: roles

    function test_P2_Operator_CannotAdminOrSweepETH() external {
        executor.setOperator(OPERATOR);
        bytes4 swapSel = executor.SWAP_SELECTOR();

        // Positive control: operator can execute.
        vm.prank(OPERATOR);
        executor.execute(_goodAtoB());

        // Operator cannot unpause / sweep / sweepETH.
        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.unpause();

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.sweep(address(usdc), OPERATOR, 1);

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.sweepETH(OPERATOR, 1);

        // Operator cannot touch allowlists / codehash pin / limits.
        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setRouterAllowed(address(buyRouter), false);

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setRouterSelectorAllowed(address(buyRouter), swapSel, false);

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setTokenAllowed(address(weth), false, false);

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setRouterCodehashPinned(address(buyRouter), bytes32(uint256(1)));

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setAllowFeeOnTransfer(true);

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setMaxFlashUSDC(1);

        // Operator cannot touch roles / ownership.
        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setOperator(ATTACKER);

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setPauser(ATTACKER);

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.transferOwnership(ATTACKER);

        // Operator cannot pause either (pauser-or-owner only).
        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotPauserOrOwner.selector));
        executor.pause();
    }

    function test_P2_Pauser_CannotAdminSweepOrExecute() external {
        executor.setPauser(PAUSER);
        bytes4 swapSel = executor.SWAP_SELECTOR();

        // Positive control: pauser can pause.
        vm.prank(PAUSER);
        executor.pause();
        assertTrue(executor.paused(), "pauser pause failed");
        executor.unpause();

        // Pauser cannot unpause / execute / sweep / sweepETH.
        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.unpause();

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOperatorOrOwner.selector));
        executor.execute(_goodAtoB());

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.sweep(address(usdc), PAUSER, 1);

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.sweepETH(PAUSER, 1);

        // Pauser cannot touch allowlists / codehash pin / limits.
        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setRouterAllowed(address(buyRouter), false);

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setRouterSelectorAllowed(address(buyRouter), swapSel, false);

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setTokenAllowed(address(weth), false, false);

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setRouterCodehashPinned(address(buyRouter), bytes32(uint256(1)));

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setAllowFeeOnTransfer(true);

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setMaxFlashUSDC(1);

        // Pauser cannot touch roles / ownership.
        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setOperator(ATTACKER);

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setPauser(ATTACKER);

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.transferOwnership(ATTACKER);
    }

    // ------------------------------------------------------------ P2: pause

    function test_P2_PauseGatesExecute_SweepStaysLive() external {
        vm.expectEmit(true, false, false, false);
        emit Paused(address(this));
        executor.pause();

        // Execute is gated while paused.
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.EnforcedPause.selector));
        executor.execute(_goodAtoB());

        // Sweep (ERC20 + ETH) stays live while paused for recovery.
        usdc.mint(address(executor), 1_000_000);
        vm.expectEmit(true, true, false, true);
        emit Swept(address(usdc), address(this), 1_000_000);
        executor.sweep(address(usdc), address(this), 1_000_000);
        assertEq(usdc.balanceOf(address(executor)), 0, "sweep while paused failed");

        vm.deal(address(executor), 1 ether);
        vm.expectEmit(true, false, false, true);
        emit SweptETH(address(this), 1 ether);
        executor.sweepETH(address(this), 1 ether);
        assertEq(address(executor).balance, 0, "sweepETH while paused failed");

        // Unpause restores execution.
        vm.expectEmit(true, false, false, false);
        emit Unpaused(address(this));
        executor.unpause();
        executor.execute(_goodAtoB());
    }

    receive() external payable {}
}
