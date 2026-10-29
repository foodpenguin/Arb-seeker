# Architecture

[繁體中文](architecture.zh-TW.md)

This framework builds atomic on-chain arbitrage bots: detect a price gap, plan a bundle of swaps,
fund it with transient capital (a flash loan or flash accounting), check it exactly against the
contract, send it. The reusable plumbing (block/flashblock driver, send pipeline, the generic
`StepArb` contract) is generic; a strategy plugs in through `OpportunitySource`, and funding
sources through `FlashLoan` / `FlashAccounting`. **Aero merger arbitrage on Base** is the first,
complete strategy.

The crate is a library (`arbitrage_framework`, `bot/src/lib.rs`) plus the Aero bot binary. The
chain plumbing (RPC pool, heads, Flashblocks stream, executor and its task, send gates, simulation,
the shared config sections) is the `onchain-core` crate, shared with `liquidation-framework` and
re-exported under its usual module names; check the repositories out side by side.

## The driver: `runner::run` + `OpportunitySource` (`bot/src/runner.rs`)

```rust
pub trait OpportunitySource<P> {
    fn name(&self) -> &'static str;
    fn on_block(&mut self, p: &P, head: &Head, exec: Option<&ExecHandle>) -> impl Future<Output = Result<()>> + Send;
    fn on_flashblocks(&mut self, p: &P, first: Flashblock, rx: &mut mpsc::Receiver<Flashblock>,
                      exec: Option<&ExecHandle>, last_block: u64) -> impl Future<Output = Result<()>> + Send;
    fn on_outcome(&mut self, block: u64, inflight: &Inflight, outcome: &Outcome,
                  gas_spent_total_wei: U256, cooldown_started: bool) -> Result<()>;
}
```

`runner::run(&mut strategy, provider, executor, heads, flashblocks, RunnerOpts { poll_ms, ws_quiet })`
owns the scheduling:

- new heads from WebSocket `newHeads` (several endpoints), deduplicated and strictly increasing;
  HTTP polling every `poll_ms` while WS has been quiet for `ws_quiet`;
- the executor (live mode) runs as its own task (`onchain_core::actor`): the driver announces each
  head to it, it polls the receipt of the tx in flight, and a settled tx reaches `on_outcome` (with
  gas accounting and whether it started the failure cooldown) through the driver loop; then
  `on_block` per head, after waiting briefly (`wait_polled`, at most `RECEIPT_WAIT`) for that
  head's receipt poll and delivering what it settled to `on_outcome`, so a tx included in it no
  longer counts as in flight and its books (e.g. the budget reservation) are settled first. A strategy sends through the `ExecHandle` it is handed;
- flashblocks batches (the first one plus whatever queued behind it) -> `on_flashblocks`, when the
  strategy runs a same-block mode.

A hook returns `Err` only for fatal errors (the driver stops); recoverable errors are logged and
swallowed inside the strategy. `exec` is `None` in observe-only mode.

## Funding: `FlashLoan` and `FlashAccounting` (`bot/src/flash.rs`)

Two traits on purpose, because the contract interacts with them differently:

| Trait | Model | Methods | Impl |
|---|---|---|---|
| `FlashLoan` | borrow a fixed amount; the lender calls back; repay amount + fee before the callback returns | `name`, `lender`, `fee(amount)` | `MorphoBlue` (no fee) |
| `FlashAccounting` | a singleton tracks the caller's deltas while unlocked; take now, settle the net before `unlock` returns | `name`, `manager`, `fee(amount)` (default 0) | `UniswapV4` (PoolManager) |

`flash::available(p, holder, tokens, block)` reads what a source can lend (its token balances, one
multicall). The capital itself is taken inside the arb contract, whose callback runs the arb, so
off-chain a strategy needs to know which contract funds it, what it costs and how much it holds.

## On-chain: `FlashArbBase.sol`, `StepArb.sol`, `AeroMigrationArb.sol`

`FlashArbBase` (abstract) is what every arb contract here shares: the two funding sources, the
steps, the security model below and the admin. A contract adds its body (`_body(bytes)`, the arb
whatever funded it), its entry points and what it sweeps afterwards. Every contract has two entry
points with **identical arguments**, so the `minProfit` calldata offset and the return type are
the same for both:

- `arb(...)` — Morpho Blue flash loan: `flashLoan` -> `onMorphoFlashLoan` -> body -> exact
  allowance for the repayment.
