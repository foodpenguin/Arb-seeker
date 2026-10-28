# Architecture

[繁體中文](architecture.zh-TW.md)

This framework builds atomic on-chain arbitrage bots: detect a price gap, plan a bundle of swaps,
fund it with transient capital (a flash loan or flash accounting), check it exactly against the
contract, send it. The reusable plumbing (block/flashblock driver, RPC pool, executor) is generic;
a strategy plugs in through `OpportunitySource`, and funding sources through `FlashLoan` /
`FlashAccounting`. **Aero merger arbitrage on Base** is the first, complete strategy.

## The driver: `runner::run` + `OpportunitySource` (`bot/src/runner.rs`)

```rust
pub trait OpportunitySource<P> {
    fn name(&self) -> &'static str;
    fn on_block(&mut self, p: &P, head: &Head, exec: Option<&mut Executor<P>>) -> impl Future<Output = Result<()>> + Send;
    fn on_flashblocks(&mut self, p: &P, first: Flashblock, rx: &mut mpsc::Receiver<Flashblock>,
                      exec: Option<&mut Executor<P>>, last_block: u64) -> impl Future<Output = Result<()>> + Send;
    fn on_outcome(&mut self, block: u64, inflight: &Inflight, outcome: &Outcome,
                  gas_spent_total_wei: U256, cooldown_started: bool) -> Result<()>;
}
```

`runner::run(&mut strategy, provider, executor, heads, flashblocks, RunnerOpts { poll_ms, ws_quiet })`
owns the scheduling:

- new heads from WebSocket `newHeads` (several endpoints), deduplicated and strictly increasing;
  HTTP polling every `poll_ms` while WS has been quiet for `ws_quiet`;
- per head: the executor's receipt poll first (outcome -> `on_outcome`, with gas accounting and
  the failure cooldown), then `on_block`;
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

## On-chain: `contracts/src/AeroMigrationArb.sol`

The arb body (`_migrateArb`: buy old AERO -> migrate -> sell new AERO) does not know where its
capital came from. Two entry points with **identical arguments** pick the funding, so the
`minProfit` calldata offset and the return type are the same for both:

- `arb(loanToken, loanAmount, buy, sell, minProfit, deadlineBlock)` — Morpho Blue flash loan:
  `flashLoan` -> `onMorphoFlashLoan` -> arb -> exact allowance for the repayment.
- `arbFlashAccounting(...)` — Uniswap V4: `unlock` -> `unlockCallback` -> `take` -> arb ->
  `sync` / transfer / `settle`, so the delta nets to zero before `unlock` returns.
  `POOL_MANAGER = address(0)` (constructor) disables it (`FundingDisabled`). While the contract
  holds the V4 lock, no swap step may go through V4 itself (a V4 router would `unlock` again).

Swap steps (`ArbStep`): kind 0 = allowlisted router with an exact allowance (amount optionally
patched into the calldata); kinds 1/2 = Solidly-style pool of an allowlisted factory (`isPool`),
output computed on-chain with `getAmountOut` / `getAmountOutWithTotalFee`; kind 3 = CL pool whose
factory is allowlisted and returns this exact pool from `getPool`, paid inside its own swap
callback (only the active pool, once, at most `amountIn`). `amountIn = type(uint256).max` spends
the contract's whole balance of `tokenIn`.

### Security model

- Only `executor` (hot wallet) starts an arb; only `owner` changes settings, unpauses or moves
  funds; two-step ownership; the executor may `pause`.
- **Transient funding tag:** each entry sets which source may call back; the callback checks both
  the sender (the funder) and the tag, then clears it. A Morpho callback during a V4-funded arb (or
  vice versa), a second callback, or a callback outside our own arb reverts `Unauthorized`.
- An entry whose funder returned without calling back reverts `NotFunded`.
- Allowances are exact and reset to zero; the PoolManager is paid exactly what was taken.
- The tx reverts unless the loan-token balance left after repayment is at least `minProfit`;
  profit and leftover AERO go to the immutable `PROFIT_RECEIVER`; nothing is held at rest.

### Tests

- `contracts/test/AeroMigrationArb.funding.t.sol` (no RPC): one malicious contract plays both
  Morpho and the PoolManager, so only the funding tag protects the arb — cross-source callback,
  second callback and silent funder are rejected; a single honest callback succeeds (control).
- `contracts/test/AeroMigrationArb.fork.t.sol` (Base fork): real migration and legacy pools; both
  funding paths. The V4 path repays the PoolManager exactly and yields the same profit as the
  Morpho path; on the test route it used 682k gas vs 703k for Morpho. Plus min profit, allowlist,
  fake pools, CL callback guard, executor / expiry / pause, disabled V4.
- `contracts/test/GridQuoter.fork.t.sol`: the simulation-only quoter.

