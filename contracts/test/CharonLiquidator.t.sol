// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import { Test } from "forge-std/Test.sol";

import { CharonLiquidator } from "../src/CharonLiquidator.sol";

// ─────────────────────────────────────────────────────────────────────────────
// Minimal ERC-20 stub used only by the rescue() ERC-20 path test.
// Lives in-file so this suite has zero external dependencies beyond forge-std.
// ─────────────────────────────────────────────────────────────────────────────
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

    function approve(address, uint256) external pure returns (bool) {
        return true;
    }

    function allowance(address, address) external pure returns (uint256) {
        return 0;
    }
}

/// @dev Contract recipient whose `receive()` writes a storage slot, costing well
///      over the 2300-gas stipend that Solidity's `transfer`/`send` forwards.
///      Used to prove that rescue()'s BNB path uses `call` (full gas) and not
///      `transfer`/`send` — critical for multisig / smart-wallet compatibility.
contract GasHungryReceiver {
    uint256 public touched;

    receive() external payable {
        // SSTORE on a cold slot is ~20k gas — guaranteed to exceed the 2300
        // stipend that `transfer`/`send` would forward.
        touched += 1;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Skeleton test suite — issue #116
//
// The CharonLiquidator at this point in the branch is a skeleton: the
// executeLiquidation and executeOperation bodies revert with a "not yet
// implemented" message after input validation and the security gates.
//
// This suite therefore focuses on the shape of the deployed contract:
//   - Constructor non-zero-address guards.
//   - Owner assignment.
//   - onlyOwner on executeLiquidation and rescue.
//   - Input validation inside executeLiquidation (per-field zero-address /
//     zero-amount / wrong-protocol reverts — reached BEFORE the "not yet
//     implemented" revert).
//   - executeOperation security gates (!pool, !initiator).
//   - rescue() happy and sad paths, including the post-#117 BNB-via-call path.
//   - Absence of an open `receive()` (post-#117) — direct BNB sends revert.
//
// Full end-to-end liquidation coverage lands with issue #12 (impl) and
// issue #22 (fork tests).
// ─────────────────────────────────────────────────────────────────────────────
contract CharonLiquidatorTest is Test {
    // ── Deterministic stub addresses ──────────────────────────────────────────
    address internal constant STUB_POOL = address(0xA11E);
    address internal constant STUB_ROUTER = address(0xB22E);

    CharonLiquidator internal liquidator;
    address internal alice;
    address internal recipient;

    function setUp() public {
        alice = makeAddr("alice");
        recipient = makeAddr("recipient");
        // msg.sender at construction is the test contract, so address(this) == owner.
        liquidator = new CharonLiquidator(STUB_POOL, STUB_ROUTER);
    }

    // ── Internal helper: a fully-valid LiquidationParams struct ──────────────
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
    // A. Constructor guards & owner binding
    // ─────────────────────────────────────────────────────────────────────────

    function test_constructor_revertsOnZeroAavePool() public {
        vm.expectRevert(bytes("!aavePool"));
        new CharonLiquidator(address(0), STUB_ROUTER);
    }

    function test_constructor_revertsOnZeroSwapRouter() public {
        vm.expectRevert(bytes("!swapRouter"));
        new CharonLiquidator(STUB_POOL, address(0));
    }

    function test_constructor_setsOwnerAndImmutables() public view {
        assertEq(liquidator.owner(), address(this), "owner must be deployer");
        assertEq(liquidator.AAVE_POOL(), STUB_POOL, "AAVE_POOL mismatch");
        assertEq(liquidator.SWAP_ROUTER(), STUB_ROUTER, "SWAP_ROUTER mismatch");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // B. Access control
    // ─────────────────────────────────────────────────────────────────────────

    function test_executeLiquidation_revertsWhenNotOwner() public {
        CharonLiquidator.LiquidationParams memory p = _validParams();
        vm.prank(alice);
        vm.expectRevert(bytes("!owner"));
        liquidator.executeLiquidation(p);
    }

    function test_rescue_revertsWhenNotOwner() public {
        vm.prank(alice);
        vm.expectRevert(bytes("!owner"));
        liquidator.rescue(address(0), recipient, 1 ether);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // C. executeLiquidation input validation (skeleton reverts come AFTER these)
    // ─────────────────────────────────────────────────────────────────────────

    function test_executeLiquidation_revertsOnWrongProtocolId() public {
        CharonLiquidator.LiquidationParams memory p = _validParams();
        p.protocolId = 0; // ProtocolId::Aave — not supported in v0.1
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

    /// @dev Validated params still hit the skeleton's "not yet implemented" revert
    ///      — this test pins the current skeleton behaviour so replacing the body
    ///      in issue #12 deliberately breaks this test (reminder to update).
    function test_executeLiquidation_skeletonStillReverts() public {
        CharonLiquidator.LiquidationParams memory p = _validParams();
        vm.expectRevert(bytes("CharonLiquidator: executeLiquidation not yet implemented"));
        liquidator.executeLiquidation(p);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // D. executeOperation security gates
    // ─────────────────────────────────────────────────────────────────────────

    function test_executeOperation_revertsWhenNotPool() public {
        vm.prank(alice); // any non-AAVE_POOL caller
        vm.expectRevert(bytes("!pool"));
        liquidator.executeOperation(address(0), 0, 0, address(liquidator), bytes(""));
    }

    function test_executeOperation_revertsWhenInitiatorNotSelf() public {
        vm.prank(STUB_POOL);
        vm.expectRevert(bytes("!initiator"));
        liquidator.executeOperation(address(0), 0, 0, alice, bytes(""));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // E. rescue()
    // ─────────────────────────────────────────────────────────────────────────

    function test_rescue_revertsOnZeroRecipient() public {
        vm.expectRevert(bytes("!to"));
        liquidator.rescue(address(0), address(0), 1 ether);
    }

    function test_rescue_revertsOnZeroAmount() public {
        vm.expectRevert(bytes("!amount"));
        liquidator.rescue(address(0), recipient, 0);
    }

    function test_rescue_transfersErc20() public {
        MockERC20 token = new MockERC20();
        token.mint(address(liquidator), 1_000);

        vm.expectEmit(true, true, false, true);
        emit CharonLiquidator.Rescued(address(token), recipient, 400);

        liquidator.rescue(address(token), recipient, 400);

        assertEq(token.balanceOf(address(liquidator)), 600, "liquidator token balance wrong");
        assertEq(token.balanceOf(recipient), 400, "recipient token balance wrong");
    }

    function test_rescue_transfersNativeBnbToEoa() public {
        vm.deal(address(liquidator), 5 ether);
        uint256 before = recipient.balance;

        vm.expectEmit(true, true, false, true);
        emit CharonLiquidator.Rescued(address(0), recipient, 2 ether);

        liquidator.rescue(address(0), recipient, 2 ether);

        assertEq(recipient.balance - before, 2 ether, "bnb not received");
        assertEq(address(liquidator).balance, 3 ether, "liquidator bnb wrong");
    }

    /// @dev Proves that rescue()'s BNB path uses `call{value}` (full gas) and not
    ///      `transfer` (2300-gas stipend). A contract recipient that writes storage
    ///      in `receive()` would cause `transfer` to fail. Covers issue #117.
    function test_rescue_bnbToGasHungryContractRecipient() public {
        GasHungryReceiver gh = new GasHungryReceiver();
        vm.deal(address(liquidator), 5 ether);
        uint256 before = address(gh).balance;

        liquidator.rescue(address(0), address(gh), 2 ether);

        assertEq(address(gh).balance - before, 2 ether, "bnb not received by contract");
        assertEq(gh.touched(), 1, "recipient fallback did not execute");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // F. No-receive policy — plain BNB sends must revert (issue #117)
    // ─────────────────────────────────────────────────────────────────────────

    function test_directBnbTransferReverts() public {
        (bool ok,) = address(liquidator).call{ value: 1 ether }("");
        assertFalse(ok, "liquidator must refuse plain BNB transfers");
    }
}
