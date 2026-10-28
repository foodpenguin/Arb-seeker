//! Transient capital for an atomic arbitrage. Two models, deliberately two traits, because the
//! contract talks to them differently:
//! - `FlashLoan`: borrow a fixed amount, the lender calls back, repay amount + fee before the
//!   callback returns (Morpho Blue: 0 fee; Balancer / Aave: a fee).
//! - `FlashAccounting`: a singleton holding the reserves tracks the caller's deltas while it is
//!   unlocked; take now, settle the net before unlock returns (Uniswap V4 PoolManager: no fee).
//!
//! The capital is taken inside the arb contract (its callback runs the arb), so off-chain a strategy
//! needs to know which contract funds it, what that costs and how much it holds. Which source a
//! given arb uses is the strategy's call (see `[aero] funding`); the contract has one entry point
//! per source with the same arguments.

use alloy::{
    eips::BlockId,
    primitives::{Address, Bytes, U256, address},
    providers::Provider,
    sol,
    sol_types::SolCall,
};
use anyhow::Result;

use crate::chain::multicall;

sol! {
    interface IErc20Balance { function balanceOf(address) external view returns (uint256); }
}

/// Callback-style flash loan with a fixed fee.
pub trait FlashLoan: Send + Sync {
    fn name(&self) -> &'static str;
    /// The contract that lends and calls back.
    fn lender(&self) -> Address;
    /// Fee for borrowing `amount`, in the same token.
    fn fee(&self, amount: U256) -> U256;
}

/// Take-and-settle flash accounting on a singleton.
pub trait FlashAccounting: Send + Sync {
    fn name(&self) -> &'static str;
    /// The singleton that holds the reserves and tracks deltas until settlement.
    fn manager(&self) -> Address;
    /// Fee for taking `amount` and settling it back, in the same token (V4: none; a V3-style pool
    /// `flash` charges the pool fee).
    fn fee(&self, _amount: U256) -> U256 {
        U256::ZERO
    }
}

/// Morpho Blue: `flashLoan(token, assets, data)` -> `onMorphoFlashLoan`, no fee.
pub struct MorphoBlue {
    pub address: Address,
}

impl FlashLoan for MorphoBlue {
    fn name(&self) -> &'static str {
        "morpho"
    }
    fn lender(&self) -> Address {
        self.address
    }
    fn fee(&self, _amount: U256) -> U256 {
        U256::ZERO
    }
}

/// Uniswap V4: `unlock(data)` -> `unlockCallback`, then `take` / `sync` + transfer + `settle`.
/// While we hold the lock no swap step may go through V4 itself (a V4 router would `unlock` again).
pub struct UniswapV4 {
    pub pool_manager: Address,
}

impl FlashAccounting for UniswapV4 {
    fn name(&self) -> &'static str {
        "v4"
    }
    fn manager(&self) -> Address {
        self.pool_manager
    }
}

/// Morpho Blue on Base (the `[aero] morpho` default).
pub const BASE_MORPHO: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");

/// What a funding source can lend right now: its balance of each token, in one multicall. `None` =
/// that read failed.
pub async fn available<P: Provider>(p: &P, holder: Address, tokens: &[Address], block: BlockId) -> Result<Vec<Option<U256>>> {
    let data: Bytes = IErc20Balance::balanceOfCall(holder).abi_encode().into();
    let calls: Vec<(Address, Bytes)> = tokens.iter().map(|t| (*t, data.clone())).collect();
    let out = multicall(p, &calls, block).await?;
    Ok(out.iter().map(|b| b.as_ref().and_then(|b| IErc20Balance::balanceOfCall::abi_decode_returns(b).ok())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sources_are_free_and_name_their_contract() {
        let m = MorphoBlue { address: BASE_MORPHO };
        let v4 = UniswapV4 { pool_manager: Address::repeat_byte(4) };
        assert_eq!((m.name(), m.lender(), m.fee(U256::from(1_000_000u64))), ("morpho", BASE_MORPHO, U256::ZERO));
        assert_eq!((v4.name(), v4.manager(), v4.fee(U256::from(1_000_000u64))), ("v4", Address::repeat_byte(4), U256::ZERO));
    }
}
