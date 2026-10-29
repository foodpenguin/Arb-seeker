# Arb-seeker

[繁體中文](README.zh-TW.md)

A framework for building atomic on-chain **arbitrage bots** in Rust ([alloy](https://github.com/alloy-rs/alloy))
and Solidity ([Foundry](https://getfoundry.sh)). The block / flashblock loop, RPC pool, transaction
sending and the choice of transient capital are generic; a strategy plugs in through a trait, and
any trade it can write as steps runs on one generic contract (`StepArb.sol`).
**Aero merger arbitrage on Base** (old AERO converts 1:1 into new AERO: buy old, migrate, sell new,
repay, in one transaction, with no capital of your own) is the example strategy. The chain plumbing
comes from [bot-onchain-core](https://github.com/foodpenguin/bot-onchain-core) (crate `onchain-core`),
shared with [Debt-keeper](https://github.com/foodpenguin/Debt-keeper). This crate is `arbitrage-framework`.

> **Status:** experimental. Nothing here is financial advice and nothing is tuned for you: the
> example configuration uses neutral values. Run it in observe mode, test sends on an anvil fork,
> and expect to lose gas while you learn.

## What you get

- **Generic strategy driver** (`runner::run` + `OpportunitySource`): new heads over WebSocket with
  HTTP polling while WS is quiet, batches of Base flashblocks for same-block work, the executor as
  its own task (receipts polled in the background, settled txs handed to the strategy), gas
  accounting.
- **Generic send pipeline** (`arb_exec::ArbExecutor`): exact check of every candidate against the
  contract in parallel, pick (floor first, then net), optional priority-fee bid re-checked at its
  own priority, send terms the hot wallet can cover, and the gates every send passes (kill switch,
  failure cooldown, daily budget for reverted gas). A strategy supplies the candidates, how to call
  and how to value its profit token.
- **`StepArb.sol`**, a generic flash-funded arb contract: borrow, run a list of steps (allowlisted
  router / target calls with exact allowances, Solidly and CL pools verified against allowlisted
  factories), repay, keep at least minProfit, sweep the listed intermediate tokens. `step_arb.rs`
  encodes it. A new strategy needs no contract of its own if it can write its trade as steps.
- **Two funding models** (`flash.rs`), deliberately separate traits:
  - `FlashLoan` — borrow a fixed amount, repay amount + fee inside the callback (`MorphoBlue`, 0 fee);
  - `FlashAccounting` — take now, settle the net before `unlock` returns (`UniswapV4` PoolManager).
- **`FlashArbBase.sol`**, what every arb contract here shares: `arb()` (Morpho) and
  `arbFlashAccounting()` (V4) entry points with identical arguments; each callback only runs inside
  an arb funded by that very source, once; executor-only; two owner allowlists, targets
  (`allowedTarget`) and pool factories (`allowedFactory`, pools are verified against them);
  minimum-profit check. `AeroMigrationArb.sol` is the Aero body on top of it (the migration built
  in); the same arb also runs on `StepArb` (`[aero] contract_kind = "steps"`). `[aero] funding =
  "morpho" | "v4" | "auto"`.
- **Aero example**: pool discovery from factories and a registry, batched grid quoting by state
  override (`GridQuoter`), greedy split across V2 and CL pools, exact check of the real transaction
  before sending, optional same-block mode that replays the in-progress block's relevant txs.
- **Execution plumbing** (`onchain-core`): multi-endpoint RPC pool, a versioned nonce file shared by
  bots on one hot wallet, failure cooldown, warm broadcast connections, kill switch.

## Layout

```
bot/src/
  lib.rs         the library; main.rs is the Aero bot built on it
  runner.rs      OpportunitySource + the generic driver
  arb_exec.rs    ArbExecutor: exact check -> pick -> bid -> send terms -> gates
  step_arb.rs    the generic StepArb contract: calldata, minProfit slot, profit decoding
  contracts.rs   the arb contracts' ABIs
  flash.rs       FlashLoan / FlashAccounting (MorphoBlue, UniswapV4)
  aero.rs        Aero merger arbitrage (example strategy)
contracts/src/
  FlashArbBase.sol       funding, steps, security model, admin (shared)
  StepArb.sol            the generic arb: any trade as steps
  AeroMigrationArb.sol   the Aero arb (migration built in)
  sim/GridQuoter.sol     quoter injected by state override (simulation only)
scripts/
  deploy_aero_arb.sh     deploy (KIND=steps: StepArb) + allowlists (run by the operator)
  accept_ownership.sh    the owner wallet accepts a two-step ownership transfer
docs/            architecture and setup (English / 繁體中文)
```

## Building

`onchain-core` is a git dependency pinned to a tag (`bot/Cargo.toml`); cargo fetches it, nothing to
check out by hand. To build against a local checkout of bot-onchain-core (e.g. while changing both),
override it from `bot/`:

```
cargo test --config 'patch."https://github.com/foodpenguin/bot-onchain-core".onchain-core.path="../../bot-onchain-core"'
```

The override rewrites `bot/Cargo.lock`; do not commit that change.

## Quick start

See [docs/setup.md](docs/setup.md) for the full guide.

```
cp .env.example .env                                  # your RPC URLs (git-ignored)
cd contracts && forge build && cd ..
cd bot && cp config.example.toml config.local.toml    # adjust limits; leave [aero] contract unset
cargo build --release
./target/release/arb-bot -c config.local.toml         # observe only: nothing is sent
```

`--live` needs `[exec] mode = "live"` in the config as well, `exec.keystore` and a deployed
`[aero] contract`, and is always started by you. Reverted arbs are capped per UTC day by
`[exec] daily_gas_budget_usd`.

## Extending

- **Another strategy**: implement `runner::OpportunitySource` (`on_block`, `on_flashblocks`,
  `on_outcome`), then call `runner::run`. Return `Err` from a hook only for fatal errors. Write the
  trade as `ArbStep`s for `StepArb` (`step_arb::calldata`), and send through
  `arb_exec::ArbExecutor` (build `Candidate`s, supply `call` and a USD valuation), as Aero does.
- **Another capital source**: implement `FlashLoan` or `FlashAccounting`, and give the contract an
  entry point with the same arguments as the existing ones plus a callback bound to that source.

The walkthrough is in [docs/architecture.md](docs/architecture.md).

## Testing

```
cd bot && cargo check --all-targets && cargo test
cd bot && cargo test e2e -- --ignored                   # the send path on a local anvil fork, both contracts
cd contracts && forge test --no-match-contract Fork     # security suites, no RPC needed
cd contracts && forge test -j 1                         # + fork tests (public endpoint by default; BASE_RPC_URL overrides)
```

## Safety

- Private keys only in an encrypted keystore (`cast wallet import`), never in code, config, `.env`
  or logs. The hot wallet holds gas only; profit goes to the contract's immutable receiver.
- RPC URLs (they contain API keys) live in `.env`; logs show endpoint names only.
- Live mode and deployments are always started by the operator. Test anything that sends on an
  anvil fork with `extra_broadcast = []` and a kill-switch file.
- Several bots may sign with one hot wallet; they coordinate nonces through one lock file per
  wallet (`~/.local/state/onchain-bots/nonce-<wallet>` by default; leave `exec.nonce_file` empty).
  All of them must be built on the same `onchain-core` nonce-file version.

Found a vulnerability? See [SECURITY.md](SECURITY.md).

## Contributing and license

See [CONTRIBUTING.md](CONTRIBUTING.md). Licensed under the
[GNU Affero General Public License v3.0 or later](LICENSE).

This software is provided without any warranty. Using it on mainnet can lose funds.
