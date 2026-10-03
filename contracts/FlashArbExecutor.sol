// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title IERC20
/// @notice Minimal ERC20 interface used by the executor (inline to avoid new
/// dependencies; the repo is offline-first and forge-std/OZ are not vendored).
interface IERC20 {
    function balanceOf(address account) external view returns (uint256);
    function allowance(address owner, address spender) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

/// @title IBalancerVault
/// @notice Subset of the Balancer V2 Vault interface needed for flash loans.
/// @dev Verified Base address: 0xBA12222222228d8Ba445958a75a0704d566BF2C8.
/// Flash-loan fee on Base is 0%, but repayment is computed generically as
/// `amount + feeAmounts[i]` so any fee regime stays solvent.
interface IBalancerVault {
    function flashLoan(address recipient, address[] memory tokens, uint256[] memory amounts, bytes memory userData)
        external;
}

/// @title FlashArbExecutor
/// @notice Minimal atomic cross-DEX arbitrage executor for Base (chain id 8453).
/// @dev Design notes (read before any testnet deploy):
/// - Sibling of the Rust paper-trading harness in this repo. This contract
///   holds funds and borrows; the Rust bot only quotes dry-runs. Never reuse a
///   paper-trading key as the owner here.
/// - Single-asset model: the flash asset is ALWAYS USDC
///   (0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913, 6 decimals). Profit is
///   measured as the post-repay USDC balance delta, so `minProfitUSDC` needs no
///   oracle. WETH-flash legs are intentionally unsupported.
/// - Two legs are executed as owner-crafted opaque router calls
///   (`buyData`/`sellData`). The executor enforces router allowlist +
///   per-selector calldata checks plus per-leg minimums via balance-diff
///   measurement. Supported production shapes:
///   Aerodrome Slipstream SwapRouter
///   (0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5, tickSpacing-based) and
///   Uniswap SwapRouter02 (0x2626664c2603336E57B271c5C0b26F421741e481,
///   fee-based) `exactInputSingle`, plus UniversalRouter
///   (0x6fF5693b99212Da76ad316178A184AB56D299b43) via explicit selector
///   allowlisting — the off-chain quoter builds the calldata, the executor
///   enforces router allowlist + per-leg minimums via balance-diff measurement.
/// - QUOTER RECIPIENT RULE: the off-chain quoter MUST build `buyData`/`sellData`
///   with the output recipient set to `address(this)` (the executor) for EVERY
///   supported shape:
///   - mock/test `swap(address,address,uint256,uint256,address)`: `to`.
///   - Aerodrome `exactInputSingle`: `recipient`.
///   - Uniswap SwapRouter02 `exactInputSingle`: `recipient`.
///   The contract decodes and enforces `recipient == address(this)` plus the
///   expected token path and buy `amountIn == flashAmount` (sell
///   `amountIn == bought` is enforced in the callback once `bought` is known)
///   for all three known selectors. For any other allowlisted selector the
///   recipient cannot be decoded generically and MUST be enforced off-chain by
///   the quoter; prefer router-specific wrappers that pin the recipient
///   on-chain when adding new selectors.
/// - Direction pins leg order to the pair orientation:
///   AtoB = leg1 sells tokenA for tokenB, leg2 sells tokenB back for tokenA
///   (requires tokenA == USDC); BtoA is mirrored (requires tokenB == USDC).
/// - Tokens arrays passed to the Vault are single-element, hence trivially
///   sorted as required by Balancer V2.
/// - `receiveFlashLoan` carries no `nonReentrant` modifier on purpose: it runs
///   INSIDE the `execute` guarded section (a modifier here would always
///   revert). Instead it requires `_status == _ENTERED` (guard held by the
///   owner-initiated `execute`), verifies the transient `_expectedHash` bound
///   to this `execute`, clears it before any external call, and uses the
///   transient `_inCallback` flag to block nested Vault callbacks (e.g. a
///   malicious router re-entering via the Vault).
/// - Approvals are fully revoked (`approve(0)`) after each router leg in the
///   same callback so no lingering allowance survives the trade.
/// - WETH is handled strictly as an ERC20 token in this contract. There is no
///   wrap/unwrap path and no `receive()`/`fallback()`; stray ETH reverts
///   instead of getting stuck, and any ETH forced in (coinbase/selfdestruct)
///   can be recovered via `sweepETH`.
/// - ADMIN SAFETY: `setMaxFlashUSDC`, router/token allowlist updates and
///   selector updates are owner-only and SHOULD be fronted by a timelock
///   (e.g. 24-48h) in production so users can exit before risk parameters
///   change. Ownership stays two-step (`transferOwnership`/`acceptOwnership`).
/// - This contract intentionally has no `receive()`/`fallback()`: stray ETH
///   transfers revert instead of getting stuck.
contract FlashArbExecutor {
    // ---------------------------------------------------------------- errors

    error NotOwner();
    error NotOperatorOrOwner();
    error NotPauserOrOwner();
    error NotPendingOwner();
    error ZeroAddress();
    error IdenticalTokens();
    error TokenNotAllowlisted(address token);
    error RouterNotAllowlisted(address router);
    error SelectorNotAllowlisted(address router, bytes4 selector);
    error BadRecipient(address got, address want);
    error BadTokenPath(address gotIn, address gotOut, address wantIn, address wantOut);
    error BadAmountIn(uint256 got, uint256 want);
    error FeeOnTransferNotAllowed(address token);
    error DirectionMismatch();
    error DeadlineExpired();
    error ZeroAmount();
    error EmptyCalldata();
    error ExceedsMaxFlash(uint256 amount, uint256 max);
    error ExceedsHardCap(uint256 max, uint256 cap);
    error NotVault(address caller);
    error UnexpectedCallback();
    error NestedCallback();
    error UnexpectedLoan(address token, uint256 amount);
    error RouterCallFailed(address router);
    error ApproveFailed();
    error TransferFailed();
    error ETHTransferFailed();
    error BuyLegTooSmall(uint256 got, uint256 min);
    error SellLegTooSmall(uint256 got, uint256 min);
    error InsufficientProfit(uint256 profit, uint256 min);
    error BadInnerMin(uint256 innerMin, uint256 executorMin);
    error RouterCodeChanged(address router, bytes32 pinned, bytes32 current);
    error EnforcedPause();
    error ExpectedPause();
    error ShortCalldata();
    error Reentrant();

    // ------------------------------------------------------------------ types

    /// @notice Leg order relative to the (tokenA, tokenB) pair orientation.
    enum Direction {
        AtoB,
        BtoA
    }

    /// @notice Allowlist entry. Only vetted NON-tax tokens may be listed with
    /// `feeOnTransfer == false`; the execution gate reverts for flagged
    /// tokens unless the global `allowFeeOnTransfer` switch is on.
    struct TokenConfig {
        bool allowed;
        bool feeOnTransfer;
    }

    /// @notice Parameters for one atomic arbitrage. Built off-chain by the
    /// owner (quoter) and executed atomically inside the flash-loan callback.
    struct ExecParams {
        address tokenA;
        address tokenB;
        Direction direction;
        /// @dev USDC borrowed from the Balancer Vault (6 decimals).
        uint256 flashAmount;
        address buyRouter;
        /// @dev Opaque call into `buyRouter`: must spend exactly `flashAmount`
        /// USDC (approved by the executor) and deliver the middle token to
        /// this contract. The quoter MUST set the router output recipient to
        /// `address(this)`; known shapes (mock `swap` + both production
        /// `exactInputSingle`) are decoded and enforced on-chain.
        bytes buyData;
        /// @dev Minimum middle-token units accepted out of leg 1.
        uint256 buyMinOut;
        address sellRouter;
        /// @dev Opaque call into `sellRouter`: must spend the bought middle
        /// token (approved by the executor) and deliver USDC to this contract.
        /// The quoter MUST set the router output recipient to `address(this)`.
        bytes sellData;
        /// @dev Minimum USDC accepted out of leg 2, before loan repayment.
        uint256 sellMinOut;
        /// @dev Net USDC retained after full repayment; revert otherwise.
        uint256 minProfitUSDC;
        /// @dev Unix seconds; the whole bundle reverts past this timestamp.
        uint256 deadline;
    }

    // ------------------------------------------------------------------ events

    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);
    event OwnershipTransferStarted(address indexed currentOwner, address indexed pendingOwner);
    event OwnershipTransferCancelled(address indexed currentOwner);
    event OperatorUpdated(address indexed operator);
    event PauserUpdated(address indexed pauser);
    event RouterAllowlistUpdated(address indexed router, bool allowed);
    event RouterSelectorUpdated(address indexed router, bytes4 indexed selector, bool allowed);
    event RouterCodehashPinnedUpdated(address indexed router, bytes32 codehash);
    event TokenAllowlistUpdated(address indexed token, bool allowed, bool feeOnTransfer);
    event AllowFeeOnTransferUpdated(bool allowed);
    event MaxFlashUSDCUpdated(uint256 maxFlashUSDC);
    event Paused(address indexed account);
    event Unpaused(address indexed account);
    event Swept(address indexed token, address indexed to, uint256 amount);
    event SweptETH(address indexed to, uint256 amount);
    event ArbitrageExecuted(
        uint256 profitUSDC, address indexed tokenA, address indexed tokenB, Direction direction, uint256 flashAmount
    );

