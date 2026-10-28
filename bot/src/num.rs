//! Chain-neutral unit helpers shared by every strategy. Off-chain estimates only: on-chain amounts
//! stay U256 end to end. Add fixed-point helpers (WAD, mul_div) here when a strategy needs them.

use alloy::primitives::U256;

/// Lossy U256 -> f64 (for logging, ranking and off-chain profit estimates only).
pub fn u256_f64(x: U256) -> f64 {
    x.to_string().parse::<f64>().unwrap_or(f64::MAX)
}

/// Token amount with `decimals` -> f64 units.
pub fn units(x: U256, decimals: u8) -> f64 {
    u256_f64(x) / 10f64.powi(decimals as i32)
}
