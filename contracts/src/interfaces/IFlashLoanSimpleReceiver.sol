// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @title IFlashLoanSimpleReceiver
/// @notice Aave V3 flash-loan simple receiver callback interface.
/// @dev Implementors MUST repay asset + premium to the Aave Pool within the same transaction.
///      Reference: https://github.com/aave/aave-v3-core/blob/master/contracts/flashloan/interfaces/IFlashLoanSimpleReceiver.sol
interface IFlashLoanSimpleReceiver {
    /// @notice Executes an operation after receiving a flash-loaned asset.
    /// @dev Called by the Aave V3 Pool after funds are transferred. The callee must
    ///      repay `amount + premium` of `asset` back to the pool before this returns.
    /// @param asset    The address of the flash-loaned ERC-20 token.
    /// @param amount   The amount that was flash-loaned.
    /// @param premium  The fee owed on top of `amount`.
    /// @param initiator The address that initiated the flash loan (must equal address(this) for CharonLiquidator).
    /// @param params   Arbitrary bytes passed by the initiator — used to forward LiquidationParams.
    /// @return True if the operation succeeded; the pool reverts if false is returned.
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external returns (bool);
}
