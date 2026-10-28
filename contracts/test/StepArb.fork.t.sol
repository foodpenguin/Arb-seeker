// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {StepArb} from "../src/StepArb.sol";
import {AeroMigrationArb, IAeroMigration} from "../src/AeroMigrationArb.sol";
import {FlashArbBase, ArbStep, IMorphoFlash, IPoolManager} from "../src/FlashArbBase.sol";
import {FixedRateRouter, IERC20T, ILegacyFactoryT} from "./AeroMigrationArb.fork.t.sol";

/// Run: forge test --match-contract StepArbFork -vv  (BASE_RPC_URL optional, FORK_BLOCK optional)
/// The Aero merger arb (buy old AERO on the legacy pool, migrate, sell new AERO) written as plain
/// steps for the generic StepArb, on a Base fork: the migration is just a kind-0 step. It must earn
/// exactly what the dedicated AeroMigrationArb earns on the same route, with both funding sources.
contract StepArbFork is Test {
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    uint256 constant LIQUID_OPEN = 1792627200; // 2026-10-22 00:00 UTC
    string constant CONFIG = "../bot/config.example.toml";
    address MORPHO;
    address POOL_MANAGER;
    address MIGRATION;
    address OLD;
    address NEW;
    address V2_FACTORY;
    address LEGACY_AERO_USDC;

    address owner = makeAddr("owner");
    address exec = makeAddr("exec");
    address receiver = makeAddr("receiver");
    StepArb arb;
    AeroMigrationArb dedicated;
    FixedRateRouter router;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://base.gateway.tenderly.co"));
        uint256 forkBlock = vm.envOr("FORK_BLOCK", uint256(53206447));
        if (forkBlock == 0) vm.createSelectFork(rpc);
        else vm.createSelectFork(rpc, forkBlock);
        if (block.timestamp < LIQUID_OPEN) vm.warp(LIQUID_OPEN + 60);
        string memory toml = vm.readFile(CONFIG);
        MORPHO = vm.parseTomlAddress(toml, ".aero.morpho");
        POOL_MANAGER = vm.parseTomlAddress(toml, ".aero.pool_manager");
        MIGRATION = vm.parseTomlAddress(toml, ".aero.migration");
        OLD = vm.parseTomlAddress(toml, ".aero.old_token");
        NEW = vm.parseTomlAddress(toml, ".aero.new_token");
        V2_FACTORY = vm.parseTomlAddress(toml, ".aero.legacy_factory");
        LEGACY_AERO_USDC = ILegacyFactoryT(V2_FACTORY).getPool(OLD, USDC, false);

        arb = new StepArb(IMorphoFlash(MORPHO), IPoolManager(POOL_MANAGER), owner, exec, receiver);
        dedicated = new AeroMigrationArb(IMorphoFlash(MORPHO), IPoolManager(POOL_MANAGER), IAeroMigration(MIGRATION), OLD, NEW, owner, exec, receiver);
        router = new FixedRateRouter(NEW, USDC, 0.95e6);
        deal(USDC, address(router), 10_000_000e6);
        vm.startPrank(owner);
        for (uint256 i; i < 2; ++i) {
            FlashArbBase c = i == 0 ? FlashArbBase(arb) : FlashArbBase(dedicated);
            c.setAllowedFactory(V2_FACTORY, true);
            c.setAllowedTarget(address(router), true);
        }
        arb.setAllowedTarget(MIGRATION, true);
        vm.stopPrank();
    }

    function _buy(uint256 usdcIn) internal view returns (ArbStep memory) {
        return ArbStep({kind: 1, target: LEGACY_AERO_USDC, tokenIn: USDC, amountIn: usdcIn, patchOffset: 0, data: ""});
    }

    function _sell(address to) internal view returns (ArbStep memory) {
        bytes memory d = abi.encodeCall(FixedRateRouter.swap, (0, to));
        return ArbStep({kind: 0, target: address(router), tokenIn: NEW, amountIn: type(uint256).max, patchOffset: 4, data: d});
    }

    /// The migration as a step: approve all old AERO to the Migration, depositLiquid(all, arb).
    function _migrate() internal view returns (ArbStep memory) {
        bytes memory d = abi.encodeCall(IAeroMigration.depositLiquid, (0, address(arb)));
        return ArbStep({kind: 0, target: MIGRATION, tokenIn: OLD, amountIn: type(uint256).max, patchOffset: 4, data: d});
    }

    function _steps(uint256 loan) internal view returns (ArbStep[] memory s) {
        s = new ArbStep[](3);
        (s[0], s[1], s[2]) = (_buy(loan), _migrate(), _sell(address(arb)));
    }

    function _one(ArbStep memory a) internal pure returns (ArbStep[] memory s) {
        s = new ArbStep[](1);
        s[0] = a;
    }

    function _sweep() internal view returns (address[] memory t) {
        t = new address[](2);
        (t[0], t[1]) = (OLD, NEW);
    }

    function test_theMigrationArbAsStepsEarnsWhatTheDedicatedContractEarns() public {
        uint256 loan = 1_000e6;
        for (uint256 v4; v4 < 2; ++v4) {
            uint256 snap = vm.snapshotState();
            vm.prank(exec);
            uint256 viaSteps = v4 == 1
                ? arb.arbFlashAccounting(USDC, loan, _steps(loan), _sweep(), 1e6, block.number)
                : arb.arb(USDC, loan, _steps(loan), _sweep(), 1e6, block.number);
            assertEq(IERC20T(USDC).balanceOf(receiver), viaSteps);
            assertEq(IERC20T(OLD).balanceOf(address(arb)) + IERC20T(NEW).balanceOf(address(arb)) + IERC20T(USDC).balanceOf(address(arb)), 0, "nothing at rest");
            vm.revertToState(snap);
            vm.prank(exec);
            uint256 viaDedicated = v4 == 1
                ? dedicated.arbFlashAccounting(USDC, loan, _one(_buy(loan)), _one(_sell(address(dedicated))), 1e6, block.number)
                : dedicated.arb(USDC, loan, _one(_buy(loan)), _one(_sell(address(dedicated))), 1e6, block.number);
            assertGt(viaSteps, 50e6);
            assertEq(viaSteps, viaDedicated, "same route, same profit");
            vm.revertToState(snap);
        }
    }

    function test_theMigrationIsATargetLikeAnyOther() public {
        vm.prank(owner);
        arb.setAllowedTarget(MIGRATION, false);
        vm.prank(exec);
        vm.expectRevert(abi.encodeWithSelector(FlashArbBase.TargetNotAllowed.selector, MIGRATION));
        arb.arb(USDC, 1_000e6, _steps(1_000e6), _sweep(), 0, block.number);
    }

    function test_anUnprofitableRouteReverts() public {
        vm.prank(exec);
        vm.expectRevert();
        arb.arb(USDC, 1_000e6, _steps(1_000e6), _sweep(), 1_000e6, block.number);
    }
}
