//! Aero merger arbitrage (2026-10-22), the first strategy (`arb-bot [--live]`). Old AERO converts 1:1 into new AERO through the
//! permissionless Migration.depositLiquid, one way only, so whenever old trades below new:
//! flash-borrow the loan token -> buy old AERO (split over legacy Aerodrome V2 + Slipstream pools)
//! -> migrate -> sell new AERO (split over Aero V2 + CL pools) -> repay
//! (contracts/src/AeroMigrationArb.sol, all in one tx).
//!
//! Venues: every DISCOVERY_EVERY blocks every factory (config + the Aero FactoryRegistry for the
//! sell side) is asked for its old- or new-AERO / loan-token pools (CL: every tick spacing it
//! knows); pools without liquidity are skipped.
//!
//! Per block: one multicall carries the USD prices and the V2 quotes; CL pools are quoted with the
//! GridQuoter injected by state override (contracts/src/sim/GridQuoter.sol, a few pools per call).
//! Buy curves are quoted on a ladder of loan sizes, sell curves on a ladder of AERO amounts. For
//! each loan size the input is split greedily over the buy pools (optimal for concave curves), the
//! old AERO bought over the sell pools likewise, and the best size wins. It is then checked exactly
//! with an eth_call of the contract itself at that size and its neighbours; only that exact result
//! is sent, with minProfit = exact x min_profit_keep (the contract reverts below it).
//!
//! Without `[aero] contract` (or without --live) it only journals what it sees: venues, spreads and
//! estimated profits (kind "aero_*", in [aero] journal_path).
//!
//! The pass runs on each sealed head. Base Denim removed Flashblocks and the in-progress ("pending")
//! state, so there is no same-block path; at 200 ms blocks a spread is taken in the next block.

use std::{
    collections::HashMap,
    io::Write,
    time::Duration,
};

use alloy::{
    eips::BlockId,
    primitives::{Address, Bytes, U256, address},
    providers::Provider,
    rpc::types::{
        TransactionRequest,
        state::{AccountOverride, StateOverride},
    },
    sol,
    sol_types::SolCall,
};
use anyhow::{Context, Result, bail};
use serde_json::json;
use tracing::{info, warn};

use crate::{
    actor::ExecHandle,
    contracts::{ArbStep, IAeroArb, IAeroMigration},
    step_arb::{self, Funding},
    arb_exec::{ArbExecutor, BidPolicy, Candidate},
    config::{AeroConfig, AeroContract, AeroFunding, Config},
    chain::multicall,
    executor::{Executor, Inflight, MinProfitSlot, Outcome},
    flash::{self, FlashAccounting, FlashLoan, MorphoBlue, UniswapV4},
    heads::Head,
    num as math,
    runner::{self, OpportunitySource, RunnerOpts},
};

/// Placeholder address GridQuoter is injected at by state override (never deployed).
const SIM_QUOTER: Address = address!("00000000000000000000000000000000000b0772");

sol! {
    interface ILegacyFactory { function getPool(address a, address b, bool stable) external view returns (address); }
    interface IAeroFactory { function getPool(address a, address b) external view returns (address); }
    interface IClFactory {
        function getPool(address a, address b, int24 tickSpacing) external view returns (address);
        function tickSpacings() external view returns (int24[]);
    }
    #[sol(rpc)]
    interface IFactoryRegistry { function targetFactories() external view returns (address[]); }
    interface ISolidlyPool {
        function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
        function getAmountOutWithTotalFee(uint256 amountIn, address tokenIn) external view returns (uint256);
    }
    interface IUsdFeed { function latestRoundData() external view returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound); }
    interface IPoolState {
        function getReserves() external view returns (uint256, uint256, uint256);
        function liquidity() external view returns (uint128);
        function token0() external view returns (address);
    }
    struct GPath { address[] pools; bool[] zeroForOne; }
    interface IGridQuoterA { function quote(GPath[] paths, uint256[] amounts) external returns (uint256[][] outs, uint256[][] gasUsed); }
    struct Call3A { address target; bool allowFailure; bytes callData; }
    struct Result3A { bool success; bytes returnData; }
    interface IMulticall3A {
        function aggregate3(Call3A[] calls) external payable returns (Result3A[] returnData);
        function getBlockNumber() external view returns (uint256);
    }
}

/// One call on a sealed block; a revert is an error, as with eth_call.
async fn call_at<P: Provider>(p: &P, tx: TransactionRequest, at: BlockId) -> Result<Bytes> {
    Ok(p.call(tx).block(at).await?)
}

/// One Multicall3 call carrying every quote of a block, with the GridQuoter injected by state
/// override (it applies to the whole call, so the multicall can reach it). Failed sub-calls are None.
async fn multicall_ov<P: Provider>(p: &P, calls: &[(Address, Bytes)], ov: &StateOverride, at: BlockId) -> Result<Vec<Option<Bytes>>> {
    let req: Vec<Call3A> = calls.iter().map(|(t, d)| Call3A { target: *t, allowFailure: true, callData: d.clone() }).collect();
    let data: Bytes = IMulticall3A::aggregate3Call { calls: req }.abi_encode().into();
    let tx = TransactionRequest::default().to(crate::chain::MULTICALL3).input(data.into());
    let out = p.call(tx).block(at).overrides(ov.clone()).await.context("aero quotes (multicall)")?;
    Ok(IMulticall3A::aggregate3Call::abi_decode_returns(&out)?.into_iter().map(|r| r.success.then_some(r.returnData)).collect())
}

/// ABI arb(...): minProfit is the 5th head word.
const ARB_MIN_PROFIT: MinProfitSlot = MinProfitSlot { offset: 4 + 4 * 32, width: 32 };
/// Loan-size ladder: this many geometric steps from MIN_LOAN_USD to max_loan_usd.
const LOAN_STEPS: usize = 24;
const MIN_LOAN_USD: f64 = 20.0;
/// New-AERO sell ladder (whole AERO): 10 x 2^k, k < NEW_STEPS (up to ~330k AERO).
const NEW_STEPS: usize = 16;
const DISCOVERY_EVERY: u64 = 30;
const WS_QUIET: Duration = Duration::from_secs(3);
/// aero_tick rows: written when the spread moved this much since the loan token's last row, when
/// the estimate is positive, or at least every TICK_LOG_EVERY blocks.
const TICK_LOG_BPS: f64 = 5.0;
const TICK_LOG_EVERY: u64 = 30;
/// CL pools per GridQuoter sub-call (each quotes a whole ladder; a sub-call that runs out of gas
/// only loses its own pools).
const CL_PER_CALL: usize = 3;
/// Greedy split resolution (chunks of the total input).
const SPLIT_CHUNKS: usize = 32;
/// Fixed sell amounts are planned x this (the bought amount can come in a little lower); the
/// largest sell pool takes whatever is left.
const SELL_FIXED_SHARE: f64 = 0.98;

/// V4 flash accounting vs the Morpho flash loan on the same route: 682k vs 703k gas (fork test
/// test_v4FundingRepaysAndMatchesMorpho, 2026-10-28).
const V4_GAS_SAVING: u64 = 21_000;

/// Gas the profit check expects an arb with `legs` swaps funded by `f` to use (`gas_base` is
/// calibrated on the Morpho flash loan).
fn arb_gas(a: &AeroConfig, legs: usize, f: &Funder) -> u64 {
    (a.gas_base + a.gas_per_leg * legs as u64).saturating_sub(f.gas_saving())
}

/// Priority-fee bidding (see arb_exec::BidPolicy) from `[aero]`.
fn bid_policy(a: &AeroConfig) -> BidPolicy {
    BidPolicy { base_priority_wei: a.priority_fee_wei, min_profit_usd: a.bid_min_profit_usd, share: a.bid_share, max_priority_wei: a.bid_max_priority_wei }
}

/// Step kinds of AeroMigrationArb.
const KIND_LEGACY_V2: u64 = 1;
const KIND_AERO_V2: u64 = 2;
const KIND_CL: u64 = 3;

#[derive(Debug, Clone, PartialEq)]
struct Venue {
    pool: Address,
    factory: Address,
    kind: u64,
    tick_spacing: Option<i32>,
    /// The pool's token0 (CL swap direction).
    token0: Address,
}

impl Venue {
    fn label(&self) -> String {
        match (self.kind, self.tick_spacing) {
            (KIND_LEGACY_V2, _) => "legacy-v2".into(),
            (KIND_AERO_V2, _) => "v2".into(),
            (_, Some(t)) => format!("cl{t}"),
            _ => "cl".into(),
        }
    }
}

/// One pool on one side for one loan token, and whether the contract may use it.
#[derive(Debug, Clone)]
struct Path {
    loan: usize,
    buy: bool,
    venue: Venue,
    allowed: bool,
}

/// What tick() carries from one block to the next.
struct TickState {
    /// USD per loan token from the latest pass (sizes the next pass's loan ladder).
    usd: Vec<f64>,
    /// loan -> (spread_bps, block) of the last aero_tick row.
    logged: HashMap<usize, (f64, u64)>,
    /// GridQuoter runtime code (state override).
    quoter: StateOverride,
    /// Gas burned by reverted arbs today (exec.daily_gas_budget_usd).
    budget: crate::arb_exec::DailyBudget,
    /// (block, timestamp) the block time is measured from.
    bt_anchor: Option<(u64, u64)>,
}

