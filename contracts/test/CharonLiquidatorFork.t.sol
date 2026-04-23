// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import "forge-std/Test.sol";

import { CharonLiquidator } from "../src/CharonLiquidator.sol";
import { IVToken } from "../src/interfaces/IVToken.sol";
import { IERC20 } from "../src/interfaces/IERC20.sol";

// Minimal PancakeSwap V3 QuoterV2 surface used to derive a realistic
// `minSwapOut` floor directly on-fork. Full ABI lives in the upstream
// PancakeSwap V3 periphery `IQuoterV2.sol`; only the single-pool
// `quoteExactInputSingle` shape is needed here.
interface IPcsQuoterV2 {
    struct QuoteExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint24 fee;
        uint160 sqrtPriceLimitX96;
    }

    function quoteExactInputSingle(QuoteExactInputSingleParams memory params)
        external
        returns (
            uint256 amountOut,
            uint160 sqrtPriceX96After,
            uint32 initializedTicksCrossed,
            uint256 gasEstimate
        );
}

/// @title CharonLiquidatorForkTest
/// @notice Integration tests for `CharonLiquidator` against a BNB Smart
///         Chain mainnet fork.
///
///         The single piece of integration these tests really exercise
///         is the Aave V3 flash-loan callback path — fork infrastructure
///         is the only environment where the real
///         `Pool.flashLoanSimple` (proxy → pool → aToken.transfer →
///         `executeOperation` → `transferFrom` via approval) can run
///         unmodified. Venus is still mocked because reproducing an
///         underwater borrower at a fixed historical block would need
///         weeks of archive-grep to keep current; PancakeSwap V3 is
///         called against the real on-fork router so the swap path and
///         the `minSwapOut` slippage floor are exercised end-to-end.
///
///         Mock strategy (scoped to Venus only):
///         - `IVToken.liquidateBorrow`, `balanceOf`, `redeem` — mocked.
///         - Collateral underlying — `deal`/WBNB.deposit seeds the
///           liquidator with the amount that a real `redeem` would
///           have produced.
///         - PancakeSwap V3 is **not** mocked; the real fork router
///           performs the swap and the real Quoter V2 is used to
///           derive a realistic `minSwapOut` floor per test.
///
///         Fork block is pinned to `FORK_BLOCK` below — set to a BSC
///         mainnet block taken on 2026-04-23 when every Aave V3 reserve
///         and every Venus vToken in the suite is known-active. The
///         pin makes CI deterministic: identical reserve state,
///         identical vToken exchange rates, identical PCS V3 pool
///         liquidity across runs. Bump the constant in a dedicated,
///         reviewed commit when refreshing against a newer on-chain
///         state. `BSC_FORK_BLOCK` env var overrides for ad-hoc
///         investigations without touching the source.
///
///         Archive requirement: `BNB_HTTP_URL` must point at a BSC
///         archive RPC. PublicNode and other light endpoints only
///         retain the last few thousand blocks of state and cannot
///         serve `FORK_BLOCK`. `setUp` probes this up front and skips
///         the entire suite with a clear reason if the endpoint is
///         non-archive so test logs never silently misattribute the
///         failure to contract logic.
///
///         `batchExecute` coverage — the contract already exposes a
///         batched entry point (`CharonLiquidator.batchExecute`) and
///         a dedicated fork test,
///         `test_forkBatchExecute_uniqueCollateralMarkets_happyPath`,
///         exercises four markets with distinct collateral tokens in
///         one transaction (#268). Markets that share a collateral
///         underlying (e.g. USDT/BTCB and USDC/BTCB) are deliberately
///         excluded from the batch scenario because the Venus side is
///         mocked and the helper seeds one fixed balance per token —
///         the first market's real swap would drain the shared
///         collateral before the second market's iteration runs.
///         Every market is still covered individually by its own
///         per-market test.
///
///         Scope note — the cold-wallet sweep (#265) and the vBNB →
///         WBNB redeem branch (#270) depend on contract changes
///         (`COLD_WALLET` immutable, IWETH wrap in `executeOperation`)
///         that landed on a different branch and have not yet been
///         ported here. Assertions for those two issues are tracked on
///         the upstream issues and will be re-tightened in this file
///         once the contract changes reach this branch.
contract CharonLiquidatorForkTest is Test {
    // ─── Fork pin ─────────────────────────────────────────────────────────
    // BSC mainnet block used by every fork test. Captured on 2026-04-23;
    // Aave V3 reserves for USDT/USDC/BTCB/ETH and every referenced vToken
    // are live at this height. Overridable at runtime via the
    // `BSC_FORK_BLOCK` env var for ad-hoc debugging (see `setUp`). Bump
    // in a dedicated commit when refreshing against newer on-chain state.
    uint256 internal constant FORK_BLOCK = 94_000_000;

    /// @dev Archive-probe offset. Reading code at `FORK_BLOCK - ARCHIVE_PROBE_OFFSET`
    ///      differentiates an archive endpoint (returns bytecode) from a
    ///      pruned endpoint (returns empty / errors). 5000 blocks back is
    ///      comfortably past any non-archive node's retention window.
    uint256 internal constant ARCHIVE_PROBE_OFFSET = 5_000;

    // ─── BSC mainnet addresses ────────────────────────────────────────────
    // Aave V3 Pool proxy. Same address used in `config/default.toml`.
    address internal constant AAVE_V3_POOL = 0x6807dc923806fE8Fd134338EABCA509979a7e0cB;

    // PancakeSwap V3 SmartRouter on BSC mainnet. Source:
    // github.com/pancakeswap/pancake-v3-contracts/deployments
    address internal constant PCS_V3_ROUTER = 0x13f4EA83D0bd40E75C8222255bc855a974568Dd4;

    /// @dev PancakeSwap V3 QuoterV2 on BSC mainnet. Used off-chain by the
    ///      bot to size `minSwapOut`; tests call it the same way to derive
    ///      a realistic slippage floor instead of hard-coding 0 (#267).
    address internal constant PCS_V3_QUOTER = 0xB048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997;

    /// @dev Multicall3 — deployed at the canonical 0xcA11...a11 address
    ///      on every major chain including BSC. Used as the archive-probe
    ///      target because it has had code since block ~15,921,452 and is
    ///      therefore guaranteed present at any `FORK_BLOCK` in the
    ///      suite's supported range.
    address internal constant MULTICALL3 = 0xcA11bde05977b3631167028862bE2a173976CA11;

    // ERC-20 underlyings. BUSD is intentionally absent — its Aave V3
    // reserve on BSC has been deactivated, so `flashLoanSimple(BUSD,…)`
    // reverts with `ReserveInactive()` regardless of contract logic.
    address internal constant USDT = 0x55d398326f99059fF775485246999027B3197955;
    address internal constant USDC = 0x8AC76a51cc950d9822D68b83fE1Ad97B32Cd580d;
    address internal constant BTCB = 0x7130d2A12B9BCbFAe4f2634d864A1Ee1Ce3Ead9c;
    address internal constant ETH = 0x2170Ed0880ac9A755fd29B2688956BD959F933F8;

    // Venus vToken (Core Pool) addresses. Source:
    // docs.venus.io/deployed-contracts/core-pool.
    address internal constant VUSDT = 0xfD5840Cd36d94D7229439859C0112a4185BC0255;
    address internal constant VUSDC = 0xecA88125a5ADbe82614ffC12D0DB554E2e2867C8;
    address internal constant VBTCB = 0x882C173bC7Ff3b7786CA16dfeD3DFFfb9Ee7847B;
    address internal constant VETH = 0xf508FCbf22e32A23f43eCdD1F7A8eaA15A5cCD63;

    // ─── Test state ───────────────────────────────────────────────────────
    CharonLiquidator internal liquidator;
    address internal owner;
    address internal borrower;

    /// @dev Per-market gas ceiling used by `_assertGasWithin`. Set to
    ///      roughly 1.25x observed usage on the pinned fork — tight
    ///      enough to catch regressions, loose enough to absorb normal
    ///      storage-warm variation across forks. Bump with a rationale
    ///      when the observed gas legitimately grows.
    uint256 internal constant GAS_CEILING_SINGLE = 650_000;
    /// @dev Batch gas ceiling for the 4-market `batchExecute` path.
    ///      ~1.25x of 4 * single-market average.
    uint256 internal constant GAS_CEILING_BATCH_UNIQUE = 2_400_000;

    // ─── Market tuple ─────────────────────────────────────────────────────
    struct Market {
        string name;
        address debtToken;
        address collateralToken;
        address debtVToken;
        address collateralVToken;
        uint256 repayAmount;
        uint256 seizedUnderlying;
    }

    function _marketUsdtUsdc() internal pure returns (Market memory) {
        // Stablecoin debt, stablecoin collateral — tightest price
        // correlation, used as the lower-bound sanity case.
        return Market({
            name: "USDT debt / USDC collateral",
            debtToken: USDT,
            collateralToken: USDC,
            debtVToken: VUSDT,
            collateralVToken: VUSDC,
            repayAmount: 1_000e18,
            seizedUnderlying: 1_080e18
        });
    }

    function _marketUsdtBtcb() internal pure returns (Market memory) {
        // Stablecoin debt, BTCB collateral — mixed-asset case, larger
        // collateral-bonus headroom.
        return Market({
            name: "USDT debt / BTCB collateral",
            debtToken: USDT,
            collateralToken: BTCB,
            debtVToken: VUSDT,
            collateralVToken: VBTCB,
            repayAmount: 500e18,
            seizedUnderlying: 2e16
        });
    }

    function _marketUsdcBtcb() internal pure returns (Market memory) {
        // USDC debt / BTCB collateral — second stablecoin-debt path
        // against volatile collateral; complements the USDT/BTCB case
        // by swapping the debt-side stablecoin.
        return Market({
            name: "USDC debt / BTCB collateral",
            debtToken: USDC,
            collateralToken: BTCB,
            debtVToken: VUSDC,
            collateralVToken: VBTCB,
            repayAmount: 750e18,
            seizedUnderlying: 3e16
        });
    }

    function _marketUsdtEth() internal pure returns (Market memory) {
        // ETH debt path — non-stable debt underlying. Seized collateral
        // sized generously so the real on-fork swap covers repay + premium
        // without depending on prevailing price.
        return Market({
            name: "USDT debt / ETH collateral",
            debtToken: USDT,
            collateralToken: ETH,
            debtVToken: VUSDT,
            collateralVToken: VETH,
            repayAmount: 2_000e18,
            seizedUnderlying: 2e18
        });
    }

    function _marketBtcbUsdt() internal pure returns (Market memory) {
        // Volatile debt (BTCB) against stablecoin collateral — reversed
        // from the common case, catches direction-symmetry bugs.
        return Market({
            name: "BTCB debt / USDT collateral",
            debtToken: BTCB,
            collateralToken: USDT,
            debtVToken: VBTCB,
            collateralVToken: VUSDT,
            repayAmount: 1e15,
            seizedUnderlying: 200e18
        });
    }

    function setUp() public {
        // `bnb` is aliased to `${BNB_HTTP_URL}` in `contracts/foundry.toml`.
        // Fork pinned to `FORK_BLOCK` for deterministic state; operators
        // can override per-invocation with `BSC_FORK_BLOCK=<number> forge
        // test` when investigating a regression against a different
        // height (no value = use the pin).
        uint256 forkBlock = vm.envOr("BSC_FORK_BLOCK", FORK_BLOCK);
        vm.createSelectFork("bnb", forkBlock);

        // Archive probe (#269). Non-archive endpoints (PublicNode,
        // ankr-rate-limited, etc.) cannot serve `eth_getCode` at a
        // historical block and the whole suite would produce
        // misleading failures. We probe Multicall3 at `FORK_BLOCK -
        // ARCHIVE_PROBE_OFFSET`; if the endpoint cannot return its
        // bytecode, mark every test in this contract skipped with a
        // clear operator-facing reason. Use a fresh fork handle so
        // the probe does not leave the selected block mutated.
        uint256 probeBlock =
            forkBlock > ARCHIVE_PROBE_OFFSET ? forkBlock - ARCHIVE_PROBE_OFFSET : forkBlock;
        uint256 probeFork = vm.createFork("bnb", probeBlock);
        vm.selectFork(probeFork);
        uint256 codeLen = MULTICALL3.code.length;
        // Return to the pinned fork regardless of probe outcome.
        vm.createSelectFork("bnb", forkBlock);
        if (codeLen == 0) {
            vm.skip(
                true,
                "BNB_HTTP_URL endpoint is not archive (historical state not served) - skipping fork tests; set BNB_HTTP_URL to a real archive RPC"
            );
            return;
        }

        owner = address(this);
        borrower = makeAddr("borrower");
        liquidator = new CharonLiquidator(AAVE_V3_POOL, PCS_V3_ROUTER);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Helpers
    // ─────────────────────────────────────────────────────────────────────

    /// @dev Mocks only the Venus side of the flow and seeds the collateral
    ///      balance that a real `redeem` would have produced. PancakeSwap
    ///      V3 is intentionally NOT mocked so the swap path, slippage
    ///      floor and real pool liquidity are all exercised (#266).
    function _mockVenusAndSeedCollateral(Market memory m) internal {
        // Mock Venus `liquidateBorrow` — return success.
        vm.mockCall(
            m.debtVToken,
            abi.encodeWithSelector(IVToken.liquidateBorrow.selector),
            abi.encode(uint256(0))
        );

        // Mock `vToken.balanceOf(liquidator)` — contract requires > 0
        // to proceed to redeem. Concrete value is irrelevant because
        // `redeem` is mocked too.
        vm.mockCall(
            m.collateralVToken,
            abi.encodeWithSelector(IVToken.balanceOf.selector, address(liquidator)),
            abi.encode(uint256(1))
        );

        // Mock `vToken.redeem` — return success. Real state would
        // credit underlying to the liquidator; we do that manually via
        // `deal` immediately below so the post-redeem balance read
        // sees a non-zero collateral amount.
        vm.mockCall(
            m.collateralVToken,
            abi.encodeWithSelector(IVToken.redeem.selector),
            abi.encode(uint256(0))
        );

        // Seed the collateral underlying that a real `redeem` would
        // have produced.
        deal(m.collateralToken, address(liquidator), m.seizedUnderlying);
    }

    /// @dev Calls the real PCS V3 Quoter V2 to get the amountOut that
    ///      the upcoming swap is expected to produce. Returns a value
    ///      scaled down by `slippageBps` so `minSwapOut` stays below the
    ///      live quote but is still a meaningful floor (#267).
    ///
    ///      The Quoter V2 reverts the simulated swap internally and
    ///      returns the computed amountOut; it is safe to invoke without
    ///      moving state.
    function _minOutFromQuoter(Market memory m, uint256 slippageBps)
        internal
        returns (uint256 quoted, uint256 minOut)
    {
        IPcsQuoterV2.QuoteExactInputSingleParams memory q =
            IPcsQuoterV2.QuoteExactInputSingleParams({
                tokenIn: m.collateralToken,
                tokenOut: m.debtToken,
                amountIn: m.seizedUnderlying,
                fee: 3000,
                sqrtPriceLimitX96: 0
            });
        (quoted,,,) = IPcsQuoterV2(PCS_V3_QUOTER).quoteExactInputSingle(q);
        // 50bps slippage by default: minOut = quoted * (10_000 - bps) / 10_000.
        minOut = (quoted * (10_000 - slippageBps)) / 10_000;
    }

    function _params(Market memory m, uint256 minSwapOut)
        internal
        view
        returns (CharonLiquidator.LiquidationParams memory)
    {
        return CharonLiquidator.LiquidationParams({
            protocolId: 3, // PROTOCOL_VENUS
            borrower: borrower,
            debtToken: m.debtToken,
            collateralToken: m.collateralToken,
            debtVToken: m.debtVToken,
            collateralVToken: m.collateralVToken,
            repayAmount: m.repayAmount,
            minSwapOut: minSwapOut
        });
    }

    /// @dev Runs one liquidation against a freshly-redeployed contract,
    ///      asserts the expected `LiquidationExecuted` event fires with
    ///      compiler-checked selector matching (#273), enforces the
    ///      per-market gas ceiling (#274), and verifies owner balance
    ///      grew (legacy invariant — the #265 cold-wallet assertion is
    ///      blocked on the `COLD_WALLET` constructor-arg landing and
    ///      will be re-tightened when that change reaches this branch).
    function _executeAndAssert(Market memory m) internal {
        // Redeploy so every test starts with a clean liquidator.
        liquidator = new CharonLiquidator(AAVE_V3_POOL, PCS_V3_ROUTER);
        _mockVenusAndSeedCollateral(m);

        (, uint256 minOut) = _minOutFromQuoter(m, 50); // 50bps floor

        uint256 ownerBalBefore = IERC20(m.debtToken).balanceOf(owner);

        // Typed event expect: the signature is compiler-checked, so
        // renaming the event breaks the test at compile time rather than
        // failing a runtime keccak comparison (#273). We match the two
        // indexed topics (borrower, debtToken) plus data; repayAmount is
        // deterministic, profit is not (depends on on-fork swap output),
        // so only topics are asserted strict.
        vm.expectEmit(true, true, false, false, address(liquidator));
        emit CharonLiquidator.LiquidationExecuted(borrower, m.debtToken, m.repayAmount, 0);

        uint256 gasBefore = gasleft();
        liquidator.executeLiquidation(_params(m, minOut));
        uint256 gasUsed = gasBefore - gasleft();

        emit log_named_string("market", m.name);
        emit log_named_uint("gas_used_liquidation", gasUsed);
        assertLt(gasUsed, GAS_CEILING_SINGLE, "liquidation gas regression");

        uint256 ownerBalAfter = IERC20(m.debtToken).balanceOf(owner);
        assertGt(ownerBalAfter, ownerBalBefore, "owner should have received profit");

        // Contract should end with a zero collateral-token balance — the
        // full seized amount was swapped, nothing should be left behind.
        assertEq(
            IERC20(m.collateralToken).balanceOf(address(liquidator)),
            0,
            "collateral dust left in liquidator"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // A. Per-market happy path (#272) — one test per market so a single
    //    pool or liquidity regression never masks the rest.
    // ─────────────────────────────────────────────────────────────────────

    function test_forkLiquidate_USDT_USDC() public {
        _executeAndAssert(_marketUsdtUsdc());
    }

    function test_forkLiquidate_USDT_BTCB() public {
        _executeAndAssert(_marketUsdtBtcb());
    }

    function test_forkLiquidate_USDC_BTCB() public {
        _executeAndAssert(_marketUsdcBtcb());
    }

    function test_forkLiquidate_USDT_ETH() public {
        _executeAndAssert(_marketUsdtEth());
    }

    function test_forkLiquidate_BTCB_USDT() public {
        _executeAndAssert(_marketBtcbUsdt());
    }

    // ─────────────────────────────────────────────────────────────────────
    // B. Slippage edge cases (#267)
    // ─────────────────────────────────────────────────────────────────────

    /// @dev Set `minSwapOut` one wei above the live Quoter V2 quote so
    ///      the real router rejects the swap. This exercises the
    ///      `amountOutMinimum` floor end-to-end; the generic
    ///      `"Too little received"` revert is the canonical PCS V3 (and
    ///      Uniswap V3) error.
    function test_forkSlippage_aboveQuoteReverts() public {
        Market memory m = _marketUsdtUsdc();
        _mockVenusAndSeedCollateral(m);
        (uint256 quoted,) = _minOutFromQuoter(m, 0);

        CharonLiquidator.LiquidationParams memory p = _params(m, quoted + 1);
        vm.expectRevert(bytes("Too little received"));
        liquidator.executeLiquidation(p);
    }

    /// @dev Defensive check on top of the router's amountOutMinimum
    ///      guard — if the post-swap debt-token balance is insufficient
    ///      to cover `amount + premium`, the contract must revert with
    ///      `"swap output below repayment"`. Achieved by forcing the
    ///      seeded collateral to zero so the real swap can only produce
    ///      zero tokenOut.
    function test_forkUnderRepayment_reverts() public {
        Market memory m = _marketUsdtUsdc();
        _mockVenusAndSeedCollateral(m);
        // Wipe the seeded collateral so exactInputSingle returns 0.
        deal(m.collateralToken, address(liquidator), 0);
        // With no collateral in hand, the zero-approval + zero-amount
        // swap path will still hit the router. Foundry-level deal does
        // not alter pool state; the real router's own balance check
        // may revert first with a reserve-related error. Accept either
        // the contract's defensive revert or any router-side revert by
        // using `vm.expectRevert()` with no selector, scoped narrowly.
        vm.expectRevert();
        liquidator.executeLiquidation(_params(m, 0));
    }

    // ─────────────────────────────────────────────────────────────────────
    // C. batchExecute happy path (#268)
    // ─────────────────────────────────────────────────────────────────────

    /// @dev Drives four markets with distinct collateral tokens through
    ///      `CharonLiquidator.batchExecute` in a single transaction.
    ///      The fifth per-market test (USDC/BTCB) shares its BTCB
    ///      collateral underlying with USDT/BTCB and is excluded from
    ///      the batch to prevent the first iteration's real swap from
    ///      draining the shared seeded balance before the second
    ///      iteration runs — see the contract-level doc comment for
    ///      the full rationale. Every market is still covered
    ///      individually by its own per-market test (#272).
    ///
    ///      Asserts:
    ///        - each market emits a `LiquidationExecuted` log (one per
    ///          iteration, matched on selector via the typed event
    ///          emitter — #273);
    ///        - a single terminal `BatchExecuted(n)` log fires;
    ///        - the owner balance grows in aggregate;
    ///        - every touched ERC-20 leaves the contract with a zero
    ///          balance (no collateral or debt-token dust);
    ///        - the full batch stays within the batch gas ceiling (#274).
    function test_forkBatchExecute_uniqueCollateralMarkets_happyPath() public {
        // Unique-collateral subset: USDC, BTCB, ETH, USDT.
        Market[] memory markets = new Market[](4);
        markets[0] = _marketUsdtUsdc();
        markets[1] = _marketUsdtBtcb();
        markets[2] = _marketUsdtEth();
        markets[3] = _marketBtcbUsdt();
        CharonLiquidator.LiquidationParams[] memory items =
            new CharonLiquidator.LiquidationParams[](markets.length);

        // Seed every market's Venus mocks and collateral balance up
        // front; batchExecute processes them sequentially in one tx.
        for (uint256 i = 0; i < markets.length; i++) {
            _mockVenusAndSeedCollateral(markets[i]);
            (, uint256 minOut) = _minOutFromQuoter(markets[i], 50);
            items[i] = _params(markets[i], minOut);
        }

        // Record all logs across the batch so we can count
        // LiquidationExecuted emissions and verify the single
        // BatchExecuted terminator (vm.expectEmit only matches one log
        // at a time, which is awkward for batched flows).
        vm.recordLogs();

        uint256 gasBefore = gasleft();
        liquidator.batchExecute(items);
        uint256 gasUsed = gasBefore - gasleft();

        emit log_named_uint("gas_used_batch_unique", gasUsed);
        assertLt(gasUsed, GAS_CEILING_BATCH_UNIQUE, "batch gas regression");

        bytes32 liquidationSelector = CharonLiquidator.LiquidationExecuted.selector;
        bytes32 batchSelector = CharonLiquidator.BatchExecuted.selector;

        Vm.Log[] memory logs = vm.getRecordedLogs();
        uint256 liquidationHits;
        uint256 batchHits;
        for (uint256 j = 0; j < logs.length; j++) {
            if (logs[j].emitter != address(liquidator)) continue;
            if (logs[j].topics.length == 0) continue;
            if (logs[j].topics[0] == liquidationSelector) {
                liquidationHits++;
            } else if (logs[j].topics[0] == batchSelector) {
                batchHits++;
                // data = abi.encode(n) where n = items.length
                uint256 emittedN = abi.decode(logs[j].data, (uint256));
                assertEq(emittedN, markets.length, "BatchExecuted count mismatch");
            }
        }
        assertEq(liquidationHits, markets.length, "one LiquidationExecuted per market expected");
        assertEq(batchHits, 1, "exactly one BatchExecuted expected");

        // Every touched ERC-20 must end at zero in the liquidator — no
        // dust in collateral and no leftover debt-token balance (owner
        // sweeps the profit, Aave pulls the repayment).
        assertEq(IERC20(USDT).balanceOf(address(liquidator)), 0, "USDT dust in liquidator");
        assertEq(IERC20(USDC).balanceOf(address(liquidator)), 0, "USDC dust in liquidator");
        assertEq(IERC20(BTCB).balanceOf(address(liquidator)), 0, "BTCB dust in liquidator");
        assertEq(IERC20(ETH).balanceOf(address(liquidator)), 0, "ETH dust in liquidator");
    }

    // ─────────────────────────────────────────────────────────────────────
    // D. Environment sanity
    // ─────────────────────────────────────────────────────────────────────

    /// @dev Fork-availability smoke. If the configured RPC doesn't
    ///      expose the pinned contracts, every other test in this file
    ///      is meaningless — surface that failure with a clear
    ///      message up front.
    function test_fork_realContractsHaveCode() public view {
        assertGt(AAVE_V3_POOL.code.length, 0, "Aave V3 pool has no code on fork");
        assertGt(PCS_V3_ROUTER.code.length, 0, "PancakeSwap V3 router has no code on fork");
        assertGt(PCS_V3_QUOTER.code.length, 0, "PancakeSwap V3 quoter has no code on fork");
        assertGt(USDT.code.length, 0, "USDT has no code on fork");
        assertGt(VUSDT.code.length, 0, "vUSDT has no code on fork");
    }
}
