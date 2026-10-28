//! `ArbExecutor`: what happens between "a strategy found an opportunity" and "a transaction is
//! sent", for any atomic arb contract that returns its profit:
//!
//! 1. exact check: every candidate calldata is run against the contract itself (in parallel), priced
//!    like the real tx (base priority fee, gas limit, max fee), because venues may read tx.gasprice
//!    (e.g. priority-fee taxes, dynamic fees);
//! 2. pick: the candidate that clears its floor with the highest net (profit - modelled gas); an
//!    exact tie keeps the earlier candidate, so a strategy orders its candidates by preference;
//! 3. bid: a big arb may bid part of its profit as priority fee. The bid is checked again at its own
//!    priority and falls back to the base bid if it no longer clears the floor there;
//! 4. send terms: the gas limit and priority the hot wallet can actually cover, and the on-chain
//!    minProfit (a share of the exact profit).
//!
//! The strategy supplies how to call (`call`: e.g. on the sealed head, or on top of a replayed
//! in-progress block) and how to value the profit token in USD; this module owns the rest.

use std::future::Future;

use alloy::{
    primitives::{Address, Bytes, U256, address},
    providers::Provider,
    rpc::types::TransactionRequest,
    sol,
    sol_types::SolCall,
};
use anyhow::{Context, Result};
use futures::future::join_all;
use tracing::{info, warn};

use crate::{
    executor::{Executor, Inflight, MinProfitSlot, Payload, SendRequest},
    num::u256_f64,
};

/// Priority-fee bidding for big arbs: when the exact profit is at least `min_profit_usd`, up to
/// `share` of it goes into the priority fee, capped at `max_priority_wei` and never below the
/// floor's net. A bid is paid on a revert too.
#[derive(Debug, Clone, Copy)]
pub struct BidPolicy {
    /// Priority fee of every tx that does not bid.
    pub base_priority_wei: u128,
    pub min_profit_usd: f64,
    pub share: f64,
    pub max_priority_wei: u128,
}

impl BidPolicy {
    /// Priority fee for an arb with this exact profit and net (USD) against its floor; `gas` is the
    /// gas the tx is expected to use.
    pub fn priority(&self, profit_usd: f64, net_usd: f64, floor_usd: f64, gas: u64, eth_usd: f64) -> u128 {
        if profit_usd < self.min_profit_usd || self.share <= 0.0 || gas == 0 || eth_usd <= 0.0 {
            return self.base_priority_wei;
        }
        let budget_usd = (profit_usd * self.share).min(net_usd - floor_usd).max(0.0);
        let extra = budget_usd / eth_usd * 1e18 / gas as f64;
        (self.base_priority_wei + extra as u128).min(self.max_priority_wei.max(self.base_priority_wei))
    }
}

/// Gas cost in USD of `gas` at `base_fee + priority`.
pub fn gas_cost_usd(gas: u64, base_fee_wei: u128, priority_wei: u128, eth_usd: f64) -> f64 {
    gas as f64 * (base_fee_wei + priority_wei) as f64 / 1e18 * eth_usd
}

sol! {
    interface IGasPriceOracle { function getL1FeeUpperBound(uint256 unsignedTxSize) external view returns (uint256); }
}
/// OP-stack GasPriceOracle predeploy (Base).
const GAS_PRICE_ORACLE: Address = address!("420000000000000000000000000000000000000F");

/// The L1 data fee (upper bound) of a tx carrying `calldata_len` bytes: paid whatever the priority.
pub async fn l1_fee_wei<P: Provider>(p: &P, calldata_len: usize) -> Result<U256> {
    let data: Bytes = IGasPriceOracle::getL1FeeUpperBoundCall { unsignedTxSize: U256::from(calldata_len + 100) }.abi_encode().into();
    let out = p.call(TransactionRequest::default().to(GAS_PRICE_ORACLE).input(data.into())).await?;
    Ok(IGasPriceOracle::getL1FeeUpperBoundCall::abi_decode_returns(&out)?)
}

/// A deadline in seconds as blocks at the observed block time (works across a block-time change,
/// e.g. 2 s -> 200 ms, without config edits).
pub fn deadline_blocks(deadline_secs: f64, block_time_secs: f64) -> u64 {
    (deadline_secs / block_time_secs.max(0.05)).ceil().max(1.0) as u64
}

