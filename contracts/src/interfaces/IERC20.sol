// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @title IERC20
/// @notice Minimal ERC-20 interface required by CharonLiquidator.
/// @dev Only the subset of ERC-20 that the liquidator and rescue logic directly call.
///      Full transfer/approval events are omitted here — they are emitted by token contracts.
interface IERC20 {
    /// @notice Returns the token balance of `account`.
    /// @param account The address to query.
    /// @return The token balance.
    function balanceOf(address account) external view returns (uint256);

    /// @notice Transfers `amount` tokens to `to` from the caller.
    /// @param to     Recipient address.
    /// @param amount Number of tokens to send.
    /// @return True on success (non-standard tokens may revert instead).
    function transfer(address to, uint256 amount) external returns (bool);

    /// @notice Approves `spender` to spend up to `amount` of the caller's tokens.
    /// @param spender The address being approved.
    /// @param amount  The allowance ceiling.
    /// @return True on success.
    function approve(address spender, uint256 amount) external returns (bool);

    /// @notice Returns the remaining allowance that `spender` may transfer on behalf of `owner`.
    /// @param owner   The token holder.
    /// @param spender The approved spender.
    /// @return The remaining allowance.
    function allowance(address owner, address spender) external view returns (uint256);
}
