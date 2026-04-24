// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import { IFlashLoanSimpleReceiver } from "./interfaces/IFlashLoanSimpleReceiver.sol";
import { IAaveV3Pool } from "./interfaces/IAaveV3Pool.sol";
import { IVToken } from "./interfaces/IVToken.sol";
import { ISwapRouter } from "./interfaces/ISwapRouter.sol";
import { IERC20 } from "./interfaces/IERC20.sol";
import { IWETH } from "./interfaces/IWETH.sol";

// ─────────────────────────────────────────────────────────────────────────────
// CharonLiquidator — multi-chain flash-loan liquidation engine, v0.1
//
// Scope (v0.1): Venus Protocol on BNB Chain.
//   1. Bot calls executeLiquidation() — single item — or batchExecute() — up
//      to MAX_BATCH_SIZE items — with repayment parameters.
//   2. Contract requests a flash loan from Aave V3 (flashLoanSimple) for each
//      item (the batch path loops over _initiateFlashLoan).
//   3. Aave calls back executeOperation(); inside we:
//        a. Approve Venus vToken to spend the debt asset.
//        b. Call vToken.liquidateBorrow() — repay debt, seize collateral vTokens.
//        c. Call vToken.redeem() — convert ALL seized vTokens to underlying.
//           Special case: vBNB returns native BNB, which we wrap into WBNB.
//        d. Swap collateral → debt asset via PancakeSwap V3 at the caller-supplied
//           fee tier (500 / 3000 / 10000 depending on the pool).
//        e. Sweep profit to the COLD wallet — hot wallet (owner) holds gas only.
//        f. Approve Aave for repayment (amount + premium); Aave pulls it after return.
//
// Security invariants:
//   - Only owner may trigger liquidations or rescue funds.
//   - executeOperation is only callable by the Aave Pool.
//   - initiator must equal address(this) — prevents a malicious pool from
//     invoking our callback with forged parameters.
//   - Reentrancy guard on executeLiquidation AND batchExecute (nonReentrant).
//     The guard is held for the full duration of the batch loop; Aave re-enters
//     executeOperation within the _entered == 2 window, which is the expected
//     path. A malicious pool attempting to re-enter batchExecute or
//     executeLiquidation mid-loop hits the guard and reverts.
//   - executeOperation NOT guarded with nonReentrant: it is called by Aave mid-
//     flash-loan, re-entering the entry-point's guard frame. The msg.sender
//     == AAVE_POOL gate is the equivalent protection for the callback.
//   - batchExecute is EVM-atomic: any revert in any item reverts the whole
//     batch. No partial state change survives. BatchExecuted is emitted only
//     on full-batch success and observers must NOT treat its absence as
//     partial progress.
//   - Lingering approvals zeroed after each consume point (vToken, SwapRouter).
//   - Profit is swept to the immutable COLD_WALLET, never to the hot wallet.
//     This enforces the CLAUDE.md safety invariant: "hot wallet holds gas only".
//   - No tx.origin usage. No delegatecall. No assembly. No upgradeability.
//   - No external library imports — all interfaces are inline/local.
// ─────────────────────────────────────────────────────────────────────────────