/// What `ArbExecutor::send` checks before every send.
pub struct Gates<'a> {
    pub kill_switch_file: &'a str,
    pub budget: &'a mut DailyBudget,
    pub daily_cap_usd: f64,
    pub now_unix: u64,
}

/// Gas burned by failed (reverted) txs per UTC day. Successful sends are not charged: they paid
/// for their own gas, and charging them would block profitable arbs on a busy day.
#[derive(Debug, Default)]
pub struct DailyBudget {
    day: u64,
    spent_usd: f64,
}

impl DailyBudget {
    fn roll(&mut self, unix_secs: u64) {
        let day = unix_secs / 86_400;
        if day != self.day {
            (self.day, self.spent_usd) = (day, 0.0);
        }
    }

    /// The actual cost of a tx that reverted on-chain.
    pub fn charge(&mut self, unix_secs: u64, usd: f64) {
        self.roll(unix_secs);
        self.spent_usd += usd;
    }

    /// Would a send that may cost `usd` (if it reverts) stay within `cap_usd` today?
    pub fn allows(&mut self, unix_secs: u64, usd: f64, cap_usd: f64) -> bool {
        self.roll(unix_secs);
        self.spent_usd + usd <= cap_usd
    }
}

/// One way to take the opportunity, as the contract would be called.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub calldata: Bytes,
    /// Modelled gas use (prices the net and sizes a bid's gas limit).
    pub gas_est: u64,
    /// Minimum net profit (USD) for this candidate to be worth sending.
    pub floor_usd: f64,
    /// L1 data fee of the tx, in USD (counted in the net; set once the fee is known).
    pub l1_usd: f64,
}

/// The candidate the exact check picked.
#[derive(Debug, Clone, Copy)]
pub struct Picked {
    /// Index into the candidates.
    pub index: usize,
    /// Profit the contract returned, in its profit token.
    pub profit: U256,
    pub gross_usd: f64,
    /// Gross minus modelled gas at the base priority fee.
    pub net_usd: f64,
}

/// What to send the picked candidate with.
#[derive(Debug, Clone, Copy)]
pub struct SendTerms {
    pub priority_fee_wei: u128,
    pub gas_limit: u64,
    pub min_profit: U256,
    /// Priority fee above the base bid, in USD (journaled).
    pub bid_usd: f64,
}

/// The exact-check / pick / bid pipeline for one arb contract at one block.
pub struct ArbExecutor {
    /// Strategy name, for logs.
    pub name: &'static str,
    pub contract: Address,
    /// The hot wallet the tx is sent from (the contract's executor).
    pub from: Address,
    /// Gas limit of the exact check and of a non-bidding send.
    pub gas_limit: u64,
    pub base_fee_wei: u128,
    pub bid: BidPolicy,
    /// Where the contract's calldata carries minProfit (patched by the executor at send time).
    pub min_profit_slot: MinProfitSlot,
    /// On-chain minProfit = exact profit x this.
    pub min_profit_keep: f64,
    pub eth_usd: f64,
    /// Decodes the profit from the contract's return data.
    pub decode: fn(&Bytes) -> Option<U256>,
}

impl ArbExecutor {
    /// The tx as it would be sent: max fee = 2 x base fee + priority.
    pub fn tx(&self, calldata: &Bytes, gas_limit: u64, priority_wei: u128) -> TransactionRequest {
        TransactionRequest::default()
            .from(self.from)
            .to(self.contract)
            .input(calldata.clone().into())
            .gas_limit(gas_limit)
            .max_fee_per_gas(self.base_fee_wei * 2 + priority_wei)
            .max_priority_fee_per_gas(priority_wei)
    }

    /// Runs every candidate against the contract, in parallel, at the base priority fee.
    pub async fn exact<F, Fut>(&self, candidates: &[Candidate], call: F) -> Vec<Result<U256>>
    where
        F: Fn(TransactionRequest) -> Fut,
        Fut: Future<Output = Result<Bytes>>,
    {
        let calls = candidates.iter().map(|c| call(self.tx(&c.calldata, self.gas_limit, self.bid.base_priority_wei)));
        join_all(calls).await.into_iter().map(|r| r.and_then(|out| (self.decode)(&out).context("undecodable profit"))).collect()
    }