    // ------------------------------------------------------------------ state

    /// @notice Balancer V2 Vault on Base (flash-loan lender).
    address public immutable VAULT;
    /// @notice Native USDC on Base (flash asset AND profit unit).
    address public immutable USDC;

    /// @notice Known test/quoter swap shape:
    /// `swap(address,address,uint256,uint256,address)`.
    /// Kept for the offline mock/test path; production legs should use the
    /// `*_EXACT_INPUT_SINGLE_SELECTOR` shapes below.
    bytes4 public constant SWAP_SELECTOR = 0xd5bcb9b5;

    /// @notice Aerodrome Slipstream SwapRouter `exactInputSingle` selector.
    /// @dev Canonical signature (verified Basescan ABI on
    /// 0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5):
    /// `exactInputSingle((address,address,int24,address,uint256,uint256,uint256,uint160))`
    /// i.e. `(tokenIn,tokenOut,tickSpacing,recipient,deadline,amountIn,amountOutMinimum,sqrtPriceLimitX96)`.
    /// On-chain MethodID 0xa026383e (see Basescan txs to the Slipstream router).
    bytes4 public constant AERO_EXACT_INPUT_SINGLE_SELECTOR = 0xa026383e;

    /// @notice Uniswap SwapRouter02 `exactInputSingle` selector.
    /// @dev Canonical signature (Uniswap swap-router-contracts `V3SwapRouter.sol`
    /// / `IV3SwapRouter.sol`, verified Basescan ABI on
    /// 0x2626664c2603336E57B271c5C0b26F421741e481):
    /// `exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))`
    /// i.e. `(tokenIn,tokenOut,fee,recipient,amountIn,amountOutMinimum,sqrtPriceLimitX96)`
    /// (no deadline field, unlike Slipstream).
    /// On-chain MethodID 0x04e45aaf (see Basescan txs to SwapRouter02).
    bytes4 public constant UNI_EXACT_INPUT_SINGLE_SELECTOR = 0x04e45aaf;