impl TickState {
    /// Seconds per block, measured over at least 10 blocks since the first head (2 s until then).
    fn block_time(&mut self, head: &Head) -> f64 {
        let (n0, t0) = *self.bt_anchor.get_or_insert((head.number, head.timestamp));
        if head.number >= n0 + 10 && head.timestamp > t0 { (head.timestamp - t0) as f64 / (head.number - n0) as f64 } else { 2.0 }
    }

    /// ETH/USD from the latest pass (the first priced loan token), or the fallback.
    fn eth_usd(&self, a: &AeroConfig, fallback: f64) -> f64 {
        a.loan_tokens.iter().zip(&self.usd).find(|(l, _)| l.is_priced()).map_or(fallback, |(_, u)| *u)
    }
}

/// Aero merger arbitrage as an `OpportunitySource`: the state the per-block pass carries between
/// events. The scheduling loop itself (heads / HTTP polling / receipt polls) is the generic
/// `runner::run`.
pub struct Aero<'a> {
    cfg: &'a Config,
    journal: std::fs::File,
    new_ladder: Vec<U256>,
    paths: Vec<Path>,
    token0s: HashMap<Address, Address>,
    last_discovery: u64,
    state: TickState,
    /// The contract keeps pool factories in their own list (`allowedFactory`); false = a deployment
    /// from before the split, where factories are in `allowedTarget`.
    factory_list: bool,
}

/// Does the contract have the separate factory allowlist? (A call to a missing function reverts.)
async fn has_factory_list<P: Provider>(p: &P, c: Address) -> bool {
    IAeroArb::new(c, p).allowedFactory(Address::ZERO).call().await.is_ok()
}

/// The call that tells whether the contract may swap on `factory`'s pools.
fn factory_allowed_call(factory: Address, factory_list: bool) -> Bytes {
    if factory_list { IAeroArb::allowedFactoryCall(factory).abi_encode().into() } else { IAeroArb::allowedTargetCall(factory).abi_encode().into() }
}

impl<'a> Aero<'a> {
    /// Validates the config, pre-flights the contract when live, loads the GridQuoter code.
    pub async fn start<P: Provider + Clone>(cfg: &'a Config, p: &P, live_wallet: Option<Address>) -> Result<Self> {
        let a = &cfg.aero;
        validate(a)?;
        let mut journal = std::fs::OpenOptions::new().create(true).append(true).open(&a.journal_path).with_context(|| format!("opening journal {}", a.journal_path))?;
        if live_wallet.is_some() && a.contract.is_none() {
            bail!("--live needs [aero] contract");
        }
        let mut factory_list = true;
        if let Some(c) = a.contract {
            retry("funding check", || check_funding(p, a, c)).await?;
            factory_list = has_factory_list(p, c).await;
            if !factory_list {
                info!(contract = %c, "aero: contract predates the separate factory allowlist; reading factories from allowedTarget");
            }
            if let Some(w) = live_wallet {
                retry("pre-flight", || preflight(p, a, c, w, factory_list)).await?;
            }
        }
        log_funding(p, a).await;
        let art: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg.router.quoter_artifact).with_context(|| format!("reading {} (run `forge build` in contracts/)", cfg.router.quoter_artifact))?)?;
        let code: Bytes = art["deployedBytecode"]["object"].as_str().context("quoter: no deployedBytecode")?.parse()?;
        let mut quoter = StateOverride::default();
        quoter.insert(SIM_QUOTER, AccountOverride { code: Some(code), ..Default::default() });
        info!(live = live_wallet.is_some(), contract = ?a.contract, "aero: start");
        writeln!(journal, "{}", json!({ "kind": "aero_start", "live": live_wallet.is_some(), "contract": a.contract }))?;
        let state = TickState {
            usd: a.loan_tokens.iter().map(|l| if l.is_priced() { cfg.exec.eth_usd_fallback.max(1.0) } else { 1.0 }).collect(),
            logged: HashMap::new(),
            quoter,
            budget: Default::default(),
            bt_anchor: None,
        };
        Ok(Self {
            cfg,
            journal,
            new_ladder: (0..NEW_STEPS).map(|k| U256::from(10u64 << k) * U256::from(10u64).pow(U256::from(18))).collect(),
            paths: vec![],
            token0s: HashMap::new(),
            last_discovery: 0,
            state,
            factory_list,
        })
    }
}

impl<P: Provider + Clone> OpportunitySource<P> for Aero<'_> {
    fn name(&self) -> &'static str {
        "aero"
    }

    async fn on_block(&mut self, p: &P, head: &Head, exec: Option<&ExecHandle>) -> Result<()> {
        let a = &self.cfg.aero;
        let block = head.number;
        if self.last_discovery == 0 || block >= self.last_discovery + DISCOVERY_EVERY {
            match discover(p, a, &mut self.token0s, self.factory_list).await {
                Ok(found) => {
                    self.last_discovery = block;
                    for path in &found {
                        if !self.paths.iter().any(|x| x.loan == path.loan && x.venue == path.venue && x.allowed == path.allowed) {
                            let side = if path.buy { "buy" } else { "sell" };
                            info!(side, venue = path.venue.label(), pool = %path.venue.pool, loan = %a.loan_tokens[path.loan].token, allowed = path.allowed, block, "aero: venue");
                            writeln!(self.journal, "{}", json!({ "kind": "aero_venue", "block": block, "side": side, "venue": path.venue.label(), "pool": path.venue.pool, "factory": path.venue.factory, "loan": a.loan_tokens[path.loan].token, "allowed": path.allowed }))?;
                        }
                    }
                    self.paths = found;
                }
                Err(e) => warn!(error = %e, "aero: discovery failed"),
            }
        }
        if !self.paths.iter().any(|x| !x.buy) {
            return Ok(());
        }
        let cooling = exec.is_some_and(|e| e.halted());
        if let Err(err) = tick(self.cfg, p, head, &self.new_ladder, &self.paths, exec, cooling, &mut self.state, &mut self.journal, BlockId::number(head.number)).await {
            warn!(error = %err, block, "aero: tick failed");
        }
        Ok(())
    }

    fn on_outcome(&mut self, block: u64, inf: &Inflight, outcome: &Outcome, gas_spent_total_wei: U256, cooldown_started: bool) -> Result<()> {
        if cooldown_started {
            writeln!(self.journal, "{}", json!({ "kind": "aero_cooldown", "block": block, "secs": self.cfg.exec.failure_cooldown_secs }))?;
        }
        // Settled: its worst-case reservation goes; a revert burned gas for nothing and counts
        // against the daily budget.
        self.state.budget.release(inf.key);
        if let Outcome::Included { success: false, gas_used, effective_gas_price, l1_fee_wei, .. } = outcome {
            let eth_usd = self.state.eth_usd(&self.cfg.aero, self.cfg.exec.eth_usd_fallback);
            let usd = (*gas_used as f64 * *effective_gas_price as f64 + *l1_fee_wei as f64) / 1e18 * eth_usd;
            let now = crate::gates::unix_secs();
            self.state.budget.charge(now, usd);
        }
        info!(tx = %inf.hash, ?outcome, "aero: outcome");
        writeln!(self.journal, "{}", json!({ "kind": "aero_outcome", "block": block, "tx": inf, "outcome": outcome, "gas_spent_total_eth": units(gas_spent_total_wei, 18) }))?;
        Ok(())
    }
}

/// Starts the Aero strategy and hands it to the generic driver (`runner::run`, which owns the
/// heads / HTTP-polling scheduling).
pub async fn run<P: Provider + Clone + Send + Sync + 'static>(cfg: &Config, p: P, exec: Option<Executor<P>>, heads: tokio::sync::watch::Receiver<Option<Head>>) -> Result<()> {
    let mut strategy = Aero::start(cfg, &p, exec.as_ref().map(|e| e.address)).await?;
    runner::run(&mut strategy, p, exec, heads, RunnerOpts { poll_ms: cfg.aero.poll_ms, ws_quiet: WS_QUIET }).await
}

/// Startup reads must survive a busy endpoint (429s are common right at launch).
async fn retry<T, F, Fut>(what: &str, f: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last = None;
    for i in 0..8u64 {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                warn!(what, attempt = i + 1, error = %e, "aero: startup read failed; retrying");
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(500 * (i + 1))).await;
            }
        }
    }
    Err(last.expect("at least one attempt").context(what.to_string()))
}

fn validate(a: &AeroConfig) -> Result<()> {
    let z = Address::ZERO;
    if a.migration == z || a.old_token == z || a.new_token == z || a.legacy_factory == z || a.loan_tokens.is_empty() {
        bail!("[aero] needs migration, old_token, new_token, legacy_factory and loan_tokens");
    }
    if a.new_factories.is_empty() && a.factory_registry.is_none() {
        bail!("[aero] needs new_factories and/or factory_registry");
    }
    if a.funding != AeroFunding::Morpho && a.pool_manager.is_none_or(|pm| pm == z) {
        bail!("[aero] funding = {:?} needs pool_manager (the Uniswap V4 PoolManager)", a.funding);
    }
    if a.funding != AeroFunding::V4 && a.morpho() == z {
        bail!("[aero] funding = {:?} needs morpho", a.funding);
    }
    Ok(())
}

/// One way to fund the arb: the contract entry point that uses it, and the source behind it.
enum Funder {
    Morpho(MorphoBlue),
    V4(UniswapV4),
}

