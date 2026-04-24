// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import { Test } from "forge-std/Test.sol";
import { CharonLiquidator } from "../src/CharonLiquidator.sol";
import { IVToken } from "../src/interfaces/IVToken.sol";
import { IWETH } from "../src/interfaces/IWETH.sol";
import { IERC20 } from "../src/interfaces/IERC20.sol";
import { ISwapRouter } from "../src/interfaces/ISwapRouter.sol";
import { IAaveV3Pool } from "../src/interfaces/IAaveV3Pool.sol";

/// @title CharonLiquidatorForkTest
/// @notice Fork-backed tests for CharonLiquidator against BSC mainnet state.
/// @dev Tests gate on BNB_RPC_URL via vm.skip() — CI without the env var skips
///      cleanly rather than failing. Where a live liquidation is too invasive
///      to stage on a fresh fork, vm.mockCall is used to exercise the target
///      code path without conjuring a real under-water borrower.
///
///      Target contract API (main):
///        constructor(address _aavePool, address _swapRouter, address _coldWallet)
///        owner  := msg.sender at construction
///        COLD_WALLET, AAVE_POOL, SWAP_ROUTER are public immutable.
///        LiquidationParams includes `swapPoolFee` (uint24) per-opportunity.
///        vBNB collateral branch: redeem(vBal) + wrap native BNB via IWETH.deposit.
///        Profit sweep: routed to COLD_WALLET, never owner.
contract CharonLiquidatorForkTest is Test {
    // ── Live BSC mainnet addresses ────────────────────────────────────────
    /// @dev Aave V3 Pool proxy on BSC. Mirrors config/default.toml `pool`.
    address internal constant AAVE_V3_POOL_BSC = 0x6807dc923806fE8Fd134338EABCA509979a7e0cB;
    /// @dev PancakeSwap V3 SwapRouter on BSC.
    address internal constant PCS_V3_ROUTER_BSC = 0x13f4EA83D0bd40E75C8222255bc855a974568Dd4;
    /// @dev Venus vBNB market on BSC.
    address internal constant VBNB_BSC = 0xA07c5b74C9B40447a954e1466938b865b6BBea36;
    /// @dev Canonical WBNB on BSC.
    address internal constant WBNB_BSC = 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c;

    /// @dev Sentinel cold-wallet address distinct from the deployer/owner.
    address internal constant COLD_WALLET = address(0xC01D);

    /// @dev Sentinel debt/collateral vToken + token pair used to drive the
    ///      non-vBNB happy path in mocked liquidations. The actual addresses
    ///      do not need to correspond to a real Venus market because every
    ///      external call into them is intercepted via vm.mockCall.
    address internal constant MOCK_DEBT_VTOKEN = address(0xD00D);
    address internal constant MOCK_DEBT_TOKEN = address(0xDEB7);
    address internal constant MOCK_COLL_VTOKEN = address(0xC077);
    address internal constant MOCK_COLL_TOKEN = address(0xC011);
    address internal constant MOCK_BORROWER = address(0xBEEF);

    CharonLiquidator internal liquidator;

    /// @dev Per-test gate. A single helper used by every test that must only
    ///      run when a BSC RPC is available. vm.skip(true) short-circuits the
    ///      rest of the test body without marking the suite failed.
    function _skipIfNoRpc() internal {
        if (bytes(vm.envOr("BNB_RPC_URL", string(""))).length == 0) {
            vm.skip(true);
        }
    }

    function setUp() public {
        // If the operator has not provided a BSC RPC URL, leave `liquidator`
        // zero-initialised. Each test re-checks via _skipIfNoRpc() before
        // interacting with the contract. This keeps the suite green in CI
        // environments without a fork endpoint.
        string memory rpc = vm.envOr("BNB_RPC_URL", string(""));
        if (bytes(rpc).length == 0) {
            return;
        }

        // Optional pin for deterministic fork tests. Absent → latest block.
        uint256 pin = vm.envOr("BNB_FORK_BLOCK", uint256(0));
        if (pin == 0) {
            vm.createSelectFork(rpc);
        } else {
            vm.createSelectFork(rpc, pin);
        }

        // address(this) is the hot-wallet owner — matches production wiring
        // where the deploying bot key is the owner.
        liquidator = new CharonLiquidator(AAVE_V3_POOL_BSC, PCS_V3_ROUTER_BSC, COLD_WALLET);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Constructor / immutable wiring
    // ─────────────────────────────────────────────────────────────────────

    /// @notice Sanity-check that ctor wires every immutable and that owner
    ///         resolves to the deployer (address(this)).
    function test_constructor_wires_immutables() public {
        _skipIfNoRpc();

        assertEq(liquidator.owner(), address(this), "owner != deployer");
        assertEq(liquidator.COLD_WALLET(), COLD_WALLET, "COLD_WALLET mismatch");
        assertEq(liquidator.AAVE_POOL(), AAVE_V3_POOL_BSC, "AAVE_POOL mismatch");
        assertEq(liquidator.SWAP_ROUTER(), PCS_V3_ROUTER_BSC, "SWAP_ROUTER mismatch");
    }

    // ─────────────────────────────────────────────────────────────────────
    // Access control
    // ─────────────────────────────────────────────────────────────────────

    /// @notice rescue() is onlyOwner. Non-owner caller must revert; owner
    ///         call against a zero-value sentinel reverts for a different
    ///         reason (!to) — we only assert the ACL gate here.
    function test_rescue_onlyOwner() public {
        _skipIfNoRpc();

        address attacker = address(0xBAD);
        vm.prank(attacker);
        vm.expectRevert(bytes("!owner"));
        liquidator.rescue(address(0), address(0x1), 1);

        // Owner call surfaces the input validator — proves we passed the
        // ACL gate even though the call itself reverts on argument checks.
        vm.expectRevert(bytes("!to"));
        liquidator.rescue(address(0), address(0), 1);
    }

    /// @notice executeOperation must reject any sender that is not the Aave
    ///         pool. This guards the flash-loan callback against forged
    ///         invocation by an unrelated contract.
    function test_executeOperation_rejectsNonAavePool() public {
        _skipIfNoRpc();

        // Minimally-valid calldata shape; contents are irrelevant because
        // the !pool guard fires before any decoding.
        bytes memory data = "";
        vm.prank(address(0xDEAD));
        vm.expectRevert(bytes("!pool"));
        liquidator.executeOperation(MOCK_DEBT_TOKEN, 1, 0, address(liquidator), data);
    }

    // ─────────────────────────────────────────────────────────────────────
    // vBNB unwrap branch
    // ─────────────────────────────────────────────────────────────────────

    /// @notice Exercises the vBNB branch end-to-end through executeOperation
    ///         using vm.mockCall to stub Venus + PancakeSwap. Confirms that
    ///         when the seized vToken is vBNB the contract:
    ///           1. Calls IVToken.redeem on vBNB.
    ///           2. Invokes IWETH.deposit with the contract's native balance.
    ///           3. Swaps the WBNB-denominated collateral and repays Aave.
    ///
    /// @dev Real liquidation would require staging an under-water Venus
    ///      position on the forked state. That is deliberately out of scope
    ///      for this unit test — the intent is to prove the vBNB code path
    ///      is reached and the unwrap step is invoked.
    function test_liquidate_vBNB_unwraps_to_wbnb() public {
        _skipIfNoRpc();

        uint256 repay = 1_000 ether;
        uint256 premium = 5 ether;
        uint256 seizedVTokens = 42 ether;
        uint256 nativeRedeemed = 10 ether;
        uint256 swapOut = repay + premium + 1; // leave 1 wei profit

        // Stub Venus debt-vToken: liquidateBorrow succeeds.
        vm.mockCall(
            MOCK_DEBT_VTOKEN,
            abi.encodeWithSelector(IVToken.liquidateBorrow.selector, MOCK_BORROWER, repay, VBNB_BSC),
            abi.encode(uint256(0))
        );
        // Stub seized-vToken balance on contract.
        vm.mockCall(
            VBNB_BSC,
            abi.encodeWithSelector(IVToken.balanceOf.selector, address(liquidator)),
            abi.encode(seizedVTokens)
        );
        // Stub vBNB.redeem → 0 success. Venus sends native BNB out-of-band;
        // we credit the liquidator's native balance via vm.deal below.
        vm.mockCall(
            VBNB_BSC,
            abi.encodeWithSelector(IVToken.redeem.selector, seizedVTokens),
            abi.encode(uint256(0))
        );
        vm.deal(address(liquidator), nativeRedeemed);

        // Stub WBNB.deposit — mockCall returns without touching the native
        // balance. The post-wrap balance check below is also mocked, so the
        // real deposit semantics don't matter for the assertion.
        vm.mockCall(WBNB_BSC, abi.encodeWithSelector(IWETH.deposit.selector), bytes(""));
        // WBNB.balanceOf(liquidator) → post-wrap collateral balance.
        vm.mockCall(
            WBNB_BSC,
            abi.encodeWithSelector(IERC20.balanceOf.selector, address(liquidator)),
            abi.encode(nativeRedeemed)
        );
        // Every ERC-20 approve() call (debt vToken, swap router, aave pool,
        // WBNB router approve) returns true regardless of target.
        vm.mockCall(
            MOCK_DEBT_TOKEN, abi.encodeWithSelector(IERC20.approve.selector), abi.encode(true)
        );
        vm.mockCall(WBNB_BSC, abi.encodeWithSelector(IERC20.approve.selector), abi.encode(true));
        // PancakeSwap V3 router: swap returns swapOut.
        vm.mockCall(
            PCS_V3_ROUTER_BSC,
            abi.encodeWithSelector(ISwapRouter.exactInputSingle.selector),
            abi.encode(swapOut)
        );
        // Debt-token balance after swap → swapOut, so profit = 1 wei.
        vm.mockCall(
            MOCK_DEBT_TOKEN,
            abi.encodeWithSelector(IERC20.balanceOf.selector, address(liquidator)),
            abi.encode(swapOut)
        );
        // Profit sweep transfer → succeeds.
        vm.mockCall(
            MOCK_DEBT_TOKEN, abi.encodeWithSelector(IERC20.transfer.selector), abi.encode(true)
        );

        CharonLiquidator.LiquidationParams memory p = CharonLiquidator.LiquidationParams({
            protocolId: 3, // PROTOCOL_VENUS
            borrower: MOCK_BORROWER,
            debtToken: MOCK_DEBT_TOKEN,
            collateralToken: WBNB_BSC, // vBNB branch requires WBNB
            debtVToken: MOCK_DEBT_VTOKEN,
            collateralVToken: VBNB_BSC,
            repayAmount: repay,
            minSwapOut: repay + premium,
            swapPoolFee: 500
        });

        // Expect vBNB.redeem to be invoked — this is the load-bearing assert
        // that the vBNB branch was entered rather than the standard one.
        vm.expectCall(VBNB_BSC, abi.encodeWithSelector(IVToken.redeem.selector, seizedVTokens));
        // Expect IWETH.deposit to be called, proving the native-to-WBNB
        // wrap step executed.
        vm.expectCall(WBNB_BSC, abi.encodeWithSelector(IWETH.deposit.selector));

        vm.prank(AAVE_V3_POOL_BSC);
        bool ok = liquidator.executeOperation(
            MOCK_DEBT_TOKEN, repay, premium, address(liquidator), abi.encode(p)
        );
        assertTrue(ok, "executeOperation returned false");
    }

    // ─────────────────────────────────────────────────────────────────────
    // Profit sweep to COLD_WALLET
    // ─────────────────────────────────────────────────────────────────────

    /// @notice After a mocked-happy-path liquidation (non-vBNB branch),
    ///         confirm that profit transfer is routed to COLD_WALLET, not
    ///         owner. The load-bearing assertion is the vm.expectCall on
    ///         IERC20.transfer(COLD_WALLET, profit).
    function test_profit_sweeps_to_cold_wallet() public {
        _skipIfNoRpc();

        uint256 repay = 1_000 ether;
        uint256 premium = 5 ether;
        uint256 seizedVTokens = 50 ether;
        uint256 collUnderlying = 2_000 ether;
        uint256 swapOut = repay + premium + 7 ether; // profit = 7 ether
        uint256 expectedProfit = swapOut - (repay + premium);

        // Debt vToken: liquidateBorrow succeeds.
        vm.mockCall(
            MOCK_DEBT_VTOKEN,
            abi.encodeWithSelector(IVToken.liquidateBorrow.selector),
            abi.encode(uint256(0))
        );
        // Collateral vToken: balanceOf + redeem.
        vm.mockCall(
            MOCK_COLL_VTOKEN,
            abi.encodeWithSelector(IVToken.balanceOf.selector, address(liquidator)),
            abi.encode(seizedVTokens)
        );
        vm.mockCall(
            MOCK_COLL_VTOKEN,
            abi.encodeWithSelector(IVToken.redeem.selector, seizedVTokens),
            abi.encode(uint256(0))
        );
        // Collateral underlying: balanceOf used both for approve amount and
        // post-redeem balance read. Approve returns true.
        vm.mockCall(
            MOCK_COLL_TOKEN,
            abi.encodeWithSelector(IERC20.balanceOf.selector, address(liquidator)),
            abi.encode(collUnderlying)
        );
        vm.mockCall(
            MOCK_COLL_TOKEN, abi.encodeWithSelector(IERC20.approve.selector), abi.encode(true)
        );
        vm.mockCall(
            MOCK_DEBT_TOKEN, abi.encodeWithSelector(IERC20.approve.selector), abi.encode(true)
        );
        // PancakeSwap: returns swapOut.
        vm.mockCall(
            PCS_V3_ROUTER_BSC,
            abi.encodeWithSelector(ISwapRouter.exactInputSingle.selector),
            abi.encode(swapOut)
        );
        // Debt token post-swap balance == swapOut (covers totalOwed + profit).
        vm.mockCall(
            MOCK_DEBT_TOKEN,
            abi.encodeWithSelector(IERC20.balanceOf.selector, address(liquidator)),
            abi.encode(swapOut)
        );
        // Debt token transfer(COLD_WALLET, profit) — returns true.
        vm.mockCall(
            MOCK_DEBT_TOKEN,
            abi.encodeWithSelector(IERC20.transfer.selector, COLD_WALLET, expectedProfit),
            abi.encode(true)
        );

        CharonLiquidator.LiquidationParams memory p = CharonLiquidator.LiquidationParams({
            protocolId: 3,
            borrower: MOCK_BORROWER,
            debtToken: MOCK_DEBT_TOKEN,
            collateralToken: MOCK_COLL_TOKEN,
            debtVToken: MOCK_DEBT_VTOKEN,
            collateralVToken: MOCK_COLL_VTOKEN,
            repayAmount: repay,
            minSwapOut: repay + premium,
            swapPoolFee: 3000
        });

        // Load-bearing assertion: profit goes to COLD_WALLET specifically.
        vm.expectCall(
            MOCK_DEBT_TOKEN,
            abi.encodeWithSelector(IERC20.transfer.selector, COLD_WALLET, expectedProfit)
        );

        vm.prank(AAVE_V3_POOL_BSC);
        bool ok = liquidator.executeOperation(
            MOCK_DEBT_TOKEN, repay, premium, address(liquidator), abi.encode(p)
        );
        assertTrue(ok, "executeOperation returned false");
    }

    // ─────────────────────────────────────────────────────────────────────
    // swapPoolFee round-trip
    // ─────────────────────────────────────────────────────────────────────

    /// @notice Confirms LiquidationParams.swapPoolFee is propagated into the
    ///         PancakeSwap router call. Uses vm.expectCall on the exact
    ///         encoded ExactInputSingleParams to assert fee == 500.
    function test_swapPoolFee_field_in_params() public {
        _skipIfNoRpc();

        uint24 fee = 500;
        uint256 repay = 100 ether;
        uint256 premium = 1 ether;
        uint256 collUnderlying = 500 ether;
        uint256 swapOut = repay + premium; // zero profit — skips transfer

        vm.mockCall(
            MOCK_DEBT_VTOKEN,
            abi.encodeWithSelector(IVToken.liquidateBorrow.selector),
            abi.encode(uint256(0))
        );
        vm.mockCall(
            MOCK_COLL_VTOKEN,
            abi.encodeWithSelector(IVToken.balanceOf.selector, address(liquidator)),
            abi.encode(uint256(1 ether))
        );
        vm.mockCall(
            MOCK_COLL_VTOKEN,
            abi.encodeWithSelector(IVToken.redeem.selector),
            abi.encode(uint256(0))
        );
        vm.mockCall(
            MOCK_COLL_TOKEN,
            abi.encodeWithSelector(IERC20.balanceOf.selector, address(liquidator)),
            abi.encode(collUnderlying)
        );
        vm.mockCall(
            MOCK_COLL_TOKEN, abi.encodeWithSelector(IERC20.approve.selector), abi.encode(true)
        );
        vm.mockCall(
            MOCK_DEBT_TOKEN, abi.encodeWithSelector(IERC20.approve.selector), abi.encode(true)
        );
        vm.mockCall(
            PCS_V3_ROUTER_BSC,
            abi.encodeWithSelector(ISwapRouter.exactInputSingle.selector),
            abi.encode(swapOut)
        );
        vm.mockCall(
            MOCK_DEBT_TOKEN,
            abi.encodeWithSelector(IERC20.balanceOf.selector, address(liquidator)),
            abi.encode(swapOut)
        );

        CharonLiquidator.LiquidationParams memory p = CharonLiquidator.LiquidationParams({
            protocolId: 3,
            borrower: MOCK_BORROWER,
            debtToken: MOCK_DEBT_TOKEN,
            collateralToken: MOCK_COLL_TOKEN,
            debtVToken: MOCK_DEBT_VTOKEN,
            collateralVToken: MOCK_COLL_VTOKEN,
            repayAmount: repay,
            minSwapOut: repay + premium,
            swapPoolFee: fee
        });

        // Assert that the router is called with the exact fee from params.
        // Build the expected params struct and encode the full call; any
        // deviation in `fee` would cause vm.expectCall to fail.
        ISwapRouter.ExactInputSingleParams memory expected = ISwapRouter.ExactInputSingleParams({
            tokenIn: MOCK_COLL_TOKEN,
            tokenOut: MOCK_DEBT_TOKEN,
            fee: fee,
            recipient: address(liquidator),
            deadline: block.timestamp,
            amountIn: collUnderlying,
            amountOutMinimum: p.minSwapOut,
            sqrtPriceLimitX96: 0
        });
        vm.expectCall(
            PCS_V3_ROUTER_BSC,
            abi.encodeWithSelector(ISwapRouter.exactInputSingle.selector, expected)
        );

        vm.prank(AAVE_V3_POOL_BSC);
        bool ok = liquidator.executeOperation(
            MOCK_DEBT_TOKEN, repay, premium, address(liquidator), abi.encode(p)
        );
        assertTrue(ok, "executeOperation returned false");
    }

    /// @notice Reject path: swapPoolFee = 0 must revert inside
    ///         executeLiquidation input validation. Confirms the field
    ///         is actually read, not silently ignored.
    function test_swapPoolFee_zero_reverts() public {
        _skipIfNoRpc();

        CharonLiquidator.LiquidationParams memory p = CharonLiquidator.LiquidationParams({
            protocolId: 3,
            borrower: MOCK_BORROWER,
            debtToken: MOCK_DEBT_TOKEN,
            collateralToken: MOCK_COLL_TOKEN,
            debtVToken: MOCK_DEBT_VTOKEN,
            collateralVToken: MOCK_COLL_VTOKEN,
            repayAmount: 1 ether,
            minSwapOut: 1 ether,
            swapPoolFee: 0
        });

        vm.expectRevert(bytes("!swapPoolFee"));
        liquidator.executeLiquidation(p);
    }
}
