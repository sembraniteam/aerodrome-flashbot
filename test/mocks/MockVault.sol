// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Token side used by the mock vault (transfer in/out + balance).
interface IERC20Lite {
    function balanceOf(address account) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
}

/// @notice Balancer-style flash-loan receiver callback.
interface IFlashBorrower {
    function receiveFlashLoan(
        address[] memory tokens,
        uint256[] memory amounts,
        uint256[] memory feeAmounts,
        bytes memory userData
    ) external;
}

/// @title MockVault
/// @notice Mimics Balancer V2 Vault `flashLoan`: funds the recipient, invokes
/// the callback, then requires full repayment (+ configurable fee).
contract MockVault {
    /// @notice Per-token flash fee in bps (default 0 = Base reality).
    mapping(address => uint256) public feeBps;

    function setFeeBps(address token, uint256 bps) external {
        feeBps[token] = bps;
    }

    function flashLoan(address recipient, address[] memory tokens, uint256[] memory amounts, bytes memory userData)
        external
    {
        uint256 n = tokens.length;
        require(n == amounts.length, "MockVault: length");
        uint256[] memory before = new uint256[](n);
        uint256[] memory fees = new uint256[](n);
        for (uint256 i = 0; i < n; ++i) {
            before[i] = IERC20Lite(tokens[i]).balanceOf(address(this));
            require(IERC20Lite(tokens[i]).transfer(recipient, amounts[i]), "MockVault: fund");
            fees[i] = amounts[i] * feeBps[tokens[i]] / 10_000;
        }
        IFlashBorrower(recipient).receiveFlashLoan(tokens, amounts, fees, userData);
        for (uint256 i = 0; i < n; ++i) {
            require(IERC20Lite(tokens[i]).balanceOf(address(this)) >= before[i] + fees[i], "MockVault: loan not repaid");
        }
    }
}