impl Funder {
    /// The sources `[aero] funding` enables, Morpho first (on equal profit it wins: deepest, proven).
    fn enabled(a: &AeroConfig) -> Vec<Funder> {
        let morpho = || Funder::Morpho(MorphoBlue { address: a.morpho() });
        let v4 = || Funder::V4(UniswapV4 { pool_manager: a.pool_manager.unwrap_or_default() });
        match a.funding {
            AeroFunding::Morpho => vec![morpho()],
            AeroFunding::V4 => vec![v4()],
            AeroFunding::Auto => vec![morpho(), v4()],
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Funder::Morpho(m) => m.name(),
            Funder::V4(v) => v.name(),
        }
    }

    /// The contract holding the capital.
    fn holder(&self) -> Address {
        match self {
            Funder::Morpho(m) => m.lender(),
            Funder::V4(v) => v.manager(),
        }
    }

    fn fee(&self, amount: U256) -> U256 {
        match self {
            Funder::Morpho(m) => m.fee(amount),
            Funder::V4(v) => v.fee(amount),
        }
    }

    /// Gas this source saves against the Morpho flash loan.
    fn gas_saving(&self) -> u64 {
        match self {
            Funder::Morpho(_) => 0,
            Funder::V4(_) => V4_GAS_SAVING,
        }
    }

    fn funding(&self) -> Funding {
        match self {
            Funder::Morpho(_) => Funding::Morpho,
            Funder::V4(_) => Funding::V4,
        }
    }

    /// Calldata for this source's entry point of the configured contract. Every entry point (both
    /// contracts, both sources) has minProfit at the same offset (`ARB_MIN_PROFIT`) and returns the
    /// profit as one uint256. On StepArb (`contract`) the migration is a step between buy and sell
    /// (depositLiquid of the whole old-AERO balance to the contract), and old / new AERO are swept.
    #[allow(clippy::too_many_arguments)]
    fn encode(&self, a: &AeroConfig, contract: Address, loan_token: Address, amount: U256, buy: Vec<ArbStep>, sell: Vec<ArbStep>, deadline: u64) -> Bytes {
        if a.contract_kind == AeroContract::Steps {
            let migrate: Bytes = IAeroMigration::depositLiquidCall { amount: U256::ZERO, recipient: contract }.abi_encode().into();
            let steps = buy.into_iter().chain([step_arb::call_step(a.migration, a.old_token, U256::MAX, migrate, 4)]).chain(sell).collect();
            return step_arb::calldata(self.funding(), loan_token, amount, steps, vec![a.old_token, a.new_token], deadline);
        }
        let (min_profit, deadline) = (U256::ZERO, U256::from(deadline));
        match self {
            Funder::Morpho(_) => IAeroArb::arbCall { loanToken: loan_token, loanAmount: amount, buy, sell, minProfit: min_profit, deadlineBlock: deadline }.abi_encode().into(),
            Funder::V4(_) => IAeroArb::arbFlashAccountingCall { loanToken: loan_token, loanAmount: amount, buy, sell, minProfit: min_profit, deadlineBlock: deadline }.abi_encode().into(),
        }
    }
}

/// What each enabled source can lend now, per loan token (once at start; the exact check is what
/// catches a source running dry later).
async fn log_funding<P: Provider>(p: &P, a: &AeroConfig) {
    let tokens: Vec<Address> = a.loan_tokens.iter().map(|l| l.token).collect();
    for f in Funder::enabled(a) {
        match flash::available(p, f.holder(), &tokens, BlockId::latest()).await {
            Ok(bal) => {
                for (l, b) in a.loan_tokens.iter().zip(bal) {
                    info!(funding = f.name(), source = %f.holder(), token = %l.token, available = b.map(|b| units(b, l.decimals)), "aero: flash liquidity");
                }
            }
            Err(e) => warn!(funding = f.name(), error = %e, "aero: flash liquidity read failed"),
        }
    }
}

/// The profit an entry point returned (`arb` and `arbFlashAccounting` share the return type).
fn decode_profit(out: &Bytes) -> Option<U256> {
    IAeroArb::arbCall::abi_decode_returns(out).ok()
}

/// The deployed contract must match the config and be ready (executor, not paused).
async fn preflight<P: Provider>(p: &P, a: &AeroConfig, c: Address, signer: Address, factory_list: bool) -> Result<()> {
    let arb = IAeroArb::new(c, p);
    anyhow::ensure!(arb.executor().call().await? == signer, "aero contract executor is not the hot wallet");
    anyhow::ensure!(arb.PROFIT_RECEIVER().call().await? != signer, "aero contract PROFIT_RECEIVER is the hot wallet: profits must go to a cold wallet");
    anyhow::ensure!(!arb.paused().call().await?, "aero contract is paused");
    match a.contract_kind {
        AeroContract::Migration => {
            anyhow::ensure!(arb.MIGRATION().call().await? == a.migration, "aero contract MIGRATION differs from config");
            anyhow::ensure!(arb.NEW_TOKEN().call().await? == a.new_token, "aero contract NEW_TOKEN differs from config");
        }
        // The migration is a step: StepArb must be allowed to call it.
        AeroContract::Steps => anyhow::ensure!(arb.allowedTarget(a.migration).call().await?, "StepArb: the Migration {} is not an allowlisted target", a.migration),
    }
    let legacy_ok = if factory_list { arb.allowedFactory(a.legacy_factory).call().await? } else { arb.allowedTarget(a.legacy_factory).call().await? };
    anyhow::ensure!(legacy_ok, "aero contract: legacy_factory not allowlisted (buy side)");
    info!(contract = %c, funding = ?a.funding, "aero: pre-flight passed");
    Ok(())
}

/// The contract can be funded the way `[aero] funding` asks (read-only, so it also runs in observe
/// mode: there a mismatch would only show as exact checks that always revert).
async fn check_funding<P: Provider>(p: &P, a: &AeroConfig, c: Address) -> Result<()> {
    let arb = IAeroArb::new(c, p);
    for f in Funder::enabled(a) {
        let theirs = match f {
            Funder::Morpho(_) => arb.MORPHO().call().await?,
            // A deployment from before V4 funding has no POOL_MANAGER(): the call itself fails.
            Funder::V4(_) => arb.POOL_MANAGER().call().await.context("aero contract has no POOL_MANAGER(): redeploy it, or set [aero] funding = \"morpho\"")?,
        };
        anyhow::ensure!(theirs == f.holder(), "aero contract {} source {theirs} differs from config {}", f.name(), f.holder());
    }
    Ok(())
}

fn decode_addr(b: Option<&Bytes>) -> Option<Address> {
    b.and_then(|b| IAeroFactory::getPoolCall::abi_decode_returns(b).ok()).filter(|a| *a != Address::ZERO)
}

/// Every pool on both sides: buy = old AERO on the legacy V2 factory and the legacy CL factories;
/// sell = new AERO on the Aero factories (config + registry), V2 or CL.
async fn discover<P: Provider>(p: &P, a: &AeroConfig, token0s: &mut HashMap<Address, Address>, factory_list: bool) -> Result<Vec<Path>> {
    let mut sell_factories = a.new_factories.clone();
    if let Some(reg) = a.factory_registry {
        match IFactoryRegistry::new(reg, p).targetFactories().call().await {
            Ok(list) => sell_factories.extend(list),
            Err(e) => warn!(error = %e, "aero: factory registry read failed"),
        }
    }
    sell_factories.sort();
    sell_factories.dedup();
    // (factory, buy side?); CL factories answer tickSpacings().
    let mut factories: Vec<(Address, bool)> = a.legacy_cl_factories.iter().map(|f| (*f, true)).collect();
    factories.extend(sell_factories.iter().map(|f| (*f, false)));
    let probe: Vec<(Address, Bytes)> = factories.iter().map(|(f, _)| (*f, IClFactory::tickSpacingsCall {}.abi_encode().into())).collect();
    let kinds = multicall(p, &probe, BlockId::latest()).await?;

    // Candidate pools (pool address filled from getPool).
    let mut calls: Vec<(Address, Bytes)> = vec![];
    let mut meta: Vec<(usize, bool, Venue)> = vec![];
    let blank = |factory: Address, kind: u64, tick_spacing: Option<i32>| Venue { pool: Address::ZERO, factory, kind, tick_spacing, token0: Address::ZERO };
    for (li, l) in a.loan_tokens.iter().enumerate() {
        calls.push((a.legacy_factory, ILegacyFactory::getPoolCall { a: a.old_token, b: l.token, stable: false }.abi_encode().into()));
        meta.push((li, true, blank(a.legacy_factory, KIND_LEGACY_V2, None)));
    }
    for ((f, buy), k) in factories.iter().zip(&kinds) {
        let spacings = k.as_ref().and_then(|b| IClFactory::tickSpacingsCall::abi_decode_returns(b).ok());
        let token = if *buy { a.old_token } else { a.new_token };
        for (li, l) in a.loan_tokens.iter().enumerate() {
            match &spacings {
                Some(ts) => {
                    for t in ts {
                        calls.push((*f, IClFactory::getPoolCall { a: token, b: l.token, tickSpacing: *t }.abi_encode().into()));
                        meta.push((li, *buy, blank(*f, KIND_CL, Some(t.as_i32()))));
                    }
                }
                // Legacy CL factories are CL by config; a sell factory without tick spacings is V2.
                None if !*buy => {
                    calls.push((*f, IAeroFactory::getPoolCall { a: token, b: l.token }.abi_encode().into()));
                    meta.push((li, false, blank(*f, KIND_AERO_V2, None)));
                }
                None => {}
            }
        }
    }
    let r = multicall(p, &calls, BlockId::latest()).await?;
    let mut out: Vec<(usize, bool, Venue)> = vec![];
    for ((li, buy, mut v), res) in meta.into_iter().zip(r) {
        let Some(pool) = decode_addr(res.as_ref()) else { continue };
        v.pool = pool;
        out.push((li, buy, v));
    }
    // Liquidity + token0 per pool. A failed liquidity probe (busy endpoint) keeps the venue:
    // dropping it would hide it for DISCOVERY_EVERY blocks; quoting an empty pool only costs quotes.
    let mut probe: Vec<(Address, Bytes)> = vec![];
    for (_, _, v) in &out {
        let state: Bytes = if v.kind == KIND_CL { IPoolState::liquidityCall {}.abi_encode().into() } else { IPoolState::getReservesCall {}.abi_encode().into() };
        probe.push((v.pool, state));
        probe.push((v.pool, IPoolState::token0Call {}.abi_encode().into()));
    }
    let st = multicall(p, &probe, BlockId::latest()).await?;
    let mut kept: Vec<(usize, bool, Venue)> = vec![];
    for (i, (li, buy, mut v)) in out.into_iter().enumerate() {
        let empty = st[2 * i].as_ref().is_some_and(|b| {
            if v.kind == KIND_CL {
                IPoolState::liquidityCall::abi_decode_returns(b).is_ok_and(|l| l == 0)
            } else {
                IPoolState::getReservesCall::abi_decode_returns(b).is_ok_and(|r| r._0.is_zero() || r._1.is_zero())
            }
        });
        // token0 never changes: cached for good, so one failed read does not hide the pool.
        let t0 = match st[2 * i + 1].as_ref().and_then(|b| IPoolState::token0Call::abi_decode_returns(b).ok()) {
            Some(t) => *token0s.entry(v.pool).or_insert(t),
            None => match token0s.get(&v.pool) {
                Some(t) => *t,
                None => continue,
            },
        };
        if empty {
            continue;
        }
        v.token0 = t0;
        kept.push((li, buy, v));
    }
    // Which ones the contract may use (its gate is the factory; one multicall).
    let allowed: Vec<bool> = match a.contract {
        Some(c) => {
            // Both lists answer a bool, so one decoder serves either.
            let q: Vec<(Address, Bytes)> = kept.iter().map(|(_, _, v)| (c, factory_allowed_call(v.factory, factory_list))).collect();
            multicall(p, &q, BlockId::latest()).await?.iter().map(|r| r.as_ref().and_then(|b| IAeroArb::allowedFactoryCall::abi_decode_returns(b).ok()).unwrap_or(false)).collect()
        }
        None => vec![true; kept.len()],
    };
    Ok(kept.into_iter().zip(allowed).map(|((loan, buy, venue), allowed)| Path { loan, buy, venue, allowed }).collect())
}

