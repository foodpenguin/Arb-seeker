// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.28;

/// @dev Common surface of Uniswap V3, PancakeSwap V3 and Aerodrome Slipstream (CL) pools.
///      Uniswap/Slipstream call back `uniswapV3SwapCallback`, PancakeSwap `pancakeV3SwapCallback`.
///      The arb contracts swap only on Slipstream / Aero CL (kind 3); GridQuoter quotes any of them.
interface IV3Pool {
    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data)
        external
        returns (int256 amount0, int256 amount1);
    function token0() external view returns (address);
    function token1() external view returns (address);
}

/// @dev TickMath bounds +/-1, the widest limits `swap` accepts. An exact-input swap still stops at
///      the limit when the pool runs out of liquidity, and then takes less than its input.
uint160 constant MIN_SQRT_PRICE_LIMIT = 4295128740;
uint160 constant MAX_SQRT_PRICE_LIMIT = 1461446703485210103287273052203988822378723970341;
