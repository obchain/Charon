// SPDX-License-Identifier: MIT
pragma solidity =0.8.24;

import "forge-std/Test.sol";

// ─────────────────────────────────────────────────────────────────────────────
// Production contract under test
// ─────────────────────────────────────────────────────────────────────────────
import { CharonLiquidator } from "../src/CharonLiquidator.sol";

// ─────────────────────────────────────────────────────────────────────────────
// Mock contracts — all defined in this file, no external imports
// ─────────────────────────────────────────────────────────────────────────────

/// @dev Minimal ERC-20 stub with full approve/transferFrom semantics.
///      Used both by the rescue tests and by the end-to-end executeOperation
///      callback tests, which rely on a real swap-router pulling `tokenIn`
///      via `transferFrom` from the liquidator.
contract MockERC20 {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function burn(address from, uint256 amount) external {
        require(balanceOf[from] >= amount, "burn: insufficient");
        balanceOf[from] -= amount;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        require(balanceOf[msg.sender] >= amount, "insufficient");
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    /// @dev Full ERC-20 transferFrom: debits allowance (unless unlimited),
    ///      validates balance, then moves tokens.
    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 a = allowance[from][msg.sender];
        require(a >= amount, "allowance");
        require(balanceOf[from] >= amount, "balance");
        if (a != type(uint256).max) {
            allowance[from][msg.sender] = a - amount;
        }
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

/// @dev Contract that ALWAYS reverts on receiving native BNB.
///      Used in the rescue() test to exercise the 2300-gas `transfer` path
///      against a non-payable recipient — see issue #135.
///
///      `payable(to).transfer(amount)` in CharonLiquidator.rescue forwards only
///      2300 gas to the receiver's fallback/receive. Any logic here (even a
///      bare revert in receive) is guaranteed to fit in that gas budget and
///      causes the outer rescue() call to revert. This pins down and documents
///      the current behaviour: rescue to a contract recipient that rejects
///      native BNB reverts the entire transaction rather than silently losing
///      funds.
contract GasHungryReceiver {
    /// @dev Reverts immediately on any native transfer — fits inside 2300 gas.
    receive() external payable {
        revert("no bnb");
    }
}

/// @dev Minimal Venus vToken mock. Supports the exact surface CharonLiquidator
///      drives inside executeOperation: liquidateBorrow, redeem, balanceOf.
///
///      - liquidateBorrow: returns the preconfigured error code and credits the
///        caller with `seizedVTokens` units so the subsequent balanceOf read in
///        the liquidator returns >0. Does NOT actually pull tokens from the
///        caller — the ERC-20 approve is set but never consumed, which matches
///        our test goal (we are validating CharonLiquidator's flow, not the
///        Venus internals).
///      - redeem: returns the preconfigured error code and mints `underlying`
///        tokens (via MockERC20) back into the caller, simulating vToken →
///        underlying conversion. The vToken balance of the caller is burned.
///      - balanceOf: reads an internal mapping credited by liquidateBorrow/
///        external seed calls.
contract MockVToken {
    mapping(address => uint256) public balanceOf;

    /// @dev ERC-20 underlying this vToken converts back into on redeem.
    MockERC20 public immutable underlying;

    /// @dev vToken units to credit the liquidator on liquidateBorrow().
    uint256 public seizedVTokens;

    /// @dev The OTHER vToken whose balanceOf[liquidator] will be credited with
    ///      seizedVTokens on liquidateBorrow. In a real Venus liquidation the
    ///      debt vToken transfers seized collateral vTokens into the caller,
    ///      so on the debt vToken mock we point this at the collateral vToken.
    MockVToken public seizeTarget;

    /// @dev Error code returned by liquidateBorrow (0 == success).
    uint256 public liqErr;

    /// @dev Error code returned by redeem (0 == success).
    uint256 public redeemErr;

    /// @dev Amount of underlying to mint into msg.sender on redeem.
    uint256 public redeemUnderlyingAmount;

    constructor(MockERC20 _underlying) {
        underlying = _underlying;
    }

    // ── Test harness setters ─────────────────────────────────────────────────

    function setSeized(uint256 s) external {
        seizedVTokens = s;
    }

    function setSeizeTarget(MockVToken t) external {
        seizeTarget = t;
    }

    function setLiqErr(uint256 e) external {
        liqErr = e;
    }

    function setRedeemErr(uint256 e) external {
        redeemErr = e;
    }

    function setRedeemUnderlyingAmount(uint256 a) external {
        redeemUnderlyingAmount = a;
    }

    function setBalance(address who, uint256 amount) external {
        balanceOf[who] = amount;
    }

    // ── IVToken surface ──────────────────────────────────────────────────────

    function liquidateBorrow(address, uint256, address) external returns (uint256) {
        // Credit the configured collateral vToken's balance on the caller.
        // If no target is configured, credit self (back-compat for tests that
        // call liquidateBorrow and read balanceOf on the same mock).
        if (address(seizeTarget) != address(0)) {
            seizeTarget.setBalance(msg.sender, seizeTarget.balanceOf(msg.sender) + seizedVTokens);
        } else {
            balanceOf[msg.sender] += seizedVTokens;
        }
        return liqErr;
    }

    function redeem(uint256 redeemTokens) external returns (uint256) {
        require(balanceOf[msg.sender] >= redeemTokens, "vbal");
        balanceOf[msg.sender] -= redeemTokens;
        if (redeemUnderlyingAmount > 0 && address(underlying) != address(0)) {
            underlying.mint(msg.sender, redeemUnderlyingAmount);
        }
        return redeemErr;
    }
}

/// @dev Minimal PancakeSwap V3 SwapRouter mock driving `exactInputSingle`.
///
///      Pulls `amountIn` of `tokenIn` from msg.sender via transferFrom (the
///      liquidator pre-approves the router), then mints `returnAmount` of
///      `tokenOut` to `recipient`. The `returnAmount` and whether to check
///      `amountOutMinimum` are driven by the test harness.
contract MockSwapRouter {
    /// @dev Exact amount of tokenOut the next swap will deliver.
    uint256 public returnAmount;

    /// @dev If true, enforce `amountOutMinimum` the same way a real router does.
    ///      Used in the minSwapOut slippage test.
    bool public enforceMinOut;

    function setReturnAmount(uint256 a) external {
        returnAmount = a;
    }

    function setEnforceMinOut(bool v) external {
        enforceMinOut = v;
    }

    /// @dev Mirrors the layout of ISwapRouter.ExactInputSingleParams. Declared
    ///      locally so this mock does not import the production interface —
    ///      the call-site ABI is what matters.
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(ExactInputSingleParams calldata p)
        external
        payable
        returns (uint256 amountOut)
    {
        // Pull input from caller via the allowance the liquidator set.
        MockERC20(p.tokenIn).transferFrom(msg.sender, address(this), p.amountIn);
        // Enforce slippage floor exactly like a real router if the test asks us to.
        if (enforceMinOut) {
            require(returnAmount >= p.amountOutMinimum, "Too little received");
        }
        // Deliver output to recipient.
        MockERC20(p.tokenOut).mint(p.recipient, returnAmount);
        return returnAmount;
    }
}

/// @dev Malicious pool that is ALSO the liquidator owner.
///
///      Attack scenario:
///        1. ReentrantPool deploys CharonLiquidator — it becomes owner.
///        2. ReentrantPool calls executeLiquidation (as owner) — nonReentrant lock set.
///        3. executeLiquidation calls flashLoanSimple (this contract).
///        4. flashLoanSimple re-calls executeLiquidation — msg.sender is still this
///           contract (the owner), so onlyOwner passes, but nonReentrant fires "reentrant".
///        5. "reentrant" revert propagates all the way back to the test.
///
///      Because the entire tx reverts, no storage observation survives post-call.
///      vm.expectRevert on "reentrant" is the complete correctness assertion.
contract ReentrantPool {
    CharonLiquidator internal liquidator;
    CharonLiquidator.LiquidationParams internal storedParams;

    constructor(address stubRouter) {
        // Deploy liquidator with this contract as AAVE_POOL — and msg.sender here
        // is the test contract, but we want ReentrantPool to be owner.  We deploy
        // from inside this constructor so msg.sender to CharonLiquidator is this.
        liquidator = new CharonLiquidator(address(this), stubRouter);
    }

    function buildParams(CharonLiquidator.LiquidationParams calldata p) external {
        storedParams = p;
    }

    /// @dev Entry point called by the test.  ReentrantPool is the owner of liquidator,
    ///      so this call passes onlyOwner and sets _entered = 2.
    function attack() external {
        liquidator.executeLiquidation(storedParams);
    }

    /// @dev Aave pool stub — re-enters executeLiquidation as msg.sender == this == owner.
    ///      onlyOwner passes; nonReentrant fires "reentrant".
    function flashLoanSimple(
        address, // receiverAddress
        address, // asset
        uint256, // amount
        bytes calldata, // params
        uint16 // referralCode
    )
        external
    {
        liquidator.executeLiquidation(storedParams);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test suite
// ─────────────────────────────────────────────────────────────────────────────

contract CharonLiquidatorTest is Test {
    // ── Deterministic stub addresses ──────────────────────────────────────────
    address internal constant STUB_POOL = address(0xA11E);
    address internal constant STUB_ROUTER = address(0xB22E);

    CharonLiquidator internal liquidator;

    // Addresses used across multiple sections — initialized in setUp.
    address internal alice;
    address internal recipient;

    // ── setUp creates one unforked liquidator; fork test makes its own ────────
    function setUp() public {
        alice = makeAddr("alice"); // non-owner attacker
        recipient = makeAddr("recipient");
        liquidator = new CharonLiquidator(STUB_POOL, STUB_ROUTER);
        // address(this) == owner because msg.sender at deploy is the test contract.
    }

    // ── Internal helper: returns a fully-valid LiquidationParams ─────────────
    function _validParams() internal returns (CharonLiquidator.LiquidationParams memory) {
        return CharonLiquidator.LiquidationParams({
            protocolId: 3, // PROTOCOL_VENUS
            borrower: makeAddr("borrower"),
            debtToken: makeAddr("debtToken"),
            collateralToken: makeAddr("collateralToken"),
            debtVToken: makeAddr("debtVToken"),
            collateralVToken: makeAddr("collateralVToken"),
            repayAmount: 1e18,
            minSwapOut: 0
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    // A. Access control (no fork)
    // ─────────────────────────────────────────────────────────────────────────

    /// @dev Non-owner calling executeLiquidation must revert with "!owner".
    function test_executeLiquidation_revertsWhenNotOwner() public {
        CharonLiquidator.LiquidationParams memory p = _validParams();
        vm.prank(alice);
        vm.expectRevert(bytes("!owner"));
        liquidator.executeLiquidation(p);
    }

    /// @dev Non-owner calling rescue must revert with "!owner".
    function test_rescue_revertsWhenNotOwner() public {
        vm.prank(alice);
        vm.expectRevert(bytes("!owner"));
        liquidator.rescue(address(0), recipient, 1 ether);
    }

    /// @dev Direct call to executeOperation (sender != AAVE_POOL) must revert "!pool".
    function test_executeOperation_revertsWhenNotPool() public {
        // address(this) is the test contract, not the stub pool — gate fires.
        vm.expectRevert(bytes("!pool"));
        liquidator.executeOperation(
            makeAddr("asset"),
            1e18,
            0,
            address(liquidator), // correct initiator, but pool check fires first
            abi.encode(_validParams())
        );
    }

    /// @dev Call from the pool address but with wrong initiator must revert "!initiator".
    function test_executeOperation_revertsWhenInitiatorNotSelf() public {
        vm.prank(STUB_POOL);
        vm.expectRevert(bytes("!initiator"));
        liquidator.executeOperation(
            makeAddr("asset"),
            1e18,
            0,
            address(0xdead), // wrong initiator
            abi.encode(_validParams())
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // B. Input validation on executeLiquidation (no fork)
    //
    // All require() guards fire BEFORE flashLoanSimple is called, so no mock
    // on STUB_POOL is needed — the tx reverts inside executeLiquidation itself.
    // ─────────────────────────────────────────────────────────────────────────

    function test_executeLiquidation_revertsOnWrongProtocolId() public {
        CharonLiquidator.LiquidationParams memory p = _validParams();
        p.protocolId = 0; // wrong — only 3 (VENUS) is accepted
        vm.expectRevert(bytes("!protocolId"));
        liquidator.executeLiquidation(p);
    }

    function test_executeLiquidation_revertsOnZeroBorrower() public {
        CharonLiquidator.LiquidationParams memory p = _validParams();
        p.borrower = address(0);
        vm.expectRevert(bytes("!borrower"));
        liquidator.executeLiquidation(p);
    }

    function test_executeLiquidation_revertsOnZeroDebtToken() public {
        CharonLiquidator.LiquidationParams memory p = _validParams();
        p.debtToken = address(0);
        vm.expectRevert(bytes("!debtToken"));
        liquidator.executeLiquidation(p);
    }

    function test_executeLiquidation_revertsOnZeroCollateralToken() public {
        CharonLiquidator.LiquidationParams memory p = _validParams();
        p.collateralToken = address(0);
        vm.expectRevert(bytes("!collateralToken"));
        liquidator.executeLiquidation(p);
    }

    function test_executeLiquidation_revertsOnZeroDebtVToken() public {
        CharonLiquidator.LiquidationParams memory p = _validParams();
        p.debtVToken = address(0);
        vm.expectRevert(bytes("!debtVToken"));
        liquidator.executeLiquidation(p);
    }

    function test_executeLiquidation_revertsOnZeroCollateralVToken() public {
        CharonLiquidator.LiquidationParams memory p = _validParams();
        p.collateralVToken = address(0);
        vm.expectRevert(bytes("!collateralVToken"));
        liquidator.executeLiquidation(p);
    }

    function test_executeLiquidation_revertsOnZeroRepayAmount() public {
        CharonLiquidator.LiquidationParams memory p = _validParams();
        p.repayAmount = 0;
        vm.expectRevert(bytes("!repayAmount"));
        liquidator.executeLiquidation(p);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // B2. Constructor zero-address guards (no fork) — issue #134
    // ─────────────────────────────────────────────────────────────────────────

    /// @dev Deployment must revert when aavePool is address(0).
    function test_constructor_revertsOnZeroAavePool() public {
        vm.expectRevert(bytes("!aavePool"));
        new CharonLiquidator(address(0), STUB_ROUTER);
    }

    /// @dev Deployment must revert when swapRouter is address(0).
    function test_constructor_revertsOnZeroSwapRouter() public {
        vm.expectRevert(bytes("!swapRouter"));
        new CharonLiquidator(STUB_POOL, address(0));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // C. rescue() behaviour (no fork)
    // ─────────────────────────────────────────────────────────────────────────

    function test_rescue_transfersErc20() public {
        MockERC20 token = new MockERC20();
        token.mint(address(liquidator), 1000);

        vm.expectEmit(true, true, false, true);
        emit CharonLiquidator.Rescued(address(token), recipient, 400);

        liquidator.rescue(address(token), recipient, 400);

        assertEq(token.balanceOf(address(liquidator)), 600, "liquidator balance wrong");
        assertEq(token.balanceOf(recipient), 400, "recipient balance wrong");
    }

    function test_rescue_transfersNativeBnb() public {
        vm.deal(address(liquidator), 5 ether);
        uint256 before = recipient.balance;

        vm.expectEmit(true, true, false, true);
        emit CharonLiquidator.Rescued(address(0), recipient, 2 ether);

        liquidator.rescue(address(0), recipient, 2 ether);

        assertEq(recipient.balance - before, 2 ether, "bnb not received");
        assertEq(address(liquidator).balance, 3 ether, "liquidator bnb wrong");
    }

    function test_rescue_revertsOnZeroRecipient() public {
        vm.expectRevert(bytes("!to"));
        liquidator.rescue(address(0), address(0), 1 ether);
    }

    function test_rescue_revertsOnZeroAmount() public {
        vm.expectRevert(bytes("!amount"));
        liquidator.rescue(address(0), recipient, 0);
    }

    /// @dev Issue #135 — `rescue` for native BNB uses `payable(to).transfer`,
    ///      which forwards only 2300 gas and reverts on failure. Recipients
    ///      that reject native BNB (multisigs with guard hooks, contracts
    ///      with expensive receive() logic) therefore cause the whole tx to
    ///      revert rather than silently losing funds.
    ///
    ///      This test pins that behaviour with a contract recipient whose
    ///      `receive()` reverts. It documents a known limitation:
    ///        - Rescuing native BNB to a Gnosis Safe or any contract that
    ///          consumes >2300 gas in its receive fallback will revert.
    ///        - The current design is therefore safe against loss-of-funds
    ///          but not against a DoS'd rescue for contract recipients.
    ///        - The production remediation (see issue #117) is to switch to
    ///          `(bool ok,) = to.call{value: amount}("")` and require(ok).
    function test_rescue_revertsWhenNativeRecipientRejects() public {
        GasHungryReceiver rejector = new GasHungryReceiver();
        vm.deal(address(liquidator), 5 ether);

        // Solidity's `transfer` reverts on receiver failure. The inner
        // "no bnb" message from GasHungryReceiver is not surfaced by
        // `transfer` — it only bubbles up a generic EVM revert. We assert
        // the outer call reverts and that no BNB has moved.
        vm.expectRevert();
        liquidator.rescue(address(0), address(rejector), 1 ether);

        assertEq(address(rejector).balance, 0, "rejector should not hold bnb");
        assertEq(address(liquidator).balance, 5 ether, "liquidator bnb should be untouched");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // D. Reentrancy guard (no fork)
    // ─────────────────────────────────────────────────────────────────────────

    /// @dev Deploy a ReentrantPool; owner calls executeLiquidation which calls the
    ///      pool's flashLoanSimple, which tries to call executeLiquidation again.
    ///      The inner call must revert with "reentrant", which bubbles out through
    ///      flashLoanSimple and is caught by vm.expectRevert.
    ///
    ///      Note: storage/event observations set inside the reentrant call frame
    ///      (e.g., rPool.reentryAttempted) are rolled back with the tx revert and
    ///      cannot be asserted post-call.  vm.expectRevert on "reentrant" is the
    ///      complete and sufficient assertion here.
    function test_executeLiquidation_isReentrancyGuarded() public {
        // ReentrantPool's constructor deploys a CharonLiquidator with itself as both
        // AAVE_POOL and owner.  This means it can call executeLiquidation (owner) and
        // serve as the pool callback (AAVE_POOL) — satisfying both guards while
        // exercising the reentrancy path.
        ReentrantPool rPool = new ReentrantPool(STUB_ROUTER);

        CharonLiquidator.LiquidationParams memory p = _validParams();
        rPool.buildParams(p);

        // rPool.attack() calls liquidator.executeLiquidation() (passes onlyOwner, sets
        // _entered=2), which calls rPool.flashLoanSimple(), which calls
        // liquidator.executeLiquidation() again — this time nonReentrant fires "reentrant".
        // The revert bubbles up through the whole call stack to vm.expectRevert.
        vm.expectRevert(bytes("reentrant"));
        rPool.attack();
    }

    // ─────────────────────────────────────────────────────────────────────────
    // E. executeOperation callback — unit coverage (no fork)
    //
    // These tests drive CharonLiquidator.executeOperation directly, pranking as
    // the configured AAVE_POOL and passing initiator == address(liquidator).
    // Venus, PancakeSwap, and the debt/collateral tokens are all mocked in-file
    // so the full callback body — including the profit sweep, the slippage
    // floor check, and the LiquidationExecuted emission — can be exercised
    // without a mainnet fork. Covers issues #126, #127, #128, #131.
    // ─────────────────────────────────────────────────────────────────────────

    /// @dev Harness bundling a fresh CharonLiquidator + all mocks wired up.
    struct CallbackHarness {
        CharonLiquidator liq;
        MockERC20 debt;
        MockERC20 coll;
        MockVToken dvt;
        MockVToken cvt;
        MockSwapRouter router;
        address pool;
        CharonLiquidator.LiquidationParams p;
    }

    /// @dev Build a harness with: liquidator owned by address(this), pool =
    ///      a fresh deterministic address, all mocks at deterministic fresh
    ///      addresses. `repayAmount`, `swapOutput`, and `minSwapOut` shape
    ///      the economics.
    function _buildHarness(uint256 repayAmount, uint256 swapOutput, uint256 minSwapOut)
        internal
        returns (CallbackHarness memory h)
    {
        h.debt = new MockERC20();
        h.coll = new MockERC20();
        h.dvt = new MockVToken(h.debt);
        h.cvt = new MockVToken(h.coll);
        h.router = new MockSwapRouter();
        // Deploy liquidator with the mock router but a fresh pool stub we control.
        h.pool = makeAddr("aavePoolHarness");
        h.liq = new CharonLiquidator(h.pool, address(h.router));

        // Wire the Venus-side mocks:
        //   - Debt vToken (dvt) is the one CharonLiquidator calls
        //     liquidateBorrow on. Point its seizeTarget at the collateral
        //     vToken (cvt), and configure it to seize 1e18 units — so after
        //     liquidateBorrow returns, cvt.balanceOf[liquidator] == 1e18.
        //   - Collateral vToken (cvt) is then redeemed by the liquidator.
        //     Configure its redeem to return 1e18 of collateral underlying
        //     into the liquidator so the swap leg has amountIn > 0.
        h.dvt.setSeizeTarget(h.cvt);
        h.dvt.setSeized(1e18);
        h.cvt.setRedeemUnderlyingAmount(1e18);

        // Router returns exactly `swapOutput` of debtToken into the liquidator.
        h.router.setReturnAmount(swapOutput);

        // Build params using the real mock addresses.
        h.p = CharonLiquidator.LiquidationParams({
            protocolId: 3,
            borrower: makeAddr("borrower"),
            debtToken: address(h.debt),
            collateralToken: address(h.coll),
            debtVToken: address(h.dvt),
            collateralVToken: address(h.cvt),
            repayAmount: repayAmount,
            minSwapOut: minSwapOut
        });
    }

    /// @dev Happy-path: executeOperation runs the full callback, repayment
    ///      succeeds, profit is swept to owner, and LiquidationExecuted emits
    ///      with the exact profit value. Covers #126 and #128.
    function test_executeOperation_fullCallback_emitsAndSweepsProfit() public {
        uint256 repay = 1e18;
        uint256 premium = 9e14; // 0.09 %, matches Aave's v3 default on BSC
        uint256 swapOut = 2e18; // profit = swapOut - (repay + premium)

        CallbackHarness memory h = _buildHarness(repay, swapOut, 0);
        address owner = h.liq.owner();
        uint256 ownerBefore = h.debt.balanceOf(owner);

        // Pre-expect the LiquidationExecuted event with exact profit value.
        uint256 expectedProfit = swapOut - (repay + premium);
        vm.expectEmit(true, true, false, true, address(h.liq));
        emit CharonLiquidator.LiquidationExecuted(
            h.p.borrower, address(h.debt), repay, expectedProfit
        );

        vm.prank(h.pool);
        bool ok = h.liq
        .executeOperation(address(h.debt), repay, premium, address(h.liq), abi.encode(h.p));

        assertTrue(ok, "callback returned false");
        // Profit swept to owner.
        assertEq(h.debt.balanceOf(owner) - ownerBefore, expectedProfit, "profit not swept");
        // Liquidator holds exactly totalOwed left for Aave to pull.
        assertEq(h.debt.balanceOf(address(h.liq)), repay + premium, "totalOwed balance wrong");
        // Approval to Aave Pool equals totalOwed.
        assertEq(h.debt.allowance(address(h.liq), h.pool), repay + premium, "aave approval wrong");
        // Post-swap router approval on collateral must be zeroed.
        assertEq(
            h.coll.allowance(address(h.liq), address(h.router)), 0, "router approval not zeroed"
        );
        // Post-liquidate vToken approval on debt must be zeroed.
        assertEq(h.debt.allowance(address(h.liq), address(h.dvt)), 0, "vtoken approval not zeroed");
    }

    /// @dev Issue #127 — executeOperation must revert when `asset` does not
    ///      match the decoded `debtToken` in the forwarded params. This can
    ///      only happen if the Aave pool is misbehaving (or the initiator
    ///      guard is bypassed), but the contract still fails closed.
    function test_executeOperation_revertsOnAssetDebtMismatch() public {
        CallbackHarness memory h = _buildHarness(1e18, 2e18, 0);
        address wrongAsset = makeAddr("wrongAsset");

        vm.prank(h.pool);
        vm.expectRevert(bytes("asset/debt mismatch"));
        h.liq.executeOperation(wrongAsset, 1e18, 0, address(h.liq), abi.encode(h.p));
    }

    /// @dev Issue #127 — executeOperation must revert when `amount` does not
    ///      match the decoded `repayAmount`. Fails closed on any pool-side
    ///      discrepancy.
    function test_executeOperation_revertsOnAmountRepayMismatch() public {
        CallbackHarness memory h = _buildHarness(1e18, 2e18, 0);

        vm.prank(h.pool);
        vm.expectRevert(bytes("amount/repay mismatch"));
        h.liq.executeOperation(address(h.debt), 2e18, 0, address(h.liq), abi.encode(h.p));
    }

    /// @dev Issue #131 — minSwapOut slippage floor is honoured by the router.
    ///      Configure the router to enforce amountOutMinimum the same way the
    ///      real PancakeSwap V3 router does, and to return a swap output
    ///      below the configured minSwapOut. The router reverts with
    ///      "Too little received"; the revert bubbles up through
    ///      executeOperation.
    function test_executeOperation_revertsWhenRouterOutputBelowMinSwapOut() public {
        uint256 repay = 1e18;
        uint256 premium = 9e14;
        uint256 swapOut = 5e17; // well below minSwapOut
        uint256 minSwapOut = 15e17; // 1.5e18, above swapOut

        CallbackHarness memory h = _buildHarness(repay, swapOut, minSwapOut);
        h.router.setEnforceMinOut(true);

        vm.prank(h.pool);
        vm.expectRevert(bytes("Too little received"));
        h.liq.executeOperation(address(h.debt), repay, premium, address(h.liq), abi.encode(h.p));
    }

    /// @dev Issue #131 — defensive balance check: even if the caller foolishly
    ///      passes minSwapOut == 0 (disabling the router-side slippage guard),
    ///      the contract's own post-swap check
    ///      `require(finalBal >= totalOwed, "swap output below repayment")`
    ///      still fails closed when the swap returns less than amount + premium.
    ///      This exercises the defensive guard independently of the router.
    function test_executeOperation_revertsWhenOutputBelowTotalOwed() public {
        uint256 repay = 1e18;
        uint256 premium = 1e17; // totalOwed = 1.1e18
        uint256 swapOut = 9e17; // less than totalOwed
        uint256 minSwapOut = 0; // caller disabled router-side check

        CallbackHarness memory h = _buildHarness(repay, swapOut, minSwapOut);
        // enforceMinOut left false — router accepts anything; the internal guard fires.

        vm.prank(h.pool);
        vm.expectRevert(bytes("swap output below repayment"));
        h.liq.executeOperation(address(h.debt), repay, premium, address(h.liq), abi.encode(h.p));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // F. Happy path on BSC mainnet fork
    // ─────────────────────────────────────────────────────────────────────────

    /// @dev Full end-to-end liquidation against live BSC state.
    ///
    ///      Status: SKIPPED pending issue #53 (feat/25-foundry-fork-tests).
    ///
    ///      Reason: Exercising the real PancakeSwap V3 router against live BSC
    ///      liquidity requires identifying a stable (tokenIn, tokenOut, fee-tier)
    ///      pair and a repayAmount that does not move the pool price enough to
    ///      breach slippage checks across BSC block windows.  Doing that research
    ///      deterministically (without an always-pinned block number and a known-
    ///      underwater borrower) is out of scope for this commit.
    ///
    ///      When #53 lands:
    ///        1. Pin a BSC block: vm.createSelectFork(vm.envString("BNB_HTTP_URL"), BLOCK);
    ///        2. Use a known-underwater borrower from the scanner output.
    ///        3. Mock only vToken.liquidateBorrow + vToken.redeem (return 0);
    ///           let the real PCS V3 router execute the swap.
    ///        4. Assert profit > 0 and LiquidationExecuted emitted.
    ///
    ///      TODO(#53): unmocked PCS swap once a stable pair + amount is identified.
    function test_executeLiquidation_endToEndOnFork() public {
        vm.skip(true);
    }
}