    /// The candidate to send: those that clear their floor first, then the higher net; an exact
    /// tie keeps the earlier one. `usd` values the profit token.
    pub fn pick(&self, candidates: &[Candidate], exact: &[Result<U256>], usd: impl Fn(U256) -> f64) -> Option<Picked> {
        let mut best: Option<Picked> = None;
        for (index, (c, r)) in candidates.iter().zip(exact).enumerate() {
            let Ok(profit) = r else { continue };
            let gross_usd = usd(*profit);
            let net_usd = gross_usd - gas_cost_usd(c.gas_est, self.base_fee_wei, self.bid.base_priority_wei, self.eth_usd) - c.l1_usd;
            let key = |net: f64, c: &Candidate| (net >= c.floor_usd, net);
            if best.as_ref().is_none_or(|b| key(net_usd, c) > key(b.net_usd, &candidates[b.index])) {
                best = Some(Picked { index, profit: *profit, gross_usd, net_usd });
            }
        }
        best
    }

    /// Priority, gas limit and minProfit for sending `c` (picked as `p`) from a wallet holding
    /// `balance_wei`. A bid goes with a tighter gas limit (2x the model) and is re-checked at its
    /// own priority through `call`. None: the wallet cannot cover the worst-case gas (logged).
    pub async fn terms<F, Fut>(&self, c: &Candidate, p: &Picked, balance_wei: f64, usd: impl Fn(U256) -> f64, call: F) -> Option<SendTerms>
    where
        F: Fn(TransactionRequest) -> Fut,
        Fut: Future<Output = Result<Bytes>>,
    {
        let base = self.bid.base_priority_wei;
        // The node wants gas_limit x maxFee (2 x base + priority) of balance up front: the highest
        // priority a gas limit leaves room for, keeping 10% of the balance aside.
        let room = |gas_limit: u64| ((balance_wei * 0.9 / gas_limit as f64) as u128).saturating_sub(self.base_fee_wei * 2);
        let keep = |profit: U256| U256::from((u256_f64(profit) * self.min_profit_keep) as u128);
        let gas_cost = p.gross_usd - p.net_usd;
        let (mut priority, mut gas_limit, mut min_profit) = (base, self.gas_limit, keep(p.profit));
        let bid = self.bid.priority(p.gross_usd, p.net_usd, c.floor_usd, c.gas_est, self.eth_usd);
        if bid > base {
            let gl = self.gas_limit.min(c.gas_est * 2);
            let pr = bid.min(room(gl));
            if pr > base {
                let at_bid = call(self.tx(&c.calldata, gl, pr)).await.ok().and_then(|out| (self.decode)(&out));
                let bid_cost = (pr - base) as f64 * c.gas_est as f64 / 1e18 * self.eth_usd;
                match at_bid.map(|p2| (p2, usd(p2) - gas_cost - bid_cost)) {
                    Some((p2, net2)) if net2 >= c.floor_usd => {
                        (priority, gas_limit, min_profit) = (pr, gl, keep(p2));
                    }
                    other => info!(strategy = self.name, bid_gwei = pr as f64 / 1e9, net_at_bid = ?other.map(|x| x.1), need = c.floor_usd, "arb: the bid does not pay at its own priority; base bid"),
                }
            }
        }
        if room(gas_limit) < priority {
            warn!(strategy = self.name, balance_eth = balance_wei / 1e18, gas_limit, "arb: hot wallet cannot cover worst-case gas; not sending");
            return None;
        }
        let bid_usd = (priority - base) as f64 * c.gas_est as f64 / 1e18 * self.eth_usd;
        Some(SendTerms { priority_fee_wei: priority, gas_limit, min_profit, bid_usd })
    }

