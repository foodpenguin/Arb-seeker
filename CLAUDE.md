# arbitrage-framework — notes for AI coding assistants and contributors

A framework for building atomic on-chain **arbitrage bots** in Rust (alloy) + Foundry. The core
(strategy driver, executor, RPC pool, flashblocks stream, funding sources) is strategy-agnostic;
Aero merger arbitrage on Base is the example strategy. Read `docs/architecture.md` before changing
core code.

## Layout
- `bot/` — Rust crate: the `arbitrage_framework` library (`lib.rs`) and the `arb-bot` binary.
  Seams: `runner.rs` (`OpportunitySource` + the generic driver), `flash.rs` (`FlashLoan`,
  `FlashAccounting`). Shared: `arb_exec.rs` (`ArbExecutor`: exact check, pick, bid, send terms,
  gates), `step_arb.rs` (the generic StepArb contract), `contracts.rs` (ABIs). Example strategy:
  `aero.rs`.
- `../onchain-core` — the plumbing (rpc, heads, Flashblocks stream, executor + its task, gates,
  simulation, shared config sections), a separate repository checked out next to this one and
  used by path. Changes there must keep this crate and `liquidation-framework` building and passing.
- `contracts/` — Foundry. `FlashArbBase.sol` (funding, steps, security model, admin),
  `StepArb.sol` (generic: any trade as steps), `AeroMigrationArb.sol` (the Aero body), and the
  simulation-only `sim/GridQuoter.sol`.
- `docs/` — architecture and setup, in English and Traditional Chinese (`*.zh-TW.md`).

## Commands
- `cd bot && cargo check --all-targets` must stay at **0 warnings**; `cargo test` must pass.
- `cargo test e2e -- --ignored` runs the send path on a local **anvil fork**, for both contracts.
- `cd contracts && forge build`; `forge test --no-match-contract Fork` runs the security suites
  without RPC; fork tests use a public endpoint by default (`BASE_RPC_URL` overrides; `-j 1` on
  free plans).

## Conventions
- Code comments in English. Knowledge-base docs in English **and** Traditional Chinese; keep the
  two versions in sync when you change one.
- Match the surrounding code: naming, comment density, idioms.
- A new strategy implements `OpportunitySource` and, if its trade can be written as steps, runs on
  `StepArb` (no new contract). A new capital source implements `FlashLoan` or `FlashAccounting`
  plus a `FlashArbBase` branch and an entry point with the same arguments as the others.
- Hooks return `Err` only for fatal errors; log and swallow recoverable ones.
- Addresses of venues that can migrate belong in config, not code.

## Safety rules
- Private keys only in an encrypted keystore (`cast wallet import`). Never in code, config, `.env`
  or logs. Never ask for or handle a keystore password; signing is started by the operator.
- RPC URLs contain API keys: they live in `.env` (git-ignored); logs and errors show endpoint
  names only.
- Automated tooling never sends mainnet transactions. `arb-bot --live` and
  `scripts/deploy_aero_arb.sh` are run by the operator. Test anything that sends on a local anvil
  fork, with `extra_broadcast = []` and a kill-switch file.
- The arb contract: only `executor` may start an arb; each funding callback only runs inside an arb
  funded by that very source, once; routers and pool factories must be owner-allowlisted, in two
  separate lists (`allowedTarget`, `allowedFactory`); a minimum profit is
  always enforced. Keep `contracts/test/AeroMigrationArb.funding.t.sol` and
  `contracts/test/StepArb.security.t.sol` green.
- The hot wallet holds gas only; profit goes to the contract's immutable receiver.
