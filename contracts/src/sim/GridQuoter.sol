// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {IV3Pool, MIN_SQRT_PRICE_LIMIT, MAX_SQRT_PRICE_LIMIT} from "../interfaces/IV3Pool.sol";

/// @title GridQuoter (simulation only, never deployed)
/// @notice Injected by state override into eth_call. Quotes many exact-input paths at many sizes in
///         one call by starting each swap and reverting from the swap callback with the output
///         amount (Uniswap Quoter technique), so every quote sees the same untouched pool state.
contract GridQuoter {
    /// Gas one hop may use (the aero arb gas_limit). A swap past it walks thousands of ticks, e.g.
    /// one that drains a tick-spacing-1 pool (17.6M gas each on 10/27): it could not be executed
    /// anyway, and ten of them took the whole multicall past ankr's 100M eth_call gas cap.
    uint256 private constant HOP_GAS = 3_000_000;

    struct Path {
        address[] pools;
        bool[] zeroForOne;
    }

    /// @return outs outs[p][a] = output of path p for amounts[a] (0 if any hop failed)
    /// @return gasUsed gasUsed[p][a] = gas the quote consumed (proxy for execution gas)
    function quote(Path[] calldata paths, uint256[] calldata amounts)
        external
        returns (uint256[][] memory outs, uint256[][] memory gasUsed)
    {
        outs = new uint256[][](paths.length);
        gasUsed = new uint256[][](paths.length);
        for (uint256 p; p < paths.length; ++p) {
            outs[p] = new uint256[](amounts.length);
            gasUsed[p] = new uint256[](amounts.length);
            uint256 tooBig = type(uint256).max; // smallest amount that ran out of gas on this path
            for (uint256 a; a < amounts.length; ++a) {
                if (amounts[a] >= tooBig) continue; // a larger swap needs at least as much gas: 0
                uint256 g = gasleft();
                uint256 amt = amounts[a];
                for (uint256 h; h < paths[p].pools.length && amt != 0; ++h) {
                    bool oog;
                    (amt, oog) = _quoteHop(paths[p].pools[h], paths[p].zeroForOne[h], amt);
                    if (oog && h == 0) tooBig = amounts[a];
                }
                outs[p][a] = amt;
                gasUsed[p][a] = g - gasleft();
            }
        }
    }

    /// (output, ran out of HOP_GAS); output 0 on any failure.
    function _quoteHop(address pool, bool zeroForOne, uint256 amountIn) private returns (uint256, bool) {
        uint256 g = gasleft();
        try IV3Pool(pool).swap{gas: HOP_GAS}(
            address(this), zeroForOne, int256(amountIn), zeroForOne ? MIN_SQRT_PRICE_LIMIT : MAX_SQRT_PRICE_LIMIT, ""
        ) {
            return (0, false); // unreachable: the callback always reverts
        } catch (bytes memory reason) {
            // Out of gas inside a sub-call: the pool reverts with the 1/64 it kept (2.956M of 3M used).
            if (reason.length != 32) return (0, g - gasleft() >= HOP_GAS - HOP_GAS / 32);
            return (abi.decode(reason, (uint256)), false);
        }
    }

    function uniswapV3SwapCallback(int256 amount0, int256 amount1, bytes calldata) external pure {
        _revertWithOut(amount0, amount1);
    }

    function pancakeV3SwapCallback(int256 amount0, int256 amount1, bytes calldata) external pure {
        _revertWithOut(amount0, amount1);
    }

    /// Algebra Integral pools (Hydrex, Quickswap V4): same swap signature, own callback name.
    function algebraSwapCallback(int256 amount0, int256 amount1, bytes calldata) external pure {
        _revertWithOut(amount0, amount1);
    }

    function _revertWithOut(int256 amount0, int256 amount1) private pure {
        uint256 out = uint256(-(amount0 < 0 ? amount0 : amount1));
        assembly {
            mstore(0, out)
            revert(0, 32)
        }
    }
}