fn units(x: U256, dec: u8) -> f64 {
    math::units(x, dec)
}

/// A quoted curve: output at each input of the ladder (both in raw token units, as f64), None
/// where the quote failed (beyond the pool's depth).
#[derive(Debug, Clone)]
struct Curve {
    xs: Vec<f64>,
    ys: Vec<Option<f64>>,
}

impl Curve {
    /// Piecewise-linear from the origin (under-estimates a concave curve); None past the last
    /// point that answered.
    fn at(&self, x: f64) -> Option<f64> {
        if x <= 0.0 {
            return Some(0.0);
        }
        let mut prev = (0.0, 0.0);
        for (x1, y1) in self.xs.iter().zip(&self.ys) {
            let y1 = (*y1)?;
            if x <= *x1 {
                let (x0, y0) = prev;
                return Some(y0 + (y1 - y0) * (x - x0) / (x1 - x0).max(1e-9));
            }
            prev = (*x1, y1);
        }
        None
    }
}

impl Curve {
    /// Largest input that was quoted (the depth we know of).
    fn cap(&self) -> f64 {
        self.xs.iter().zip(&self.ys).take_while(|(_, y)| y.is_some()).last().map_or(0.0, |(x, _)| *x)
    }
}

/// Greedy split of `total` over concave curves: each step puts up to one chunk (total /
/// SPLIT_CHUNKS) where it earns the most per unit, and a pool shallower than a chunk still gets
/// what it can take (its last quoted size). Returns (input per curve, total output); None if the
/// curves together cannot take `total`.
fn split(curves: &[&Curve], total: f64) -> Option<(Vec<f64>, f64)> {
    if curves.is_empty() || total <= 0.0 {
        return None;
    }
    let chunk = total / SPLIT_CHUNKS as f64;
    let caps: Vec<f64> = curves.iter().map(|c| c.cap()).collect();
    let mut used = vec![0.0; curves.len()];
    let mut outs = vec![0.0; curves.len()];
    let mut left = total;
    // Every step fills a chunk or exhausts a pool: bounded by chunks + pools.
    for _ in 0..SPLIT_CHUNKS + curves.len() + 1 {
        if left <= total * 1e-9 {
            break;
        }
        let mut best: Option<(usize, f64, f64, f64)> = None; // (pool, rate, dx, y)
        for (i, c) in curves.iter().enumerate() {
            let dx = chunk.min(left).min(caps[i] - used[i]);
            if dx <= 0.0 {
                continue;
            }
            let Some(y) = c.at(used[i] + dx) else { continue };
            let rate = (y - outs[i]) / dx;
            if best.is_none_or(|(_, r, _, _)| rate > r) {
                best = Some((i, rate, dx, y));
            }
        }
        let (i, _, dx, y) = best?;
        used[i] += dx;
        outs[i] = y;
        left -= dx;
    }
    (left <= total * 1e-9).then(|| (used, outs.iter().sum()))
}

/// Plan for one loan size: buy split, sell split, estimated output (loan units, raw).
#[derive(Debug, Clone)]
struct Plan {
    loan: f64,
    buy: Vec<(usize, f64)>,
    sell: Vec<(usize, f64)>,
    out: f64,
}

/// `buys` / `sells` are (path index, curve) for one loan token's allowed paths.
fn plan(loan: f64, buys: &[(usize, Curve)], sells: &[(usize, Curve)]) -> Option<Plan> {
    let bc: Vec<&Curve> = buys.iter().map(|(_, c)| c).collect();
    let (bu, old_out) = split(&bc, loan)?;
    let sc: Vec<&Curve> = sells.iter().map(|(_, c)| c).collect();
    let (su, out) = split(&sc, old_out)?;
    Some(Plan {
        loan,
        buy: buys.iter().zip(bu).filter(|(_, u)| *u > 0.0).map(|((pi, _), u)| (*pi, u)).collect(),
        sell: sells.iter().zip(su).filter(|(_, u)| *u > 0.0).map(|((pi, _), u)| (*pi, u)).collect(),
        out,
    })
}

fn step(kind: u64, target: Address, token_in: Address, amount: U256) -> ArbStep {
    ArbStep { kind: U256::from(kind), target, tokenIn: token_in, amountIn: amount, patchOffset: U256::ZERO, data: Bytes::new() }
}

/// Contract steps for a plan. Buy: fixed amounts, the largest takes the loan remainder. Sell:
/// fixed amounts x SELL_FIXED_SHARE, the largest takes whatever was bought on top.
fn steps(a: &AeroConfig, paths: &[Path], loan_token: Address, p: &Plan) -> (Vec<ArbStep>, Vec<ArbStep>) {
    let side = |legs: &[(usize, f64)], token: Address, share: f64| -> Vec<ArbStep> {
        let mut legs: Vec<(usize, f64)> = legs.to_vec();
        legs.sort_by(|x, y| x.1.total_cmp(&y.1));
        let last = legs.len().saturating_sub(1);
        legs.iter()
            .enumerate()
            .map(|(i, (pi, u))| {
                let v = &paths[*pi].venue;
                let amount = if i == last { U256::MAX } else { U256::from((u * share) as u128) };
                step(v.kind, v.pool, token, amount)
            })
            .collect()
    };
    (side(&p.buy, loan_token, 1.0), side(&p.sell, a.new_token, SELL_FIXED_SHARE))
}