    /// Sends `c` on these terms through `e`, after the gates every strategy must pass (enforced
    /// here, not left to each strategy): the kill-switch file, the executor's failure cooldown, and
    /// the daily budget for gas burned by reverts (what this tx would burn if it reverted: L2 gas
    /// at its priority plus the L1 fee). None = not sent (logged).
    #[allow(clippy::too_many_arguments)]
    pub async fn send<P: alloy::providers::Provider>(&self, e: &mut Executor<P>, c: &Candidate, t: &SendTerms, gates: Gates<'_>, block: u64, deadline_block: u64) -> Result<Option<Inflight>> {
        if std::path::Path::new(gates.kill_switch_file).exists() {
            info!(strategy = self.name, "arb: kill switch file present; not sending");
            return Ok(None);
        }
        if e.halted() {
            info!(strategy = self.name, "arb: sending paused (failure cooldown); not sending");
            return Ok(None);
        }
        let worst_usd = gas_cost_usd(c.gas_est, self.base_fee_wei, t.priority_fee_wei, self.eth_usd) + c.l1_usd;
        if !gates.budget.allows(gates.now_unix, worst_usd, gates.daily_cap_usd) {
            info!(strategy = self.name, worst_usd, budget_usd = gates.daily_cap_usd, "arb: daily gas budget for reverts used up; not sending");
            return Ok(None);
        }
        Ok(Some(e.send(self.request(c, t, block, deadline_block)).await?))
    }

