// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {AeroMigrationArb, ArbStep, IMorphoFlash, IPoolManager, IAeroMigration} from "../src/AeroMigrationArb.sol";

interface IERC20T {
    function balanceOf(address) external view returns (uint256);
    function approve(address, uint256) external returns (bool);
    function transfer(address, uint256) external returns (bool);
    function transferFrom(address, address, uint256) external returns (bool);
}

interface ILegacyFactoryT {
    function getPool(address a, address b, bool stable) external view returns (address);
}

interface IMigrationT {
    function V2_ESCROW() external view returns (address);
}

/// Sell-side stand-in until the new Aero pools exist: pays `rateE18` USDC units per new-AERO wei
/// (exact input, pulls `amountIn` via transferFrom like a router).
contract FixedRateRouter {
    address immutable IN;
    address immutable OUT;
    uint256 public rateE18;

    constructor(address in_, address out_, uint256 rate) {
        IN = in_;
        OUT = out_;
        rateE18 = rate;
    }

    function swap(uint256 amountIn, address to) external returns (uint256 out) {
        IERC20T(IN).transferFrom(msg.sender, address(this), amountIn);
        out = amountIn * rateE18 / 1e18;
        IERC20T(OUT).transfer(to, out);
    }
}

interface IPoolFactoryT {
    function createPool(address a, address b) external returns (address);
    function getPool(address a, address b) external view returns (address);
}

interface IPoolT {
    function mint(address to) external returns (uint256);
}

interface IClFactoryT {
    function createPool(address a, address b, int24 tickSpacing, uint160 sqrtPriceX96) external returns (address);
    function getPool(address a, address b, int24 tickSpacing) external view returns (address);
}

interface IClPoolT {
    function mint(address recipient, int24 tickLower, int24 tickUpper, uint128 amount, bytes calldata data)
        external
        returns (uint256, uint256);
}

/// Claims to be a CL pool of an allowlisted factory; the factory's getPool returns the real one.
contract FakeClPool {
    address public factory;
    address public token0;
    address public token1;
    int24 public tickSpacing;

    constructor(address f, address a, address b, int24 ts) {
        factory = f;
        (token0, token1) = a < b ? (a, b) : (b, a);
        tickSpacing = ts;
    }

    function swap(address, bool, int256, uint160, bytes calldata) external pure returns (int256, int256) {
        return (0, 0);
    }
}

/// Claims an allowlisted factory but is not one of its pools.
contract FakePool {
    address public factory;

    constructor(address f) {
        factory = f;
    }

    function token0() external pure returns (address) {
        return address(0);
    }

    function getAmountOut(uint256, address) external pure returns (uint256) {
        return 1;
    }
}

