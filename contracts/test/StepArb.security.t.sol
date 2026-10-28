// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {StepArb} from "../src/StepArb.sol";
import {FlashArbBase, ArbStep, IMorphoFlash, IPoolManager} from "../src/FlashArbBase.sol";

contract SToken {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address s, uint256 a) external returns (bool) {
        allowance[msg.sender][s] = a;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

/// Swaps `a` for `b` 1:2 (pulls `amountIn` of a, pays 2x of b).
contract DoublingRouter {
    SToken immutable A;
    SToken immutable B;

    constructor(SToken a, SToken b) {
        (A, B) = (a, b);
    }

    function swap(uint256 amountIn, address to) external {
        A.transferFrom(msg.sender, address(this), amountIn);
        B.mint(to, 2 * amountIn);
    }
}

/// Converts `b` back into `a` 1:1 (another kind-0 target: a migration, a wrapper...), leaving 1 wei
/// of `b` behind as dust the arb must sweep.
contract Converter {
    SToken immutable A;
    SToken immutable B;

    constructor(SToken a, SToken b) {
        (A, B) = (a, b);
    }

    function convert(uint256 amount, address to) external {
        B.transferFrom(msg.sender, address(this), amount - 1);
        A.mint(to, amount - 1);
    }
}

/// Plays Morpho AND the PoolManager (honestly, unless armed otherwise): lends `loan`, calls back,
/// takes the repayment.
contract Funder {
    enum Mode {
        Honest,
        Silent,
        Cross,
        Twice
    }

    Mode public mode;
    StepArb public arb;
    SToken public loan;

    function set(Mode m, StepArb a, SToken l) external {
        (mode, arb, loan) = (m, a, l);
    }

    function flashLoan(address, uint256 assets, bytes calldata data) external {
        if (mode == Mode.Silent) return;
        if (mode == Mode.Cross) arb.unlockCallback(abi.encode(assets, data));
        loan.mint(address(arb), assets);
        arb.onMorphoFlashLoan(assets, data);
        if (mode == Mode.Twice) arb.onMorphoFlashLoan(assets, data);
        loan.transferFrom(address(arb), address(this), assets);
    }

    function unlock(bytes calldata data) external returns (bytes memory) {
        if (mode == Mode.Silent) return "";
        if (mode == Mode.Cross) arb.onMorphoFlashLoan(0, data);
        arb.unlockCallback(data);
        if (mode == Mode.Twice) arb.unlockCallback(data);
        return "";
    }

    function take(address, address to, uint256 amount) external {
        loan.mint(to, amount);
    }

    function sync(address) external {}

    function settle() external payable returns (uint256) {
        return 0;
    }
}

/// Run: forge test --match-contract StepArbSecurity  (no fork needed)
contract StepArbSecurityTest is Test {
    address owner = makeAddr("owner");
    address exec = makeAddr("exec");
    address receiver = makeAddr("receiver");
    SToken a = new SToken();
    SToken b = new SToken();
    Funder funder = new Funder();
    DoublingRouter router = new DoublingRouter(a, b);
    Converter conv = new Converter(a, b);
    StepArb arb;

    function setUp() public {
        arb = new StepArb(IMorphoFlash(address(funder)), IPoolManager(address(funder)), owner, exec, receiver);
        vm.startPrank(owner);
        arb.setAllowedTarget(address(router), true);
        arb.setAllowedTarget(address(conv), true);
        vm.stopPrank();
        funder.set(Funder.Mode.Honest, arb, a);
    }

    /// a -> b (x2) -> a (1 wei of b left): borrows 100 a, ends with 199 a, repays 100.
    function _steps() internal view returns (ArbStep[] memory s) {
        s = new ArbStep[](2);
        s[0] = ArbStep({kind: 0, target: address(router), tokenIn: address(a), amountIn: 100, patchOffset: 4, data: abi.encodeCall(DoublingRouter.swap, (0, address(arb)))});
        s[1] = ArbStep({kind: 0, target: address(conv), tokenIn: address(b), amountIn: type(uint256).max, patchOffset: 4, data: abi.encodeCall(Converter.convert, (0, address(arb)))});
    }

    function _sweep() internal view returns (address[] memory t) {
        t = new address[](1);
        t[0] = address(b);
    }

    function _run(bool v4, uint256 minProfit) internal returns (uint256) {
        vm.prank(exec);
        return v4 ? arb.arbFlashAccounting(address(a), 100, _steps(), _sweep(), minProfit, block.number) : arb.arb(address(a), 100, _steps(), _sweep(), minProfit, block.number);
    }

    function test_bothFundingSourcesRunTheStepsRepayAndPayTheReceiver() public {
        assertEq(_run(false, 99), 99, "2 x 100 - 1 - 100");
        assertEq(a.balanceOf(receiver), 99);
        assertEq(_run(true, 99), 99);
        assertEq(a.balanceOf(receiver), 198);
        assertEq(b.balanceOf(receiver), 2, "the dust of each run, swept");
        assertEq(a.balanceOf(address(arb)) + b.balanceOf(address(arb)), 0, "nothing at rest");
        assertEq(a.allowance(address(arb), address(router)) + b.allowance(address(arb), address(conv)), 0, "allowances reset");
    }

    function test_leftoversOfTheSweepListGoToTheReceiver() public {
        // Without b in the sweep list its dust would stay; with it, nothing does.
        vm.prank(exec);
        arb.arb(address(a), 100, _steps(), new address[](0), 0, block.number);
        assertEq(b.balanceOf(address(arb)), 1);
        _run(false, 0);
        assertEq(b.balanceOf(address(arb)), 0, "the next run converted the old dust...");
        assertEq(b.balanceOf(receiver), 1, "...and swept the new");
    }

    function test_minProfitIsEnforced() public {
        vm.prank(exec);
        vm.expectRevert(abi.encodeWithSelector(FlashArbBase.InsufficientProfit.selector, 99, 100));
        arb.arb(address(a), 100, _steps(), _sweep(), 100, block.number);
    }

    function test_onlyTheExecutorBeforeTheDeadlineWhileNotPaused() public {
        vm.expectRevert(FlashArbBase.Unauthorized.selector);
        arb.arb(address(a), 100, _steps(), _sweep(), 0, block.number);
        vm.prank(exec);
        vm.expectRevert(FlashArbBase.Expired.selector);
        arb.arb(address(a), 100, _steps(), _sweep(), 0, block.number - 1);
        vm.prank(exec);
        arb.pause();
        vm.prank(exec);
        vm.expectRevert(FlashArbBase.IsPaused.selector);
        arb.arb(address(a), 100, _steps(), _sweep(), 0, block.number);
        vm.expectRevert(FlashArbBase.Unauthorized.selector);
        vm.prank(exec);
        arb.unpause();
    }

    function test_targetsMustBeAllowlistedAndFactoriesAreNotTargets() public {
        vm.prank(owner);
        arb.setAllowedTarget(address(conv), false);
        vm.prank(exec);
        vm.expectRevert(abi.encodeWithSelector(FlashArbBase.TargetNotAllowed.selector, address(conv)));
        arb.arb(address(a), 100, _steps(), _sweep(), 0, block.number);
        vm.prank(owner);
        arb.setAllowedFactory(address(conv), true);
        vm.prank(exec);
        vm.expectRevert(abi.encodeWithSelector(FlashArbBase.TargetNotAllowed.selector, address(conv)));
        arb.arb(address(a), 100, _steps(), _sweep(), 0, block.number);
    }

    function test_unknownKindsAndBadPatchesRevert() public {
        ArbStep[] memory s = _steps();
        s[1].kind = 9;
        vm.prank(exec);
        vm.expectRevert(FlashArbBase.BadKind.selector);
        arb.arb(address(a), 100, s, _sweep(), 0, block.number);
        s = _steps();
        s[0].patchOffset = s[0].data.length;
        vm.prank(exec);
        vm.expectRevert(FlashArbBase.BadPatch.selector);
        arb.arb(address(a), 100, s, _sweep(), 0, block.number);
    }

    function test_callbacksOnlyFromTheirFunderInsideOwnArbOnce() public {
        vm.expectRevert(FlashArbBase.Unauthorized.selector);
        arb.onMorphoFlashLoan(0, abi.encode(_steps()));
        vm.expectRevert(FlashArbBase.Unauthorized.selector);
        vm.prank(address(funder));
        arb.unlockCallback(abi.encode(uint256(0), abi.encode(_steps())));
        for (uint256 m = 2; m <= 3; ++m) {
            funder.set(Funder.Mode(m), arb, a);
            vm.prank(exec);
            vm.expectRevert(FlashArbBase.Unauthorized.selector);
            arb.arb(address(a), 100, _steps(), _sweep(), 0, block.number);
            vm.prank(exec);
            vm.expectRevert(FlashArbBase.Unauthorized.selector);
            arb.arbFlashAccounting(address(a), 100, _steps(), _sweep(), 0, block.number);
        }
        funder.set(Funder.Mode.Silent, arb, a);
        vm.prank(exec);
        vm.expectRevert(FlashArbBase.NotFunded.selector);
        arb.arb(address(a), 100, _steps(), _sweep(), 0, block.number);
    }

    function test_v4DisabledWithoutPoolManager() public {
        StepArb noV4 = new StepArb(IMorphoFlash(address(funder)), IPoolManager(address(0)), owner, exec, receiver);
        vm.prank(exec);
        vm.expectRevert(FlashArbBase.FundingDisabled.selector);
        noV4.arbFlashAccounting(address(a), 100, _steps(), _sweep(), 0, block.number);
    }
}