    /// The executor's send request for `c` on these terms.
    pub fn request(&self, c: &Candidate, t: &SendTerms, block: u64, deadline_block: u64) -> SendRequest {
        SendRequest {
            payload: Payload { to: self.contract, calldata: c.calldata.clone(), min_profit: self.min_profit_slot },
            min_profit: t.min_profit,
            gas_limit: t.gas_limit,
            base_fee_wei: self.base_fee_wei,
            priority_fee_wei: t.priority_fee_wei,
            market: usize::MAX,
            user: self.contract,
            block,
            deadline_block,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn word(x: u64) -> Bytes {
        Bytes::from(U256::from(x).to_be_bytes::<32>().to_vec())
    }

    fn decode(out: &Bytes) -> Option<U256> {
        (out.len() == 32).then(|| U256::from_be_slice(out))
    }

    fn exec() -> ArbExecutor {
        ArbExecutor {
            name: "test",
            contract: Address::repeat_byte(0xc0),
            from: Address::repeat_byte(0xee),
            gas_limit: 3_000_000,
            base_fee_wei: 10_000_000,
            bid: BidPolicy { base_priority_wei: 1_000_000, min_profit_usd: 10.0, share: 0.25, max_priority_wei: 1_000_000_000 },
            min_profit_slot: MinProfitSlot { offset: 132, width: 32 },
            min_profit_keep: 0.5,
            eth_usd: 2000.0,
            decode,
        }
    }

    fn cand(gas_est: u64, floor_usd: f64) -> Candidate {
        Candidate { calldata: Bytes::from(vec![gas_est as u8]), gas_est, floor_usd, l1_usd: 0.0 }
    }

    /// Profit token = USD with 6 decimals.
    fn usd(x: U256) -> f64 {
        u256_f64(x) / 1e6
    }

    #[test]
    fn daily_budget_caps_failed_spend_and_resets_at_utc_midnight() {
        let mut b = DailyBudget::default();
        assert!(b.allows(10, 4.0, 5.0));
        b.charge(10, 4.5);
        assert!(!b.allows(20, 1.0, 5.0), "4.5 + 1 > 5");
        assert!(b.allows(20, 0.5, 5.0));
        assert!(b.allows(86_400 + 1, 4.9, 5.0), "a new day starts from zero");
    }

    #[test]
    fn deadlines_follow_the_block_time() {
        assert_eq!(deadline_blocks(4.0, 2.0), 2);
        assert_eq!(deadline_blocks(4.0, 0.2), 20, "200 ms blocks");
        assert_eq!(deadline_blocks(0.1, 2.0), 1, "at least one block");
    }

    #[tokio::test]
    async fn the_l1_fee_counts_in_the_net() {
        let e = exec();
        let call = |_tx: TransactionRequest| async { Ok(word(5_000_000)) };
        let mut cands = [cand(100, 1.0), cand(101, 1.0)];
        cands[0].l1_usd = 4.5;
        let p = e.pick(&cands, &e.exact(&cands, call).await, usd).unwrap();
        assert_eq!(p.index, 1, "same profit, the first pays more L1 fee");
        assert!(p.net_usd > 4.9 && p.net_usd < 5.0, "5 USD profit minus a little gas, no L1 fee: {}", p.net_usd);
    }

    #[test]
    fn bid_policy_bids_part_of_big_profits_only() {
        let b = BidPolicy { base_priority_wei: 10_000_000, min_profit_usd: 2.0, share: 0.25, max_priority_wei: 1_000_000_000 };
        let (gas, eth) = (1_000_000u64, 2400.0);
        assert_eq!(b.priority(1.5, 1.4, 0.3, gas, eth), 10_000_000, "below min_profit_usd: base bid");
        // $2.78 profit: 25% = $0.695 over 1M gas at $2400 = 0.2896 gwei on top of the base.
        let p = b.priority(2.78, 2.7, 2.0, gas, eth);
        assert!((p as f64 - (10_000_000.0 + 0.695 / 2400.0 * 1e18 / 1e6)).abs() < 1e3, "{p}");
        // The floor caps it: only net - floor = $0.2 may go to the bid.
        let p = b.priority(2.78, 2.2, 2.0, gas, eth);
        assert!((p as f64 - (10_000_000.0 + 0.2 / 2400.0 * 1e18 / 1e6)).abs() < 1e3, "{p}");
        // And so does the cap.
        assert_eq!(b.priority(100.0, 99.0, 1.0, gas, eth), 1_000_000_000);
        assert_eq!(BidPolicy { share: 0.0, ..b }.priority(100.0, 99.0, 1.0, gas, eth), 10_000_000, "bidding off");
    }

    #[tokio::test]
    async fn exact_runs_every_candidate_and_pick_prefers_floor_then_net_then_order() {
        let e = exec();
        // Profits by gas_est tag: 100 -> 5 USD, 101 -> 50 USD, 102 -> revert, 103 -> 50 USD.
        let call = |tx: TransactionRequest| {
            let tag = tx.input.input().unwrap()[0];
            async move {
                match tag {
                    100 => Ok(word(5_000_000)),
                    101 | 103 => Ok(word(50_000_000)),
                    _ => anyhow::bail!("execution reverted"),
                }
            }
        };
        let cands = [cand(100, 1.0), cand(101, 100.0), cand(102, 1.0), cand(103, 1.0)];
        let exact = e.exact(&cands, call).await;
        assert!(exact[2].is_err() && exact[1].is_ok());
        let p = e.pick(&cands, &exact, usd).unwrap();
        // 101 nets the most but misses its 100 USD floor; 103 ties it and clears its floor.
        assert_eq!((p.index, p.profit), (3, U256::from(50_000_000u64)));
        // Equal nets and floors: the earlier candidate wins.
        let tie = [cand(103, 1.0), cand(103, 1.0)];
        let p = e.pick(&tie, &e.exact(&tie, call).await, usd).unwrap();
        assert_eq!(p.index, 0);
    }

    #[tokio::test]
    async fn terms_bid_when_it_still_pays_else_base_and_refuse_without_balance() {
        let e = exec();
        let c = cand(200_000, 1.0);
        let picked = Picked { index: 0, profit: U256::from(100_000_000u64), gross_usd: 100.0, net_usd: 99.9 };
        let seen = Mutex::new(vec![]);
        // At a bid the contract still makes 100 USD: the bid stands.
        let pays = |tx: TransactionRequest| {
            seen.lock().unwrap().push(tx.max_priority_fee_per_gas.unwrap());
            async { Ok(word(100_000_000)) }
        };
        let t = e.terms(&c, &picked, 1e18, usd, pays).await.unwrap();
        assert!(t.priority_fee_wei > e.bid.base_priority_wei && t.gas_limit == 400_000, "{t:?}");
        assert_eq!(t.min_profit, U256::from(50_000_000u64));
        assert!(t.bid_usd > 0.0 && seen.lock().unwrap().len() == 1);
        // At its own priority the arb makes almost nothing (e.g. a priority-fee tax): base bid.
        let taxed = |_tx: TransactionRequest| async { Ok(word(1)) };
        let t = e.terms(&c, &picked, 1e18, usd, taxed).await.unwrap();
        assert_eq!((t.priority_fee_wei, t.gas_limit, t.bid_usd), (e.bid.base_priority_wei, e.gas_limit, 0.0));
        // A wallet that cannot pay gas_limit x max fee: nothing is sent.
        assert!(e.terms(&c, &picked, 1e9, usd, taxed).await.is_none());
        let req = e.request(&c, &SendTerms { priority_fee_wei: 7, gas_limit: 9, min_profit: U256::from(3u8), bid_usd: 0.0 }, 5, 7);
        assert_eq!((req.payload.to, req.priority_fee_wei, req.gas_limit, req.deadline_block), (e.contract, 7, 9, 7));
    }
}