/// @title CharonLiquidator
/// @notice On-chain executor for flash-loan-backed liquidations across DeFi protocols.
///         v0.1 supports Venus Protocol on BNB Chain.
/// @dev Implements IFlashLoanSimpleReceiver for the Aave V3 flash-loan callback.
///      The bot (hot wallet = owner) is the sole authorized caller of
///      executeLiquidation and batchExecute. All profit is routed to the
///      immutable cold wallet set at construction.
contract CharonLiquidator is IFlashLoanSimpleReceiver {
    // ─────────────────────────────────────────────────────────────────────────
    // Protocol ID constants — must mirror the Rust `ProtocolId` enum order.
    // ─────────────────────────────────────────────────────────────────────────

    /// @dev ProtocolId::Venus = 3 in the Rust enum (0-indexed: Aave=0, Compound=1, ...).
    uint8 internal constant PROTOCOL_VENUS = 3;

    /// @dev Absolute ceiling on the number of liquidations in a single batchExecute call.
    ///      The Rust batcher (`Batcher::MAX_BATCH_SIZE`) defaults to 3; 10 gives
    ///      headroom for future tuning. Prevents a compromised owner key from
    ///      burning unbounded gas in one tx.
    uint256 internal constant MAX_BATCH_SIZE = 10;

    // ─────────────────────────────────────────────────────────────────────────
    // BNB Chain canonical addresses — hard-coded for the v0.1 BSC-only scope.
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Venus vBNB market — the only vToken whose underlying is native BNB.
    ///         Venus `redeem()` on this market transfers native BNB to msg.sender
    ///         rather than calling `IERC20.transfer`, so the standard ERC-20
    ///         balance read used for every other vToken returns zero here.
    ///         Mainnet: https://bscscan.com/address/0xA07c5b74C9B40447a954e1466938b865b6BBea36
    address internal constant VBNB = 0xA07c5b74C9B40447a954e1466938b865b6BBea36;

    /// @notice Canonical Wrapped BNB (WBNB). PancakeSwap V3 pools are quoted in WBNB,
    ///         so any vBNB-seized position must be wrapped before the swap leg.
    ///         Mainnet: https://bscscan.com/address/0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c
    address internal constant WBNB = 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c;

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

    /// @notice The bot's hot wallet. Only address authorised to call
    ///         executeLiquidation, batchExecute, and rescue. By policy it holds
    ///         gas only — profit is never routed here.
    address public immutable owner;

    /// @notice Aave V3 Pool proxy on BNB Chain.
    ///         Mainnet: 0x6807dc923806fE8Fd134338EABCA509979a7e08
    address public immutable AAVE_POOL;

    /// @notice PancakeSwap V3 SwapRouter on BNB Chain.
    ///         Mainnet: 0x13f4EA83D0bd40E75C8222255bc855a974568Dd4
    address public immutable SWAP_ROUTER;

    /// @notice Cold wallet — sole recipient of liquidation profit.
    /// @dev Profit is transferred here inside executeOperation, never to the hot
    ///      wallet. Enforces the CLAUDE.md safety invariant that the bot wallet
    ///      holds gas only. Set once at construction and immutable thereafter.
    address public immutable COLD_WALLET;

    // ─────────────────────────────────────────────────────────────────────────
    // Structs
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice All parameters required to execute a single Venus liquidation.
    /// @dev Packed into `bytes` and forwarded through Aave's flashLoanSimple `params`
    ///      argument so executeOperation can decode them without extra storage.
    ///      Field layout must remain stable — the Rust side abi-encodes this struct.
    ///      NOTE: the companion Rust `LiquidationParams` builder lives in the
    ///      charon-executor crate. Its `BatchParams` (for `batchExecute`) and the
    ///      `CharonLiquidationParams` (for `executeLiquidation`) must mirror this
    ///      layout exactly, including `swapPoolFee`.
    struct LiquidationParams {
        /// @dev Protocol identifier. Must equal PROTOCOL_VENUS (3) for v0.1.
        uint8 protocolId;
        /// @dev The underwater borrower whose position is being liquidated.
        address borrower;
        /// @dev Underlying ERC-20 token that the borrower owes (e.g., USDT).
        address debtToken;
        /// @dev Underlying ERC-20 token posted as collateral (e.g., WBNB for vBNB).
        ///      For the vBNB path this MUST be WBNB — the contract wraps the
        ///      native BNB returned by Venus into WBNB before the swap.
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
        /// @dev PancakeSwap V3 pool fee tier (hundredths of a bip) for the
        ///      collateral → debt swap. Live tiers on PCS V3: 100 / 500 / 2500 /
        ///      10000; Uniswap-equivalent 3000 is also deployed. BTCB, ETH and XVS
        ///      deep pools are at 500 or 10000, not 3000 — hardcoding would route
        ///      through an empty pool and revert. Supplied per-opportunity by the
        ///      off-chain router. Must be non-zero.
        uint24 swapPoolFee;
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Events
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Emitted when a liquidation cycle completes successfully.
    /// @param borrower    The liquidated account.
    /// @param debtToken   The underlying asset that was repaid.
    /// @param repayAmount The amount of debtToken that was repaid.
    /// @param profit      Net profit in debtToken units swept to the cold wallet.
    /// @param recipient   The cold wallet address that received `profit` (indexed
    ///                    so off-chain monitors can filter by destination).
    event LiquidationExecuted(
        address indexed borrower,
        address indexed debtToken,
        uint256 repayAmount,
        uint256 profit,
        address indexed recipient
    );

    /// @notice Emitted when the owner recovers tokens or native BNB via rescue().
    /// @param token  The ERC-20 address that was rescued, or address(0) for native BNB.
    /// @param to     The recipient of the recovered funds.
    /// @param amount The amount transferred.
    event Rescued(address indexed token, address indexed to, uint256 amount);

    /// @notice Emitted at the end of a successful batchExecute call.
    /// @dev Emitted only on full-batch success. If any item in the batch reverts,
    ///      the entire transaction reverts atomically and this event is NOT emitted.
    ///      Observers can therefore treat a BatchExecuted emission as proof that all
    ///      `count` flash loans initiated by this call completed successfully.
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

    /// @notice Deploys CharonLiquidator and permanently binds it to one Aave Pool,
    ///         one PancakeSwap V3 router, and one cold-wallet profit recipient.
    /// @dev msg.sender becomes the immutable owner (the bot's hot wallet).
    ///      All three addresses are validated non-zero at construction.
    ///      The cold wallet is required: the CLAUDE.md safety invariant forbids
    ///      parking profit in the hot wallet.
    /// @param _aavePool   Aave V3 Pool proxy address on BNB Chain.
    /// @param _swapRouter PancakeSwap V3 SwapRouter address on BNB Chain.
    /// @param _coldWallet Cold-wallet address that receives all liquidation profit.
    constructor(address _aavePool, address _swapRouter, address _coldWallet) {
        require(_aavePool != address(0), "!aavePool");
        require(_swapRouter != address(0), "!swapRouter");
        require(_coldWallet != address(0), "!coldWallet");
        owner = msg.sender;
        AAVE_POOL = _aavePool;
        SWAP_ROUTER = _swapRouter;
        COLD_WALLET = _coldWallet;
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
    /// @dev Called exclusively by the off-chain bot (owner). Iterates over `items`
    ///      and calls _initiateFlashLoan for each. A revert in any iteration reverts
    ///      the entire batch atomically — there is no partial execution.
    ///
    ///      **Atomicity contract.** Execution is EVM-atomic. If any item reverts — on
    ///      input validation inside _initiateFlashLoan, on the Aave flashLoanSimple
    ///      call, inside executeOperation's Venus / PancakeSwap path, or on the final
    ///      Aave repayment pull — all prior items in the same batch are also reverted
    ///      and no state change from this call survives. Profits already swept to
    ///      COLD_WALLET by earlier items are rolled back together with the rest of
    ///      the state on revert. BatchExecuted is emitted only on full-batch success;
    ///      observers must NOT treat the absence of a revert event as partial progress.
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
    ///           If the seized vToken is vBNB, wrap the returned native BNB into WBNB.
    ///        f. Swap collateral underlying → debt token via PancakeSwap V3 at the
    ///           caller-supplied pool fee tier.
    ///        g. Zero out SwapRouter approval (consumed).
    ///        h. Verify post-swap balance covers totalOwed.
    ///        i. Sweep profit to COLD_WALLET (NEVER to the hot wallet / owner).
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

        // vBNB returns NATIVE BNB, not an ERC-20. Wrap the full native balance
        // into WBNB so the swap leg can treat it uniformly with every other
        // vToken underlying. Reading IERC20(vBNB-underlying).balanceOf would
        // return zero and the swap would revert. Wrapping before the balance
        // read ensures `collateralBal` picks up the full seized amount.
        if (p.collateralVToken == VBNB) {
            uint256 nativeBal = address(this).balance;
            require(nativeBal > 0, "no native BNB redeemed");
            IWETH(WBNB).deposit{ value: nativeBal }();
        }

        // ── Step 5: swap collateral underlying → debt token via PancakeSwap V3 ─
        // Read the full collateral balance just redeemed (or wrapped, for vBNB);
        // use it as exact amountIn.
        uint256 collateralBal = IERC20(p.collateralToken).balanceOf(address(this));

        // Approve the router for the exact amount we are about to swap.
        IERC20(p.collateralToken).approve(SWAP_ROUTER, collateralBal);

        ISwapRouter(SWAP_ROUTER)
            .exactInputSingle(
                ISwapRouter.ExactInputSingleParams({
                    tokenIn: p.collateralToken,
                    tokenOut: p.debtToken,
                    fee: p.swapPoolFee, // caller-supplied — 500 / 2500 / 3000 / 10000 depending on pool
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

        // ── Step 7: sweep profit to COLD WALLET ───────────────────────────────
        // Profit must leave this contract to the cold wallet (NOT the hot-wallet
        // owner) before we approve Aave. This enforces the CLAUDE.md safety
        // invariant: hot wallet holds gas only. Sweeping before approval also
        // prevents Aave from pulling more than totalOwed if the debt token has
        // quirks (fee-on-transfer, rebasing, etc.).
        uint256 profit = finalBal - totalOwed;
        if (profit > 0) {
            // Return value MUST be checked. Standard ERC-20s revert on failure,
            // but BEP-20 tokens and some legacy implementations return `false`
            // without reverting. Without this check a silent `false` would leave
            // profit stranded in this contract while emitting a success log, and
            // the subsequent Aave approval/repayment would still settle — the
            // operator would think the liquidation netted the full profit while
            // the cold wallet received nothing. Reverting here keeps the sweep
            // and the event log in lockstep.
            bool ok = IERC20(p.debtToken).transfer(COLD_WALLET, profit);
            require(ok, "profit: transfer failed");
        }

        // ── Step 8: emit before the final approval so logs reflect the full state ─
        emit LiquidationExecuted(p.borrower, p.debtToken, p.repayAmount, profit, COLD_WALLET);

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
    ///      For ERC-20: calls token.transfer(to, amount) and checks the return value.
    ///      For native BNB: uses a low-level call{value: amount}("") with success check.
    ///
    ///      Security notes:
    ///        - onlyOwner: only the hot wallet can pull funds.
    ///        - `to` is validated non-zero to prevent burning.
    ///        - Uses IERC20.transfer directly (no SafeERC20) because this is a
    ///          no-external-dependency build; fee-on-transfer tokens may transfer
    ///          less than `amount` — that edge case is acceptable in rescue context.
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
    // Internal — shared flash-loan initiator
    // ─────────────────────────────────────────────────────────────────────────

    /// @notice Validates `p`, encodes it, and requests a flashLoanSimple from Aave.
    /// @dev Called by both executeLiquidation and batchExecute. Must NOT be decorated
    ///      with nonReentrant — the caller already holds the lock. Adding nonReentrant
    ///      here would deadlock the flash loan because Aave re-enters executeOperation
    ///      (which runs inside _entered == 2) before this function returns.
    ///
    ///      The eight require guards here (including the vBNB→WBNB pairing check)
    ///      are the single canonical validation point for any liquidation initiated
    ///      by this contract. Keep input validation here; do not duplicate it in
    ///      executeOperation, where the params arrive through abi.decode(data) and
    ///      are re-checked only for asset/amount consistency with the flash-loan
    ///      terms.
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
        require(p.swapPoolFee > 0, "!swapPoolFee");
        // On the vBNB path the underlying returned by Venus is native BNB, which
        // the contract wraps into WBNB before swapping. Enforce that the caller
        // declared WBNB as collateralToken so the swap leg routes through a real
        // pool and post-swap balance checks read the correct token.
        if (p.collateralVToken == VBNB) {
            require(p.collateralToken == WBNB, "vBNB requires WBNB");
        }

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
    // Receive — native BNB is intentionally rejected
    // ─────────────────────────────────────────────────────────────────────────
    //
    // No `receive()` or `fallback()` is defined. Plain BNB transfers to this
    // contract revert. Rationale:
    //   - v0.1 does not liquidate the vBNB native-BNB market at the protocol
    //     level: the vBNB branch above would only fire if Venus `redeem()` on
    //     vBNB could credit this contract with native BNB. Venus's current
    //     vBNB implementation forwards native BNB via `.call{value:...}("")`,
    //     which requires a `receive()` on the recipient; absent one, the
    //     redeem reverts and the vBNB branch is unreachable end-to-end.
    //   - An open `receive()` would silently accumulate BNB from any sender,
    //     making misrouted funds hard to notice and providing free storage
    //     for griefers / mixers.
    //   - When the vBNB market is activated operationally, reintroduce a gated
    //     `receive()` that requires `msg.sender == VBNB` so only the Venus
    //     contract can push native BNB into this contract during redeem.
    //
    // If BNB is ever trapped here (e.g. as a SELFDESTRUCT beneficiary), the
    // owner can still recover it via rescue(address(0), ...) because
    // SELFDESTRUCT credits the balance without invoking `receive()`.
}