    /// @notice Aerodrome Slipstream SwapRouter on Base (production router).
    address public constant AERO_SLIPSTREAM_ROUTER = 0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5;
    /// @notice Uniswap SwapRouter02 on Base (production router).
    address public constant UNI_SWAP_ROUTER02 = 0x2626664c2603336E57B271c5C0b26F421741e481;
    /// @notice Uniswap UniversalRouter on Base (explicit-selector-only).
    /// @dev NEVER auto-allowlisted: `setRouterAllowed` skips all auto-enables
    /// for this address. Any selector must be enabled explicitly per use via
    /// `setRouterSelectorAllowed`.
    address public constant UNIVERSAL_ROUTER = 0x6fF5693b99212Da76ad316178A184AB56D299b43;

    /// @notice Absolute ceiling for `maxFlashUSDC` (10M USDC, 6 decimals).
    /// @dev Guards against owner fat-finger / key-compromise setting an
    /// unbounded flash cap. Changes to this constant require a redeploy.
    uint256 public constant MAX_FLASH_USDC_HARD_CAP = 10_000_000_000_000;

    /// @notice Two-step ownership: current owner, then pending owner.
    address public owner;
    address public pendingOwner;

    /// @notice Operator: may call `execute` only (bot key). Zero by default.
    address public operator;
    /// @notice Pauser: may call `pause` only (monitor key). Zero by default.
    address public pauser;

    /// @notice Emergency halt; gates `execute` only (sweep stays live).
    bool public paused;

    /// @notice Per-trade USDC flash cap. Default mirrors
    /// `config/default.toml` max_flash_usdc ($5000 = 5_000_000_000 base units).
    uint256 public maxFlashUSDC;

    /// @notice Global fee-on-transfer kill switch. Default closed (false):
    /// flagged tokens revert even if allowlisted.
    bool public allowFeeOnTransfer;

    /// @notice Router allowlist (Aerodrome Slipstream / Uni V3 family only).
    mapping(address => bool) public routerAllowed;
    /// @notice Per-router calldata selector allowlist. Only explicitly
    /// allowlisted `(router, selector)` pairs may be executed.
    mapping(address => mapping(bytes4 => bool)) public routerSelectorAllowed;
    /// @notice Token allowlist with per-token tax flag.
    mapping(address => TokenConfig) public tokenConfigs;
    /// @notice Optional pinned runtime codehash per router (0 = unpinned,
    /// check skipped for backwards compatibility). Set via
    /// `setRouterCodehashPinned`; enforced in `_validate` (hence both in
    /// `execute` pre-flight and in the `receiveFlashLoan` re-validation).
    mapping(address => bytes32) public routerCodehashPinned;

    uint256 private constant _NOT_ENTERED = 1;
    uint256 private constant _ENTERED = 2;
    uint256 private _status = _NOT_ENTERED;

    /// @notice Hash of the `userData` bound to the in-flight `execute`.
    /// @dev Transient: set before the Vault call, verified + cleared at the
    /// top of the callback. Blocks unsolicited Vault callbacks (attacker
    /// calling `flashLoan` with recipient == executor directly).
    bytes32 private transient _expectedHash;
    /// @notice True while the flash-loan callback body is running.
    /// @dev Transient: blocks nested Vault callbacks re-entering
    /// `receiveFlashLoan` from inside a router leg.
    bool private transient _inCallback;

    // -------------------------------------------------------------- modifiers

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier onlyOperatorOrOwner() {
        if (msg.sender != owner && msg.sender != operator) revert NotOperatorOrOwner();
        _;
    }

    modifier onlyPauserOrOwner() {
        if (msg.sender != owner && msg.sender != pauser) revert NotPauserOrOwner();
        _;
    }

    modifier whenNotPaused() {
        if (paused) revert EnforcedPause();
        _;
    }

    modifier nonReentrant() {
        if (_status == _ENTERED) revert Reentrant();
        _status = _ENTERED;
        _;
        _status = _NOT_ENTERED;
    }

