// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import { IFlashLoanSimpleReceiver } from "./interfaces/IFlashLoanSimpleReceiver.sol";
import { IERC20 } from "./interfaces/IERC20.sol";

// ─────────────────────────────────────────────────────────────────────────────
// CharonLiquidator — multi-chain flash-loan liquidation engine, v0.1
//
// Scope (v0.1): Venus Protocol on BNB Chain.
//   1. Bot calls executeLiquidation() with repayment parameters.
//   2. Contract requests a flash loan from Aave V3 (flashLoanSimple).
//   3. Aave calls back executeOperation(); inside we:
//        a. Approve Venus vToken to spend the debt asset.
//        b. Call vToken.liquidateBorrow() — repay debt, seize collateral vTokens.
//        c. Call vToken.redeemUnderlying() — convert seized vTokens to underlying.
//        d. Swap collateral → debt asset via PancakeSwap V3.
//        e. Repay Aave (amount + premium).
//        f. Transfer profit to owner.
//   Steps (a–f) are NOT implemented in this skeleton — bodies revert loudly.
//
// Security invariants (enforced even in skeleton):
//   - Only owner may trigger liquidations or rescue funds.
//   - executeOperation is only callable by the Aave Pool.
//   - initiator must equal address(this) — prevents a malicious pool from
//     invoking our callback with forged parameters.
//   - No tx.origin usage. No delegatecall. No assembly. No upgradeability.
//   - No external imports — all interfaces are inline/local for zero-dependency
//     forge build in the skeleton phase.
// ─────────────────────────────────────────────────────────────────────────────

