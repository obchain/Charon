// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @title IWETH
/// @notice Minimal interface for the Wrapped BNB (WBNB) contract on BNB Chain.
/// @dev Canonical WBNB address on BSC mainnet: 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c.
///      Identical ABI to canonical WETH9 — `deposit()` wraps native BNB into the ERC-20
///      representation that DEX pools expect; `withdraw()` unwraps it back to native.
///      CharonLiquidator uses this exclusively on the vBNB redemption path: Venus
///      `vBNB.redeem()` returns native BNB, so the contract wraps the balance into
///      WBNB before forwarding to the PancakeSwap V3 router.
interface IWETH {
    /// @notice Wraps the attached native BNB into an equivalent WBNB balance for msg.sender.
    /// @dev MUST be called with non-zero `msg.value` — the contract mints 1:1 to the sender.
    function deposit() external payable;

    /// @notice Unwraps `amount` WBNB of the caller back to native BNB.
    /// @dev Reverts if the caller's WBNB balance is below `amount`.
    /// @param amount The number of WBNB to unwrap.
    function withdraw(uint256 amount) external;
}