    // ------------------------------------------------------------ constructor

    /// @param vault_ Balancer V2 Vault (Base: 0xBA12...F2C8).
    /// @param usdc_ Native USDC (Base: 0x8335...2913).
    constructor(address vault_, address usdc_) {
        if (vault_ == address(0) || usdc_ == address(0)) revert ZeroAddress();
        VAULT = vault_;
        USDC = usdc_;
        owner = msg.sender;
        maxFlashUSDC = 5_000_000_000;
        emit OwnershipTransferred(address(0), msg.sender);
        emit MaxFlashUSDCUpdated(5_000_000_000);
    }

    // ---------------------------------------------------------------- execute

    /// @notice Run one atomic arbitrage: borrow USDC, buy leg, sell leg,
    /// repay, enforce `minProfitUSDC`. Everything reverts on any failure.
    /// @dev Callable by `operator` or `owner`.
    /// @param p Execution parameters (see struct NatSpec).
    function execute(ExecParams calldata p) external nonReentrant onlyOperatorOrOwner whenNotPaused {
        _validate(p);
        // Profit baseline: pre-flash USDC balance (prior retained profits).
        uint256 dust = IERC20(USDC).balanceOf(address(this));

        // Single-element arrays are trivially sorted per Vault requirements.
        address[] memory tokens = new address[](1);
        tokens[0] = USDC;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = p.flashAmount;

        bytes memory userData = abi.encode(p, dust);
        // Bind this execute to its callback. The `nonReentrant` guard is
        // already held here (first modifier), so any reentry into `execute`
        // reverts; the callback additionally requires `_status == _ENTERED`
        // plus hash verification. The external call below IS the trade.
        _expectedHash = keccak256(userData);
        IBalancerVault(VAULT).flashLoan(address(this), tokens, amounts, userData);
        // The Vault must have called back (which clears the hash). A missing
        // callback leaves the hash set and reverts here.
        if (_expectedHash != bytes32(0)) revert UnexpectedCallback();
    }

    /// @notice Balancer V2 flash-loan callback. Only the Vault may call it,
    /// and only while an operator/owner-initiated `execute` is in flight.
    /// @dev Runs inside `execute`'s reentrancy guard by design (hence no
    /// `nonReentrant` modifier, which would always revert). Order: authenticate
    /// caller, require guard entered, block nesting, verify + clear the transient
    /// binding, then run the legs. Approvals are revoked after each leg.
    function receiveFlashLoan(
        address[] memory tokens,
        uint256[] memory amounts,
        uint256[] memory feeAmounts,
        bytes memory userData
    ) external {
        if (msg.sender != VAULT) revert NotVault(msg.sender);
        if (_status != _ENTERED) revert UnexpectedCallback();
        if (_inCallback) revert NestedCallback();
        if (_expectedHash == bytes32(0)) revert UnexpectedCallback();
        if (keccak256(userData) != _expectedHash) revert UnexpectedCallback();
        // Clear before any external call: prevents replay/nesting with the
        // same userData within this transaction.
        _expectedHash = bytes32(0);
        _inCallback = true;

        if (tokens.length != 1 || amounts.length != 1 || feeAmounts.length != 1) {
            revert UnexpectedLoan(address(0), 0);
        }
        if (tokens[0] != USDC) revert UnexpectedLoan(tokens[0], amounts[0]);

        (ExecParams memory p, uint256 dust) = abi.decode(userData, (ExecParams, uint256));
        // Authoritative re-validation: the callback is the on-chain choke point.
        _validate(p);
        if (amounts[0] != p.flashAmount) revert UnexpectedLoan(tokens[0], amounts[0]);

        address middle = p.direction == Direction.AtoB ? p.tokenB : p.tokenA;

        // Leg 1: USDC -> middle token on the buy router.
        _approveExact(USDC, p.buyRouter, p.flashAmount);
        uint256 midBefore = IERC20(middle).balanceOf(address(this));
        _callRouter(p.buyRouter, p.buyData);
        // No lingering allowance: revoke in the same callback.
        _rawApprove(USDC, p.buyRouter, 0);
        uint256 bought = IERC20(middle).balanceOf(address(this)) - midBefore;
        if (bought < p.buyMinOut) revert BuyLegTooSmall(bought, p.buyMinOut);
        // The quoter-built sell calldata must spend exactly what leg 1 bought.
        _enforceSellAmountIn(p.sellData, bought);

        // Leg 2: middle token -> USDC on the sell router.
        _approveExact(middle, p.sellRouter, bought);
        uint256 usdcBefore = IERC20(USDC).balanceOf(address(this));
        _callRouter(p.sellRouter, p.sellData);
        // No lingering allowance: revoke in the same callback.
        _rawApprove(middle, p.sellRouter, 0);
        uint256 proceeds = IERC20(USDC).balanceOf(address(this)) - usdcBefore;
        if (proceeds < p.sellMinOut) revert SellLegTooSmall(proceeds, p.sellMinOut);

        // Repay loan + any Vault fee (0% on Base; handled generically).
        uint256 repay = amounts[0] + feeAmounts[0];
        _safeTransfer(USDC, VAULT, repay);

        uint256 profit = IERC20(USDC).balanceOf(address(this)) - dust;
        if (profit < p.minProfitUSDC) revert InsufficientProfit(profit, p.minProfitUSDC);

        _inCallback = false;
        emit ArbitrageExecuted(profit, p.tokenA, p.tokenB, p.direction, p.flashAmount);
    }

