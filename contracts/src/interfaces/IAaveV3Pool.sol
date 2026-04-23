// SPDX-License-Identifier: MIT
pragma solidity =0.8.24;

/// @title IAaveV3Pool
/// @notice Stub interface for the Aave V3 Pool contract deployed on BNB Chain.
/// @dev Only the entry points required by CharonLiquidator v0.1 are declared here.
///      Additional methods (liquidationCall, supply, withdraw, etc.) will be added in
///      future commits as the implementation grows.
///      BNB Chain mainnet Pool proxy: 0x6807dc923806fE8Fd134338EABCA509979a7e08
interface IAaveV3Pool {
    /// @notice Allows a smart contract to access the liquidity of the pool within one transaction,
    ///         as long as the amount taken plus a fee is returned.
    /// @dev The receiving contract must implement IFlashLoanSimpleReceiver and repay in executeOperation.
    /// @param receiverAddress The address of the contract that will receive the flash-loaned funds
    ///                        and must implement IFlashLoanSimpleReceiver.
    /// @param asset           The address of the ERC-20 asset to flash-borrow.
    /// @param amount          The amount to flash-borrow.
    /// @param params          Variadic packed params to pass to the receiver as extra information.
    /// @param referralCode    The code used to register the integrator for the referral program.
    ///                        Pass 0 if no referral.
    function flashLoanSimple(
        address receiverAddress,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16 referralCode
    ) external;
}