## The example strategy: Aero (`bot/src/aero.rs`)

Old AERO converts 1:1 into new AERO through the permissionless `Migration.depositLiquid` (one way
only). Whenever old trades below new: borrow the loan token -> buy old AERO (split over legacy
Aerodrome V2 and Slipstream pools) -> migrate -> sell new AERO (split over Aero V2 and CL pools)
-> repay, in one tx.

- **Discovery** (every few dozen blocks): every configured factory, plus the Aero FactoryRegistry on
  the sell side, is asked for its old- or new-AERO / loan-token pools (CL: every tick spacing);
  pools without liquidity are skipped. Addresses live in `[aero]`, never in code.
- **Quoting** (per block): one multicall for USD prices and V2 quotes; CL pools are quoted with
  `GridQuoter` injected by state override. Buy curves on a ladder of loan sizes, sell curves on a
  ladder of AERO amounts.
- **Planning:** for each loan size the input is split greedily over the buy pools (optimal for
  concave curves), the old AERO bought likewise over the sell pools; the best net size wins.
  The estimate subtracts the cheapest enabled source's flash fee and modelled gas.
- **Exact check:** an `eth_call` of the contract itself at the best size and its neighbours, priced
  like the real tx (fees that read `tx.gasprice` are honoured). Only an exact result is sent, with
  `minProfit = exact x min_profit_keep`.
- **Funding** (`[aero] funding`): `"morpho"` (default) calls `arb`; `"v4"` calls
  `arbFlashAccounting`; `"auto"` exact-checks every size through both entry points in parallel
  (twice the eth_calls) and sends the higher net after each source's own modelled gas. Whenever
  `[aero] contract` is set (observe mode too), the bot checks that the contract's `MORPHO()` /
  `POOL_MANAGER()` match the config and refuses to start otherwise.
- **Same-block mode** (`[aero] same_block`, Base Flashblocks): the same pass runs on the
  in-progress block whenever its flashblocks carry txs that may move our pools; those txs are
  replayed on the sealed head ahead of the quotes and the exact check, so a gap opened early in a
  block can be taken in that block. A learner keeps track of which tx targets actually touch our
  pools, so unrelated traffic does not trigger passes.
- **Priority fee:** a base priority fee, optionally a bid from a share of a large exact profit,
  re-checked at its own priority (some pool fees read `tx.gasprice`) and capped by
  `bid_max_priority_wei` and the wallet balance.
- **Journal:** JSON lines in `[aero] journal_path` (`aero_start`, `aero_venue`, `aero_tick`,
  `aero_opp`, `aero_sent`, `aero_outcome`, `aero_cooldown`, `aero_learned`, `aero_same_block`);
  without `contract` or without `--live` it only observes.

## Plumbing

| Module | Responsibility |
|---|---|
| `config` | arb-only config + RPC resolution (URLs only from `.env`) |
| `chain` | chain-neutral reads (Multicall3) |
| `num` | unit helpers |
| `rpc` | multi-endpoint pool: race / failover, per-endpoint method and log-range limits, lag handling; logs show endpoint names only |
| `heads` | new-head signal (WS `newHeads`, several endpoints) |
| `flashblocks` | Base Flashblocks raw stream |
| `executor` | keystore signing, one tx in flight, broadcast to every endpoint (+ optional `extra_broadcast`) over warm connections, per-wallet shared nonce lock (`~/.local/state/onchain-bots/nonce-<wallet>`), failure cooldown (`max_failures_in_window` within `failure_window_secs` pauses sending for `failure_cooldown_secs`) |
| `runner` | the driver above |
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
6. Exact-check every opportunity against the contract before sending, and put `minProfit` in the
   calldata so the contract enforces it (`Payload.min_profit` tells the executor where).

## Adding a funding source

1. Implement `FlashLoan` (callback with a fee) or `FlashAccounting` (take and settle) in `flash.rs`.
2. Add an entry point to the contract with the **same arguments** as the existing ones, a funding
   tag constant, and a callback that checks the sender and the tag, clears the tag, runs
   `_migrateArb` (or your body) and repays. Keep `_finish`'s `NotFunded` and `minProfit` checks.
3. Extend the strategy's funder list (`Funder` in `aero.rs`): its name, holder, fee, gas difference
   and calldata encoding.
4. Tests: add the source to the malicious-funder suite (`AeroMigrationArb.funding.t.sol`), and a
   fork test proving it repays exactly and matches the other sources' profit.

## Commands

```bash
cd contracts && forge build
forge test --no-match-contract Fork -vv          # funding guard tests, no RPC
BASE_RPC_URL=<url> forge test -j 1 -vv           # fork tests (public endpoint by default)
cd ../bot && cargo test
./target/release/arb-bot -c config.local.toml            # observe only
./target/release/arb-bot -c config.local.toml --live     # live: operator-started only
```