    // ------------------------------------------------------------- validation

    /// @notice Shared pre-flight checks for `execute` and `receiveFlashLoan`.
    function _validate(ExecParams memory p) internal view {
        if (block.timestamp > p.deadline) revert DeadlineExpired();
        if (p.flashAmount == 0) revert ZeroAmount();
        if (p.flashAmount > maxFlashUSDC) revert ExceedsMaxFlash(p.flashAmount, maxFlashUSDC);
        if (p.tokenA == address(0) || p.tokenB == address(0)) revert ZeroAddress();
        if (p.tokenA == p.tokenB) revert IdenticalTokens();
        _requireToken(p.tokenA);
        _requireToken(p.tokenB);
        if (p.buyRouter == address(0) || p.sellRouter == address(0)) revert ZeroAddress();
        if (!routerAllowed[p.buyRouter]) revert RouterNotAllowlisted(p.buyRouter);
        if (!routerAllowed[p.sellRouter]) revert RouterNotAllowlisted(p.sellRouter);
        _checkRouterCodehash(p.buyRouter);
        _checkRouterCodehash(p.sellRouter);
        // The flash asset is always USDC, so it must be leg 1's input token.
        if (p.direction == Direction.AtoB) {
            if (p.tokenA != USDC) revert DirectionMismatch();
        } else {
            if (p.tokenB != USDC) revert DirectionMismatch();
        }
        if (p.buyData.length == 0 || p.sellData.length == 0) revert EmptyCalldata();
        address middle = p.direction == Direction.AtoB ? p.tokenB : p.tokenA;
        // Router calldata gate: allowlisted (router, selector) + recipient /
        // token-path pinning where the shape is known. Buy amountIn must equal
        // the flash amount; sell amountIn is pinned to `bought` in the callback.
        _checkLegCalldata(p.buyRouter, p.buyData, USDC, middle, p.flashAmount, true);
        _checkLegCalldata(p.sellRouter, p.sellData, middle, USDC, 0, false);
        // Bind inner router-level `amountOutMinimum` to the executor-level
        // `buyMinOut`/`sellMinOut` for the known production selectors. The
        // inner minimum must be at least as tight as the executor minimum;
        // balance-diff checks in the callback stay as the authoritative gate.
        _enforceInnerMin(p.buyData, p.buyMinOut);
        _enforceInnerMin(p.sellData, p.sellMinOut);
        if (p.minProfitUSDC == 0) revert ZeroAmount();
    }

    /// @notice Token gate: allowlisted AND (non-tax OR globally permitted).
    function _requireToken(address t) internal view {
        TokenConfig memory c = tokenConfigs[t];
        if (!c.allowed) revert TokenNotAllowlisted(t);
        if (c.feeOnTransfer && !allowFeeOnTransfer) revert FeeOnTransferNotAllowed(t);
    }

    /// @notice Validate one leg's opaque router calldata.
    /// @dev Requires the calldata selector to be allowlisted for the router.
    /// For the known shapes, additionally decodes and enforces:
    /// tokenIn/tokenOut match the expected path, `recipient`/`to` is the
    /// executor, and (when `checkAmountIn`) `amountIn` equals the expected
    /// spend:
    /// - `SWAP_SELECTOR`: `swap(address,address,uint256,uint256,address)`.
    /// - `AERO_EXACT_INPUT_SINGLE_SELECTOR`: Slipstream `exactInputSingle`
    ///   `(address,address,int24,address,uint256,uint256,uint256,uint160)`;
    ///   the inner `deadline` must not be expired and `sqrtPriceLimitX96` /
    ///   `tickSpacing` are passed through to the router unenforced.
    /// - `UNI_EXACT_INPUT_SINGLE_SELECTOR`: SwapRouter02 `exactInputSingle`
    ///   `(address,address,uint24,address,uint256,uint256,uint160)`;
    ///   `sqrtPriceLimitX96` / `fee` are passed through unenforced.
    /// Unknown selectors rely on the (router, selector) allowlist plus the
    /// off-chain quoter pinning the recipient to the executor.
    function _checkLegCalldata(
        address router,
        bytes memory data,
        address wantIn,
        address wantOut,
        uint256 wantAmountIn,
        bool checkAmountIn
    ) internal view {
        bytes4 sel = _selector(data);
        if (!routerSelectorAllowed[router][sel]) revert SelectorNotAllowlisted(router, sel);
        if (sel == SWAP_SELECTOR) {
            (address tokenIn, address tokenOut, uint256 amountIn,, address to) =
                abi.decode(_sliceCalldataBody(data), (address, address, uint256, uint256, address));
            if (tokenIn != wantIn || tokenOut != wantOut) revert BadTokenPath(tokenIn, tokenOut, wantIn, wantOut);
            if (to != address(this)) revert BadRecipient(to, address(this));
            if (checkAmountIn && amountIn != wantAmountIn) revert BadAmountIn(amountIn, wantAmountIn);
            if (!checkAmountIn && amountIn == 0) revert ZeroAmount();
        } else if (sel == AERO_EXACT_INPUT_SINGLE_SELECTOR) {
            (address tokenIn, address tokenOut,, address recipient, uint256 deadline, uint256 amountIn,,) = abi.decode(
                _sliceCalldataBody(data), (address, address, int24, address, uint256, uint256, uint256, uint160)
            );
            if (tokenIn != wantIn || tokenOut != wantOut) revert BadTokenPath(tokenIn, tokenOut, wantIn, wantOut);
            if (recipient != address(this)) revert BadRecipient(recipient, address(this));
            if (deadline < block.timestamp) revert DeadlineExpired();
            if (checkAmountIn && amountIn != wantAmountIn) revert BadAmountIn(amountIn, wantAmountIn);
            if (!checkAmountIn && amountIn == 0) revert ZeroAmount();
        } else if (sel == UNI_EXACT_INPUT_SINGLE_SELECTOR) {
            (address tokenIn, address tokenOut,, address recipient, uint256 amountIn,,) =
                abi.decode(_sliceCalldataBody(data), (address, address, uint24, address, uint256, uint256, uint160));
            if (tokenIn != wantIn || tokenOut != wantOut) revert BadTokenPath(tokenIn, tokenOut, wantIn, wantOut);
            if (recipient != address(this)) revert BadRecipient(recipient, address(this));
            if (checkAmountIn && amountIn != wantAmountIn) revert BadAmountIn(amountIn, wantAmountIn);
            if (!checkAmountIn && amountIn == 0) revert ZeroAmount();
        }
    }

