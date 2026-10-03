// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Token side used by the mock router.
interface IERC20Swap {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

/// @title MockRouter
/// @notice Stand-in for a DEX swap router. The test sets a fixed conversion
/// rate per (tokenIn -> tokenOut); `swap` pulls `amountIn` from the caller and
/// pushes the output to `to`. Invoked by the executor via opaque calldata,
/// exactly like a real router would be.
contract MockRouter {
    struct Rate {
        uint256 num;
        uint256 den;
        bool set;
    }

    mapping(address => mapping(address => Rate)) public rates;

    event Swap(
        address indexed tokenIn, address indexed tokenOut, uint256 amountIn, uint256 amountOut, address indexed to
    );

    function setRate(address tokenIn, address tokenOut, uint256 num, uint256 den) external {
        require(den != 0, "MockRouter: den");
        rates[tokenIn][tokenOut] = Rate({num: num, den: den, set: true});
    }

    function swap(address tokenIn, address tokenOut, uint256 amountIn, uint256 minOut, address to)
        external
        returns (uint256 out)
    {
        Rate memory r = rates[tokenIn][tokenOut];
        require(r.set, "MockRouter: no rate");
        require(IERC20Swap(tokenIn).transferFrom(msg.sender, address(this), amountIn), "MockRouter: pull");
        out = amountIn * r.num / r.den;
        require(out >= minOut, "MockRouter: minOut");
        require(IERC20Swap(tokenOut).transfer(to, out), "MockRouter: push");
        emit Swap(tokenIn, tokenOut, amountIn, out, to);
    }
}
