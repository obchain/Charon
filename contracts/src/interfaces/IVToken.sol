// SPDX-License-Identifier: MIT
pragma solidity =0.8.24;

/// @title IVToken
/// @notice Stub interface for Venus Protocol vToken contracts on BNB Chain.
/// @dev Only the entry points required by CharonLiquidator v0.1 are declared.
///      Venus vTokens follow the Compound V2 cToken model.
///      Additional methods (mint, redeem, borrow, borrowBalanceCurrent, etc.) will be
///      added in future commits as the implementation grows.
///      Venus Comptroller (unitroller) on BNB Chain: 0xfD36E2c2a6789Db23113685031d7F16329158384
interface IVToken {
    /// @notice The caller repays `repayAmount` of the underlying asset on behalf of `borrower`
    ///         and seizes `vTokenCollateral` from the borrower in return.
    /// @dev Caller must have pre-approved this contract to spend `repayAmount` of the debt token.
    ///      Returns an error code (0 = success) following the Compound V2 convention.
    ///      Reverts on failure in more recent Venus deployments.
    /// @param borrower          The account whose borrow is being repaid.
    /// @param repayAmount       The amount of the underlying debt asset to repay.
    /// @param vTokenCollateral  The vToken address of the collateral to seize.
    /// @return 0 on success, non-zero error code on failure.
    function liquidateBorrow(address borrower, uint256 repayAmount, address vTokenCollateral)
        external
        returns (uint256);

    /// @notice Redeems vTokens for the specified amount of the underlying asset.
    /// @dev Returns an error code (0 = success). The caller must hold at least enough vTokens
    ///      to cover `redeemAmount` of underlying after conversion.
    /// @param redeemAmount The amount of underlying asset to receive.
    /// @return 0 on success, non-zero error code on failure.
    function redeemUnderlying(uint256 redeemAmount) external returns (uint256);

    /// @notice Redeems exactly `redeemTokens` vTokens for the corresponding underlying asset.
    /// @dev Compound V2 / Venus API. Transfers the caller's vTokens back to the protocol and
    ///      returns the proportional underlying. Returns an error code (0 = success).
    ///      CharonLiquidator uses this variant to drain the full seized vToken balance in one
    ///      call after liquidateBorrow(), avoiding any rounding that redeemUnderlying would
    ///      introduce when converting an inexact underlying amount to vTokens.
    /// @param redeemTokens The number of vTokens to burn.
    /// @return 0 on success, non-zero error code on failure.
    function redeem(uint256 redeemTokens) external returns (uint256);

    /// @notice Returns the vToken balance of `account`.
    /// @dev Used by rescue() to validate amounts before pulling vTokens out of the contract.
    /// @param account The address to query.
    /// @return The vToken balance (in vToken units, not underlying).
    function balanceOf(address account) external view returns (uint256);
}