/// Quotes, all in one multicall eth_call: USD prices, V2 curves, and the CL curves through the
/// GridQuoter (CL_PER_CALL pools per sub-call). Returns (usd per loan, curve per path).
async fn quote<P: Provider>(cfg: &Config, p: &P, paths: &[Path], loans: &[Vec<U256>], new_ladder: &[U256], quoter: &StateOverride, at: BlockId) -> Result<(Vec<Option<f64>>, Vec<Option<Curve>>)> {
    let a = &cfg.aero;
    let f = math::u256_f64;
    let mut calls: Vec<(Address, Bytes)> = vec![];
    let mut usd_idx = vec![];
    // USD per loan token, read in the same multicall: a Chainlink feed (unless stale), else a pool
    // quote. (feed call, pool call) indices; both None = a USD stablecoin.
    for l in &a.loan_tokens {
        let feed = l.usd_feed.map(|f| {
            calls.push((f, IUsdFeed::latestRoundDataCall {}.abi_encode().into()));
            calls.len() - 1
        });
        let pool = l.usd_pool.map(|up| {
            calls.push((up, ISolidlyPool::getAmountOutCall { amountIn: U256::from(10u64).pow(U256::from(l.decimals)), tokenIn: l.token }.abi_encode().into()));
            calls.len() - 1
        });
        usd_idx.push((feed, pool));
    }
    let ladder = |path: &Path| -> Vec<U256> { if path.buy { loans[path.loan].clone() } else { new_ladder.to_vec() } };
    let token_in = |path: &Path| -> Address { if path.buy { a.loan_tokens[path.loan].token } else { a.new_token } };
    let mut v2_idx: Vec<Option<usize>> = vec![];
    let mut cl: Vec<usize> = vec![];
    for (pi, path) in paths.iter().enumerate() {
        let v = &path.venue;
        if v.kind == KIND_CL {
            v2_idx.push(None);
            cl.push(pi);
            continue;
        }
        v2_idx.push(Some(calls.len()));
        for x in ladder(path) {
            let d: Bytes = if v.kind == KIND_LEGACY_V2 {
                ISolidlyPool::getAmountOutCall { amountIn: x, tokenIn: token_in(path) }.abi_encode().into()
            } else {
                ISolidlyPool::getAmountOutWithTotalFeeCall { amountIn: x, tokenIn: token_in(path) }.abi_encode().into()
            };
            calls.push((v.pool, d));
        }
    }
    // CL groups share one amount ladder per call: buy pools of one loan token, or sell pools.
    let keys: std::collections::BTreeSet<(bool, usize)> = cl.iter().map(|pi| (paths[*pi].buy, paths[*pi].loan)).collect();
    let mut groups: Vec<(Vec<usize>, Vec<U256>)> = vec![];
    for key in keys {
        let members: Vec<usize> = cl.iter().copied().filter(|pi| (paths[*pi].buy, paths[*pi].loan) == key).collect();
        for c in members.chunks(CL_PER_CALL) {
            groups.push((c.to_vec(), ladder(&paths[c[0]])));
        }
    }
    // The GridQuoter calls ride in the same multicall (the override reaches them), so a block
    // costs one eth_call however many pools there are.
    let mut cl_idx = vec![];
    for (members, amounts) in &groups {
        let gpaths: Vec<GPath> = members.iter().map(|pi| GPath { pools: vec![paths[*pi].venue.pool], zeroForOne: vec![token_in(&paths[*pi]) == paths[*pi].venue.token0] }).collect();
        cl_idx.push(calls.len());
        calls.push((SIM_QUOTER, IGridQuoterA::quoteCall { paths: gpaths, amounts: amounts.clone() }.abi_encode().into()));
    }
    let r = multicall_ov(p, &calls, quoter, at).await?;
    let cl_res: Vec<Result<Vec<Vec<U256>>>> = cl_idx
        .iter()
        .map(|i| {
            let b = r[*i].as_ref().context("CL quote sub-call failed")?;
            anyhow::Ok(IGridQuoterA::quoteCall::abi_decode_returns(b)?.outs)
        })
        .collect();
    let usd: Vec<Option<f64>> = usd_idx
        .iter()
        .enumerate()
        .map(|(li, (feed, pool))| {
            if feed.is_none() && pool.is_none() {
                return Some(1.0);
            }
            let now = crate::gates::unix_secs();
            let from_feed = feed.and_then(|i| r[i].as_ref()).and_then(|b| IUsdFeed::latestRoundDataCall::abi_decode_returns(b).ok()).and_then(|d| {
                let age = now.saturating_sub(d.updatedAt.saturating_to::<u64>());
                if age > a.usd_feed_max_age_secs {
                    warn!(loan = %a.loan_tokens[li].token, age_s = age, "aero: USD feed is stale; falling back to the pool / last price");
                    return None;
                }
                d.answer.is_positive().then(|| units(d.answer.into_raw(), 8))
            });
            from_feed.or_else(|| pool.and_then(|i| r[i].as_ref()).and_then(|b| ISolidlyPool::getAmountOutCall::abi_decode_returns(b).ok()).map(|v| units(v, 6)))
        })
        .collect();
    let mut curves: Vec<Option<Curve>> = vec![None; paths.len()];
    for (pi, path) in paths.iter().enumerate() {
        let Some(start) = v2_idx[pi] else { continue };
        let xs = ladder(path);
        let ys: Vec<Option<f64>> = (0..xs.len()).map(|k| r[start + k].as_ref().and_then(|b| ISolidlyPool::getAmountOutCall::abi_decode_returns(b).ok()).filter(|v| !v.is_zero()).map(f)).collect();
        curves[pi] = Some(Curve { xs: xs.iter().map(|x| f(*x)).collect(), ys });
    }
    for ((members, amounts), res) in groups.iter().zip(cl_res) {
        match res {
            Ok(outs) => {
                for (pi, o) in members.iter().zip(outs) {
                    let ys = o.iter().map(|v| (!v.is_zero()).then(|| f(*v))).collect();
                    curves[*pi] = Some(Curve { xs: amounts.iter().map(|x| f(*x)).collect(), ys });
                }
            }
            Err(e) => warn!(error = %e, pools = members.len(), "aero: CL quotes failed"),
        }
    }
    Ok((usd, curves))
}