/// Run: forge test --match-contract AeroMigrationArbFork -vv  (BASE_RPC_URL optional, FORK_BLOCK optional)
/// Forks Base and warps past the liquid-migration opening (2026-10-22 00:00 UTC), so it exercises
/// the real Migration.depositLiquid and the real legacy Aerodrome pool; only the new-AERO sale is
/// a stand-in (no new pools existed before launch).
contract AeroMigrationArbFork is Test {
    address MORPHO;
    address POOL_MANAGER;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    uint256 constant LIQUID_OPEN = 1792627200; // 2026-10-22 00:00 UTC
    /// Aero / legacy Aerodrome addresses come from the bot config ([aero]), never from this file
    /// (CLAUDE.md: Aerodrome's contracts change with the Aero merger); the rest is derived on-chain.
    string constant CONFIG = "../bot/config.example.toml";
    address MIGRATION;
    address OLD;
    address NEW;
    address V2_FACTORY; // legacy Aerodrome V2 PoolFactory
    address AERO_VOLATILE_FACTORY;
    address[] NEW_FACTORIES; // Aero V2 + CL
    address[] LEGACY_CL_FACTORIES; // legacy Slipstream
    address LEGACY_AERO_USDC;
    address VE; // legacy veAERO (holds the migration lock)

    address owner = makeAddr("owner");
    address exec = makeAddr("exec");
    address receiver = makeAddr("receiver");
    AeroMigrationArb arb;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://base.gateway.tenderly.co"));
        uint256 forkBlock = vm.envOr("FORK_BLOCK", uint256(0));
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
        AERO_VOLATILE_FACTORY = vm.parseTomlAddressArray(toml, ".aero.new_factories")[0];
        NEW_FACTORIES = vm.parseTomlAddressArray(toml, ".aero.new_factories");
        LEGACY_CL_FACTORIES = vm.parseTomlAddressArray(toml, ".aero.legacy_cl_factories");
        LEGACY_AERO_USDC = ILegacyFactoryT(V2_FACTORY).getPool(OLD, USDC, false);
        VE = IMigrationT(MIGRATION).V2_ESCROW();
        arb = new AeroMigrationArb(
            IMorphoFlash(MORPHO), IPoolManager(POOL_MANAGER), IAeroMigration(MIGRATION), OLD, NEW, owner, exec, receiver
        );
        vm.startPrank(owner);
        arb.setAllowedTarget(V2_FACTORY, true);
        vm.stopPrank();
    }

    /// Buy old AERO straight on the legacy AERO/USDC volatile pool (kind 1).
    function _buy(uint256 usdcIn) internal view returns (ArbStep memory) {
        return ArbStep({kind: 1, target: LEGACY_AERO_USDC, tokenIn: USDC, amountIn: usdcIn, patchOffset: 0, data: ""});
    }

    /// Sell all migrated new AERO; amountIn is patched in at its ABI offset (4 + 0).
    function _sell(FixedRateRouter r) internal view returns (ArbStep memory) {
        bytes memory d = abi.encodeCall(FixedRateRouter.swap, (0, address(arb)));
        return ArbStep({kind: 0, target: address(r), tokenIn: NEW, amountIn: type(uint256).max, patchOffset: 4, data: d});
    }

    function _router(uint256 rateE18) internal returns (FixedRateRouter r) {
        r = new FixedRateRouter(NEW, USDC, rateE18);
        deal(USDC, address(r), 10_000_000e6);
        vm.prank(owner);
        arb.setAllowedTarget(address(r), true);
    }

    function _steps(ArbStep memory a) internal pure returns (ArbStep[] memory s) {
        s = new ArbStep[](1);
        s[0] = a;
    }

    function test_profitableArbMigratesAndPaysReceiver() public {
        // New AERO bid 10% above old's current price (~$0.86): 1 AERO (1e18) -> 0.95 USDC (0.95e6).
        FixedRateRouter r = _router(0.95e6);
        uint256 loan = 1_000e6;
        uint256 migratedBefore = IERC20T(OLD).balanceOf(VE);
        vm.prank(exec);
        uint256 profit = arb.arb(USDC, loan, _steps(_buy(loan)), _steps(_sell(r)), 1e6, block.number);
        assertGt(profit, 50e6, "~10% of 1000 USDC minus pool fee/impact");
        assertEq(IERC20T(USDC).balanceOf(receiver), profit);
        assertEq(IERC20T(USDC).balanceOf(address(arb)), 0, "nothing at rest");
        assertEq(IERC20T(OLD).balanceOf(address(arb)), 0);
        assertEq(IERC20T(NEW).balanceOf(address(arb)), 0);
        // The old AERO really went into the migration lock (held by the old VotingEscrow).
        assertGt(IERC20T(OLD).balanceOf(VE), migratedBefore);
    }

    /// Both legs straight on Solidly pools: buy on the legacy AERO/USDC vAMM (kind 1), sell on a
    /// new Aero volatile NEW/USDC pool created on the fork (kind 2), priced 10% above old.
    function test_directPoolsBothLegs() public {
        address pool = IPoolFactoryT(AERO_VOLATILE_FACTORY).getPool(NEW, USDC);
        if (pool == address(0)) pool = IPoolFactoryT(AERO_VOLATILE_FACTORY).createPool(NEW, USDC);
        // Added on top of whatever the pool holds (deal on the pool itself would break its reserves).
        deal(NEW, address(this), 1_000_000e18);
        deal(USDC, address(this), 950_000e6);
        IERC20T(NEW).transfer(pool, 1_000_000e18);
        IERC20T(USDC).transfer(pool, 950_000e6);
        IPoolT(pool).mint(address(this));
        vm.startPrank(owner);
        arb.setAllowedTarget(V2_FACTORY, true);
        arb.setAllowedTarget(AERO_VOLATILE_FACTORY, true);
        vm.stopPrank();
        uint256 loan = 1_000e6;
        ArbStep memory b = ArbStep({kind: 1, target: LEGACY_AERO_USDC, tokenIn: USDC, amountIn: loan, patchOffset: 0, data: ""});
        ArbStep memory s = ArbStep({kind: 2, target: pool, tokenIn: NEW, amountIn: type(uint256).max, patchOffset: 0, data: ""});
        vm.prank(exec);
        uint256 profit = arb.arb(USDC, loan, _steps(b), _steps(s), 1e6, block.number);
        // ~96 USDC before the real pool had liquidity; ~8.5 USDC on 2026-10-28 (it is arbed now).
        assertGt(profit, 1e6, "profitable");
        assertEq(IERC20T(USDC).balanceOf(receiver), profit);
        assertEq(IERC20T(NEW).balanceOf(address(arb)), 0);
        emit log_named_decimal_uint("direct-pools profit on 1000 USDC", profit, 6);
    }

    /// A NEW/USDC CL pool created on the fork at ~$0.95 (~10% above old): the first (Aero CL factory
    /// from the config, tick spacing) with no pool there yet that accepts one (real pools exist at
    /// other prices). Returns (pool, factory).
    function _freshClPool() internal returns (address pool, address factory) {
        return _freshClPoolWith(1.5e18, -300000, -250000);
    }

    /// Same, with liquidity `liq` on [lo, hi] (rounded inward to the tick spacing).
    function _freshClPoolWith(uint128 liq, int24 lo, int24 hi) internal returns (address pool, address factory) {
        int24[5] memory spacings = [int24(200), 250, 150, 50, 10];
        for (uint256 i; i < NEW_FACTORIES.length && pool == address(0); ++i) {
            address f = NEW_FACTORIES[i];
            try IClFactoryT(f).getPool(NEW, USDC, 200) returns (address) {} catch { continue; } // not a CL factory
            for (uint256 j; j < spacings.length && pool == address(0); ++j) {
                if (IClFactoryT(f).getPool(NEW, USDC, spacings[j]) != address(0)) continue;
                try IClFactoryT(f).createPool(NEW, USDC, spacings[j], 77222060634363710668800) returns (address p) {
                    (pool, factory) = (p, f);
                    // [-300000, -250000] rounded inward to the spacing.
                    int24 ts = spacings[j];
                    deal(NEW, address(this), 5_000_000e18);
                    deal(USDC, address(this), 5_000_000e6);
                    mintPool = p;
                    IClPoolT(p).mint(address(this), lo / ts * ts, hi / ts * ts, liq, "");
                } catch {}
            }
        }
        require(pool != address(0), "no configured CL factory accepts a fresh pool");
    }

    /// Both legs straight on CL pools (kind 3): buy old AERO on the legacy Slipstream AERO/USDC
    /// pool (tick spacing 100), sell new AERO on a fresh Aero CL pool. No routers involved.
    function test_clDirectBothLegs() public {
        (address sellPool, address sellFactory) = _freshClPool();
        address legacyCl = LEGACY_CL_FACTORIES[0];
        address buyPool = IClFactoryT(legacyCl).getPool(OLD, USDC, 100);
        require(buyPool != address(0), "legacy AERO/USDC cl100 pool");
        vm.startPrank(owner);
        arb.setAllowedTarget(legacyCl, true);
        arb.setAllowedTarget(sellFactory, true);
        vm.stopPrank();
        uint256 loan = 1_000e6;
        ArbStep memory b = ArbStep({kind: 3, target: buyPool, tokenIn: USDC, amountIn: loan, patchOffset: 0, data: ""});
        ArbStep memory s = ArbStep({kind: 3, target: sellPool, tokenIn: NEW, amountIn: type(uint256).max, patchOffset: 0, data: ""});
        vm.prank(exec);
        uint256 profit = arb.arb(USDC, loan, _steps(b), _steps(s), 1e6, block.number);
        assertGt(profit, 10e6);
        assertEq(IERC20T(NEW).balanceOf(address(arb)), 0, "whole migrated balance sold");
        assertEq(IERC20T(USDC).balanceOf(address(arb)), 0, "nothing at rest");
        emit log_named_decimal_uint("CL-direct profit on 1000 USDC", profit, 6);
    }

    /// Split legs: buy on the legacy V2 pool AND the legacy CL pool, sell on the Aero V2 pool AND a
    /// fresh CL pool (fixed amounts first, the last leg takes the remainder).
    function test_splitLegsAcrossV2AndCl() public {
        (address sellPool, address sellFactory) = _freshClPool();
        address legacyCl = LEGACY_CL_FACTORIES[0];
        address buyCl = IClFactoryT(legacyCl).getPool(OLD, USDC, 100);
        vm.startPrank(owner);
        arb.setAllowedTarget(legacyCl, true);
        arb.setAllowedTarget(sellFactory, true);
        vm.stopPrank();
        ArbStep[] memory b = new ArbStep[](2);
        b[0] = ArbStep({kind: 3, target: buyCl, tokenIn: USDC, amountIn: 400e6, patchOffset: 0, data: ""});
        b[1] = ArbStep({kind: 1, target: LEGACY_AERO_USDC, tokenIn: USDC, amountIn: type(uint256).max, patchOffset: 0, data: ""});
        ArbStep[] memory s = new ArbStep[](1);
        s[0] = ArbStep({kind: 3, target: sellPool, tokenIn: NEW, amountIn: type(uint256).max, patchOffset: 0, data: ""});
        vm.prank(exec);
        uint256 profit = arb.arb(USDC, 1_000e6, b, s, 1e6, block.number);
        assertGt(profit, 10e6);
        assertEq(IERC20T(USDC).balanceOf(address(arb)), 0);
        assertEq(IERC20T(OLD).balanceOf(address(arb)), 0);
        emit log_named_decimal_uint("split-legs profit on 1000 USDC", profit, 6);
    }

    /// A CL leg that only partly fills (its pool runs out of liquidity) leaves its input behind;
    /// the last leg (amountIn = max) sells it, so nothing stays in the contract.
    function test_partialClFillRollsIntoTheLastLeg() public {
        // Tiny pool: a sliver of liquidity just around the ~$0.95 price (tick ~ -276836).
        (address tiny, address f1) = _freshClPoolWith(1e14, -277000, -276600);
        (address deep, address f2) = _freshClPool();
        assertTrue(tiny != deep);
        vm.startPrank(owner);
        arb.setAllowedTarget(f1, true);
        arb.setAllowedTarget(f2, true);
        vm.stopPrank();
        ArbStep[] memory s = new ArbStep[](2);
        s[0] = ArbStep({kind: 3, target: tiny, tokenIn: NEW, amountIn: 500e18, patchOffset: 0, data: ""});
        s[1] = ArbStep({kind: 3, target: deep, tokenIn: NEW, amountIn: type(uint256).max, patchOffset: 0, data: ""});
        uint256 tinyBefore = IERC20T(NEW).balanceOf(tiny);
        vm.prank(exec);
        uint256 profit = arb.arb(USDC, 1_000e6, _steps(_buy(1_000e6)), s, 1e6, block.number);
        uint256 intoTiny = IERC20T(NEW).balanceOf(tiny) - tinyBefore;
        assertGt(intoTiny, 0, "the tiny pool took something");
        assertLt(intoTiny, 500e18, "it only partly filled");
        assertGt(profit, 10e6);
        assertEq(IERC20T(NEW).balanceOf(address(arb)), 0, "the remainder went to the last leg");
        assertEq(IERC20T(USDC).balanceOf(address(arb)), 0, "nothing at rest");
        emit log_named_decimal_uint("partial-fill run: new AERO into the tiny pool", intoTiny, 18);
    }

    /// A contract posing as a CL pool of an allowlisted factory is refused (getPool disagrees).
    function test_fakeClPoolRejected() public {
        address legacyCl = LEGACY_CL_FACTORIES[0];
        vm.prank(owner);
        arb.setAllowedTarget(legacyCl, true);
        FakeClPool fake = new FakeClPool(legacyCl, OLD, USDC, 100);
        ArbStep memory b = ArbStep({kind: 3, target: address(fake), tokenIn: USDC, amountIn: 1_000e6, patchOffset: 0, data: ""});
        vm.prank(exec);
        vm.expectRevert(); // TargetNotAllowed, wrapped by Morpho's callback revert
        arb.arb(USDC, 1_000e6, _steps(b), new ArbStep[](0), 0, block.number);
    }

    /// Nobody but the pool being swapped right now may trigger the payment callback.
    function test_clCallbackOnlyFromActivePool() public {
        deal(USDC, address(arb), 1_000e6); // even with funds sitting there
        vm.expectRevert(AeroMigrationArb.Unauthorized.selector);
        arb.uniswapV3SwapCallback(0, 1_000e6, "");
    }

    address mintPool;

    function uniswapV3MintCallback(uint256 owed0, uint256 owed1, bytes calldata) external {
        require(msg.sender == mintPool, "pool");
        if (owed0 > 0) IERC20T(NEW).transfer(msg.sender, owed0); // token0 = NEW (lower address)
        if (owed1 > 0) IERC20T(USDC).transfer(msg.sender, owed1);
    }

    function test_fakePoolRejected() public {
        vm.prank(owner);
        arb.setAllowedTarget(V2_FACTORY, true);
        FakePool fake = new FakePool(V2_FACTORY);
        ArbStep memory b = ArbStep({kind: 1, target: address(fake), tokenIn: USDC, amountIn: 1_000e6, patchOffset: 0, data: ""});
        vm.prank(exec);
        vm.expectRevert(); // TargetNotAllowed (isPool false), wrapped by Morpho's callback revert
        arb.arb(USDC, 1_000e6, _steps(b), new ArbStep[](0), 0, block.number);
    }

    function test_unprofitableArbReverts() public {
        FixedRateRouter r = _router(0.30e6); // far below the AERO price (0.80 stopped losing at ~$0.69 on 10/27)
        vm.prank(exec);
        vm.expectRevert();
        arb.arb(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 1, block.number);
    }

    function test_minProfitEnforced() public {
        FixedRateRouter r = _router(0.95e6);
        vm.prank(exec);
        vm.expectRevert(); // InsufficientProfit
        arb.arb(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 500e6, block.number);
    }

    function test_onlyExecutor() public {
        FixedRateRouter r = _router(0.95e6);
        vm.expectRevert(AeroMigrationArb.Unauthorized.selector);
        arb.arb(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 0, block.number);
    }

    function test_callbackOnlyInsideOwnArb() public {
        vm.prank(MORPHO);
        vm.expectRevert(AeroMigrationArb.Unauthorized.selector);
        arb.onMorphoFlashLoan(1, abi.encode(new ArbStep[](0), new ArbStep[](0)));
    }

    function test_targetMustBeAllowed() public {
        FixedRateRouter r = new FixedRateRouter(NEW, USDC, 0.95e6); // not allowlisted
        vm.prank(exec);
        vm.expectRevert(); // TargetNotAllowed (wrapped by Morpho's callback revert)
        arb.arb(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 0, block.number);
    }

    function test_expiredAndPaused() public {
        FixedRateRouter r = _router(0.95e6);
        vm.prank(exec);
        vm.expectRevert(AeroMigrationArb.Expired.selector);
        arb.arb(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 0, block.number - 1);
        vm.prank(exec);
        arb.pause();
        vm.prank(exec);
        vm.expectRevert(AeroMigrationArb.IsPaused.selector);
        arb.arb(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 0, block.number);
        vm.prank(exec);
        vm.expectRevert(AeroMigrationArb.Unauthorized.selector);
        arb.unpause();
    }

    function test_migrationNotOpenBeforeLaunch() public {
        vm.warp(LIQUID_OPEN - 3600);
        FixedRateRouter r = _router(0.95e6);
        vm.prank(exec);
        vm.expectRevert(); // MigrationNotOpen
        arb.arb(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 0, block.number);
    }

    // ---------------------------------------------------------------- V4 flash accounting

    /// The same arb funded by the real V4 PoolManager: it gets back exactly what it lent, the profit
    /// is the Morpho-funded one to the unit (both are 0 fee), and gas is logged side by side.
    function test_v4FundingRepaysAndMatchesMorpho() public {
        FixedRateRouter r = _router(0.95e6);
        uint256 loan = 1_000e6;
        uint256 pmBefore = IERC20T(USDC).balanceOf(POOL_MANAGER);
        uint256 s = vm.snapshotState();
        vm.prank(exec);
        uint256 g = gasleft();
        uint256 viaMorpho = arb.arb(USDC, loan, _steps(_buy(loan)), _steps(_sell(r)), 1e6, block.number);
        uint256 gasMorpho = g - gasleft();
        vm.revertToState(s);

        vm.prank(exec);
        g = gasleft();
        uint256 viaV4 = arb.arbFlashAccounting(USDC, loan, _steps(_buy(loan)), _steps(_sell(r)), 1e6, block.number);
        uint256 gasV4 = g - gasleft();
        assertEq(viaV4, viaMorpho, "same arb, same profit");
        assertGt(viaV4, 50e6);
        assertEq(IERC20T(USDC).balanceOf(POOL_MANAGER), pmBefore, "PoolManager repaid exactly");
        assertEq(IERC20T(USDC).balanceOf(receiver), viaV4);
        assertEq(IERC20T(USDC).balanceOf(address(arb)), 0, "nothing at rest");
        assertEq(IERC20T(NEW).balanceOf(address(arb)), 0);
        emit log_named_uint("gas, Morpho flash loan", gasMorpho);
        emit log_named_uint("gas, V4 flash accounting", gasV4);
    }

    /// Direct pools on both legs under V4 funding (no router; nothing re-enters the PoolManager).
    function test_v4FundingDirectClPools() public {
        (address sellPool, address sellFactory) = _freshClPool();
        address legacyCl = LEGACY_CL_FACTORIES[0];
        address buyPool = IClFactoryT(legacyCl).getPool(OLD, USDC, 100);
        vm.startPrank(owner);
        arb.setAllowedTarget(legacyCl, true);
        arb.setAllowedTarget(sellFactory, true);
        vm.stopPrank();
        ArbStep memory b = ArbStep({kind: 3, target: buyPool, tokenIn: USDC, amountIn: 1_000e6, patchOffset: 0, data: ""});
        ArbStep memory sl = ArbStep({kind: 3, target: sellPool, tokenIn: NEW, amountIn: type(uint256).max, patchOffset: 0, data: ""});
        vm.prank(exec);
        uint256 profit = arb.arbFlashAccounting(USDC, 1_000e6, _steps(b), _steps(sl), 1e6, block.number);
        assertGt(profit, 10e6);
        assertEq(IERC20T(USDC).balanceOf(address(arb)), 0);
    }

    function test_v4MinProfitEnforced() public {
        FixedRateRouter r = _router(0.95e6);
        vm.prank(exec);
        vm.expectRevert(); // InsufficientProfit
        arb.arbFlashAccounting(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 500e6, block.number);
    }

    function test_v4UnprofitableReverts() public {
        FixedRateRouter r = _router(0.30e6);
        vm.prank(exec);
        vm.expectRevert(); // the sale cannot pay the PoolManager back
        arb.arbFlashAccounting(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 1, block.number);
    }

    function test_v4OnlyExecutorExpiredPaused() public {
        FixedRateRouter r = _router(0.95e6);
        vm.expectRevert(AeroMigrationArb.Unauthorized.selector);
        arb.arbFlashAccounting(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 0, block.number);
        vm.prank(exec);
        vm.expectRevert(AeroMigrationArb.Expired.selector);
        arb.arbFlashAccounting(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 0, block.number - 1);
        vm.prank(exec);
        arb.pause();
        vm.prank(exec);
        vm.expectRevert(AeroMigrationArb.IsPaused.selector);
        arb.arbFlashAccounting(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 0, block.number);
    }

    function test_v4TargetMustBeAllowed() public {
        FixedRateRouter r = new FixedRateRouter(NEW, USDC, 0.95e6); // not allowlisted
        vm.prank(exec);
        vm.expectRevert(); // TargetNotAllowed (wrapped by the PoolManager's callback revert)
        arb.arbFlashAccounting(USDC, 1_000e6, _steps(_buy(1_000e6)), _steps(_sell(r)), 0, block.number);
    }

    /// The unlock callback: not from anyone else, and not from the real PoolManager outside our arb
    /// (another contract's unlock never calls us back: V4 calls back unlock's caller).
    function test_v4CallbackOnlyFromPoolManagerInsideOwnArb() public {
        deal(USDC, address(arb), 1_000e6); // even with funds sitting there
        bytes memory data = abi.encode(uint256(1_000e6), new ArbStep[](0), new ArbStep[](0));
        vm.expectRevert(AeroMigrationArb.Unauthorized.selector);
        arb.unlockCallback(data);
        vm.prank(POOL_MANAGER);
        vm.expectRevert(AeroMigrationArb.Unauthorized.selector);
        arb.unlockCallback(data);
    }

    function test_v4DisabledWithoutPoolManager() public {
        AeroMigrationArb noV4 = new AeroMigrationArb(
            IMorphoFlash(MORPHO), IPoolManager(address(0)), IAeroMigration(MIGRATION), OLD, NEW, owner, exec, receiver
        );
        vm.prank(exec);
        vm.expectRevert(AeroMigrationArb.FundingDisabled.selector);
        noV4.arbFlashAccounting(USDC, 1_000e6, new ArbStep[](0), new ArbStep[](0), 0, block.number);
    }
}