    /// @notice Enforce that the sell leg spends exactly what leg 1 bought.
    /// @dev Applies to all known shapes (`SWAP_SELECTOR` + both production
    /// `exactInputSingle` selectors); other selectors are quoter-pinned.
    function _enforceSellAmountIn(bytes memory sellData, uint256 bought) internal view {
        bytes4 sel = _selector(sellData);
        if (sel == SWAP_SELECTOR) {
            (,, uint256 amountIn,,) =
                abi.decode(_sliceCalldataBody(sellData), (address, address, uint256, uint256, address));
            if (amountIn != bought) revert BadAmountIn(amountIn, bought);
        } else if (sel == AERO_EXACT_INPUT_SINGLE_SELECTOR) {
            (,,,,, uint256 amountIn,,) = abi.decode(
                _sliceCalldataBody(sellData), (address, address, int24, address, uint256, uint256, uint256, uint160)
            );
            if (amountIn != bought) revert BadAmountIn(amountIn, bought);
        } else if (sel == UNI_EXACT_INPUT_SINGLE_SELECTOR) {
            (,,,, uint256 amountIn,,) =
                abi.decode(_sliceCalldataBody(sellData), (address, address, uint24, address, uint256, uint256, uint160));
            if (amountIn != bought) revert BadAmountIn(amountIn, bought);
        }
    }

    /// @notice Require the inner router-level `amountOutMinimum` (known
    /// production selectors only) to be at least as tight as the
    /// executor-level minimum. Unknown selectors (including the mock/test
    /// `SWAP_SELECTOR`) skip this check: the executor balance-diff minimums
    /// (`buyMinOut`/`sellMinOut`) remain the gate there.
    /// @dev Reverts with `BadInnerMin(innerMin, executorMin)` when the inner
    /// minimum is looser than the executor minimum.
    function _enforceInnerMin(bytes memory data, uint256 executorMin) internal pure {
        bytes4 sel = _selector(data);
        if (sel == AERO_EXACT_INPUT_SINGLE_SELECTOR) {
            (,,,,,, uint256 innerMin,) = abi.decode(
                _sliceCalldataBody(data), (address, address, int24, address, uint256, uint256, uint256, uint160)
            );
            if (innerMin < executorMin) revert BadInnerMin(innerMin, executorMin);
        } else if (sel == UNI_EXACT_INPUT_SINGLE_SELECTOR) {
            (,,,,, uint256 innerMin,) =
                abi.decode(_sliceCalldataBody(data), (address, address, uint24, address, uint256, uint256, uint160));
            if (innerMin < executorMin) revert BadInnerMin(innerMin, executorMin);
        }
    }

    /// @notice Enforce a pinned router codehash, if one is set.
    /// @dev `bytes32(0)` means unpinned: the check is skipped for backwards
    /// compatibility. Otherwise `router.codehash` must equal the pinned value.
    function _checkRouterCodehash(address router) internal view {
        bytes32 pinned = routerCodehashPinned[router];
        if (pinned == bytes32(0)) return;
        bytes32 current = router.codehash;
        if (current != pinned) revert RouterCodeChanged(router, pinned, current);
    }

    /// @notice First 4 bytes of calldata as a selector.
    function _selector(bytes memory data) internal pure returns (bytes4 sel) {
        if (data.length < 4) revert ShortCalldata();
        assembly {
            sel := mload(add(data, 32))
        }
    }

