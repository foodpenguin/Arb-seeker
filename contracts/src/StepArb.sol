// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {FlashArbBase, ArbStep, IMorphoFlash, IPoolManager} from "./FlashArbBase.sol";

/// @title StepArb
/// @notice A generic flash-funded atomic arb: borrow the loan token, run the steps, repay, keep at
///         least `minProfit`. Whatever the strategy (a cross-pool spread, a 1:1 conversion such as a
///         token migration or a wrapper, a triangle), it is expressed as steps: router / target calls
///         with exact allowances (kind 0: also conversions such as `depositLiquid`), Solidly pools
///         (1 / 2) and CL pools (3), every target and factory owner-allowlisted.
/// @dev Funding, steps and the security model are FlashArbBase's. `arb` (Morpho Blue flash loan)
///      and `arbFlashAccounting` (Uniswap V4) take the same arguments, so minProfit sits at the same
///      calldata offset (selector + 4 words). `sweep` lists the intermediate tokens whose leftovers
///      (exact-input rounding, partial fills) go to PROFIT_RECEIVER after the arb, so nothing stays
///      at rest; sweeping only ever pays the immutable receiver.
contract StepArb is FlashArbBase {
    event Arb(address indexed loanToken, uint256 loanAmount, uint256 steps, uint256 profit);

    constructor(IMorphoFlash morpho, IPoolManager poolManager, address owner_, address executor_, address profitReceiver_)
        FlashArbBase(morpho, poolManager, owner_, executor_, profitReceiver_)
    {}

    /// @notice Funded by a Morpho Blue flash loan.
    /// @param loanToken Flash-borrowed; profit is counted in it.
    /// @param steps Steps turning `loanAmount` of loanToken back into (more) loanToken.
    /// @param sweep Intermediate tokens to sweep to PROFIT_RECEIVER afterwards.
    /// @param minProfit Minimum loanToken left after repaying the flash loan; reverts below it.
    /// @param deadlineBlock Reverts if included after this block (stale opportunity).
    function arb(
        address loanToken,
        uint256 loanAmount,
        ArbStep[] calldata steps,
        address[] calldata sweep,
        uint256 minProfit,
        uint256 deadlineBlock
    ) external returns (uint256 profit) {
        _flash(FUND_MORPHO, loanToken, loanAmount, abi.encode(steps), deadlineBlock);
        profit = _done(loanToken, loanAmount, steps.length, sweep, minProfit);
    }

    /// @notice Same as `arb`, funded by Uniswap V4 flash accounting.
    function arbFlashAccounting(
        address loanToken,
        uint256 loanAmount,
        ArbStep[] calldata steps,
        address[] calldata sweep,
        uint256 minProfit,
        uint256 deadlineBlock
    ) external returns (uint256 profit) {
        _flash(FUND_V4, loanToken, loanAmount, abi.encode(steps), deadlineBlock);
        profit = _done(loanToken, loanAmount, steps.length, sweep, minProfit);
    }

    function _done(address loanToken, uint256 loanAmount, uint256 n, address[] calldata sweep, uint256 minProfit) private returns (uint256 profit) {
        profit = _finish(loanToken, minProfit);
        for (uint256 i; i < sweep.length; ++i) {
            _sweep(sweep[i]);
        }
        emit Arb(loanToken, loanAmount, n, profit);
    }

    function _body(bytes memory body) internal override {
        _runSteps(0, abi.decode(body, (ArbStep[])));
    }
}
