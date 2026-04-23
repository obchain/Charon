// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import { IFlashLoanSimpleReceiver } from "./interfaces/IFlashLoanSimpleReceiver.sol";
import { IAaveV3Pool } from "./interfaces/IAaveV3Pool.sol";
import { IVToken } from "./interfaces/IVToken.sol";
import { ISwapRouter } from "./interfaces/ISwapRouter.sol";
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
//        c. Call vToken.redeem() — convert ALL seized vTokens to underlying.
//        d. Swap collateral → debt asset via PancakeSwap V3.
//        e. Sweep profit to owner.
//        f. Approve Aave for repayment (amount + premium); Aave pulls it after return.
//
// Security invariants:
//   - Only owner may trigger liquidations or rescue funds.
//   - executeOperation is only callable by the Aave Pool.
//   - initiator must equal address(this) — prevents a malicious pool from
//     invoking our callback with forged parameters.
//   - Reentrancy guard on executeLiquidation and batchExecute (nonReentrant).
//   - executeOperation NOT guarded with nonReentrant: it is called by Aave mid-
//     flash-loan, re-entering the guard frame. The msg.sender == AAVE_POOL gate
//     is the equivalent protection for the callback.
//   - Lingering approvals zeroed after each consume point (vToken, SwapRouter).
//   - No tx.origin usage. No delegatecall. No assembly. No upgradeability.
//   - No external library imports — all interfaces are inline/local.
// ─────────────────────────────────────────────────────────────────────────────

