// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title Vm
/// @notice Minimal Foundry cheatcode interface (subset actually used).
/// @dev Declared inline so `forge test` works offline without forge-std.
interface Vm {
    function prank(address msgSender) external;
    function startPrank(address msgSender) external;
    function stopPrank() external;
    function expectRevert(bytes calldata revertData) external;
    function expectEmit(bool checkTopic1, bool checkTopic2, bool checkTopic3, bool checkData) external;
    function deal(address who, uint256 newBalance) external;
    function envOr(string calldata name, string calldata defaultValue) external returns (string memory value);
    function createSelectFork(string calldata urlOrAlias) external returns (uint256 forkId);
}

/// @title TestBase
/// @notice Tiny assertion library replacing forge-std for offline use.
abstract contract TestBase {
    Vm internal constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    function assertTrue(bool cond, string memory message) internal pure {
        require(cond, message);
    }

    function assertEq(uint256 a, uint256 b, string memory message) internal pure {
        require(a == b, message);
    }

    function assertEq(address a, address b, string memory message) internal pure {
        require(a == b, message);
    }
}