    /// @notice Calldata body (everything after the 4-byte selector).
    /// @dev Byte loop is gas-inefficient but correct; assembly slice is the
    ///      upgrade path if profiling shows it matters.
    function _sliceCalldataBody(bytes memory data) internal pure returns (bytes memory body) {
        // ponytail: O(n) copy; assembly slice cheaper. Upgrade when gas
        // profiling flags router-calldata prep as hot.
        body = new bytes(data.length - 4);
        for (uint256 i = 0; i < body.length; ++i) {
            body[i] = data[i + 4];
        }
    }

    // ---------------------------------------------------------------- helpers

    /// @notice Low-level router call that bubbles the original revert reason.
    function _callRouter(address router, bytes memory data) internal {
        (bool ok, bytes memory ret) = router.call(data);
        if (!ok) {
            if (ret.length != 0) {
                assembly {
                    revert(add(ret, 32), mload(ret))
                }
            }
            revert RouterCallFailed(router);
        }
    }

    /// @notice Minimal approval pattern: grant exactly `amount`, touching
    /// allowance only when insufficient. Resets to zero first for tokens
    /// that reject non-zero-to-non-zero changes.
    function _approveExact(address token, address spender, uint256 amount) internal {
        uint256 cur = IERC20(token).allowance(address(this), spender);
        if (cur >= amount) return;
        if (cur != 0) _rawApprove(token, spender, 0);
        _rawApprove(token, spender, amount);
    }

    function _rawApprove(address token, address spender, uint256 amount) internal {
        (bool ok, bytes memory ret) = token.call(abi.encodeWithSelector(IERC20.approve.selector, spender, amount));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert ApproveFailed();
    }

    /// @notice Transfer that also supports tokens returning no boolean.
    function _safeTransfer(address token, address to, uint256 amount) internal {
        (bool ok, bytes memory ret) = token.call(abi.encodeWithSelector(IERC20.transfer.selector, to, amount));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert TransferFailed();
    }

    // ------------------------------------------------------------------ admin

    /// @notice Allowlist a swap router (e.g. Slipstream / SwapRouter02).
    /// @dev When enabling a router, ONLY the two production selectors
    /// (`AERO_EXACT_INPUT_SINGLE_SELECTOR` and `UNI_EXACT_INPUT_SINGLE_SELECTOR`)
    /// are auto-enabled for that router; disable any shape the router must
    /// never serve via `setRouterSelectorAllowed`. The mock/test
    /// `SWAP_SELECTOR` is NEVER auto-enabled — it must be enabled explicitly
    /// per mock router via `setRouterSelectorAllowed` (tests only) and must
    /// never be enabled on the production router addresses. The
    /// UniversalRouter (`UNIVERSAL_ROUTER`) is explicit-selector-only: no
    /// selector is auto-enabled for it. Any further (unknown) selector must
    /// be explicitly enabled per router via `setRouterSelectorAllowed` —
    /// there is no wildcard.
    function setRouterAllowed(address router, bool allowed) external onlyOwner {
        if (router == address(0)) revert ZeroAddress();
        routerAllowed[router] = allowed;
        emit RouterAllowlistUpdated(router, allowed);
        if (allowed) {
            // UniversalRouter stays explicit-selector-only: no auto-enable.
            if (router == UNIVERSAL_ROUTER) return;
            if (!routerSelectorAllowed[router][AERO_EXACT_INPUT_SINGLE_SELECTOR]) {
                routerSelectorAllowed[router][AERO_EXACT_INPUT_SINGLE_SELECTOR] = true;
                emit RouterSelectorUpdated(router, AERO_EXACT_INPUT_SINGLE_SELECTOR, true);
            }
            if (!routerSelectorAllowed[router][UNI_EXACT_INPUT_SINGLE_SELECTOR]) {
                routerSelectorAllowed[router][UNI_EXACT_INPUT_SINGLE_SELECTOR] = true;
                emit RouterSelectorUpdated(router, UNI_EXACT_INPUT_SINGLE_SELECTOR, true);
            }
        }
    }

    /// @notice Allowlist a `(router, selector)` calldata pair.
    /// @dev Only explicitly allowlisted pairs execute. For unknown selectors
    /// the quoter MUST pin the output recipient to the executor off-chain;
    /// prefer router-specific wrappers that pin it on-chain.
    function setRouterSelectorAllowed(address router, bytes4 selector, bool allowed) external onlyOwner {
        if (router == address(0)) revert ZeroAddress();
        if (selector == bytes4(0)) revert ZeroAmount();
        routerSelectorAllowed[router][selector] = allowed;
        emit RouterSelectorUpdated(router, selector, allowed);
    }

    /// @notice Pin (or unpin with `bytes32(0)`) the expected runtime codehash
    /// for a router. When pinned, `_validate` requires
    /// `router.codehash == codehash` and reverts with `RouterCodeChanged`
    /// otherwise (e.g. router upgraded or swapped). Unpinned (`0`) skips the
    /// check for backwards compatibility.
    function setRouterCodehashPinned(address router, bytes32 codehash) external onlyOwner {
        if (router == address(0)) revert ZeroAddress();
        routerCodehashPinned[router] = codehash;
        emit RouterCodehashPinnedUpdated(router, codehash);
    }