/// @title CharonLiquidator
/// @notice On-chain executor for flash-loan-backed liquidations across DeFi protocols.
///         v0.1 supports Venus Protocol on BNB Chain.
/// @dev Implements IFlashLoanSimpleReceiver for the Aave V3 flash-loan callback.
///      The bot (hot wallet = owner) is the sole authorized caller of executeLiquidation
///      and batchExecute.
contract CharonLiquidator is IFlashLoanSimpleReceiver {
    // ─────────────────────────────────────────────────────────────────────────
    // Protocol ID constants — must mirror the Rust `ProtocolId` enum order.
    // ─────────────────────────────────────────────────────────────────────────

    /// @dev ProtocolId::Venus = 3 in the Rust enum (0-indexed: Aave=0, Compound=1, ...).
    uint8 internal constant PROTOCOL_VENUS = 3;

    /// @dev Absolute ceiling on the number of liquidations in a single batchExecute call.
    ///      The Rust batcher uses 3 by default; 10 gives headroom for future tuning.
    ///      Prevents a compromised owner key from burning unbounded gas in one tx.
    uint256 internal constant MAX_BATCH_SIZE = 10;

    // ─────────────────────────────────────────────────────────────────────────
    // Reentrancy guard — simple two-state lock.
    // Stored as uint256 rather than bool to match the Solidity optimizer's
    // preferred SSTORE encoding and avoid zero→non-zero cold-write gas cost
    // on the first call (storage slot is initialized to 1 at deploy time).
    // ─────────────────────────────────────────────────────────────────────────

    uint256 private _entered = 1;

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

    /// @notice Emitted at the end of a successful batchExecute call.
    /// @param count The number of liquidations initiated in the batch.
    event BatchExecuted(uint256 count);

    // ─────────────────────────────────────────────────────────────────────────
    // Modifiers
    // ─────────────────────────────────────────────────────────────────────────

    /// @dev Restricts a function to the deploying hot wallet (owner).
    modifier onlyOwner() {
        require(msg.sender == owner, "!owner");
        _;
    }

    /// @dev Prevents reentrant calls into executeLiquidation and batchExecute.
    ///      Uses 1/2 rather than 0/1 to avoid cold-write SSTORE costs on every call.
    ///      NOT applied to executeOperation — that function is called by Aave mid-
    ///      flash-loan and is already protected by the msg.sender == AAVE_POOL gate.
    ///      Applying nonReentrant to executeOperation would deadlock the flash loan.
    ///      NOT applied to _initiateFlashLoan — it is an internal helper called with
    ///      the guard already held by the outer entry point.
    modifier nonReentrant() {
        require(_entered == 1, "reentrant");
        _entered = 2;
        _;
        _entered = 1;
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
    // External — owner entry points
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Initiates a flash-loan-backed liquidation of a Venus borrower.
    /// @dev Called exclusively by the off-chain bot (owner). Delegates to
    ///      _initiateFlashLoan after acquiring the reentrancy lock.
    ///
    ///      Flow:
    ///        1. Validate inputs (inside _initiateFlashLoan).
    ///        2. ABI-encode params to bytes.
    ///        3. Call IAaveV3Pool.flashLoanSimple — Aave transfers debtToken to this
    ///           contract then immediately calls executeOperation().
    ///        4. After executeOperation returns true, Aave pulls amount + premium
    ///           using the allowance set inside the callback. No further state work
    ///           is required here.
    ///
    /// @param params All parameters describing the Venus liquidation opportunity.
    function executeLiquidation(LiquidationParams calldata params) external onlyOwner nonReentrant {
        _initiateFlashLoan(params);
    }

    /// @notice Initiates multiple flash-loan-backed liquidations in a single transaction.
    /// @dev Called exclusively by the off-chain bot (owner). Iterates over `items` and
    ///      calls _initiateFlashLoan for each. A revert in any iteration reverts the
    ///      entire batch atomically — there is no partial execution.
    ///
    ///      The nonReentrant guard is held for the full duration of the loop. Each
    ///      _initiateFlashLoan invocation calls Aave's flashLoanSimple, which re-enters
    ///      executeOperation within the _entered == 2 window; that is the expected and
    ///      safe path. A malicious pool attempting to re-enter batchExecute mid-loop
    ///      would hit the nonReentrant guard and revert.
    ///
    /// @param items Array of LiquidationParams, one per borrower to liquidate.
    ///              Must be non-empty and no longer than MAX_BATCH_SIZE.
    function batchExecute(LiquidationParams[] calldata items) external onlyOwner nonReentrant {
        uint256 n = items.length;
        require(n > 0, "!items");
        require(n <= MAX_BATCH_SIZE, "batch too large");

        for (uint256 i = 0; i < n; i++) {
            _initiateFlashLoan(items[i]);
        }

        emit BatchExecuted(n);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // External — Aave V3 flash-loan callback
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Aave V3 flash-loan callback. Called by the Pool immediately after
    ///         transferring `amount` of `asset` to this contract.
    /// @dev Security gates (preserved from skeleton):
    ///        1. msg.sender == AAVE_POOL  — only the genuine Aave Pool may call this.
    ///        2. initiator == address(this) — only flash loans we ourselves initiated.
    ///
    ///      Full liquidation flow:
    ///        a. Decode LiquidationParams from `data`.
    ///        b. Sanity-check asset/amount match decoded params.
    ///        c. Approve debtVToken and call liquidateBorrow on Venus.
    ///        d. Zero out debtVToken approval (consumed).
    ///        e. Redeem all seized collateral vTokens for underlying.
    ///        f. Swap collateral underlying → debt token via PancakeSwap V3.
    ///        g. Zero out SwapRouter approval (consumed).
    ///        h. Verify post-swap balance covers totalOwed.
    ///        i. Sweep profit to owner.
    ///        j. Emit LiquidationExecuted.
    ///        k. Approve Aave Pool for totalOwed (Aave pulls this after we return).
    ///        l. Return true.
    ///
    ///      NOTE: nonReentrant is intentionally NOT applied here. Applying it would
    ///      deadlock the flash loan because executeLiquidation / batchExecute already
    ///      holds the lock (_entered == 2) when Aave re-enters this callback within
    ///      the same tx. The msg.sender == AAVE_POOL gate is the equivalent protection.
    ///
    /// @param asset     The flash-loaned ERC-20 token (must equal p.debtToken).
    /// @param amount    The flash-loan principal (must equal p.repayAmount).
    /// @param premium   The Aave fee owed on top of `amount`.
    /// @param initiator The address that initiated the flash loan (must be address(this)).
    /// @param data      ABI-encoded LiquidationParams forwarded from _initiateFlashLoan.
    /// @return True on success; Aave reverts the entire tx if false is returned.
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata data
    ) external override returns (bool) {
        // ── Security gates (from skeleton — do not remove) ────────────────────
        // Gate 1: only the real Aave Pool can invoke this callback.
        require(msg.sender == AAVE_POOL, "!pool");
        // Gate 2: we only process flash loans we ourselves requested.
        require(initiator == address(this), "!initiator");

        // ── Step 1: decode parameters ─────────────────────────────────────────
        LiquidationParams memory p = abi.decode(data, (LiquidationParams));

        // ── Step 2: sanity — confirm Aave gave us exactly what we asked for ───
        // These checks catch any pool-side discrepancy and validate that the
        // encoded params are consistent with the actual flash-loan terms.
        require(asset == p.debtToken, "asset/debt mismatch");
        require(amount == p.repayAmount, "amount/repay mismatch");

        // ── Step 3: liquidate on Venus ────────────────────────────────────────
        // Approve the debt vToken to spend exactly repayAmount of the debt asset.
        // Venus pulls this during liquidateBorrow; approval is zeroed immediately
        // after to eliminate lingering allowances.
        IERC20(p.debtToken).approve(p.debtVToken, p.repayAmount);

        uint256 liqErr = IVToken(p.debtVToken)
            .liquidateBorrow(
                p.borrower,
                p.repayAmount,
                p.collateralVToken // seized vTokens land in address(this)
            );
        require(liqErr == 0, "venus liquidate failed");

        // Zero out vToken approval — liquidateBorrow has consumed it.
        // Protects against a malicious or re-upgraded vToken contract
        // attempting a second pull in future calls.
        IERC20(p.debtToken).approve(p.debtVToken, 0);

        // ── Step 4: redeem seized collateral vTokens for underlying ───────────
        // balanceOf gives us the exact vToken units seized by liquidateBorrow.
        // We use redeem(vTokenAmount) rather than redeemUnderlying(underlyingAmount)
        // to drain the full balance in one call without rounding loss.
        uint256 vBal = IVToken(p.collateralVToken).balanceOf(address(this));
        require(vBal > 0, "no collateral seized");

        uint256 redeemErr = IVToken(p.collateralVToken).redeem(vBal);
        require(redeemErr == 0, "venus redeem failed");

        // ── Step 5: swap collateral underlying → debt token via PancakeSwap V3 ─
        // Read the full collateral balance just redeemed; use it as exact amountIn.
        uint256 collateralBal = IERC20(p.collateralToken).balanceOf(address(this));

        // Approve the router for the exact amount we are about to swap.
        IERC20(p.collateralToken).approve(SWAP_ROUTER, collateralBal);

        ISwapRouter(SWAP_ROUTER)
            .exactInputSingle(
                ISwapRouter.ExactInputSingleParams({
                    tokenIn: p.collateralToken,
                    tokenOut: p.debtToken,
                    fee: 3000, // 0.30 % pool — most liquid tier on PCS V3 for major pairs
                    recipient: address(this),
                    deadline: block.timestamp,
                    amountIn: collateralBal,
                    amountOutMinimum: p.minSwapOut, // router reverts if output < this
                    sqrtPriceLimitX96: 0 // no price limit — slippage floor above is enough
                })
            );

        // Zero out router approval — exactInputSingle has consumed it.
        IERC20(p.collateralToken).approve(SWAP_ROUTER, 0);

        // ── Step 6: verify post-swap balance covers repayment ─────────────────
        uint256 totalOwed = amount + premium;
        uint256 finalBal = IERC20(p.debtToken).balanceOf(address(this));

        // Defensive check on top of the router's amountOutMinimum guard:
        // ensures the contract cannot accidentally under-repay Aave even if
        // minSwapOut was set below totalOwed by the caller.
        require(finalBal >= totalOwed, "swap output below repayment");

        // ── Step 7: sweep profit to owner ─────────────────────────────────────
        // Profit must leave this contract before we approve Aave, otherwise Aave
        // could theoretically pull more than totalOwed if the token has quirks.
        uint256 profit = finalBal - totalOwed;
        if (profit > 0) {
            // transfer return value not checked: owner is a trusted address set at
            // construction; a failure here reverts the whole tx (excess funds stay
            // in the contract until rescued). Standard ERC-20s revert on failure.
            IERC20(p.debtToken).transfer(owner, profit);
        }

        // ── Step 8: emit before the final approval so logs reflect the full state ─
        emit LiquidationExecuted(p.borrower, p.debtToken, p.repayAmount, profit);

        // ── Step 9: approve Aave to pull totalOwed ────────────────────────────
        // Aave pulls amount + premium from this contract after executeOperation
        // returns true. We set approval here; Aave consumes it entirely, so
        // there is no practical way to zero it out post-return in this call frame.
        IERC20(p.debtToken).approve(AAVE_POOL, totalOwed);

        return true;
    }

    // ─────────────────────────────────────────────────────────────────────────
    // External — safety hatch
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Recovers ERC-20 tokens or native BNB that are stuck in this contract.
    /// @dev Fully implemented — this is a safety hatch, not core liquidation logic.
    ///      For ERC-20: calls token.transfer(to, amount).
    ///      For native BNB: uses payable(to).transfer(amount).
    ///
    ///      Security notes:
    ///        - onlyOwner: only the hot wallet can pull funds.
    ///        - `to` is validated non-zero to prevent burning.
    ///        - Uses IERC20.transfer directly (no SafeERC20) because this is a
    ///          no-external-dependency build; fee-on-transfer tokens may transfer
    ///          less than `amount` — that edge case is acceptable in rescue context.
    ///        - Native transfer uses Solidity's `transfer` which forwards 2300 gas
    ///          and reverts on failure — appropriate for a trusted owner address.
    ///
    /// @param token  ERC-20 contract address, or address(0) for native BNB.
    /// @param to     Recipient address. Must be non-zero.
    /// @param amount Number of tokens (or wei) to transfer.
    function rescue(address token, address to, uint256 amount) external onlyOwner {
        require(to != address(0), "!to");
        require(amount > 0, "!amount");

        if (token == address(0)) {
            // Native BNB path.
            // `transfer` reverts on failure and caps forwarded gas at 2300,
            // which is appropriate for a trusted owner EOA.
            payable(to).transfer(amount);
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
    // Internal — shared flash-loan initiator
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Validates `p`, encodes it, and requests a flashLoanSimple from Aave.
    /// @dev Called by both executeLiquidation and batchExecute. Must NOT be decorated
    ///      with nonReentrant — the caller already holds the lock. Adding nonReentrant
    ///      here would deadlock the flash loan because Aave re-enters executeOperation
    ///      (which runs inside _entered == 2) before this function returns.
    ///
    ///      The seven require guards here are identical to those that previously lived
    ///      in executeLiquidation and are the single canonical validation point for any
    ///      liquidation initiated by this contract.
    ///
    /// @param p The fully-populated LiquidationParams for one liquidation.
    function _initiateFlashLoan(LiquidationParams memory p) internal {
        // ── Input validation ──────────────────────────────────────────────────
        require(p.protocolId == PROTOCOL_VENUS, "!protocolId");
        require(p.borrower != address(0), "!borrower");
        require(p.debtToken != address(0), "!debtToken");
        require(p.collateralToken != address(0), "!collateralToken");
        require(p.debtVToken != address(0), "!debtVToken");
        require(p.collateralVToken != address(0), "!collateralVToken");
        require(p.repayAmount > 0, "!repayAmount");

        // ── Encode params and request the flash loan ──────────────────────────
        // Aave forwards `encoded` verbatim to executeOperation as the `data`
        // argument; we decode it there to recover the liquidation parameters.
        bytes memory encoded = abi.encode(p);

        IAaveV3Pool(AAVE_POOL)
            .flashLoanSimple(
                address(this), // receiver — this contract implements the callback
                p.debtToken, // asset   — the token we need to repay Venus with
                p.repayAmount, // amount  — exact principal to borrow
                encoded, // params  — forwarded to executeOperation
                0 // referralCode — no referral
            );
        // Aave has pulled amount + premium via the allowance set in executeOperation.
        // Nothing further to do in this frame.
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Receive — accept native BNB
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Allows this contract to receive native BNB (e.g., from vBNB redemption
    ///         or direct top-up by the operator) so that rescue() can withdraw it.
    receive() external payable { }
}
