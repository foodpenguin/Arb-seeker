# arbitrage-framework

[繁體中文](README.zh-TW.md)

A framework for building atomic on-chain **arbitrage bots** in Rust ([alloy](https://github.com/alloy-rs/alloy))
and Solidity ([Foundry](https://getfoundry.sh)). The block / flashblock loop, RPC pool, transaction
sending and the choice of transient capital are generic; a strategy plugs in through a trait.
**Aero merger arbitrage on Base** (old AERO converts 1:1 into new AERO: buy old, migrate, sell new,
repay, in one transaction, with no capital of your own) is the example strategy.

> **Status:** experimental. Nothing here is financial advice and nothing is tuned for you: the
> example configuration uses neutral values. Run it in observe mode, test sends on an anvil fork,
> and expect to lose gas while you learn.

## What you get

- **Generic strategy driver** (`runner::run` + `OpportunitySource`): new heads over WebSocket with
  HTTP polling while WS is quiet, batches of Base flashblocks for same-block work, per-block receipt
  polling of the transaction in flight, gas accounting.
- **Generic send pipeline** (`arb_exec::ArbExecutor`): exact check of every candidate against the
  contract in parallel, pick (floor first, then net), optional priority-fee bid re-checked at its
  own priority, send terms the hot wallet can cover. A strategy supplies the candidates, how to call
  and how to value its profit token.
- **Two funding models** (`flash.rs`), deliberately separate traits:
  - `FlashLoan` — borrow a fixed amount, repay amount + fee inside the callback (`MorphoBlue`, 0 fee);
  - `FlashAccounting` — take now, settle the net before `unlock` returns (`UniswapV4` PoolManager).
- **`AeroMigrationArb.sol`**: the arb body does not know its funder; `arb()` (Morpho) and
  `arbFlashAccounting()` (V4) take identical arguments. Each callback only runs inside an arb funded
  by that very source, once; executor-only; two owner allowlists, routers (`allowedTarget`) and
  pool factories (`allowedFactory`, pools are verified against them); minimum-profit check. `[aero] funding = "morpho" | "v4" | "auto"`.
- **Aero example**: pool discovery from factories and a registry, batched grid quoting by state
  override (`GridQuoter`), greedy split across V2 and CL pools, exact check of the real transaction
  before sending, optional same-block mode that replays the in-progress block's relevant txs.
- **Execution plumbing**: multi-endpoint RPC pool, nonce coordination between bots that share a hot
  wallet, failure cooldown, warm broadcast connections, kill switch.

## Layout

```
bot/src/
  runner.rs      OpportunitySource + the generic driver
  arb_exec.rs    ArbExecutor: exact check -> pick -> bid -> send terms
  flash.rs       FlashLoan / FlashAccounting (MorphoBlue, UniswapV4)
  aero.rs        Aero merger arbitrage (example strategy)
  executor.rs    signing, broadcasting, nonce file, cooldown
  rpc.rs heads.rs flashblocks.rs chain.rs num.rs   chain plumbing
contracts/src/
  AeroMigrationArb.sol   arb contract (one entry point per funding source)
  sim/GridQuoter.sol     quoter injected by state override (simulation only)
scripts/
  deploy_aero_arb.sh     deploy + allowlist pool factories (run by the operator)
  accept_ownership.sh    the owner wallet accepts a two-step ownership transfer
docs/            architecture and setup (English / 繁體中文)
```

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
  `on_outcome`), then call `runner::run`. Return `Err` from a hook only for fatal errors. Send
  through `arb_exec::ArbExecutor` (build `Candidate`s, supply `call` and a USD valuation), as Aero does.
- **Another capital source**: implement `FlashLoan` or `FlashAccounting`, and give the contract an
  entry point with the same arguments as the existing ones plus a callback bound to that source.

The walkthrough is in [docs/architecture.md](docs/architecture.md).

## Testing

```
cd bot && cargo check --all-targets && cargo test
cd contracts && forge test --no-match-contract Fork     # funding guards, no RPC needed
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

Found a vulnerability? See [SECURITY.md](SECURITY.md).

## Contributing and license

See [CONTRIBUTING.md](CONTRIBUTING.md). Licensed under the
[GNU Affero General Public License v3.0 or later](LICENSE).

This software is provided without any warranty. Using it on mainnet can lose funds.
