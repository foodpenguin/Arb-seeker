//! The generic step-execution arb contract (`contracts/src/StepArb.sol`): flash-borrow the loan
//! token (Morpho Blue flash loan or Uniswap V4 flash accounting), run a list of steps, repay, keep
//! at least minProfit, sweep the listed intermediate tokens to the profit receiver. A strategy that
//! can express its trade as steps needs no contract of its own: it builds `ArbStep`s, encodes them
//! with `calldata`, and hands the result to `ArbExecutor` as a `Candidate` (minProfit at
//! `MIN_PROFIT`, profit decoded with `decode_profit`).

use alloy::{
    primitives::{Address, Bytes, U256},
    sol_types::SolCall,
};

pub use crate::contracts::ArbStep;
use crate::{contracts::IStepArb, executor::MinProfitSlot};

/// minProfit in both entry points: selector + 4 head words (loanToken, loanAmount, steps offset,
/// sweep offset).
pub const MIN_PROFIT: MinProfitSlot = MinProfitSlot { offset: 4 + 4 * 32, width: 32 };

/// Which entry point, i.e. which flash source funds the arb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Funding {
    /// `arb`: Morpho Blue flash loan.
    Morpho,
    /// `arbFlashAccounting`: Uniswap V4 PoolManager take + settle.
    V4,
}

/// Calldata borrowing `amount` of `loan_token`, running `steps`, sweeping `sweep`, valid through
/// `deadline_block`. minProfit is left 0: the executor patches it at send time (`MIN_PROFIT`).
pub fn calldata(funding: Funding, loan_token: Address, amount: U256, steps: Vec<ArbStep>, sweep: Vec<Address>, deadline_block: u64) -> Bytes {
    let (min_profit, deadline) = (U256::ZERO, U256::from(deadline_block));
    match funding {
        Funding::Morpho => IStepArb::arbCall { loanToken: loan_token, loanAmount: amount, steps, sweep, minProfit: min_profit, deadlineBlock: deadline }.abi_encode().into(),
        Funding::V4 => IStepArb::arbFlashAccountingCall { loanToken: loan_token, loanAmount: amount, steps, sweep, minProfit: min_profit, deadlineBlock: deadline }.abi_encode().into(),
    }
}

/// The profit either entry point returned.
pub fn decode_profit(out: &Bytes) -> Option<U256> {
    IStepArb::arbCall::abi_decode_returns(out).ok()
}

/// A kind-0 step: approve `token_in` (all of it when `amount_in` = U256::MAX) to `target` and call
/// it with `data`, the amount written into `data` at `patch_offset` (0 = not patched).
pub fn call_step(target: Address, token_in: Address, amount_in: U256, data: Bytes, patch_offset: usize) -> ArbStep {
    ArbStep { kind: U256::ZERO, target, tokenIn: token_in, amountIn: amount_in, patchOffset: U256::from(patch_offset), data }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::patch_min_profit;

    #[test]
    fn min_profit_sits_where_the_executor_patches_it_for_both_entry_points() {
        let step = call_step(Address::repeat_byte(1), Address::repeat_byte(2), U256::MAX, Bytes::from_static(&[9; 36]), 4);
        for f in [Funding::Morpho, Funding::V4] {
            let cd = calldata(f, Address::repeat_byte(3), U256::from(5u8), vec![step.clone()], vec![Address::repeat_byte(4)], 77);
            let patched = patch_min_profit(&cd, MIN_PROFIT, U256::from(1234u64)).unwrap();
            let call = IStepArb::arbCall::abi_decode_raw(&patched[4..]).unwrap();
            assert_eq!((call.minProfit, call.deadlineBlock, call.sweep.len(), call.steps[0].patchOffset), (U256::from(1234u64), U256::from(77u8), 1, U256::from(4u8)));
        }
        assert_eq!(decode_profit(&Bytes::from(U256::from(42u8).to_be_bytes::<32>().to_vec())), Some(U256::from(42u8)));
    }
}
