// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "../contracts/FlashArbExecutor.sol";
import "./TestBase.sol";
import "./mocks/MockERC20.sol";
import "./mocks/MockVault.sol";
import "./mocks/MockRouter.sol";
import "./mocks/MockProdRouter.sol";

/// @title FlashArbExecutorTest
/// @notice Offline unit tests with mocked Vault/routers. No fork, no RPC.
contract FlashArbExecutorTest is TestBase {
    event ArbitrageExecuted(
        uint256 profitUSDC, address indexed tokenA, address indexed tokenB, uint8 direction, uint256 flashAmount
    );

    event OperatorUpdated(address indexed operator);
    event PauserUpdated(address indexed pauser);

    address private constant ATTACKER = address(0xA71CE);
    address private constant OPERATOR = address(0x0BE970A);
    address private constant PAUSER = address(0x9A05EA);

    FlashArbExecutor private executor;
    MockERC20 private usdc; // 6 decimals, like native Base USDC
    MockERC20 private weth; // 18 decimals
    MockTaxToken private tax; // 10% fee-on-transfer token
    MockVault private vault;
    MockRouter private buyRouter;
    MockRouter private sellRouter;
    MockAeroSlipstreamRouter private aeroRouter;
    MockUniSwapRouter02 private uniRouter;

    // 500 USDC flash; $5 min profit; both in USDC base units (6 decimals).
    uint256 private constant FLASH = 500_000_000;
    uint256 private constant MIN_PROFIT = 5_000_000;

    // Profitable fixture: 500 USDC -> 0.00025 WETH -> 510 USDC (+10 USDC).
    uint256 private constant BOUGHT = 250_000_000_000_000; // 2.5e14 wei
    uint256 private constant PROCEEDS = 510_000_000;
    uint256 private constant PROFIT = 10_000_000;

    function setUp() external {
        usdc = new MockERC20("USD Coin", "USDC", 6);
        weth = new MockERC20("Wrapped Ether", "WETH", 18);
        tax = new MockTaxToken("Tax Token", "TAX", 18);
        vault = new MockVault();
        buyRouter = new MockRouter();
        sellRouter = new MockRouter();
        aeroRouter = new MockAeroSlipstreamRouter();
        uniRouter = new MockUniSwapRouter02();
        executor = new FlashArbExecutor(address(vault), address(usdc));

        executor.setTokenAllowed(address(usdc), true, false);
        executor.setTokenAllowed(address(weth), true, false);
        executor.setRouterAllowed(address(buyRouter), true);
        executor.setRouterAllowed(address(sellRouter), true);
        executor.setRouterAllowed(address(aeroRouter), true);
        executor.setRouterAllowed(address(uniRouter), true);
        // Mock/test SWAP selector is NEVER auto-enabled: enable explicitly
        // per mock router (tests only; never on production router addresses).
        executor.setRouterSelectorAllowed(address(buyRouter), executor.SWAP_SELECTOR(), true);
        executor.setRouterSelectorAllowed(address(sellRouter), executor.SWAP_SELECTOR(), true);

        // Fund the venue mocks.
        usdc.mint(address(vault), 100_000_000_000);
        weth.mint(address(buyRouter), 1 ether);
        usdc.mint(address(sellRouter), 10_000_000_000);
        weth.mint(address(aeroRouter), 1 ether);
        usdc.mint(address(aeroRouter), 10_000_000_000);
        weth.mint(address(uniRouter), 1 ether);
        usdc.mint(address(uniRouter), 10_000_000_000);

        // Profitable rates by default.
        buyRouter.setRate(address(usdc), address(weth), 500_000_000_000, 1_000_000);
        sellRouter.setRate(address(weth), address(usdc), 2040, 1_000_000_000);
        aeroRouter.setRate(address(usdc), address(weth), 500_000_000_000, 1_000_000);
        aeroRouter.setRate(address(weth), address(usdc), 2040, 1_000_000_000);
        uniRouter.setRate(address(usdc), address(weth), 500_000_000_000, 1_000_000);
        uniRouter.setRate(address(weth), address(usdc), 2040, 1_000_000_000);
    }

    // ------------------------------------------------------------ helpers

    function _params(
        address tokenA,
        address tokenB,
        FlashArbExecutor.Direction direction,
        uint256 buyMin,
        uint256 sellMin,
        uint256 minProfit,
        uint256 deadline,
        uint256 flash
    ) internal view returns (FlashArbExecutor.ExecParams memory p) {
        bytes memory buyData = abi.encodeWithSelector(
            MockRouter.swap.selector, address(usdc), address(weth), flash, buyMin, address(executor)
        );
        bytes memory sellData = abi.encodeWithSelector(
            MockRouter.swap.selector, address(weth), address(usdc), BOUGHT, sellMin, address(executor)
        );
        p = FlashArbExecutor.ExecParams({
            tokenA: tokenA,
            tokenB: tokenB,
            direction: direction,
            flashAmount: flash,
            buyRouter: address(buyRouter),
            buyData: buyData,
            buyMinOut: buyMin,
            sellRouter: address(sellRouter),
            sellData: sellData,
            sellMinOut: sellMin,
            minProfitUSDC: minProfit,
            deadline: deadline
        });
    }

    function _goodAtoB() internal view returns (FlashArbExecutor.ExecParams memory) {
        return _params(
            address(usdc),
            address(weth),
            FlashArbExecutor.Direction.AtoB,
            BOUGHT,
            PROCEEDS,
            MIN_PROFIT,
            block.timestamp,
            FLASH
        );
    }

    // ------------------------------------------- production-selector helpers

    /// @notice Aero Slipstream buy leg: USDC -> middle, real struct layout.
    function _aeroBuy(address tokenIn, address tokenOut, uint256 amountIn, uint256 minOut, address recipient)
        internal
        view
        returns (bytes memory)
    {
        MockAeroSlipstreamRouter.ExactInputSingleParams memory q =
            MockAeroSlipstreamRouter.ExactInputSingleParams({
                tokenIn: tokenIn,
                tokenOut: tokenOut,
                tickSpacing: int24(200),
                recipient: recipient,
                deadline: block.timestamp,
                amountIn: amountIn,
                amountOutMinimum: minOut,
                sqrtPriceLimitX96: 0
            });
        return abi.encodeCall(MockAeroSlipstreamRouter.exactInputSingle, (q));
    }

    /// @notice Uni SwapRouter02 sell/buy leg: real struct layout (no deadline).
    function _uniLeg(address tokenIn, address tokenOut, uint256 amountIn, uint256 minOut, address recipient)
        internal
        view
        returns (bytes memory)
    {
        MockUniSwapRouter02.ExactInputSingleParams memory q = MockUniSwapRouter02.ExactInputSingleParams({
            tokenIn: tokenIn,
            tokenOut: tokenOut,
            fee: uint24(3000),
            recipient: recipient,
            amountIn: amountIn,
            amountOutMinimum: minOut,
            sqrtPriceLimitX96: 0
        });
        return abi.encodeCall(MockUniSwapRouter02.exactInputSingle, (q));
    }

    /// @notice Profitable AtoB fixture with REAL production calldata:
    /// buy USDC -> WETH on the Aero-shape mock, sell WETH -> USDC on the
    /// Uni-shape mock.
    function _goodProdAtoB() internal view returns (FlashArbExecutor.ExecParams memory p) {
        p = FlashArbExecutor.ExecParams({
            tokenA: address(usdc),
            tokenB: address(weth),
            direction: FlashArbExecutor.Direction.AtoB,
            flashAmount: FLASH,
            buyRouter: address(aeroRouter),
            buyData: _aeroBuy(address(usdc), address(weth), FLASH, BOUGHT, address(executor)),
            buyMinOut: BOUGHT,
            sellRouter: address(uniRouter),
            sellData: _uniLeg(address(weth), address(usdc), BOUGHT, PROCEEDS, address(executor)),
            sellMinOut: PROCEEDS,
            minProfitUSDC: MIN_PROFIT,
            deadline: block.timestamp
        });
    }

    // --------------------------------------------------------------- tests

    function test_Execute_Success_AtoB() external {
        vm.expectEmit(true, true, false, true);
        emit ArbitrageExecuted(PROFIT, address(usdc), address(weth), uint8(FlashArbExecutor.Direction.AtoB), FLASH);
        executor.execute(_goodAtoB());

        // Vault made whole; profit retained by the executor.
        assertEq(usdc.balanceOf(address(vault)), 100_000_000_000, "vault not repaid");
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "profit mismatch");

        // Owner can sweep retained profit.
        executor.sweep(address(usdc), address(this), PROFIT);
        assertEq(usdc.balanceOf(address(this)), PROFIT, "sweep mismatch");
        assertEq(usdc.balanceOf(address(executor)), 0, "executor not drained");
    }

    function test_Execute_Success_BtoA() external {
        FlashArbExecutor.ExecParams memory p = _params(
            address(weth),
            address(usdc),
            FlashArbExecutor.Direction.BtoA,
            BOUGHT,
            PROCEEDS,
            MIN_PROFIT,
            block.timestamp,
            FLASH
        );
        vm.expectEmit(true, true, false, true);
        emit ArbitrageExecuted(PROFIT, address(weth), address(usdc), uint8(FlashArbExecutor.Direction.BtoA), FLASH);
        executor.execute(p);
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "profit mismatch");
    }

    function test_RevertWhen_ProfitBelowMin() external {
        // Worse sell rate: 500 USDC -> 0.00025 WETH -> 502 USDC.
        // Repayment succeeds, but +2 USDC < $5 min profit.
        sellRouter.setRate(address(weth), address(usdc), 2008, 1_000_000_000);
        FlashArbExecutor.ExecParams memory p = _params(
            address(usdc),
            address(weth),
            FlashArbExecutor.Direction.AtoB,
            BOUGHT,
            FLASH, // leg-2 min accepts 502 USDC so the failure lands on the profit check
            MIN_PROFIT,
            block.timestamp,
            FLASH
        );
        vm.expectRevert(
            abi.encodeWithSelector(FlashArbExecutor.InsufficientProfit.selector, uint256(2_000_000), MIN_PROFIT)
        );
        executor.execute(p);
    }

    function test_RevertWhen_RouterNotAllowlisted() external {
        FlashArbExecutor.ExecParams memory p = _goodAtoB();
        p.sellRouter = ATTACKER;
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.RouterNotAllowlisted.selector, ATTACKER));
        executor.execute(p);
    }

    function test_RevertWhen_TokenNotAllowlisted() external {
        FlashArbExecutor.ExecParams memory p = _goodAtoB();
        p.tokenB = ATTACKER;
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.TokenNotAllowlisted.selector, ATTACKER));
        executor.execute(p);
    }

    function test_RevertWhen_DirectionMismatch() external {
        // AtoB requires tokenA == USDC (flash asset is leg-1 input).
        FlashArbExecutor.ExecParams memory p = _goodAtoB();
        p.tokenA = address(weth);
        p.tokenB = address(usdc);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.DirectionMismatch.selector));
        executor.execute(p);
    }

    function test_RevertWhen_FeeOnTransferBlocked() external {
        executor.setTokenAllowed(address(tax), true, true);
        tax.mint(address(buyRouter), 100_000_000_000);
        buyRouter.setRate(address(usdc), address(tax), 2, 1);
        FlashArbExecutor.ExecParams memory p = _params(
            address(usdc), address(tax), FlashArbExecutor.Direction.AtoB, 0, 0, MIN_PROFIT, block.timestamp, FLASH
        );
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.FeeOnTransferNotAllowed.selector, address(tax)));
        executor.execute(p);
    }

    function test_FeeOnTransfer_AllowedWhenEnabled() external {
        // Deterministic taxed math: buy 500 USDC -> 1000 TAX out, 10% kept as
        // fee => executor keeps 900 TAX (balance-diff accounting). Sell 900
        // TAX at 2x => 1800 USDC; USDC itself is untaxed. Repay 500 USDC =>
        // profit 1300 USDC. The gate (not the math) was the only barrier.
        executor.setTokenAllowed(address(tax), true, true);
        executor.setAllowFeeOnTransfer(true);
        tax.mint(address(buyRouter), 100_000_000_000);
        buyRouter.setRate(address(usdc), address(tax), 2, 1);
        sellRouter.setRate(address(tax), address(usdc), 2, 1);
        usdc.mint(address(sellRouter), 10_000_000_000);

        bytes memory buyData = abi.encodeWithSelector(
            MockRouter.swap.selector, address(usdc), address(tax), FLASH, uint256(0), address(executor)
        );
        bytes memory sellData = abi.encodeWithSelector(
            MockRouter.swap.selector, address(tax), address(usdc), uint256(900_000_000), uint256(0), address(executor)
        );
        FlashArbExecutor.ExecParams memory p = FlashArbExecutor.ExecParams({
            tokenA: address(usdc),
            tokenB: address(tax),
            direction: FlashArbExecutor.Direction.AtoB,
            flashAmount: FLASH,
            buyRouter: address(buyRouter),
            buyData: buyData,
            buyMinOut: 0,
            sellRouter: address(sellRouter),
            sellData: sellData,
            sellMinOut: 0,
            minProfitUSDC: MIN_PROFIT,
            deadline: block.timestamp
        });
        executor.execute(p);
        assertEq(usdc.balanceOf(address(executor)), 1_300_000_000, "taxed profit mismatch");
    }

    function test_OnlyOwner() external {
        FlashArbExecutor.ExecParams memory p = _goodAtoB();

        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOperatorOrOwner.selector));
        executor.execute(p);

        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setRouterAllowed(address(buyRouter), false);

        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setTokenAllowed(address(weth), false, false);

        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setMaxFlashUSDC(1);

        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotPauserOrOwner.selector));
        executor.pause();

        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.unpause();

        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.sweep(address(usdc), ATTACKER, 1);

        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.transferOwnership(ATTACKER);

        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setOperator(ATTACKER);

        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setPauser(ATTACKER);
    }

    function test_RevertWhen_DeadlineExpired() external {
        FlashArbExecutor.ExecParams memory p = _goodAtoB();
        p.deadline = 0; // block.timestamp defaults to 1 in tests
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.DeadlineExpired.selector));
        executor.execute(p);
    }

    function test_RevertWhen_ExceedsMaxFlash() external {
        FlashArbExecutor.ExecParams memory p = _goodAtoB();
        p.flashAmount = 501_000_000; // default cap is 500 USDC
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.ExceedsMaxFlash.selector, uint256(501_000_000), FLASH));
        executor.execute(p);
    }

    function test_RevertWhen_DirectCallback() external {
        address[] memory tokens = new address[](1);
        tokens[0] = address(usdc);
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = FLASH;
        uint256[] memory fees = new uint256[](1);
        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotVault.selector, ATTACKER));
        executor.receiveFlashLoan(tokens, amounts, fees, "");
    }

    function test_PauseFlow() external {
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.EnforcedPause.selector));
        executor.unpause();

        executor.pause();
        assertTrue(executor.paused(), "not paused");

        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.ExpectedPause.selector));
        executor.pause();

        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.EnforcedPause.selector));
        executor.execute(_goodAtoB());

        executor.unpause();
        assertTrue(!executor.paused(), "still paused");
        executor.execute(_goodAtoB());
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "profit mismatch");
    }

    function test_OwnershipTwoStep() external {
        executor.transferOwnership(ATTACKER);
        assertEq(executor.pendingOwner(), ATTACKER, "pending owner mismatch");
        vm.prank(ATTACKER);
        executor.acceptOwnership();
        assertEq(executor.owner(), ATTACKER, "owner mismatch");
        assertEq(executor.pendingOwner(), address(0), "pending not cleared");
    }

    // ------------------------------------------------- audit regression tests

    function test_RevertWhen_UnsolicitedVaultCallback() external {
        // Even the Vault itself cannot invoke the callback outside an
        // owner-initiated `execute` (no transient binding, guard not entered).
        address[] memory tokens = new address[](1);
        tokens[0] = address(usdc);
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = FLASH;
        uint256[] memory fees = new uint256[](1);
        vm.prank(address(vault));
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.UnexpectedCallback.selector));
        executor.receiveFlashLoan(tokens, amounts, fees, "");
    }

    function test_RevertWhen_NestedVaultCallback() external {
        NestRouter nest = new NestRouter(address(vault), address(executor), address(usdc));
        executor.setRouterAllowed(address(nest), true);
        executor.setRouterSelectorAllowed(address(nest), executor.SWAP_SELECTOR(), true);
        FlashArbExecutor.ExecParams memory p = _goodAtoB();
        p.buyRouter = address(nest);
        p.buyData = abi.encodeWithSelector(
            MockRouter.swap.selector, address(usdc), address(weth), FLASH, BOUGHT, address(executor)
        );
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NestedCallback.selector));
        executor.execute(p);
    }

    function test_RevertWhen_RecipientNotExecutor() external {
        FlashArbExecutor.ExecParams memory p = _goodAtoB();
        p.buyData =
            abi.encodeWithSelector(MockRouter.swap.selector, address(usdc), address(weth), FLASH, BOUGHT, ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.BadRecipient.selector, ATTACKER, address(executor)));
        executor.execute(p);
    }

    function test_AllowancesZeroAfterLegs() external {
        executor.execute(_goodAtoB());
        assertEq(usdc.allowance(address(executor), address(buyRouter)), 0, "buy allowance lingers");
        assertEq(weth.allowance(address(executor), address(sellRouter)), 0, "sell allowance lingers");
    }

    function test_RevertWhen_SweepZeroToken() external {
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.ZeroAddress.selector));
        executor.sweep(address(0), address(this), 1);
    }

    function test_SweepETH() external {
        (bool ok,) = address(executor).call{value: 0}("");
        assertTrue(!ok, "executor should reject stray ETH");
        uint256 before = address(this).balance;
        vm.deal(address(executor), 1 ether);
        executor.sweepETH(address(this), 1 ether);
        assertEq(address(this).balance - before, 1 ether, "eth sweep mismatch");
    }

    function test_RevertWhen_ExceedsHardCap() external {
        vm.expectRevert(
            abi.encodeWithSelector(
                FlashArbExecutor.ExceedsHardCap.selector, uint256(99_000_000_000_000), uint256(10_000_000_000_000)
            )
        );
        executor.setMaxFlashUSDC(99_000_000_000_000);
    }

    // --------------------------------------- production-selector tests

    function test_Selectors_MatchProductionRouters() external view {
        // Mock encodings use the same struct layouts as the real routers, so
        // their compiler-derived selectors must equal the pinned constants
        // (which in turn match the on-chain MethodIDs on Basescan).
        assertEq(
            uint256(uint32(MockAeroSlipstreamRouter.exactInputSingle.selector)),
            uint256(uint32(bytes4(0xa026383e))),
            "aero selector mismatch"
        );
        assertEq(
            uint256(uint32(MockUniSwapRouter02.exactInputSingle.selector)),
            uint256(uint32(bytes4(0x04e45aaf))),
            "uni selector mismatch"
        );
        assertEq(
            uint256(uint32(executor.AERO_EXACT_INPUT_SINGLE_SELECTOR())),
            uint256(uint32(bytes4(0xa026383e))),
            "executor aero selector mismatch"
        );
        assertEq(
            uint256(uint32(executor.UNI_EXACT_INPUT_SINGLE_SELECTOR())),
            uint256(uint32(bytes4(0x04e45aaf))),
            "executor uni selector mismatch"
        );
    }

    function test_ProductionSelectors_AutoEnabled() external view {
        assertTrue(
            executor.routerSelectorAllowed(address(aeroRouter), executor.AERO_EXACT_INPUT_SINGLE_SELECTOR()),
            "aero selector not auto-enabled"
        );
        assertTrue(
            executor.routerSelectorAllowed(address(uniRouter), executor.UNI_EXACT_INPUT_SINGLE_SELECTOR()),
            "uni selector not auto-enabled"
        );
    }

    function test_Execute_Success_ProductionSelectors_AeroBuy_UniSell() external {
        vm.expectEmit(true, true, false, true);
        emit ArbitrageExecuted(PROFIT, address(usdc), address(weth), uint8(FlashArbExecutor.Direction.AtoB), FLASH);
        executor.execute(_goodProdAtoB());
        assertEq(usdc.balanceOf(address(vault)), 100_000_000_000, "vault not repaid");
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "profit mismatch");
    }

    function test_Execute_Success_ProductionSelectors_UniBuy_AeroSell() external {
        FlashArbExecutor.ExecParams memory p = FlashArbExecutor.ExecParams({
            tokenA: address(usdc),
            tokenB: address(weth),
            direction: FlashArbExecutor.Direction.AtoB,
            flashAmount: FLASH,
            buyRouter: address(uniRouter),
            buyData: _uniLeg(address(usdc), address(weth), FLASH, BOUGHT, address(executor)),
            buyMinOut: BOUGHT,
            sellRouter: address(aeroRouter),
            sellData: _aeroBuy(address(weth), address(usdc), BOUGHT, PROCEEDS, address(executor)),
            sellMinOut: PROCEEDS,
            minProfitUSDC: MIN_PROFIT,
            deadline: block.timestamp
        });
        executor.execute(p);
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "profit mismatch");
    }

    function test_RevertWhen_AeroRecipientNotExecutor() external {
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        p.buyData = _aeroBuy(address(usdc), address(weth), FLASH, BOUGHT, ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.BadRecipient.selector, ATTACKER, address(executor)));
        executor.execute(p);
    }

    function test_RevertWhen_UniRecipientNotExecutor() external {
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        p.sellData = _uniLeg(address(weth), address(usdc), BOUGHT, PROCEEDS, ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.BadRecipient.selector, ATTACKER, address(executor)));
        executor.execute(p);
    }

    function test_RevertWhen_AeroBuyBadAmountIn() external {
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        p.buyData = _aeroBuy(address(usdc), address(weth), FLASH - 1, BOUGHT, address(executor));
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.BadAmountIn.selector, FLASH - 1, FLASH));
        executor.execute(p);
    }

    function test_RevertWhen_UniSellBadAmountIn() external {
        // Sell amountIn must equal exactly what leg 1 bought; the check runs
        // in the callback once `bought` is known.
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        p.sellData = _uniLeg(address(weth), address(usdc), BOUGHT + 1, PROCEEDS, address(executor));
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.BadAmountIn.selector, BOUGHT + 1, BOUGHT));
        executor.execute(p);
    }

    function test_RevertWhen_AeroSellBadAmountIn() external {
        FlashArbExecutor.ExecParams memory p = FlashArbExecutor.ExecParams({
            tokenA: address(usdc),
            tokenB: address(weth),
            direction: FlashArbExecutor.Direction.AtoB,
            flashAmount: FLASH,
            buyRouter: address(uniRouter),
            buyData: _uniLeg(address(usdc), address(weth), FLASH, BOUGHT, address(executor)),
            buyMinOut: BOUGHT,
            sellRouter: address(aeroRouter),
            sellData: _aeroBuy(address(weth), address(usdc), BOUGHT + 1, PROCEEDS, address(executor)),
            sellMinOut: PROCEEDS,
            minProfitUSDC: MIN_PROFIT,
            deadline: block.timestamp
        });
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.BadAmountIn.selector, BOUGHT + 1, BOUGHT));
        executor.execute(p);
    }

    function test_RevertWhen_ProductionBadTokenPath() external {
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        // Flipped path on the Aero buy leg.
        p.buyData = _aeroBuy(address(weth), address(usdc), FLASH, BOUGHT, address(executor));
        vm.expectRevert(
            abi.encodeWithSelector(
                FlashArbExecutor.BadTokenPath.selector, address(weth), address(usdc), address(usdc), address(weth)
            )
        );
        executor.execute(p);
    }

    function test_RevertWhen_AeroDeadlineExpired() external {
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        MockAeroSlipstreamRouter.ExactInputSingleParams memory q = MockAeroSlipstreamRouter.ExactInputSingleParams({
            tokenIn: address(usdc),
            tokenOut: address(weth),
            tickSpacing: int24(200),
            recipient: address(executor),
            deadline: 0, // expired: block.timestamp defaults to 1 in tests
            amountIn: FLASH,
            amountOutMinimum: BOUGHT,
            sqrtPriceLimitX96: 0
        });
        p.buyData = abi.encodeCall(MockAeroSlipstreamRouter.exactInputSingle, (q));
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.DeadlineExpired.selector));
        executor.execute(p);
    }

    function test_RevertWhen_ProductionSelectorDisabled() external {
        // Unknown/disabled selectors need an explicit owner allowlist entry:
        // disabling the Aero selector reverts, re-enabling restores success.
        executor.setRouterSelectorAllowed(address(aeroRouter), executor.AERO_EXACT_INPUT_SINGLE_SELECTOR(), false);
        vm.expectRevert(
            abi.encodeWithSelector(
                FlashArbExecutor.SelectorNotAllowlisted.selector,
                address(aeroRouter),
                executor.AERO_EXACT_INPUT_SINGLE_SELECTOR()
            )
        );
        executor.execute(_goodProdAtoB());
        executor.setRouterSelectorAllowed(address(aeroRouter), executor.AERO_EXACT_INPUT_SINGLE_SELECTOR(), true);
        executor.execute(_goodProdAtoB());
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "profit mismatch");
    }

    // ------------------------------------- No.1: allowlist auto-enable freeze

    function test_ProductionRouter_NoMockAutoEnable_UrExplicitOnly() external {
        address aeroProd = 0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5;
        address uniProd = 0x2626664c2603336E57B271c5C0b26F421741e481;
        address ur = 0x6fF5693b99212Da76ad316178A184AB56D299b43;
        assertEq(executor.UNIVERSAL_ROUTER(), ur, "UR constant mismatch");

        executor.setRouterAllowed(aeroProd, true);
        executor.setRouterAllowed(uniProd, true);
        executor.setRouterAllowed(ur, true);

        // Production routers: the 2 production selectors auto-enabled.
        assertTrue(
            executor.routerSelectorAllowed(aeroProd, executor.AERO_EXACT_INPUT_SINGLE_SELECTOR()),
            "aero prod: aero selector not auto-enabled"
        );
        assertTrue(
            executor.routerSelectorAllowed(aeroProd, executor.UNI_EXACT_INPUT_SINGLE_SELECTOR()),
            "aero prod: uni selector not auto-enabled"
        );
        assertTrue(
            executor.routerSelectorAllowed(uniProd, executor.AERO_EXACT_INPUT_SINGLE_SELECTOR()),
            "uni prod: aero selector not auto-enabled"
        );
        assertTrue(
            executor.routerSelectorAllowed(uniProd, executor.UNI_EXACT_INPUT_SINGLE_SELECTOR()),
            "uni prod: uni selector not auto-enabled"
        );
        // Mock selector MUST never be auto-enabled on production addresses.
        assertTrue(
            !executor.routerSelectorAllowed(aeroProd, executor.SWAP_SELECTOR()), "aero prod: mock selector auto-enabled"
        );
        assertTrue(
            !executor.routerSelectorAllowed(uniProd, executor.SWAP_SELECTOR()), "uni prod: mock selector auto-enabled"
        );
        // UR: explicit-selector-only — nothing auto-enabled, not even prod selectors.
        assertTrue(!executor.routerSelectorAllowed(ur, executor.SWAP_SELECTOR()), "UR: mock selector auto-enabled");
        assertTrue(
            !executor.routerSelectorAllowed(ur, executor.AERO_EXACT_INPUT_SINGLE_SELECTOR()),
            "UR: aero selector auto-enabled"
        );
        assertTrue(
            !executor.routerSelectorAllowed(ur, executor.UNI_EXACT_INPUT_SINGLE_SELECTOR()),
            "UR: uni selector auto-enabled"
        );
        // UR stays usable via explicit selector allowlisting.
        bytes4 customSel = bytes4(0x12345678);
        executor.setRouterSelectorAllowed(ur, customSel, true);
        assertTrue(executor.routerSelectorAllowed(ur, customSel), "UR: explicit selector not enabled");
    }

    function test_MockRouter_NoSwapAutoEnable() external {
        MockRouter fresh = new MockRouter();
        executor.setRouterAllowed(address(fresh), true);
        // Mock selector is never auto-enabled, even for fresh mock routers.
        assertTrue(
            !executor.routerSelectorAllowed(address(fresh), executor.SWAP_SELECTOR()), "fresh mock: swap auto-enabled"
        );
        // Production selectors still auto-enable; mock needs explicit opt-in.
        assertTrue(
            executor.routerSelectorAllowed(address(fresh), executor.AERO_EXACT_INPUT_SINGLE_SELECTOR()),
            "fresh mock: aero not auto-enabled"
        );
        executor.setRouterSelectorAllowed(address(fresh), executor.SWAP_SELECTOR(), true);
        assertTrue(
            executor.routerSelectorAllowed(address(fresh), executor.SWAP_SELECTOR()),
            "fresh mock: explicit swap not enabled"
        );
    }

    // ------------------------------------------------- No.2: split roles

    function test_SetOperator_Pauser_EventsAndZeroChecks() external {
        vm.expectEmit(true, false, false, true);
        emit OperatorUpdated(OPERATOR);
        executor.setOperator(OPERATOR);
        assertEq(executor.operator(), OPERATOR, "operator mismatch");

        vm.expectEmit(true, false, false, true);
        emit PauserUpdated(PAUSER);
        executor.setPauser(PAUSER);
        assertEq(executor.pauser(), PAUSER, "pauser mismatch");

        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.ZeroAddress.selector));
        executor.setOperator(address(0));

        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.ZeroAddress.selector));
        executor.setPauser(address(0));
    }

    function test_Operator_CanExecute_CannotAdmin() external {
        executor.setOperator(OPERATOR);
        bytes4 swapSel = executor.SWAP_SELECTOR();

        // Operator can execute.
        vm.prank(OPERATOR);
        executor.execute(_goodAtoB());
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "operator profit mismatch");

        // Operator cannot allowlist / configure / sweep / pause / unpause.
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
        executor.setMaxFlashUSDC(1);

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.sweep(address(usdc), OPERATOR, 1);

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotPauserOrOwner.selector));
        executor.pause();

        vm.prank(OPERATOR);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.unpause();
    }

    function test_Pauser_CanPause_CannotUnpauseExecuteSweep() external {
        executor.setPauser(PAUSER);

        // Pauser can pause.
        vm.prank(PAUSER);
        executor.pause();
        assertTrue(executor.paused(), "not paused");

        // Pauser cannot unpause.
        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.unpause();

        // Pauser cannot execute.
        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOperatorOrOwner.selector));
        executor.execute(_goodAtoB());

        // Pauser cannot sweep / allowlist.
        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.sweep(address(usdc), PAUSER, 1);

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setRouterAllowed(address(buyRouter), false);

        vm.prank(PAUSER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setMaxFlashUSDC(1);

        // Owner resumes.
        executor.unpause();
        assertTrue(!executor.paused(), "still paused");
    }

    function test_Owner_RetainsAll_AfterRolesSet() external {
        executor.setOperator(OPERATOR);
        executor.setPauser(PAUSER);

        // Owner can still execute.
        executor.execute(_goodAtoB());
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "owner profit mismatch");

        // Owner can still pause/unpause.
        executor.pause();
        assertTrue(executor.paused(), "not paused");
        executor.unpause();
        assertTrue(!executor.paused(), "still paused");

        // Owner can still manage allowlists, caps, roles, sweep.
        executor.setRouterSelectorAllowed(address(buyRouter), executor.SWAP_SELECTOR(), false);
        assertTrue(!executor.routerSelectorAllowed(address(buyRouter), executor.SWAP_SELECTOR()), "disable failed");
        executor.setRouterSelectorAllowed(address(buyRouter), executor.SWAP_SELECTOR(), true);
        executor.setMaxFlashUSDC(1_000_000);
        assertEq(executor.maxFlashUSDC(), 1_000_000, "cap mismatch");
        executor.setMaxFlashUSDC(FLASH);
        executor.sweep(address(usdc), address(this), PROFIT);
        assertEq(usdc.balanceOf(address(executor)), 0, "sweep failed");
    }

    // --------------------------------------- No.3: inner minOut binding

    function test_RevertWhen_AeroBuyInnerMinLooser() external {
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        // Inner buy min (BOUGHT - 1) looser than executor buyMinOut (BOUGHT).
        p.buyData = _aeroBuy(address(usdc), address(weth), FLASH, BOUGHT - 1, address(executor));
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.BadInnerMin.selector, BOUGHT - 1, BOUGHT));
        executor.execute(p);
    }

    function test_RevertWhen_UniSellInnerMinLooser() external {
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        // Inner sell min (PROCEEDS - 1) looser than executor sellMinOut.
        p.sellData = _uniLeg(address(weth), address(usdc), BOUGHT, PROCEEDS - 1, address(executor));
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.BadInnerMin.selector, PROCEEDS - 1, PROCEEDS));
        executor.execute(p);
    }

    function test_RevertWhen_UniBuyInnerMinLooser() external {
        FlashArbExecutor.ExecParams memory p = FlashArbExecutor.ExecParams({
            tokenA: address(usdc),
            tokenB: address(weth),
            direction: FlashArbExecutor.Direction.AtoB,
            flashAmount: FLASH,
            buyRouter: address(uniRouter),
            buyData: _uniLeg(address(usdc), address(weth), FLASH, BOUGHT - 1, address(executor)),
            buyMinOut: BOUGHT,
            sellRouter: address(aeroRouter),
            sellData: _aeroBuy(address(weth), address(usdc), BOUGHT, PROCEEDS, address(executor)),
            sellMinOut: PROCEEDS,
            minProfitUSDC: MIN_PROFIT,
            deadline: block.timestamp
        });
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.BadInnerMin.selector, BOUGHT - 1, BOUGHT));
        executor.execute(p);
    }

    function test_RevertWhen_AeroSellInnerMinLooser() external {
        FlashArbExecutor.ExecParams memory p = FlashArbExecutor.ExecParams({
            tokenA: address(usdc),
            tokenB: address(weth),
            direction: FlashArbExecutor.Direction.AtoB,
            flashAmount: FLASH,
            buyRouter: address(uniRouter),
            buyData: _uniLeg(address(usdc), address(weth), FLASH, BOUGHT, address(executor)),
            buyMinOut: BOUGHT,
            sellRouter: address(aeroRouter),
            sellData: _aeroBuy(address(weth), address(usdc), BOUGHT, PROCEEDS - 1, address(executor)),
            sellMinOut: PROCEEDS,
            minProfitUSDC: MIN_PROFIT,
            deadline: block.timestamp
        });
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.BadInnerMin.selector, PROCEEDS - 1, PROCEEDS));
        executor.execute(p);
    }

    function test_InnerMinTighterThanExecutorMin_Passes() external {
        // Tight inner mins (== actual output) with looser executor mins still
        // execute: inner >= executor is the binding, balance-diff stays green.
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        p.buyMinOut = BOUGHT - 1;
        p.sellMinOut = PROCEEDS - 1;
        executor.execute(p);
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "profit mismatch");
    }

    function test_InnerMinTighterThanExecutorMin_Passes_SwappedRouters() external {
        FlashArbExecutor.ExecParams memory p = FlashArbExecutor.ExecParams({
            tokenA: address(usdc),
            tokenB: address(weth),
            direction: FlashArbExecutor.Direction.AtoB,
            flashAmount: FLASH,
            buyRouter: address(uniRouter),
            buyData: _uniLeg(address(usdc), address(weth), FLASH, BOUGHT, address(executor)),
            buyMinOut: BOUGHT - 1,
            sellRouter: address(aeroRouter),
            sellData: _aeroBuy(address(weth), address(usdc), BOUGHT, PROCEEDS, address(executor)),
            sellMinOut: PROCEEDS - 1,
            minProfitUSDC: MIN_PROFIT,
            deadline: block.timestamp
        });
        executor.execute(p);
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "profit mismatch");
    }

    // --------------------------------------- No.3: router codehash monitor

    event RouterCodehashPinnedUpdated(address indexed router, bytes32 codehash);

    function test_CodehashMonitor_UnpinnedSkipsCheck() external view {
        (bytes32 pinned, bytes32 current, bool match_) = executor.checkRouterCodehash(address(aeroRouter));
        assertTrue(pinned == bytes32(0), "expected unpinned");
        assertTrue(current != bytes32(0), "expected router code");
        assertTrue(match_, "unpinned should match");
    }

    function test_CodehashMonitor_PinCurrentPasses() external {
        bytes32 current = address(aeroRouter).codehash;
        vm.expectEmit(true, false, false, true);
        emit RouterCodehashPinnedUpdated(address(aeroRouter), current);
        executor.setRouterCodehashPinned(address(aeroRouter), current);
        assertEq(uint256(executor.routerCodehashPinned(address(aeroRouter))), uint256(current), "pin mismatch");

        (bytes32 pinned, bytes32 cur, bool match_) = executor.checkRouterCodehash(address(aeroRouter));
        assertTrue(pinned == current, "pinned mismatch");
        assertTrue(cur == current, "current mismatch");
        assertTrue(match_, "should match");

        // Pinned-to-current still executes.
        executor.execute(_goodProdAtoB());
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "profit mismatch");
    }

    function test_CodehashMonitor_PinMismatchReverts() external {
        // Simulate a router change: pin the buy router to a DIFFERENT mock
        // address's codehash (uni mock bytecode != aero mock bytecode).
        bytes32 other = address(uniRouter).codehash;
        assertTrue(other != address(aeroRouter).codehash, "mocks share codehash; cannot simulate change");
        executor.setRouterCodehashPinned(address(aeroRouter), other);

        (bytes32 pinned, bytes32 cur, bool match_) = executor.checkRouterCodehash(address(aeroRouter));
        assertTrue(pinned == other, "pinned mismatch");
        assertTrue(cur != pinned, "should differ");
        assertTrue(!match_, "should not match");

        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        vm.expectRevert(
            abi.encodeWithSelector(FlashArbExecutor.RouterCodeChanged.selector, address(aeroRouter), other, cur)
        );
        executor.execute(p);
    }

    function test_CodehashMonitor_SellRouterPinMismatchReverts() external {
        bytes32 other = address(aeroRouter).codehash;
        assertTrue(other != address(uniRouter).codehash, "mocks share codehash; cannot simulate change");
        executor.setRouterCodehashPinned(address(uniRouter), other);

        (, bytes32 cur,) = executor.checkRouterCodehash(address(uniRouter));
        vm.expectRevert(
            abi.encodeWithSelector(FlashArbExecutor.RouterCodeChanged.selector, address(uniRouter), other, cur)
        );
        executor.execute(_goodProdAtoB());
    }

    function test_CodehashMonitor_UnpinRestoresExecution() external {
        bytes32 other = address(uniRouter).codehash;
        executor.setRouterCodehashPinned(address(aeroRouter), other);
        vm.expectRevert(
            abi.encodeWithSelector(
                FlashArbExecutor.RouterCodeChanged.selector, address(aeroRouter), other, address(aeroRouter).codehash
            )
        );
        executor.execute(_goodProdAtoB());

        // Unpin (0) restores backwards-compatible execution.
        executor.setRouterCodehashPinned(address(aeroRouter), bytes32(0));
        (,, bool match_) = executor.checkRouterCodehash(address(aeroRouter));
        assertTrue(match_, "unpinned should match");
        executor.execute(_goodProdAtoB());
        assertEq(usdc.balanceOf(address(executor)), PROFIT, "profit mismatch");
    }

    function test_CodehashMonitor_OnlyOwner() external {
        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setRouterCodehashPinned(address(aeroRouter), bytes32(uint256(1)));

        vm.prank(ATTACKER);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.NotOwner.selector));
        executor.setRouterCodehashPinned(address(0), bytes32(0));
    }

    function test_CodehashMonitor_RevertWhen_ZeroRouter() external {
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.ZeroAddress.selector));
        executor.setRouterCodehashPinned(address(0), bytes32(uint256(1)));
    }

    // --------------------------------------- No.4: vault fee>0 path

    function test_VaultFee_SmallFee_StillProfitable() external {
        // 30bps Vault fee on 500 USDC = 1.5 USDC: repay 501.5M, proceeds
        // 510M => profit 8.5M still clears the $5 min. The Vault keeps its
        // fee; the executor retains the remainder.
        vault.setFeeBps(address(usdc), 30);
        executor.execute(_goodAtoB());
        assertEq(usdc.balanceOf(address(executor)), 8_500_000, "fee>0 profit mismatch");
        assertEq(usdc.balanceOf(address(vault)), 100_000_000_000 + 1_500_000, "vault fee mismatch");
    }

    function test_VaultFee_HighFee_RevertsNoLoss() external {
        // 200bps Vault fee on 500 USDC = 10 USDC: repay 510M, proceeds 510M
        // => profit 0 < $5 min. The whole bundle reverts atomically: the
        // Vault is untouched and the executor holds nothing (revert, not
        // lose).
        vault.setFeeBps(address(usdc), 200);
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.InsufficientProfit.selector, uint256(0), MIN_PROFIT));
        executor.execute(_goodAtoB());
        assertEq(usdc.balanceOf(address(vault)), 100_000_000_000, "vault moved on revert");
        assertEq(usdc.balanceOf(address(executor)), 0, "executor kept funds on revert");
    }

    // --------------------------------------- No.4: sandwich/front-run drill

    function test_Sandwich_10bpsAdverseMove_RevertsNoLoss() external {
        // Front-run pushes the sell leg ~10bps against us: 250e12 WETH now
        // buys 509.75M USDC (< tight 510M sellMinOut). The whole bundle must
        // revert with zero fund movement, not settle thinner. (The mock
        // router's own minOut trips first; the executor's SellLegTooSmall
        // stays as defense-in-depth for taxed/unknown-selector paths.)
        sellRouter.setRate(address(weth), address(usdc), 2039, 1_000_000_000);
        FlashArbExecutor.ExecParams memory p = _goodAtoB();
        (bool ok,) = address(executor).call(abi.encodeCall(FlashArbExecutor.execute, (p)));
        assertTrue(!ok, "10bps sandwich must revert");
        assertEq(usdc.balanceOf(address(vault)), 100_000_000_000, "vault moved on sandwich revert");
        assertEq(usdc.balanceOf(address(executor)), 0, "executor kept funds on sandwich revert");
    }

    function test_Sandwich_50bpsAdverseMove_RevertsNoLoss() external {
        // 50bps adverse move on the sell leg: 507.50M USDC out.
        sellRouter.setRate(address(weth), address(usdc), 2030, 1_000_000_000);
        FlashArbExecutor.ExecParams memory p = _goodAtoB();
        (bool ok,) = address(executor).call(abi.encodeCall(FlashArbExecutor.execute, (p)));
        assertTrue(!ok, "50bps sandwich must revert");
        assertEq(usdc.balanceOf(address(vault)), 100_000_000_000, "vault moved on sandwich revert");
        assertEq(usdc.balanceOf(address(executor)), 0, "executor kept funds on sandwich revert");
    }

    function test_Sandwich_ProdSelectors_50bpsMove_RevertsNoLoss() external {
        // Same drill through the REAL production calldata shapes (Aero buy /
        // Uni sell): adverse 50bps move reverts, balances untouched.
        uniRouter.setRate(address(weth), address(usdc), 2030, 1_000_000_000);
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        (bool ok,) = address(executor).call(abi.encodeCall(FlashArbExecutor.execute, (p)));
        assertTrue(!ok, "50bps sandwich (prod selectors) must revert");
        assertEq(usdc.balanceOf(address(vault)), 100_000_000_000, "vault moved on sandwich revert");
        assertEq(usdc.balanceOf(address(executor)), 0, "executor kept funds on sandwich revert");
    }

    // --------------------------------------- No.4: deadline matrix (offline)

    function test_Deadline_OuterExpired_ProdSelectors() external {
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        p.deadline = 0; // block.timestamp defaults to 1 in tests
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.DeadlineExpired.selector));
        executor.execute(p);
    }

    function test_Deadline_OuterOk_InnerExpired_ProdSelectors() external {
        // Outer deadline fine but the Aero inner deadline expired: the
        // executor's inner-deadline pin must still revert.
        FlashArbExecutor.ExecParams memory p = _goodProdAtoB();
        p.deadline = block.timestamp + 30;
        MockAeroSlipstreamRouter.ExactInputSingleParams memory q = MockAeroSlipstreamRouter.ExactInputSingleParams({
            tokenIn: address(usdc),
            tokenOut: address(weth),
            tickSpacing: int24(200),
            recipient: address(executor),
            deadline: 0,
            amountIn: FLASH,
            amountOutMinimum: BOUGHT,
            sqrtPriceLimitX96: 0
        });
        p.buyData = abi.encodeCall(MockAeroSlipstreamRouter.exactInputSingle, (q));
        vm.expectRevert(abi.encodeWithSelector(FlashArbExecutor.DeadlineExpired.selector));
        executor.execute(p);
    }

    receive() external payable {}
}

/// @title NestRouter
/// @notice Malicious router mock: re-enters the executor via the Vault from
/// inside leg 1 to exercise the nested-callback guard.
contract NestRouter {
    address public vault;
    address public exec;
    address public token;

    constructor(address vault_, address exec_, address token_) {
        vault = vault_;
        exec = exec_;
        token = token_;
    }

    function swap(address, address, uint256, uint256, address) external returns (uint256) {
        address[] memory tokens = new address[](1);
        tokens[0] = token;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 1;
        MockVault(vault).flashLoan(exec, tokens, amounts, "nested");
        return 0;
    }
}
