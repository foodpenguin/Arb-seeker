//! Arbitrage bot framework.
//!
//! - `runner`: the strategy driver (`OpportunitySource`): heads, Flashblocks, settled txs;
//! - `arb_exec`: `ArbExecutor`, the send pipeline for any atomic arb contract that returns its
//!   profit (exact check, pick, bid, send terms, the shared gates);
//! - `step_arb`: the generic step-execution arb contract (StepArb.sol): any trade written as steps;
//! - `flash`: funding sources (`FlashLoan` / `FlashAccounting`);
//! - `aero`: Aero merger arbitrage on Base, the example strategy.
//!
//! The plumbing (RPC pool, heads, Flashblocks, executor, simulation, gates) comes from
//! `onchain-core` and is re-exported here under its usual module names.

pub use onchain_core::{actor, chain, executor, flashblocks, gates, heads, num, rpc, sim};

pub mod aero;
pub mod arb_exec;
pub mod config;
pub mod contracts;
pub mod flash;
pub mod runner;
pub mod step_arb;
