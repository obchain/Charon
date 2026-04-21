// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "forge-std/Test.sol";

// ─────────────────────────────────────────────────────────────────────────────
// Production contract under test
// ─────────────────────────────────────────────────────────────────────────────
import { CharonLiquidator } from "../src/CharonLiquidator.sol";

// ─────────────────────────────────────────────────────────────────────────────
// Mock contracts — all defined in this file, no external imports
// ─────────────────────────────────────────────────────────────────────────────

/// @dev Minimal ERC-20 stub used only in rescue tests.
contract MockERC20 {
    mapping(address => uint256) public balanceOf;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        require(balanceOf[msg.sender] >= amount, "insufficient");
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    // Satisfy IERC20.approve/allowance selectors so vm.mockCall doesn't need them.
    function approve(address, uint256) external pure returns (bool) {
        return true;
    }

    function allowance(address, address) external pure returns (uint256) {
        return 0;
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
    // E. Happy path on BSC mainnet fork
    // ─────────────────────────────────────────────────────────────────────────

    /// @dev Full end-to-end liquidation against live BSC state.
    ///
    ///      Status: SKIPPED pending issue #22.x
    ///
    ///      Reason: Exercising the real PancakeSwap V3 router against live BSC
    ///      liquidity requires identifying a stable (tokenIn, tokenOut, fee-tier)
    ///      pair and a repayAmount that does not move the pool price enough to
    ///      breach slippage checks across BSC block windows.  Doing that research
    ///      deterministically (without an always-pinned block number and a known-
    ///      underwater borrower) is out of scope for this commit.
    ///
    ///      When #22.x lands:
    ///        1. Pin a BSC block: vm.createSelectFork(vm.envString("BNB_HTTP_URL"), BLOCK);
    ///        2. Use a known-underwater borrower from the scanner output.
    ///        3. Mock only vToken.liquidateBorrow + vToken.redeem (return 0);
    ///           let the real PCS V3 router execute the swap.
    ///        4. Assert profit > 0 and LiquidationExecuted emitted.
    ///
    ///      TODO(#22.x): unmocked PCS swap once a stable pair + amount is identified.
    function test_executeLiquidation_endToEndOnFork() public {
        vm.skip(true);
    }
}
