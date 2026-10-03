// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Token side used by the production-shape mock routers.
interface IERC20ProdSwap {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

/// @title MockAeroSlipstreamRouter
/// @notice Offline stand-in for the Aerodrome Slipstream SwapRouter v1
/// (0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5) `exactInputSingle` shape:
/// `exactInputSingle((address,address,int24,address,uint256,uint256,uint256,uint160))`
/// i.e. `(tokenIn,tokenOut,tickSpacing,recipient,deadline,amountIn,amountOutMinimum,sqrtPriceLimitX96)`.
/// @dev Selector 0xa026383e (verified Basescan ABI + on-chain MethodID).
/// Simplified fixed-rate fill: pulls `amountIn` from the caller, pushes the
/// output to `recipient`. `tickSpacing`/`sqrtPriceLimitX96` are accepted and
/// ignored (passthrough, as in the executor); `deadline` is enforced like the
/// real router.
contract MockAeroSlipstreamRouter {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        int24 tickSpacing;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    struct Rate {
        uint256 num;
        uint256 den;
        bool set;
    }

    mapping(address => mapping(address => Rate)) public rates;

    event ExactInputSingle(
        address indexed tokenIn,
        address indexed tokenOut,
        uint256 amountIn,
        uint256 amountOut,
        address indexed recipient
    );

    function setRate(address tokenIn, address tokenOut, uint256 num, uint256 den) external {
        require(den != 0, "MockAero: den");
        rates[tokenIn][tokenOut] = Rate({num: num, den: den, set: true});
    }

    function exactInputSingle(ExactInputSingleParams calldata params) external returns (uint256 out) {
        require(params.deadline >= block.timestamp, "MockAero: deadline");
        Rate memory r = rates[params.tokenIn][params.tokenOut];
        require(r.set, "MockAero: no rate");
        require(
            IERC20ProdSwap(params.tokenIn).transferFrom(msg.sender, address(this), params.amountIn), "MockAero: pull"
        );
        out = params.amountIn * r.num / r.den;
        require(out >= params.amountOutMinimum, "MockAero: minOut");
        require(IERC20ProdSwap(params.tokenOut).transfer(params.recipient, out), "MockAero: push");
        emit ExactInputSingle(params.tokenIn, params.tokenOut, params.amountIn, out, params.recipient);
    }
}

/// @title MockUniSwapRouter02
/// @notice Offline stand-in for Uniswap SwapRouter02
/// (0x2626664c2603336E57B271c5C0b26F421741e481) `exactInputSingle` shape:
/// `exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))`
/// i.e. `(tokenIn,tokenOut,fee,recipient,amountIn,amountOutMinimum,sqrtPriceLimitX96)`
/// (no deadline field, unlike Slipstream).
/// @dev Selector 0x04e45aaf (Uniswap swap-router-contracts `V3SwapRouter.sol` /
/// verified Basescan ABI + on-chain MethodID). Simplified fixed-rate fill:
/// pulls `amountIn` from the caller, pushes the output to `recipient`.
/// `fee`/`sqrtPriceLimitX96` are accepted and ignored (passthrough).
contract MockUniSwapRouter02 {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    struct Rate {
        uint256 num;
        uint256 den;
        bool set;
    }

    mapping(address => mapping(address => Rate)) public rates;

    event ExactInputSingle(
        address indexed tokenIn,
        address indexed tokenOut,
        uint256 amountIn,
        uint256 amountOut,
        address indexed recipient
    );

    function setRate(address tokenIn, address tokenOut, uint256 num, uint256 den) external {
        require(den != 0, "MockUni: den");
        rates[tokenIn][tokenOut] = Rate({num: num, den: den, set: true});
    }

    function exactInputSingle(ExactInputSingleParams calldata params) external returns (uint256 out) {
        Rate memory r = rates[params.tokenIn][params.tokenOut];
        require(r.set, "MockUni: no rate");
        require(
            IERC20ProdSwap(params.tokenIn).transferFrom(msg.sender, address(this), params.amountIn), "MockUni: pull"
        );
        out = params.amountIn * r.num / r.den;
        require(out >= params.amountOutMinimum, "MockUni: minOut");
        require(IERC20ProdSwap(params.tokenOut).transfer(params.recipient, out), "MockUni: push");
        emit ExactInputSingle(params.tokenIn, params.tokenOut, params.amountIn, out, params.recipient);
    }
}