    /// @notice Inspect the codehash monitor for a router.
    /// @return pinned The pinned codehash (`bytes32(0)` = unpinned, skipped).
    /// @return current The router's current runtime codehash.
    /// @return match_ True when unpinned or `current == pinned`.
    function checkRouterCodehash(address router) external view returns (bytes32 pinned, bytes32 current, bool match_) {
        pinned = routerCodehashPinned[router];
        current = router.codehash;
        match_ = pinned == bytes32(0) || pinned == current;
    }

    /// @notice Allowlist a token. List ONLY vetted non-tax tokens with
    /// `feeOnTransfer == false`; set the flag honestly for taxed tokens.
    function setTokenAllowed(address token, bool allowed, bool feeOnTransfer) external onlyOwner {
        if (token == address(0)) revert ZeroAddress();
        tokenConfigs[token] = TokenConfig({allowed: allowed, feeOnTransfer: feeOnTransfer});
        emit TokenAllowlistUpdated(token, allowed, feeOnTransfer);
    }

    /// @notice Global fee-on-transfer switch. Keep `false` unless a taxed
    /// token has been explicitly modeled and reviewed.
    function setAllowFeeOnTransfer(bool allowed) external onlyOwner {
        allowFeeOnTransfer = allowed;
        emit AllowFeeOnTransferUpdated(allowed);
    }

    /// @notice Adjust the per-trade USDC flash cap (0 = halt new trades).
    /// @dev Capped by `MAX_FLASH_USDC_HARD_CAP`. In production, front this
    /// (and all allowlist changes) with a timelock so users can exit before
    /// risk parameters change. Ownership remains two-step.
    function setMaxFlashUSDC(uint256 maxFlashUSDC_) external onlyOwner {
        if (maxFlashUSDC_ > MAX_FLASH_USDC_HARD_CAP) revert ExceedsHardCap(maxFlashUSDC_, MAX_FLASH_USDC_HARD_CAP);
        maxFlashUSDC = maxFlashUSDC_;
        emit MaxFlashUSDCUpdated(maxFlashUSDC_);
    }

    /// @notice Set the operator (may call `execute` only). Owner retains all.
    /// @dev `address(0)` revokes: a compromised hot key is nulled, not rotated.
    function setOperator(address operator_) external onlyOwner {
        operator = operator_;
        emit OperatorUpdated(operator_);
    }

    /// @notice Set the pauser (may call `pause` only). Owner retains all.
    /// @dev `address(0)` revokes: a compromised monitor key is nulled.
    function setPauser(address pauser_) external onlyOwner {
        pauser = pauser_;
        emit PauserUpdated(pauser_);
    }

    /// @notice Halt `execute` (recovery via `sweep` stays available).
    /// @dev Callable by `pauser` or `owner`. Resume stays owner-only.
    function pause() external onlyPauserOrOwner {
        if (paused) revert ExpectedPause();
        paused = true;
        emit Paused(msg.sender);
    }

    /// @notice Resume `execute`.
    function unpause() external onlyOwner {
        if (!paused) revert EnforcedPause();
        paused = false;
        emit Unpaused(msg.sender);
    }

    /// @notice Recover any token balance (profits, dust, stuck funds) to `to`.
    /// @dev Emits after the transfer succeeds so logs never precede effects.
    function sweep(address token, address to, uint256 amount) external nonReentrant onlyOwner {
        if (token == address(0)) revert ZeroAddress();
        if (to == address(0)) revert ZeroAddress();
        if (amount == 0) revert ZeroAmount();
        _safeTransfer(token, to, amount);
        emit Swept(token, to, amount);
    }

    /// @notice Recover ETH forced into the contract (no `receive`/`fallback`;
    /// normal sends revert). WETH stays an ERC20 in this system — never wrap
    /// or unwrap here; use this only to rescue stuck native ETH.
    function sweepETH(address to, uint256 amount) external nonReentrant onlyOwner {
        if (to == address(0)) revert ZeroAddress();
        if (amount == 0) revert ZeroAmount();
        (bool ok,) = to.call{value: amount}("");
        if (!ok) revert ETHTransferFailed();
        emit SweptETH(to, amount);
    }

    /// @notice Start two-step ownership transfer.
    function transferOwnership(address next) external onlyOwner {
        if (next == address(0)) revert ZeroAddress();
        pendingOwner = next;
        emit OwnershipTransferStarted(owner, next);
    }

    /// @notice Complete two-step ownership transfer (called by `pendingOwner`).
    function acceptOwnership() external {
        if (msg.sender != pendingOwner) revert NotPendingOwner();
        address prev = owner;
        address next = pendingOwner;
        owner = next;
        pendingOwner = address(0);
        emit OwnershipTransferred(prev, next);
    }

    /// @notice Cancel a pending ownership transfer (fat-finger recovery).
    function cancelOwnership() external onlyOwner {
        pendingOwner = address(0);
        emit OwnershipTransferCancelled(msg.sender);
    }
}