- `arbFlashAccounting(...)` — Uniswap V4: `unlock` -> `unlockCallback` -> `take` -> body ->
  `sync` / transfer / `settle`, so the delta nets to zero before `unlock` returns.
  `POOL_MANAGER = address(0)` (constructor) disables it (`FundingDisabled`). While the contract
  holds the V4 lock, no swap step may go through V4 itself (a V4 router would `unlock` again).

The contracts:

- **`StepArb`** (generic): `arb(loanToken, loanAmount, steps, sweep, minProfit, deadlineBlock)`.
  The body runs `steps`; afterwards every token in `sweep` (the intermediate tokens) is sent to
  `PROFIT_RECEIVER`, so exact-input rounding or a partial fill never stays at rest. A conversion
  (a token migration, a wrapper) is a kind-0 step on an allowlisted target. Constructor:
  `(morpho, poolManager, owner, executor, profitReceiver)`.
- **`AeroMigrationArb`** (the example): `arb(loanToken, loanAmount, buy, sell, minProfit,
  deadlineBlock)`; the body is buy -> `Migration.depositLiquid` of all old AERO (reverts
  `NothingBought` if none) -> sell, and old / new AERO are swept. The same arb runs on StepArb with
  the migration as a step (`StepArb.fork.t.sol` checks the profit is identical; StepArb costs
  ~5k more gas on the e2e route, ~0.9%).

Steps (`ArbStep`): kind 0 = allowlisted target with an exact allowance (amount optionally
patched into the calldata); kinds 1/2 = Solidly-style pool of an allowlisted factory (`isPool`),
output computed on-chain with `getAmountOut` / `getAmountOutWithTotalFee`; kind 3 = CL pool whose
factory is allowlisted and returns this exact pool from `getPool`, paid inside its own swap
callback (only the active pool, once, at most `amountIn`). `amountIn = type(uint256).max` spends
the contract's whole balance of `tokenIn`.

### Security model

- Only `executor` (hot wallet) starts an arb; only `owner` changes settings, unpauses or moves
  funds; two-step ownership; the executor or the owner may `pause` (an operator stop with the hot
  key at hand; the bot itself never calls it, its kill switch is a file).
- **Transient funding tag:** each entry sets which source may call back; the callback checks both
  the sender (the funder) and the tag, then clears it. A Morpho callback during a V4-funded arb (or
  vice versa), a second callback, or a callback outside our own arb reverts `Unauthorized`.
- An entry whose funder returned without calling back reverts `NotFunded`.
- **Two allowlists:** targets (kind 0, which get an allowance and arbitrary calldata) in
  `allowedTarget`, pool factories (kinds 1-3, only asked whether a pool is theirs) in
  `allowedFactory` (`setAllowedFactory`). A factory is never callable as a router, and a router
  cannot vouch for pools.
- Allowances are exact and reset to zero; the PoolManager is paid exactly what was taken.
- The tx reverts unless the loan-token balance left after repayment is at least `minProfit`;
  profit and the swept tokens go to the immutable `PROFIT_RECEIVER`; nothing is held at rest.

### Tests

- `contracts/test/AeroMigrationArb.funding.t.sol` (no RPC): one malicious contract plays both
  Morpho and the PoolManager, so only the funding tag protects the arb — cross-source callback,
  second callback and silent funder are rejected; a single honest callback succeeds (control).
- `contracts/test/AeroMigrationArb.fork.t.sol` (Base fork): real migration and legacy pools; both
  funding paths. The V4 path repays the PoolManager exactly and yields the same profit as the
  Morpho path; on the test route it used 682k gas vs 703k for Morpho. Plus min profit, allowlist,
  fake pools, CL callback guard, executor / expiry / pause, disabled V4, factories and routers as
  separate lists (`test_factoriesAndRoutersAreSeparateLists`). Pinned to a fixed block.
- `contracts/test/StepArb.security.t.sol` (no RPC): both funding sources run the steps, repay and
  pay the receiver; the sweep list; minProfit; executor / expiry / pause; targets allowlisted and
  factories not targets; bad kinds and patches; callbacks only from their funder inside our own
  arb, once; silent funder; disabled V4.
- `contracts/test/StepArb.fork.t.sol` (Base fork): the Aero migration arb written as steps earns
  exactly what AeroMigrationArb earns on the same route, with both funding sources; the Migration
  must be an allowlisted target.
