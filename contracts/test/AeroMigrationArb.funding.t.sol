// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {AeroMigrationArb, IAeroMigration} from "../src/AeroMigrationArb.sol";
import {FlashArbBase, ArbStep, IMorphoFlash, IPoolManager} from "../src/FlashArbBase.sol";

contract MockToken {
    mapping(address => uint256) public balanceOf;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address, uint256) external pure returns (bool) {
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

contract NoopMigration {
    function depositLiquid(uint256, address) external payable {}
}

/// Plays Morpho AND the PoolManager (the contract is deployed with this address for both), so the
/// sender checks pass and only the transient funding tag stands between a callback and the arb.
contract EvilFunder {
    enum Mode {
        Silent, // never calls back
        Once, // calls its own callback once (the honest funder)
        Cross, // calls the OTHER funding source's callback
        Twice // calls its own callback twice
    }

    Mode public mode;
    AeroMigrationArb public arb;

    function set(Mode m, AeroMigrationArb a) external {
        (mode, arb) = (m, a);
    }

    function _empty() internal pure returns (ArbStep[] memory) {
        return new ArbStep[](0);
    }

    function flashLoan(address, uint256, bytes calldata data) external {
        if (mode == Mode.Cross) arb.unlockCallback(abi.encode(uint256(0), _empty(), _empty()));
        if (mode == Mode.Once || mode == Mode.Twice) arb.onMorphoFlashLoan(0, data);
        if (mode == Mode.Twice) arb.onMorphoFlashLoan(0, data);
    }

    function unlock(bytes calldata data) external returns (bytes memory) {
        if (mode == Mode.Cross) arb.onMorphoFlashLoan(0, abi.encode(_empty(), _empty()));
        if (mode == Mode.Once || mode == Mode.Twice) arb.unlockCallback(data);
        if (mode == Mode.Twice) arb.unlockCallback(data);
        return "";
    }

    function take(address, address, uint256) external {}
    function sync(address) external {}

    function settle() external payable returns (uint256) {
        return 0;
    }
}

/// Run: forge test --match-contract AeroMigrationArbFunding  (no fork needed)
contract AeroMigrationArbFundingTest is Test {
    address owner = makeAddr("owner");
    address exec = makeAddr("exec");
    address receiver = makeAddr("receiver");
    MockToken loan = new MockToken();
    MockToken oldToken = new MockToken();
    MockToken newToken = new MockToken();
    EvilFunder evil = new EvilFunder();
    AeroMigrationArb arb;

    function setUp() public {
        arb = new AeroMigrationArb(
            IMorphoFlash(address(evil)),
            IPoolManager(address(evil)),
            IAeroMigration(address(new NoopMigration())),
            address(oldToken),
            address(newToken),
            owner,
            exec,
            receiver
        );
        // Something to "migrate", so a first callback gets all the way through the arb body.
        oldToken.mint(address(arb), 1);
    }

    /// Arms the funder, then runs the executor's arb (Morpho- or V4-funded); `err` = 0: must succeed.
    function _run(EvilFunder.Mode m, bool v4, bytes4 err) internal {
        evil.set(m, arb);
        vm.prank(exec);
        if (err != bytes4(0)) vm.expectRevert(err);
        if (v4) arb.arbFlashAccounting(address(loan), 0, new ArbStep[](0), new ArbStep[](0), 0, block.number);
        else arb.arb(address(loan), 0, new ArbStep[](0), new ArbStep[](0), 0, block.number);
    }

    /// Control: one callback from the right source goes all the way through.
    function test_ownCallbackOnceSucceeds() public {
        _run(EvilFunder.Mode.Once, false, 0);
        oldToken.mint(address(arb), 1); // swept to the receiver by the first run
        _run(EvilFunder.Mode.Once, true, 0);
    }

    function test_morphoArbRejectsTheV4Callback() public {
        _run(EvilFunder.Mode.Cross, false, FlashArbBase.Unauthorized.selector);
    }

    function test_v4ArbRejectsTheMorphoCallback() public {
        _run(EvilFunder.Mode.Cross, true, FlashArbBase.Unauthorized.selector);
    }

    function test_morphoCallbackRunsOnce() public {
        _run(EvilFunder.Mode.Twice, false, FlashArbBase.Unauthorized.selector);
    }

    function test_v4CallbackRunsOnce() public {
        _run(EvilFunder.Mode.Twice, true, FlashArbBase.Unauthorized.selector);
    }

    function test_funderThatNeverCallsBackReverts() public {
        _run(EvilFunder.Mode.Silent, false, FlashArbBase.NotFunded.selector);
        _run(EvilFunder.Mode.Silent, true, FlashArbBase.NotFunded.selector);
    }
}
