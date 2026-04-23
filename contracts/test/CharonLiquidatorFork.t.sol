// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import "forge-std/Test.sol";

import { CharonLiquidator } from "../src/CharonLiquidator.sol";
import { IVToken } from "../src/interfaces/IVToken.sol";
import { ISwapRouter } from "../src/interfaces/ISwapRouter.sol";
import { IERC20 } from "../src/interfaces/IERC20.sol";

/// @title CharonLiquidatorForkTest
/// @notice Integration tests for `CharonLiquidator` against a BNB Smart
///         Chain mainnet fork.
///
///         The single piece of integration these tests really exercise
///         is the Aave V3 flash-loan callback path — fork infrastructure
///         is the only environment where the real
///         `Pool.flashLoanSimple` (proxy → pool → aToken.transfer →
///         `executeOperation` → `transferFrom` via approval) can run
///         unmodified. Venus and PancakeSwap are mocked so the tests
///         don't depend on locating an underwater borrower at a
///         specific historical block — a deterministic exercise that
///         would need weeks of archive-grep to keep current.
///
///         The mock strategy:
///         - `IVToken.liquidateBorrow`, `balanceOf`, `redeem` — mocked.
///           No real Venus state is touched.
///         - Collateral underlying — `vm.deal` seeds the liquidator
///           with the amount that a real `redeem` would have produced.
///         - `ISwapRouter.exactInputSingle` — mocked to return a fixed
///           amountOut. The contract ignores the return value, so the
///           mock only has to succeed; post-swap balance comes from a
///           pre-seeded deal of the debt token.
///
///         Fork block is unpinned (`vm.createSelectFork("bnb")`) so the
///         suite runs against whatever head the operator's archive RPC
///         exposes. Pinning can be reintroduced via `BSC_FORK_BLOCK`
///         if reproducibility against a specific Aave state version
///         becomes important.
contract CharonLiquidatorForkTest is Test {
    // ─── BSC mainnet addresses ────────────────────────────────────────────
    // Aave V3 Pool proxy. Same address used in `config/default.toml`.
    address internal constant AAVE_V3_POOL = 0x6807dc923806fE8Fd134338EABCA509979a7e0cB;

    // PancakeSwap V3 SmartRouter on BSC mainnet. Source:
    // github.com/pancakeswap/pancake-v3-contracts/deployments
    address internal constant PCS_V3_ROUTER = 0x13f4EA83D0bd40E75C8222255bc855a974568Dd4;

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

    function _markets() internal pure returns (Market[] memory m) {
        m = new Market[](5);
        // Stablecoin debt, stablecoin collateral — tightest price
        // correlation, used as the lower-bound sanity case.
        m[0] = Market({
            name: "USDT debt / USDC collateral",
            debtToken: USDT,
            collateralToken: USDC,
            debtVToken: VUSDT,
            collateralVToken: VUSDC,
            repayAmount: 1_000e18,
            seizedUnderlying: 1_080e18
        });
        // Stablecoin debt, BTCB collateral — mixed-asset case, larger
        // collateral-bonus headroom.
        m[1] = Market({
            name: "USDT debt / BTCB collateral",
            debtToken: USDT,
            collateralToken: BTCB,
            debtVToken: VUSDT,
            collateralVToken: VBTCB,
            repayAmount: 500e18,
            seizedUnderlying: 1e16
        });
        // USDC debt / BTCB collateral — second stablecoin-debt path
        // against volatile collateral; complements market 1 by
        // swapping the debt-side stablecoin.
        m[2] = Market({
            name: "USDC debt / BTCB collateral",
            debtToken: USDC,
            collateralToken: BTCB,
            debtVToken: VUSDC,
            collateralVToken: VBTCB,
            repayAmount: 750e18,
            seizedUnderlying: 15e15
        });
        // ETH debt path — non-stable debt underlying.
        m[3] = Market({
            name: "USDT debt / ETH collateral",
            debtToken: USDT,
            collateralToken: ETH,
            debtVToken: VUSDT,
            collateralVToken: VETH,
            repayAmount: 2_000e18,
            seizedUnderlying: 6e17
        });
        // Volatile debt (BTCB) against stablecoin collateral — reversed
        // from the common case, catches direction-symmetry bugs.
        m[4] = Market({
            name: "BTCB debt / USDT collateral",
            debtToken: BTCB,
            collateralToken: USDT,
            debtVToken: VBTCB,
            collateralVToken: VUSDT,
            repayAmount: 1e15,
            seizedUnderlying: 120e18
        });
    }

    function setUp() public {
        // `bnb` is aliased to `${BNB_HTTP_URL}` in `contracts/foundry.toml`.
        vm.createSelectFork("bnb");

        owner = address(this);
        borrower = makeAddr("borrower");
        liquidator = new CharonLiquidator(AAVE_V3_POOL, PCS_V3_ROUTER);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Helpers
    // ─────────────────────────────────────────────────────────────────────

    /// @dev Sets up the mocks and balances so one liquidation round-trip
    ///      completes successfully against real Aave state. Aave
    ///      transfers `repayAmount` debt-token into the liquidator as a
    ///      normal part of `flashLoanSimple`; this helper seeds the
    ///      *extra* balance needed to cover the premium and a small
    ///      profit margin on top.
    function _mockVenusAndPcs(Market memory m, uint256 profitSurplus) internal {
        // Aave premium + profit buffer must exist in the liquidator's
        // debt-token balance before Aave's post-callback pull-back.
        // Compute a conservative premium (0.05% is the Aave V3 default
        // on BSC) plus the desired surplus.
        uint256 premium = (m.repayAmount * 5) / 10_000;
        uint256 surplus = premium + profitSurplus;
        deal(m.debtToken, address(liquidator), surplus);

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

        // Mock PancakeSwap V3 `exactInputSingle` — the contract does
        // not read the return value, so we only need the call to
        // succeed. Real `swap → receive debtToken` is simulated by the
        // `deal(debtToken, ...)` above plus the Aave transfer that the
        // real Pool performs during `flashLoanSimple`.
        vm.mockCall(
            PCS_V3_ROUTER,
            abi.encodeWithSelector(ISwapRouter.exactInputSingle.selector),
            abi.encode(uint256(0))
        );
    }

    function _params(Market memory m) internal view returns (CharonLiquidator.LiquidationParams memory) {
        return CharonLiquidator.LiquidationParams({
            protocolId: 3, // PROTOCOL_VENUS
            borrower: borrower,
            debtToken: m.debtToken,
            collateralToken: m.collateralToken,
            debtVToken: m.debtVToken,
            collateralVToken: m.collateralVToken,
            repayAmount: m.repayAmount,
            minSwapOut: 0 // loose gate — the post-swap balance check is the real safety net
        });
    }

    // ─────────────────────────────────────────────────────────────────────
    // A. Parametric happy-path across five Venus markets
    // ─────────────────────────────────────────────────────────────────────

    /// @dev Iterates the market table and asserts a clean round-trip for
    ///      each pair. A single `test_*` wrapper keeps the output terse;
    ///      failure messages identify the offending market by name.
    function test_forkHappyPath_acrossAllMarkets() public {
        Market[] memory markets = _markets();
        for (uint256 i = 0; i < markets.length; i++) {
            _assertHappyPath(markets[i]);
        }
    }

    function _assertHappyPath(Market memory m) internal {
        // Redeploy so every iteration starts with a clean liquidator.
        liquidator = new CharonLiquidator(AAVE_V3_POOL, PCS_V3_ROUTER);

        // 10 USD worth of surplus in debtToken units — enough to cover
        // the Aave premium and leave a small profit, small enough that
        // dust tokens like BTCB (18-dec 1e-10 smallest unit) don't
        // overflow the seeded balance.
        _mockVenusAndPcs(m, 10e18);

        uint256 ownerBalBefore = IERC20(m.debtToken).balanceOf(owner);

        // Start log recording so we can assert `LiquidationExecuted`
        // fires with matching topic1 (borrower) and topic2 (debtToken).
        // `vm.expectEmit` is brittle in a loop where other logs fire
        // between setup and the emit we care about; `recordLogs` lets
        // us filter after the fact without ordering assumptions.
        vm.recordLogs();
        liquidator.executeLiquidation(_params(m));

        bytes32 expectedSelector = keccak256(
            "LiquidationExecuted(address,address,uint256,uint256)"
        );
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bool found = false;
        for (uint256 j = 0; j < logs.length; j++) {
            if (
                logs[j].emitter == address(liquidator)
                    && logs[j].topics.length == 3
                    && logs[j].topics[0] == expectedSelector
                    && logs[j].topics[1] == bytes32(uint256(uint160(borrower)))
                    && logs[j].topics[2] == bytes32(uint256(uint160(m.debtToken)))
            ) {
                found = true;
                break;
            }
        }
        assertTrue(found, string.concat(m.name, ": LiquidationExecuted not emitted"));

        uint256 ownerBalAfter = IERC20(m.debtToken).balanceOf(owner);
        assertGt(
            ownerBalAfter,
            ownerBalBefore,
            string.concat(m.name, ": owner should have received profit")
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // B. Slippage edge cases
    // ─────────────────────────────────────────────────────────────────────

    /// @dev `minSwapOut` above what the (mocked) router produces must
    ///      revert from the router itself. We simulate that by mocking
    ///      the router call to revert with PancakeSwap's canonical
    ///      `"Too little received"` message.
    function test_fork_slippage_tooTight_reverts() public {
        Market memory m = _markets()[0];
        _mockVenusAndPcs(m, 10e18);

        // Override the router mock with a revert so the slippage path
        // is exercised end-to-end.
        vm.mockCallRevert(
            PCS_V3_ROUTER,
            abi.encodeWithSelector(ISwapRouter.exactInputSingle.selector),
            bytes("Too little received")
        );

        CharonLiquidator.LiquidationParams memory p = _params(m);
        p.minSwapOut = type(uint256).max; // any real router would revert

        vm.expectRevert(bytes("Too little received"));
        liquidator.executeLiquidation(p);
    }

    /// @dev If the swap "succeeds" (mocked to no-op) but the post-swap
    ///      debt-token balance falls short of `amount + premium`, the
    ///      contract's defensive check must revert. Achieved by seeding
    ///      only part of the premium.
    function test_fork_underRepayment_reverts() public {
        Market memory m = _markets()[0];
        // Seed ZERO surplus — the debt-token balance after Aave's
        // transfer is exactly `amount`, which is < `amount + premium`.
        _mockVenusAndPcs(m, 0);
        deal(m.debtToken, address(liquidator), 0); // undo the helper's seeding

        vm.expectRevert(bytes("swap output below repayment"));
        liquidator.executeLiquidation(_params(m));
    }

    // ─────────────────────────────────────────────────────────────────────
    // C. Environment sanity
    // ─────────────────────────────────────────────────────────────────────

    /// @dev Fork-availability smoke. If the configured RPC doesn't
    ///      expose the pinned contracts, every other test in this file
    ///      is meaningless — surface that failure with a clear
    ///      message up front.
    function test_fork_realContractsHaveCode() public view {
        assertGt(AAVE_V3_POOL.code.length, 0, "Aave V3 pool has no code on fork");
        assertGt(PCS_V3_ROUTER.code.length, 0, "PancakeSwap V3 router has no code on fork");
        assertGt(USDT.code.length, 0, "USDT has no code on fork");
        assertGt(VUSDT.code.length, 0, "vUSDT has no code on fork");
    }
}