- `contracts/test/GridQuoter.fork.t.sol`: the simulation-only quoter.
- `bot/src/aero.rs` `e2e_arb_executor_sends_a_profitable_arb_on_anvil_fork` and
  `e2e_the_same_arb_runs_on_the_generic_step_arb` (ignored; local anvil fork): deploy the contract
  (AeroMigrationArb / StepArb) and allowlist like an owner would, then take an arb through the real
  legacy pool and Migration (a fixed-rate stand-in router sells the new AERO) via `exact` / `pick` /
  `terms` / `send` and the executor (both funding sources exact-checked, the better one sent), and checks
  that the kill-switch gate stops the next send.

## The send pipeline: `ArbExecutor` (`bot/src/arb_exec.rs`)

What happens between "a strategy found an opportunity" and "a transaction is sent", for any atomic
arb contract that returns its profit. The strategy builds one `Candidate { calldata, gas_est,
floor_usd, l1_usd }` per way of taking the opportunity, in order of preference, and supplies two things: how
to call (`call`: a closure running a `TransactionRequest` on the state it priced, e.g. the sealed
head or a replayed in-progress block) and how to value the profit token in USD.

1. `exact(candidates, call)`: every candidate runs against the contract in parallel, priced like
   the real tx (base priority fee, gas limit, max fee = 2 x base fee + priority), because venues may
   read `tx.gasprice`; the return data is decoded by the contract-specific `decode`.
2. `pick(candidates, exact, usd)` -> `Picked { index, profit, gross_usd, net_usd }`: net = profit
   minus the candidate's modelled gas (`gas_cost_usd`) minus its L1 data fee (`l1_usd`, which the
   strategy fills in from `l1_fee_wei`, the GasPriceOracle's upper bound for the calldata size);
   candidates that clear their floor first, then the higher net; an exact tie keeps the earlier
   candidate.
3. `terms(candidate, picked, balance, usd, call)` -> `SendTerms { priority_fee_wei, gas_limit,
   min_profit, bid_usd }`: `BidPolicy::priority` may bid a share of a large profit (never more than
   net - floor, capped by `max_priority_wei`); a bid goes with a tighter gas limit (2x the model) and
   is re-checked at its own priority, falling back to the base bid if it no longer clears the floor
   there; the wallet must cover gas limit x max fee (None otherwise); `min_profit` = exact profit x
   `min_profit_keep`.
4. `request(candidate, terms, block, deadline)` -> the executor's `SendRequest` (minProfit is patched
   at the contract's `min_profit_slot`).
5. `send(executor, candidate, terms, gates, block, deadline)`: the gates every strategy shares, then
   the executor. `Gates { kill_switch_file, budget, daily_cap_usd, now_unix }`: a kill-switch file,
   the executor's failure cooldown, and a daily budget that the candidate's worst case if it reverts
   (gas at its priority + L1 fee) must fit in, reserved there while the tx is in flight (release it
   with `DailyBudget::release(inflight.key)` in `on_outcome`); any of them = `Ok(None)`, nothing
   sent.

Logs from this module read `arb: ...` with a `strategy` field. It re-exports what every strategy's
send path needs from `onchain_core::gates`: `deadline_blocks(deadline_secs, block_time)` (a
deadline in seconds as blocks at the observed block time, so a 2 s -> 200 ms change needs no config
edit), `DailyBudget` (gas burned by reverted txs per UTC day: `charge` after a revert, `allows`
before a send, `reserve` / `release` around a tx in flight), `gas_cost_usd` and `l1_fee_wei`.

### Writing a trade for `StepArb` (`bot/src/step_arb.rs`)