/// @title CharonLiquidator
/// @notice On-chain executor for flash-loan-backed liquidations across DeFi protocols.
///         v0.1 supports Venus Protocol on BNB Chain.
/// @dev Implements IFlashLoanSimpleReceiver for the Aave V3 flash-loan callback.
///      The bot (hot wallet = owner) is the sole authorized caller of executeLiquidation.
///      All liquidation and swap logic is stubbed — see issue #12.
contract CharonLiquidator is IFlashLoanSimpleReceiver {
    // ─────────────────────────────────────────────────────────────────────────
    // Protocol ID constants — must mirror the Rust `ProtocolId` enum order.
    // ─────────────────────────────────────────────────────────────────────────

    /// @dev ProtocolId::Venus = 3 in the Rust enum (0-indexed: Aave=0, Compound=1, ...).
    uint8 internal constant PROTOCOL_VENUS = 3;

    // ─────────────────────────────────────────────────────────────────────────
    // Immutable configuration — set once at construction, never changed.
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice The bot's hot wallet. Only address authorised to call executeLiquidation and rescue.
    address public immutable owner;

    /// @notice Aave V3 Pool proxy on BNB Chain.
    ///         Mainnet: 0x6807dc923806fE8Fd134338EABCA509979a7e08
    address public immutable AAVE_POOL;

    /// @notice PancakeSwap V3 SwapRouter on BNB Chain.
    ///         Mainnet: 0x13f4EA83D0bd40E75C8222255bc855a974568Dd4
    address public immutable SWAP_ROUTER;

    // ─────────────────────────────────────────────────────────────────────────
    // Structs
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice All parameters required to execute a single Venus liquidation.
    /// @dev Packed into `bytes` and forwarded through Aave's flashLoanSimple `params`
    ///      argument so executeOperation can decode them without extra storage.
    ///      Field layout must remain stable — the Rust side abi-encodes this struct.
    struct LiquidationParams {
        /// @dev Protocol identifier. Must equal PROTOCOL_VENUS (3) for v0.1.
        uint8 protocolId;
        /// @dev The underwater borrower whose position is being liquidated.
        address borrower;
        /// @dev Underlying ERC-20 token that the borrower owes (e.g., USDT).
        address debtToken;
        /// @dev Underlying ERC-20 token posted as collateral (e.g., BNB/WBNB).
        address collateralToken;
        /// @dev Venus vToken representing the debt side (e.g., vUSDT).
        address debtVToken;
        /// @dev Venus vToken representing the collateral side (e.g., vBNB).
        address collateralVToken;
        /// @dev Amount of debtToken to repay, capped at the Venus close factor.
        uint256 repayAmount;
        /// @dev Minimum amount of debtToken to receive from the collateral swap.
        ///      Acts as a slippage floor — revert if swap output falls below this.
        uint256 minSwapOut;
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Events
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Emitted when a liquidation cycle completes successfully.
    /// @param borrower    The liquidated account.
    /// @param debtToken   The underlying asset that was repaid.
    /// @param repayAmount The amount of debtToken that was repaid.
    /// @param profit      Net profit in debtToken units retained by this contract.
    event LiquidationExecuted(
        address indexed borrower, address indexed debtToken, uint256 repayAmount, uint256 profit
    );

    /// @notice Emitted when the owner recovers tokens or native BNB via rescue().
    /// @param token  The ERC-20 address that was rescued, or address(0) for native BNB.
    /// @param to     The recipient of the recovered funds.
    /// @param amount The amount transferred.
    event Rescued(address indexed token, address indexed to, uint256 amount);

    // ─────────────────────────────────────────────────────────────────────────
    // Modifiers
    // ─────────────────────────────────────────────────────────────────────────

    /// @dev Restricts a function to the deploying hot wallet (owner).
    ///      Uses a string revert for maximum compatibility with off-chain tooling
    ///      that parses revert reasons at this stage of the skeleton.
    modifier onlyOwner() {
        require(msg.sender == owner, "!owner");
        _;
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Constructor
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Deploys CharonLiquidator and permanently binds it to one Aave Pool
    ///         and one PancakeSwap V3 router.
    /// @dev msg.sender becomes the immutable owner (the bot's hot wallet).
    ///      Both addresses are validated non-zero at construction.
    /// @param _aavePool   Aave V3 Pool proxy address on BNB Chain.
    /// @param _swapRouter PancakeSwap V3 SwapRouter address on BNB Chain.
    constructor(address _aavePool, address _swapRouter) {
        require(_aavePool != address(0), "!aavePool");
        require(_swapRouter != address(0), "!swapRouter");
        owner = msg.sender;
        AAVE_POOL = _aavePool;
        SWAP_ROUTER = _swapRouter;
    }

    // ─────────────────────────────────────────────────────────────────────────
    // External — owner entry point
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Initiates a flash-loan-backed liquidation of a Venus borrower.
    /// @dev Called exclusively by the off-chain bot (owner). The function encodes
    ///      `params` and requests a flash loan from Aave; the actual liquidation
    ///      logic executes inside executeOperation().
    ///
    ///      Checks performed here (skeleton phase):
    ///        - Caller is owner (onlyOwner modifier).
    ///        - protocolId == PROTOCOL_VENUS.
    ///        - Key addresses are non-zero.
    ///        - repayAmount > 0.
    ///
    ///      BODY NOT IMPLEMENTED — see issue #12.
    /// @param params All parameters describing the Venus liquidation opportunity.
    function executeLiquidation(LiquidationParams calldata params) external onlyOwner {
        // Input validation — performed even in skeleton so the deployed shape is correct.
        require(params.protocolId == PROTOCOL_VENUS, "!protocolId");
        require(params.borrower != address(0), "!borrower");
        require(params.debtToken != address(0), "!debtToken");
        require(params.collateralToken != address(0), "!collateralToken");
        require(params.debtVToken != address(0), "!debtVToken");
        require(params.collateralVToken != address(0), "!collateralVToken");
        require(params.repayAmount > 0, "!repayAmount");

        revert("CharonLiquidator: executeLiquidation not yet implemented");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // External — Aave V3 flash-loan callback
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Aave V3 flash-loan callback. Called by the Pool immediately after
    ///         transferring `amount` of `asset` to this contract.
    /// @dev Two security gates are checked before any logic runs:
    ///        1. msg.sender == AAVE_POOL  — only the genuine Aave Pool may call this.
    ///        2. initiator == address(this) — only flash loans we ourselves initiated.
    ///      Both checks together prevent any external actor from using our callback
    ///      as a weapon (e.g., to drain approved allowances).
    ///
    ///      Full implementation (decode params, liquidate Venus, swap, repay):
    ///      see issue #12.
    ///
    /// @dev Parameters: (asset, amount, premium, initiator, data) — see IFlashLoanSimpleReceiver.
    ///      `asset`, `amount`, `premium`, and `data` are unnamed in this skeleton to suppress
    ///      unused-variable compiler warnings; they will be named and consumed in issue #12.
    ///      `initiator` is named because the security gate reads it.
    /// @return True on success (unreachable in skeleton — revert fires first).
    function executeOperation(
        address, /* asset     — flash-loaned ERC-20; used in issue #12 */
        uint256, /* amount    — flash-loan principal; used in issue #12 */
        uint256, /* premium   — Aave fee; used in issue #12 */
        address initiator,
        bytes calldata /* data — ABI-encoded LiquidationParams; used in issue #12 */
    )
        external
        override
        returns (bool)
    {
        // Security gate 1: only the real Aave Pool can invoke this callback.
        require(msg.sender == AAVE_POOL, "!pool");
        // Security gate 2: we only process flash loans we ourselves requested.
        require(initiator == address(this), "!initiator");

        revert("CharonLiquidator: executeOperation not yet implemented");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // External — safety hatch
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Recovers ERC-20 tokens or native BNB that are stuck in this contract.
    /// @dev Fully implemented — this is a safety hatch, not core liquidation logic.
    ///      For ERC-20: calls token.transfer(to, amount) and checks the return value.
    ///      For native BNB: uses a low-level call{value: amount}("") with success check.
    ///
    ///      Security notes:
    ///        - onlyOwner: only the hot wallet can pull funds.
    ///        - `to` is validated non-zero to prevent burning.
    ///        - Uses IERC20.transfer directly (no SafeERC20) because this is a
    ///          skeleton with no OZ dependency; full impl (#12) should assess
    ///          whether fee-on-transfer tokens need special handling here.
    ///        - Native transfer uses `call` rather than `transfer` or `send`.
    ///          Solidity's `transfer` forwards a hard-coded 2300-gas stipend which
    ///          reverts against any recipient whose fallback does non-trivial work
    ///          (Gnosis Safe and most multisigs, smart-contract wallets, any
    ///          custody solution that logs inbound receipts). `call` forwards the
    ///          remaining gas and is the EIP-1884-safe primitive; its boolean
    ///          return value is checked to surface failures as reverts.
    ///
    /// @param token  ERC-20 contract address, or address(0) for native BNB.
    /// @param to     Recipient address. Must be non-zero.
    /// @param amount Number of tokens (or wei) to transfer.
    function rescue(address token, address to, uint256 amount) external onlyOwner {
        require(to != address(0), "!to");
        require(amount > 0, "!amount");

        if (token == address(0)) {
            // Native BNB path.
            // Use `call` with full remaining gas so the recipient may be a multisig
            // or smart-contract wallet (Gnosis Safe, etc.). The 2300-gas stipend of
            // `transfer`/`send` is insufficient post-EIP-1884 for such recipients
            // and would trap funds in this contract.
            (bool ok,) = payable(to).call{ value: amount }("");
            require(ok, "rescue: bnb transfer failed");
        } else {
            // ERC-20 path.
            // The return value is checked to handle tokens that return false rather than reverting.
            // NOTE: fee-on-transfer or rebasing tokens may transfer less than `amount`;
            //       that edge case is acceptable in the rescue context (excess stays in contract).
            bool ok = IERC20(token).transfer(to, amount);
            require(ok, "rescue: transfer failed");
        }

        emit Rescued(token, to, amount);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Receive — native BNB is intentionally rejected
    // ─────────────────────────────────────────────────────────────────────────
    //
    // No `receive()` or `fallback()` is defined. Plain BNB transfers to this
    // contract revert. Rationale:
    //   - v0.1 does not liquidate the vBNB native-BNB market; all supported
    //     Venus markets settle collateral in ERC-20 (WBNB, BUSD, USDT, ...).
    //   - An open `receive()` would silently accumulate BNB from any sender,
    //     making misrouted funds hard to notice and providing free storage
    //     for griefers / mixers.
    //   - If the vBNB market is added later, reintroduce a gated `receive()`
    //     that requires `msg.sender == vBNB_MARKET` so only the Venus
    //     contract can push native BNB into this contract during redeem.
    //
    // If BNB is ever trapped here (e.g. as a SELFDESTRUCT beneficiary), the
    // owner can still recover it via rescue(address(0), ...) because
    // SELFDESTRUCT credits the balance without invoking `receive()`.
}
