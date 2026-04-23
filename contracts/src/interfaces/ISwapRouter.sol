// SPDX-License-Identifier: MIT
pragma solidity =0.8.24;

/// @title ISwapRouter
/// @notice Minimal interface for the PancakeSwap V3 SwapRouter on BNB Chain.
/// @dev PancakeSwap V3 is a fork of Uniswap V3; the SwapRouter ABI is identical.
///      Only exactInputSingle is needed by CharonLiquidator v0.1 — the single-hop
///      swap from seized collateral back into the debt token.
///      BNB Chain mainnet SwapRouter: 0x13f4EA83D0bd40E75C8222255bc855a974568Dd4
interface ISwapRouter {
    /// @notice Parameters for a single-pool exact-input swap.
    /// @dev Mirrors IV3SwapRouter.ExactInputSingleParams from Uniswap V3 / PCS V3.
    struct ExactInputSingleParams {
        /// @dev Token being sold (the collateral underlying recovered after redemption).
        address tokenIn;
        /// @dev Token being bought (the debt token needed to repay Aave).
        address tokenOut;
        /// @dev Pool fee tier in hundredths of a bip (e.g. 3000 = 0.30 %).
        uint24 fee;
        /// @dev Recipient of the output tokens.
        address recipient;
        /// @dev Unix timestamp after which the transaction reverts.
        uint256 deadline;
        /// @dev Exact amount of tokenIn to swap.
        uint256 amountIn;
        /// @dev Minimum acceptable amount of tokenOut; router reverts if not met.
        uint256 amountOutMinimum;
        /// @dev Square-root price limit in Q64.96 format. Pass 0 for no limit.
        uint160 sqrtPriceLimitX96;
    }

    /// @notice Swaps `amountIn` of one token for as much as possible of another token.
    /// @dev The router pulls `amountIn` from msg.sender (caller must pre-approve).
    ///      Reverts if the resulting output is below `params.amountOutMinimum`.
    /// @param params The parameters for the swap, encoded as `ExactInputSingleParams`.
    /// @return amountOut The amount of `tokenOut` received.
    function exactInputSingle(ExactInputSingleParams calldata params)
        external
        payable
        returns (uint256 amountOut);
}