A strategy whose trade is a sequence of swaps and conversions needs no contract of its own: build
the `ArbStep`s (`call_step` for a kind-0 call; pool steps as in Aero's `steps`), and
`step_arb::calldata(funding, loan_token, amount, steps, sweep, deadline_block)` gives the
`Candidate`'s calldata; `step_arb::MIN_PROFIT` is the `min_profit_slot` and
`step_arb::decode_profit` the `decode`. Allowlist every kind-0 target (`setAllowedTarget`) and pool
factory (`setAllowedFactory`) the steps use.

## The example strategy: Aero (`bot/src/aero.rs`)

Old AERO converts 1:1 into new AERO through the permissionless `Migration.depositLiquid` (one way
only). Whenever old trades below new: borrow the loan token -> buy old AERO (split over legacy
Aerodrome V2 and Slipstream pools) -> migrate -> sell new AERO (split over Aero V2 and CL pools)
-> repay, in one tx.

- **Discovery** (every few dozen blocks): every configured factory, plus the Aero FactoryRegistry on
  the sell side, is asked for its old- or new-AERO / loan-token pools (CL: every tick spacing);
  pools without liquidity are skipped. Addresses live in `[aero]`, never in code.
- **Quoting** (per block, pinned to the head's block number so every read sees one state): one
  multicall for USD prices and V2 quotes; CL pools are quoted with
  `GridQuoter` injected by state override. Buy curves on a ladder of loan sizes, sell curves on a
  ladder of AERO amounts.
- **Planning:** for each loan size the input is split greedily over the buy pools (optimal for
  concave curves), the old AERO bought likewise over the sell pools; the best net size wins.
  The estimate subtracts the cheapest enabled source's flash fee and modelled gas.
- **Exact check and send** go through `ArbExecutor`: one `Candidate` per size (the best size and its
  neighbours, re-split each) x enabled funding source, Morpho first so it keeps exact ties, each
  with its own modelled gas (`arb_gas`) and the loan-size floor. `call` runs on the sealed head or,
  in same-block mode, on the replayed in-progress block. The L1 data fee (one read for the largest
  calldata, alongside the exact calls) is counted in every candidate's net. Only an exact result is
  sent, with `minProfit = exact x min_profit_keep`, and the deadline is `[aero] deadline_secs`
  converted with the block time measured from the heads.
- **Daily gas budget:** a send whose worst-case cost if it reverts (L2 gas at its priority + L1
  fee) would take today's spend on reverted arbs past `[exec] daily_gas_budget_usd` is skipped;
  `on_outcome` releases the reservation and charges each reverted arb's real cost.
- **USD prices:** a loan token that is not a USD stablecoin is priced in the same multicall, by a
  Chainlink feed (`usd_feed`, 8 decimals, `latestRoundData`) when set and fresh (updated within
  `[aero] usd_feed_max_age_secs`, default 3600), else by a pool quote (`usd_pool`, `getAmountOut`);
  a feed is preferred because a pool can drain or be pushed. The example prices WETH with the Base
  ETH/USD feed. The first priced token also gives `eth_usd`.
- **Contract** (`[aero] contract_kind`): `"migration"` (default) = AeroMigrationArb, the migration
  built in; `"steps"` = StepArb, the migration written as a kind-0 step between buy and sell
  (`depositLiquid(all old AERO, contract)`) and old / new AERO swept; the pre-flight then checks
  that the Migration is an allowlisted target instead of reading `MIGRATION()`.
- **Funding** (`[aero] funding`): `"morpho"` (default) calls `arb`; `"v4"` calls
  `arbFlashAccounting`; `"auto"` exact-checks every size through both entry points in parallel
  (twice the eth_calls) and sends the higher net after each source's own modelled gas. Whenever
  `[aero] contract` is set (observe mode too), the bot checks that the contract's `MORPHO()` /
  `POOL_MANAGER()` match the config and refuses to start otherwise. Live pre-flight also refuses a
  contract whose `PROFIT_RECEIVER` is the hot wallet.
- **Live mode** needs both `[exec] mode = "live"` in the config and `--live` on the command line
  (neither a stray flag nor a copied config sends alone), plus `exec.keystore` and `[aero] contract`.
- **Same-block mode** (`[aero] same_block`, Base Flashblocks): the same pass runs on the
  in-progress block whenever its flashblocks carry txs that may move our pools; those txs are
  replayed on the sealed head ahead of the quotes and the exact check, so a gap opened early in a
  block can be taken in that block. A learner keeps track of which tx targets actually touch our
  pools, so unrelated traffic does not trigger passes.
- **Priority fee:** `bid_policy` turns `[aero]` (`priority_fee_wei`, `bid_min_profit_usd`,
  `bid_share`, `bid_max_priority_wei`) into the `BidPolicy` that `ArbExecutor::terms` applies: a base
  priority fee, optionally a bid from a share of a large exact profit, re-checked at its own
  priority (Aero pools tax priority fees) and capped by the wallet balance.
- **Journal:** JSON lines in `[aero] journal_path` (`aero_start`, `aero_venue`, `aero_tick`,
  `aero_opp`, `aero_sent`, `aero_outcome`, `aero_cooldown`, `aero_learned`, `aero_same_block`);
  without `contract` or without `--live` it only observes.

## Plumbing

| Module | Responsibility |
|---|---|
| `config` | arb-only config; `[[rpc]]` / `env_file` and `[exec]` are onchain-core's `RpcSettings` / `SendSettings` (URLs only from `.env`; a relative `kill_switch_file` or `env_file` is anchored at the config file's directory) |
| `chain`, `num` (onchain-core) | Multicall3 reads, unit helpers |
| `rpc` (onchain-core) | multi-endpoint pool: race / failover, per-endpoint method and log-range limits, lag handling; capacity errors matched by phrases (rate / plan / range limits), deterministic errors (revert, gas limit, insufficient funds, nonce...) never fail over; logs show endpoint names only |
| `heads`, `flashblocks` (onchain-core) | new-head signal (WS `newHeads`, several endpoints); Base Flashblocks raw stream |
| `executor`, `actor` (onchain-core) | keystore signing, broadcast to every endpoint (+ optional `extra_broadcast`) over warm connections, versioned per-wallet shared nonce file (`~/.local/state/onchain-bots/nonce-<wallet>`; fails closed: no lock or an unknown format = nothing sent), stuck-tx replacement, failure cooldown (`[exec] max_failures_in_window` within `failure_window_secs` pauses sending for `failure_cooldown_secs`); the executor as its own task. One arb in flight at a time here (`max_inflight` 1) |
| `gates` (onchain-core) | kill switch, daily revert budget, deadlines, gas and L1-fee costs |
| `runner` | the driver above |
| `arb_exec` | `ArbExecutor`: exact check, pick, bid, send terms, gates (above) |
| `step_arb`, `contracts` | StepArb calldata; the contracts' ABIs |
| `flash` | funding sources |
| `aero` | the example strategy |

## Adding a strategy

1. Create `bot/src/<strategy>.rs` with a struct holding the strategy state and a constructor that
   validates its config section and does any one-time reads.
2. Implement `OpportunitySource`: `on_block` for each sealed head, `on_flashblocks` if you have a
   same-block mode (otherwise ignore the batch), `on_outcome` to account for your sent tx.
3. Return `Err` only for fatal errors. Recoverable ones (an RPC hiccup, a failed quote) are logged.
4. Call `runner::run(&mut strategy, provider, executor, heads, flashblocks, opts)` from `main.rs`.
5. The runner test in `runner.rs` drives the driver with a mock strategy; copy it for yours.
6. Send through `ArbExecutor` (see Aero's `tick`): build `Candidate`s, give it your `call` and a USD
   valuation, then `exact` -> `pick` -> `terms` -> `send` (with the `Gates`) on the `ExecHandle`.
   The contract must return its profit (set `decode`) and take `minProfit` in its calldata
   (`min_profit_slot`), so the exact check is real and the contract enforces the floor: StepArb
   does, so the simplest path is to write the trade as steps (see above).

## Adding a funding source

1. Implement `FlashLoan` (callback with a fee) or `FlashAccounting` (take and settle) in `flash.rs`.
2. In `FlashArbBase.sol`: a funding tag constant, a branch in `_flash` that starts the flash, and a
   callback that checks the sender and the tag, clears the tag, runs `_body` and repays; then an
   entry point with the **same arguments** as the existing ones in each contract. Keep `_finish`'s
   `NotFunded` and `minProfit` checks.
3. Extend the strategy's funder list (`Funder` in `aero.rs`): its name, holder, fee, gas difference
   and calldata encoding.
4. Tests: add the source to the malicious-funder suites (`AeroMigrationArb.funding.t.sol`,
   `StepArb.security.t.sol`), and a fork test proving it repays exactly and matches the other
   sources' profit.

## Commands

```bash
cd contracts && forge build
forge test --no-match-contract Fork -vv          # funding guard tests, no RPC
BASE_RPC_URL=<url> forge test -j 1 -vv           # fork tests (public endpoint by default)
cd ../bot && cargo test
./target/release/arb-bot -c config.local.toml            # observe only
./target/release/arb-bot -c config.local.toml --live     # live: operator-started only
```