/// (path index, curve) of one loan token's allowed, quoted paths on one side.
fn side_curves(paths: &[Path], curves: &[Option<Curve>], loan: usize, buy: bool) -> Vec<(usize, Curve)> {
    paths
        .iter()
        .enumerate()
        .filter(|(_, x)| x.loan == loan && x.buy == buy && x.allowed)
        .filter_map(|(pi, _)| curves[pi].clone().map(|c| (pi, c)))
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn tick<P: Provider + Clone>(
    cfg: &Config,
    p: &P,
    head: &Head,
    new_ladder: &[U256],
    paths: &[Path],
    exec: Option<&ExecHandle>,
    cooling: bool,
    state: &mut TickState,
    journal: &mut std::fs::File,
    at: BlockId,
) -> Result<()> {
    let a = &cfg.aero;
    let block = head.number;
    // Loan ladders in loan units, sized with the previous pass's USD price (first pass:
    // exec.eth_usd_fallback for priced tokens, 1.0 for stablecoins).
    let loans: Vec<Vec<U256>> = a
        .loan_tokens
        .iter()
        .enumerate()
        .map(|(li, l)| {
            let r = (a.max_loan_usd / MIN_LOAN_USD).powf(1.0 / (LOAN_STEPS - 1) as f64);
            let usd = state.usd[li].max(1e-9);
            (0..LOAN_STEPS).map(|k| U256::from((MIN_LOAN_USD * r.powi(k as i32) / usd * 10f64.powi(l.decimals as i32)) as u128)).collect()
        })
        .collect();
    // The pass quotes at the head itself, pinned by number, so every call of the pass and the exact
    // check read the same state even across endpoints. Under Denim (200 ms blocks) what moves the
    // pools before our tx is the next block; a spread is taken there.
    let (usd, curves) = quote(cfg, p, paths, &loans, new_ladder, &state.quoter, at).await?;
    let exact_at = at;

    for (li, u) in usd.iter().enumerate() {
        if let Some(u) = u.filter(|u| *u > 0.0) {
            state.usd[li] = u;
        }
    }

    // Gas in USD for a plan with `legs` swaps funded by `f` (the price of the WETH-like loan token,
    // if any).
    let eth_usd = state.eth_usd(a, cfg.exec.eth_usd_fallback);
    let gas_usd = |legs: usize, f: &Funder| crate::arb_exec::gas_cost_usd(arb_gas(a, legs, f), head.base_fee, a.priority_fee_wei, eth_usd);

    // Minimum net profit for a loan of `loan_usd`: the absolute floor, and min_profit_bps of the loan.
    let floor = |loan_usd: f64| a.min_profit_usd.max(loan_usd * a.min_profit_bps / 1e4);

    // Funding sources, and the cheapest one's cost (flash fee + gas, USD) for a loan with `legs` swaps
    // (estimate only; the exact check runs the real entry point of each source).
    let funders = Funder::enabled(a);
    let funding_cost = |loan: f64, legs: usize, dec: f64, u: f64| {
        funders.iter().map(|f| math::u256_f64(f.fee(U256::from(loan as u128))) / dec * u + gas_usd(legs, f)).fold(f64::INFINITY, f64::min)
    };

    // ---- per loan token: best plan over the loan ladder (profit net of gas and flash fee)
    let mut best: Option<(usize, Plan, f64)> = None; // (loan, plan, est net profit usd)
    for (li, l) in a.loan_tokens.iter().enumerate() {
        let Some(u) = usd[li] else { continue };
        let (buys, sells) = (side_curves(paths, &curves, li, true), side_curves(paths, &curves, li, false));
        if buys.is_empty() || sells.is_empty() {
            continue;
        }
        let dec = 10f64.powi(l.decimals as i32);
        // lb = best net over the ladder (journaled); lq = best net among sizes that clear the floor
        // (a bigger loan can net more at a thinner, riskier margin).
        let mut lb: Option<(Plan, f64)> = None;
        let mut lq: Option<(Plan, f64)> = None;
        for x in &loans[li] {
            let Some(pl) = plan(math::u256_f64(*x), &buys, &sells) else { continue };
            let net_usd = (pl.out - pl.loan) / dec * u - funding_cost(pl.loan, pl.buy.len() + pl.sell.len(), dec, u);
            if net_usd >= floor(pl.loan / dec * u) && lq.as_ref().is_none_or(|(_, b)| net_usd > *b) {
                lq = Some((pl.clone(), net_usd));
            }
            if lb.as_ref().is_none_or(|(_, b)| net_usd > *b) {
                lb = Some((pl, net_usd));
            }
        }
        // Spread at the smallest size: best buy price vs best sell price (USD per AERO).
        let p_old = buys.iter().filter_map(|(_, c)| c.ys[0].map(|y| c.xs[0] / dec * u / (y / 1e18))).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.min(v))));
        let p_new = sells.iter().filter_map(|(_, c)| c.ys[0].map(|y| y / dec * u / (c.xs[0] / 1e18))).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))));
        let spread_bps = match (p_old, p_new) {
            (Some(o), Some(n)) => Some((n / o - 1.0) * 1e4),
            _ => None,
        };
        let est = lb.as_ref().map(|(_, e)| *e);
        let s = spread_bps.unwrap_or(0.0);
        let due = state.logged.get(&li).is_none_or(|(s0, b0)| (s - s0).abs() >= TICK_LOG_BPS || block >= b0 + TICK_LOG_EVERY);
        if est.is_some_and(|e| e > 0.0) || due {
            state.logged.insert(li, (s, block));
            writeln!(
                journal,
                "{}",
                json!({ "kind": "aero_tick", "block": block, "loan": l.token, "buy_pools": buys.len(), "sell_pools": sells.len(), "p_old": p_old, "p_new": p_new, "spread_bps": spread_bps,
                        "best_loan": lb.as_ref().map(|(pl, _)| pl.loan / dec), "est_profit_usd": est })
            )?;
        }
        if let Some((pl, e)) = lq
            && best.as_ref().is_none_or(|(_, _, b)| e > *b)
        {
            best = Some((li, pl, e));
        }
    }
    let Some((li, best_plan, est)) = best else { return Ok(()) };
    let l = &a.loan_tokens[li];
    let dec = 10f64.powi(l.decimals as i32);
    let route = |pl: &Plan| -> serde_json::Value {
        let leg = |(pi, u): &(usize, f64), d: f64| json!({ "venue": paths[*pi].venue.label(), "pool": paths[*pi].venue.pool, "in": u / d });
        json!({ "buy": pl.buy.iter().map(|x| leg(x, dec)).collect::<Vec<_>>(), "sell": pl.sell.iter().map(|x| leg(x, 1e18)).collect::<Vec<_>>() })
    };
    info!(block, loan = best_plan.loan / dec, est_net_usd = est, buy_pools = best_plan.buy.len(), sell_pools = best_plan.sell.len(), "aero: OPPORTUNITY (estimate)");
    let Some(contract) = a.contract else {
        writeln!(journal, "{}", json!({ "kind": "aero_opp", "block": block, "loan": best_plan.loan / dec, "route": route(&best_plan), "est_profit_usd": est, "exact_usd": null }))?;
        return Ok(());
    };

    // ---- exact check, bid and send (arb_exec.rs): the best size and its neighbours (re-split
    // each), each through every enabled funding source (funding = "auto": both, in parallel), Morpho
    // first so it keeps exact ties. Priced like the real tx: Aero's MEV tax and dynamic fees read
    // tx.gasprice, which is 0 in a plain eth_call. The pending nonce is fetched at the same time
    // (another bot may sign with the same hot wallet and its sends are invisible to this process).
    let deadline = block + crate::arb_exec::deadline_blocks(a.deadline_secs, state.block_time(head));
    let ae = ArbExecutor {
        name: "aero",
        contract,
        from: exec.map_or(cfg.exec.executor, |e| e.address()),
        gas_limit: a.gas_limit,
        base_fee_wei: head.base_fee,
        bid: bid_policy(a),
        min_profit_slot: ARB_MIN_PROFIT,
        min_profit_keep: a.min_profit_keep,
        eth_usd,
        decode: decode_profit,
    };
    let (buys, sells) = (side_curves(paths, &curves, li, true), side_curves(paths, &curves, li, false));
    let plans: Vec<Plan> = [0.7, 1.0, 1.4].iter().filter_map(|k| plan(best_plan.loan * k, &buys, &sells)).collect();
    // (plan, source, amount) per candidate.
    let mut built: Vec<(usize, &Funder, U256)> = vec![];
    let mut cands: Vec<Candidate> = vec![];
    for (pi, pl) in plans.iter().enumerate() {
        let amt = U256::from(pl.loan as u128);
        let (buy, sell) = steps(a, paths, l.token, pl);
        for f in &funders {
            built.push((pi, f, amt));
            cands.push(Candidate {
                calldata: f.encode(a, contract, l.token, amt, buy.clone(), sell.clone(), deadline),
                // Each source at its own gas: with equal profit (both 0 fee) V4 wins on its gas
                // saving, Morpho wherever V4 cannot fund the size.
                gas_est: arb_gas(a, pl.buy.len() + pl.sell.len(), f),
                floor_usd: floor(pl.loan / dec * state.usd[li]),
                l1_usd: 0.0,
            });
        }
    }
    let call = |tx: TransactionRequest| call_at(p, tx, exact_at);
    // The L1 data fee (one read for the largest calldata) is paid whatever the priority: in the net.
    let l1 = crate::arb_exec::l1_fee_wei(p, cands.iter().map(|c| c.calldata.len()).max().unwrap_or(0));
    let (exact, l1) = tokio::join!(ae.exact(&cands, call), l1);
    let l1_usd = l1.map_or(0.0, |w| math::u256_f64(w) / 1e18 * eth_usd);
    for c in &mut cands {
        c.l1_usd = l1_usd;
    }
    let usd = |x: U256| units(x, l.decimals) * state.usd[li];
    // Each failure tagged with its funding source (with funding = "auto", which one broke matters).
    let errors: Vec<String> = exact
        .iter()
        .zip(&built)
        .filter_map(|(r, (_, f, _))| r.as_ref().err().map(|e| format!("{}: {}", f.name(), e.to_string().chars().take(160).collect::<String>())))
        .collect();
    let Some(pk) = ae.pick(&cands, &exact, usd) else {
        writeln!(journal, "{}", json!({ "kind": "aero_opp", "block": block, "loan": best_plan.loan / dec, "route": route(&best_plan), "est_profit_usd": est, "exact_usd": null, "errors": errors }))?;
        return Ok(());
    };
    let (pi, source, amt) = built[pk.index];
    let (c, funder) = (&cands[pk.index], source.name());
    let (profit_usd, net_usd) = (pk.gross_usd, pk.net_usd);
    info!(block, loan = units(amt, l.decimals), profit_usd, net_usd, funding = funder, "aero: exact check");
    writeln!(journal, "{}", json!({ "kind": "aero_opp", "block": block, "loan": units(amt, l.decimals), "route": route(&plans[pi]), "est_profit_usd": est, "exact_usd": profit_usd, "gas_usd": profit_usd - net_usd, "net_usd": net_usd, "funding": funder, "cooling": cooling }))?;
    if net_usd < c.floor_usd {
        info!(block, net_usd, need = c.floor_usd, "aero: exact net below the floor; not sending");
        return Ok(());
    }

    // ---- send (live only; kill switch, cooldown, one tx in flight, daily gas budget for reverts;
    // the running gas total is in aero_outcome)
    let Some(e) = exec else { return Ok(()) };
    if cooling || !e.can_send() {
        return Ok(()); // cooling down, or the previous arb is still in flight
    }
    // The node wants gas_limit x maxFee up front, which a bid raises: the executor's background
    // balance reading, minus what is in flight.
    let Some(balance) = e.spendable().map(math::u256_f64) else {
        warn!("aero: no hot-wallet balance reading yet; not sending");
        return Ok(());
    };
    let Some(t) = ae.terms(c, &pk, balance, usd, call).await else { return Ok(()) };
    // Daily gas budget: what this tx burns if it reverts (L2 gas at its priority + L1 fee).
    let gates = crate::arb_exec::Gates { kill_switch_file: &cfg.exec.kill_switch_file, budget: &mut state.budget, daily_cap_usd: cfg.exec.daily_gas_budget_usd, now_unix: head.timestamp };
    match ae.send(e, c, &t, gates, block, deadline).await {
        Ok(None) => {}
        Ok(Some(inf)) => {
            warn!(tx = %inf.hash, profit_usd, net_usd, priority_gwei = t.priority_fee_wei as f64 / 1e9, bid_usd = t.bid_usd, "aero: SENT");
            writeln!(journal, "{}", json!({ "kind": "aero_sent", "block": block, "tx": inf, "loan": units(amt, l.decimals), "route": route(&plans[pi]), "exact_usd": profit_usd, "net_usd": net_usd, "funding": funder, "priority_fee_wei": t.priority_fee_wei, "bid_usd": t.bid_usd }))?;
        }
        Err(err) => warn!(error = %err, "aero: send failed"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(points: &[(f64, f64)]) -> Curve {
        Curve { xs: points.iter().map(|p| p.0).collect(), ys: points.iter().map(|p| Some(p.1)).collect() }
    }

    #[test]
    fn curve_is_linear_between_points_and_stops_past_the_last() {
        let c = Curve { xs: vec![10.0, 20.0, 40.0], ys: vec![Some(100.0), Some(180.0), None] };
        assert_eq!(c.at(5.0), Some(50.0), "from the origin");
        assert_eq!(c.at(15.0), Some(140.0));
        assert_eq!(c.at(30.0), None, "no quote at 40: unknown, not extrapolated");
    }

    #[test]
    fn greedy_split_uses_the_deeper_pool_more() {
        // Similar price at the start, but b is deeper (its curve bends later).
        let a = curve(&[(10.0, 9.0), (20.0, 16.0), (40.0, 24.0)]);
        let b = curve(&[(10.0, 9.5), (20.0, 18.5), (40.0, 34.0)]);
        let (used, out) = split(&[&a, &b], 40.0).unwrap();
        assert!((used.iter().sum::<f64>() - 40.0).abs() < 1e-9);
        assert!(used[1] > used[0], "{used:?}");
        assert!(out > b.at(40.0).unwrap(), "splitting beats the best single pool: {out}");
    }

    #[test]
    fn split_fails_when_the_pools_cannot_take_it_all() {
        let a = Curve { xs: vec![10.0], ys: vec![Some(9.0)] };
        assert!(split(&[&a], 1000.0).is_none());
    }

    #[test]
    fn a_pool_shallower_than_a_chunk_still_gets_its_share() {
        // `small` is the best price but only quoted up to 10; a chunk of 1000/32 = 31 would not fit.
        let small = curve(&[(5.0, 5.0), (10.0, 9.9)]);
        let deep = curve(&[(250.0, 220.0), (500.0, 430.0), (1000.0, 800.0)]);
        let (used, _) = split(&[&small, &deep], 1000.0).unwrap();
        assert!((used[0] - 10.0).abs() < 1e-9, "small pool takes its whole depth: {used:?}");
        assert!((used[0] + used[1] - 1000.0).abs() < 1e-6);
    }

    #[test]
    fn steps_give_the_remainder_to_the_largest_leg() {
        let a = AeroConfig { new_token: Address::repeat_byte(1), ..Default::default() };
        let v = |b: u8, kind| Venue { pool: Address::repeat_byte(b), factory: Address::ZERO, kind, tick_spacing: None, token0: Address::ZERO };
        let paths = vec![
            Path { loan: 0, buy: true, venue: v(10, KIND_LEGACY_V2), allowed: true },
            Path { loan: 0, buy: true, venue: v(11, KIND_CL), allowed: true },
            Path { loan: 0, buy: false, venue: v(20, KIND_AERO_V2), allowed: true },
            Path { loan: 0, buy: false, venue: v(21, KIND_CL), allowed: true },
        ];
        let p = Plan { loan: 1000.0, buy: vec![(0, 700.0), (1, 300.0)], sell: vec![(2, 100.0), (3, 900.0)], out: 0.0 };
        let (buy, sell) = steps(&a, &paths, Address::repeat_byte(2), &p);
        assert_eq!((buy[0].target, buy[0].amountIn), (Address::repeat_byte(11), U256::from(300u64)));
        assert_eq!((buy[1].target, buy[1].amountIn, buy[1].kind), (Address::repeat_byte(10), U256::MAX, U256::from(KIND_LEGACY_V2)));
        assert_eq!((sell[0].target, sell[0].amountIn), (Address::repeat_byte(20), U256::from(98u64)));
        assert_eq!((sell[1].target, sell[1].amountIn, sell[1].kind), (Address::repeat_byte(21), U256::MAX, U256::from(KIND_CL)));
        assert!(sell.iter().all(|s| s.tokenIn == Address::repeat_byte(1)));
    }

    #[test]
    fn min_profit_slot_is_the_fifth_word() {
        let cd: Bytes = IAeroArb::arbCall { loanToken: Address::ZERO, loanAmount: U256::ZERO, buy: vec![], sell: vec![], minProfit: U256::from(0xabcdu64), deadlineBlock: U256::ZERO }
            .abi_encode()
            .into();
        let w = &cd[ARB_MIN_PROFIT.offset..ARB_MIN_PROFIT.offset + 32];
        assert_eq!(U256::from_be_slice(w), U256::from(0xabcdu64));
    }

    #[test]
    fn both_funding_entry_points_share_arguments_slot_and_return() {
        let v4: Bytes = IAeroArb::arbFlashAccountingCall { loanToken: Address::ZERO, loanAmount: U256::ZERO, buy: vec![], sell: vec![], minProfit: U256::from(0xabcdu64), deadlineBlock: U256::ZERO }
            .abi_encode()
            .into();
        assert_eq!(U256::from_be_slice(&v4[ARB_MIN_PROFIT.offset..ARB_MIN_PROFIT.offset + 32]), U256::from(0xabcdu64), "the executor patches the same slot");

        let pm = Address::repeat_byte(4);
        let a = AeroConfig { funding: AeroFunding::Auto, pool_manager: Some(pm), ..Default::default() };
        let fs = Funder::enabled(&a);
        assert_eq!(fs.iter().map(|f| (f.name(), f.holder())).collect::<Vec<_>>(), vec![("morpho", flash::BASE_MORPHO), ("v4", pm)], "Morpho first: it wins ties");
        let step = ArbStep { kind: U256::from(1u8), target: POOL, tokenIn: Address::repeat_byte(9), amountIn: U256::MAX, patchOffset: U256::ZERO, data: Bytes::new() };
        let enc = |f: &Funder| f.encode(&AeroConfig::default(), Address::ZERO, Address::repeat_byte(9), U256::from(5u8), vec![step.clone()], vec![step.clone()], 77);
        let (m, v) = (enc(&fs[0]), enc(&fs[1]));
        assert_eq!((&m[..4], &v[..4]), (&IAeroArb::arbCall::SELECTOR[..], &IAeroArb::arbFlashAccountingCall::SELECTOR[..]));
        assert_eq!(m[4..], v[4..], "same arguments");
        assert_eq!(decode_profit(&Bytes::from(U256::from(42u8).to_be_bytes::<32>().to_vec())), Some(U256::from(42u8)));
        assert_eq!(Funder::enabled(&AeroConfig { funding: AeroFunding::V4, ..a.clone() }).len(), 1);
        // Same route, same (zero) fee: V4 is modelled cheaper, so "auto" prefers it on equal profit.
        let g = AeroConfig { gas_base: 500_000, gas_per_leg: 100_000, ..a.clone() };
        assert_eq!((arb_gas(&g, 2, &fs[0]), arb_gas(&g, 2, &fs[1])), (700_000, 700_000 - V4_GAS_SAVING));
        assert_eq!((fs[0].fee(U256::from(10u8)), fs[1].fee(U256::from(10u8))), (U256::ZERO, U256::ZERO));
    }

    #[test]
    fn funding_needs_its_source_configured() {
        let r = Address::repeat_byte;
        let ok = AeroConfig { migration: r(1), old_token: r(2), new_token: r(3), legacy_factory: r(5), new_factories: vec![r(6)], loan_tokens: toml::from_str::<AeroConfig>("loan_tokens = [{ token = \"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913\", decimals = 6 }]").unwrap().loan_tokens, ..Default::default() };
        assert!(validate(&ok).is_ok());
        let v4 = AeroConfig { funding: AeroFunding::V4, ..ok.clone() };
        assert!(validate(&v4).unwrap_err().to_string().contains("needs pool_manager"));
        assert!(validate(&AeroConfig { pool_manager: Some(r(4)), ..v4 }).is_ok());
        assert!(validate(&AeroConfig { morpho: Some(Address::ZERO), ..ok }).unwrap_err().to_string().contains("needs morpho"));
    }

    const POOL: Address = Address::repeat_byte(0x42);

    #[test]
    fn bid_policy_comes_from_the_aero_config() {
        let a = AeroConfig { priority_fee_wei: 7, bid_min_profit_usd: 2.0, bid_share: 0.25, bid_max_priority_wei: 9, ..Default::default() };
        let b = bid_policy(&a);
        assert_eq!((b.base_priority_wei, b.min_profit_usd, b.share, b.max_priority_wei), (7, 2.0, 0.25, 9));
    }

}

#[cfg(test)]
mod e2e {
    //! The send path end to end on a LOCAL anvil fork of Base (never mainnet): AeroMigrationArb
    //! deployed and set up like the owner would, an arb through the real legacy pool and the real
    //! Migration (the new-AERO sale is a fixed-rate stand-in router from the Foundry tests), checked
    //! exactly, picked, priced and sent by ArbExecutor through the Executor, for both funding sources.
    //!
    //! Run: cargo test e2e -- --ignored --nocapture   (needs `anvil` on PATH and `forge build` in
    //! contracts/; BASE_FORK_URL optional)
    use alloy::{
        network::{EthereumWallet, TransactionBuilder},
        node_bindings::Anvil,
        primitives::B256,
        providers::{ProviderBuilder, ext::AnvilApi},
        signers::local::PrivateKeySigner,
    };

    use super::*;
    use crate::{
        arb_exec::{ArbExecutor, BidPolicy, Candidate, DailyBudget, Gates},
        executor::{Broadcaster, FailurePolicy},
    };

    sol! {
        interface IArbAdmin {
            function setAllowedTarget(address target, bool allowed) external;
            function setAllowedFactory(address factory, bool allowed) external;
        }
        interface IFixedRateRouter { function swap(uint256 amountIn, address to) external returns (uint256); }
        interface IErc20E { function balanceOf(address) external view returns (uint256); function transfer(address to, uint256 amount) external returns (bool); }
    }
    const MORPHO: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
    const POOL_MANAGER: Address = address!("498581fF718922c3f8e6A244956aF099B2652b2b");
    const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");

    fn deploy_code(artifact: &str, args: &[u8]) -> Bytes {
        let a: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(artifact).unwrap_or_else(|_| panic!("{artifact}: run `forge build` in contracts/"))).unwrap();
        let mut code = a["bytecode"]["object"].as_str().unwrap().parse::<Bytes>().unwrap().to_vec();
        code.extend_from_slice(args);
        code.into()
    }

    #[tokio::test]
    #[ignore = "needs anvil + network; run with --ignored"]
    async fn e2e_arb_executor_sends_a_profitable_arb_on_anvil_fork() {
        run(AeroContract::Migration).await;
    }

    /// The same arb on the generic StepArb: the migration is one more step.
    #[tokio::test]
    #[ignore = "needs anvil + network; run with --ignored"]
    async fn e2e_the_same_arb_runs_on_the_generic_step_arb() {
        run(AeroContract::Steps).await;
    }

    async fn run(kind: AeroContract) {
        let cfg = Config::load("config.example.toml").unwrap();
        let a = &cfg.aero;
        let fork = std::env::var("BASE_FORK_URL").unwrap_or_else(|_| "https://base.gateway.tenderly.co".into());
        let anvil = Anvil::new().fork(fork).chain_id(8453).spawn();
        let url = anvil.endpoint_url();
        let owner: PrivateKeySigner = anvil.keys()[0].clone().into();
        let hot: PrivateKeySigner = anvil.keys()[1].clone().into();
        let receiver = anvil.addresses()[2];
        let p = ProviderBuilder::new().connect_http(url.clone());
        let owner_p = ProviderBuilder::new().wallet(EthereumWallet::from(owner.clone())).connect_http(url.clone());

        // 1. Deploy as the owner would, allowlist the legacy V2 factory (pool steps) and the stand-in
        //    router (router step). The router pays 0.95 USDC per new AERO: above old AERO's price.
        let word = |x: Address| B256::left_padding_from(x.as_slice()).0;
        let (artifact, ctor) = match kind {
            AeroContract::Migration => ("../contracts/out/AeroMigrationArb.sol/AeroMigrationArb.json", vec![MORPHO, POOL_MANAGER, a.migration, a.old_token, a.new_token, owner.address(), hot.address(), receiver]),
            AeroContract::Steps => ("../contracts/out/StepArb.sol/StepArb.json", vec![MORPHO, POOL_MANAGER, owner.address(), hot.address(), receiver]),
        };
        let args: Vec<u8> = ctor.iter().flat_map(|x| word(*x)).collect();
        let rc = owner_p.send_transaction(TransactionRequest::default().with_deploy_code(deploy_code(artifact, &args))).await.unwrap().get_receipt().await.unwrap();
        let arb = rc.contract_address.expect("deployed");
        let mut rargs = [word(a.new_token), word(USDC)].concat();
        rargs.extend_from_slice(&U256::from(950_000u64).to_be_bytes::<32>());
        let rc = owner_p.send_transaction(TransactionRequest::default().with_deploy_code(deploy_code("../contracts/out/AeroMigrationArb.fork.t.sol/FixedRateRouter.json", &rargs))).await.unwrap().get_receipt().await.unwrap();
        let router = rc.contract_address.expect("router deployed");
        let mut admin = vec![IArbAdmin::setAllowedFactoryCall { factory: a.legacy_factory, allowed: true }.abi_encode(), IArbAdmin::setAllowedTargetCall { target: router, allowed: true }.abi_encode()];
        if kind == AeroContract::Steps {
            // The migration is a step of StepArb: a target like any other.
            admin.push(IArbAdmin::setAllowedTargetCall { target: a.migration, allowed: true }.abi_encode());
        }
        for data in admin {
            assert!(owner_p.send_transaction(TransactionRequest::default().with_to(arb).with_input(data)).await.unwrap().get_receipt().await.unwrap().status());
        }
        // Fund the stand-in router with USDC (from Morpho's balance, impersonated on the fork).
        p.anvil_impersonate_account(MORPHO).await.unwrap();
        p.anvil_set_balance(MORPHO, U256::from(10u64).pow(U256::from(18))).await.unwrap();
        let fund = IErc20E::transferCall { to: router, amount: U256::from(1_000_000_000_000u64) }.abi_encode();
        assert!(p.send_transaction(TransactionRequest::default().with_from(MORPHO).with_to(USDC).with_input(fund)).await.unwrap().get_receipt().await.unwrap().status());

        // 2. The candidates: 1000 USDC through each funding source (Morpho first).
        let q = ILegacyFactory::getPoolCall { a: a.old_token, b: USDC, stable: false }.abi_encode();
        let pool = Address::from_slice(&p.call(TransactionRequest::default().with_to(a.legacy_factory).with_input(q)).await.unwrap()[12..32]);
        let loan = U256::from(1_000_000_000u64);
        let buy = vec![ArbStep { kind: U256::from(KIND_LEGACY_V2), target: pool, tokenIn: USDC, amountIn: loan, patchOffset: U256::ZERO, data: Bytes::new() }];
        let sell_data: Bytes = IFixedRateRouter::swapCall { amountIn: U256::ZERO, to: arb }.abi_encode().into();
        let sell = vec![ArbStep { kind: U256::ZERO, target: router, tokenIn: a.new_token, amountIn: U256::MAX, patchOffset: U256::from(4u8), data: sell_data }];
        let n = p.get_block_number().await.unwrap();
        let base_fee = p.get_block_by_number(alloy::eips::BlockNumberOrTag::Latest).await.unwrap().unwrap().header.base_fee_per_gas.unwrap_or(0) as u128;
        let funding = AeroConfig { funding: AeroFunding::Auto, pool_manager: Some(POOL_MANAGER), contract: Some(arb), contract_kind: kind, ..a.clone() };
        let funders = Funder::enabled(&funding);
        let cands: Vec<Candidate> = funders
            .iter()
            .map(|f| Candidate { calldata: f.encode(&funding, arb, USDC, loan, buy.clone(), sell.clone(), n + 20), gas_est: arb_gas(a, 2, f), floor_usd: 1.0, l1_usd: 0.0 })
            .collect();
        let ae = ArbExecutor {
            name: "aero-e2e",
            contract: arb,
            from: hot.address(),
            gas_limit: a.gas_limit,
            base_fee_wei: base_fee.max(1),
            bid: BidPolicy { base_priority_wei: 1_000_000, min_profit_usd: 1e9, share: 0.0, max_priority_wei: 1_000_000 },
            min_profit_slot: ARB_MIN_PROFIT,
            min_profit_keep: 0.5,
            eth_usd: 2400.0,
            decode: decode_profit,
        };
        let call = |tx: TransactionRequest| {
            let p = &p;
            async move { Ok(p.call(tx).await?) }
        };
        let usd = |x: U256| units(x, 6);

        // 3. Exact check both, pick, price, send through the gates, settle.
        let exact = ae.exact(&cands, call).await;
        assert!(exact.iter().all(|r| r.is_ok()), "both sources fund it: {exact:?}");
        let pk = ae.pick(&cands, &exact, usd).expect("a profitable candidate");
        assert_eq!(funders[pk.index].name(), "v4", "equal profit: the cheaper V4 entry wins");
        let bcast = Broadcaster::new(reqwest::Client::new(), vec![("anvil".into(), url.to_string())]);
        let policy = FailurePolicy { max_in_window: 3, window: std::time::Duration::from_secs(600), cooldown: std::time::Duration::from_secs(600) };
        let ex = Executor::new(p.clone(), hot.clone(), 8453, bcast, policy);
        let balance = math::u256_f64(ex.balance().await.unwrap());
        // The executor runs as its own task, as in the bot; this test announces the blocks.
        let (blocks_tx, blocks_rx) = tokio::sync::watch::channel(n);
        let (ex, mut settled) = crate::actor::spawn(ex, blocks_rx);
        let t = ae.terms(&cands[pk.index], &pk, balance, usd, call).await.expect("the wallet covers it");
        let mut budget = DailyBudget::default();
        let kill = std::env::temp_dir().join(format!("arb-e2e-no-kill-{}-{kind:?}", std::process::id()));
        let gates = Gates { kill_switch_file: kill.to_str().unwrap(), budget: &mut budget, daily_cap_usd: 5.0, now_unix: 0 };
        let inf = ae.send(&ex, &cands[pk.index], &t, gates, n, n + 20).await.unwrap().expect("sent");
        // Each announced block makes the task look for the receipt (the fork may take a moment
        // to mine it).
        let mut got = None;
        for _ in 0..50 {
            blocks_tx.send_replace(p.get_block_number().await.unwrap());
            if let Ok(s) = tokio::time::timeout(std::time::Duration::from_millis(400), settled.recv()).await {
                got = s;
                break;
            }
        }
        let s = got.expect("settled in time");
        assert_eq!(s.inflight.hash, inf.hash);
        let Outcome::Included { success, gas_used, .. } = s.outcome else { panic!("no outcome: {:?}", s.outcome) };
        assert!(success, "the arb reverted");
        let q = IErc20E::balanceOfCall(receiver).abi_encode();
        let got = U256::from_be_slice(&p.call(TransactionRequest::default().with_to(USDC).with_input(q)).await.unwrap()[..32]);
        assert!(got > U256::ZERO, "profit reached the receiver");
        println!("e2e arb ({kind:?}): tx {} via {} gas {gas_used}, profit {} USDC (6dp)", inf.hash, funders[pk.index].name(), got);

        // 4. The gates hold: a kill-switch file stops the next send before the executor sees it.
        std::fs::write(&kill, b"stop").unwrap();
        let gates = Gates { kill_switch_file: kill.to_str().unwrap(), budget: &mut budget, daily_cap_usd: 5.0, now_unix: 0 };
        assert!(ae.send(&ex, &cands[pk.index], &t, gates, n, n + 20).await.unwrap().is_none(), "kill switch");
        let _ = std::fs::remove_file(&kill);
    }
}
