// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {FlashArbBase, ArbStep, IERC20Bal, IMorphoFlash, IPoolManager} from "./FlashArbBase.sol";

interface IAeroMigration {
    /// @notice Old AERO -> new AERO, 1:1, sent to `recipient` (Aero merger, Base).
    function depositLiquid(uint256 amount, address recipient) external payable;
}

/// @title AeroMigrationArb
/// @notice Old AERO converts 1:1 into new AERO (Migration.depositLiquid, permissionless). When old
///         AERO trades below new AERO, this buys old with flash-borrowed capital, migrates, sells
///         new and repays, all in one tx, without capital.
/// @dev Funding, steps and the security model are FlashArbBase's. Two entry points with the same
///      arguments (so the same minProfit calldata offset) pick the funding: `arb` (Morpho Blue
///      flash loan) and `arbFlashAccounting` (Uniswap V4). The migration is built in between the
///      buy and sell steps; leftover old / new AERO goes to PROFIT_RECEIVER. The same arb also runs
///      on the generic StepArb, with the migration as a kind-0 step.
contract AeroMigrationArb is FlashArbBase {
    IAeroMigration public immutable MIGRATION;
    address public immutable OLD_TOKEN;
    address public immutable NEW_TOKEN;

    uint256 private transient migratedInFlight;

    error NothingBought();

    event Arb(address indexed loanToken, uint256 loanAmount, uint256 migrated, uint256 profit);

    constructor(
        IMorphoFlash morpho,
        IPoolManager poolManager,
        IAeroMigration migration,
        address oldToken,
        address newToken,
        address owner_,
        address executor_,
        address profitReceiver_
    ) FlashArbBase(morpho, poolManager, owner_, executor_, profitReceiver_) {
        MIGRATION = migration;
        OLD_TOKEN = oldToken;
        NEW_TOKEN = newToken;
    }

    /// @notice Funded by a Morpho Blue flash loan.
    /// @param loanToken Flash-borrowed (e.g. USDC); profit is counted in it.
    /// @param buy Steps turning `loanAmount` of loanToken into old AERO.
    /// @param sell Steps turning the migrated new AERO back into loanToken.
    /// @param minProfit Minimum loanToken left after repaying the flash loan; reverts below it.
    /// @param deadlineBlock Reverts if included after this block (stale opportunity).
    function arb(
        address loanToken,
        uint256 loanAmount,
        ArbStep[] calldata buy,
        ArbStep[] calldata sell,
        uint256 minProfit,
        uint256 deadlineBlock
    ) external returns (uint256 profit) {
        _flash(FUND_MORPHO, loanToken, loanAmount, abi.encode(buy, sell), deadlineBlock);
        profit = _done(loanToken, loanAmount, minProfit);
    }

    /// @notice Same as `arb`, funded by Uniswap V4 flash accounting (take from the PoolManager,
    ///         settle before unlock returns).
    function arbFlashAccounting(
        address loanToken,
        uint256 loanAmount,
        ArbStep[] calldata buy,
        ArbStep[] calldata sell,
        uint256 minProfit,
        uint256 deadlineBlock
    ) external returns (uint256 profit) {
        _flash(FUND_V4, loanToken, loanAmount, abi.encode(buy, sell), deadlineBlock);
        profit = _done(loanToken, loanAmount, minProfit);
    }

    function _done(address loanToken, uint256 loanAmount, uint256 minProfit) private returns (uint256 profit) {
        profit = _finish(loanToken, minProfit);
        // Dust the steps did not spend (exact-input routers, rounding).
        _sweep(OLD_TOKEN);
        _sweep(NEW_TOKEN);
        emit Arb(loanToken, loanAmount, migratedInFlight, profit);
    }

    /// @dev loan token -> old AERO -> migrate -> new AERO -> loan token.
    function _body(bytes memory body) internal override {
        (ArbStep[] memory buy, ArbStep[] memory sell) = abi.decode(body, (ArbStep[], ArbStep[]));
        _runSteps(0, buy);
        uint256 migrated = IERC20Bal(OLD_TOKEN).balanceOf(address(this));
        if (migrated == 0) revert NothingBought();
        _forceApprove(OLD_TOKEN, address(MIGRATION), migrated);
        MIGRATION.depositLiquid(migrated, address(this));
        _forceApprove(OLD_TOKEN, address(MIGRATION), 0);
        migratedInFlight = migrated;
        _runSteps(1, sell);
    }
}
