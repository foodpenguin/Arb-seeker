# Contributing

[繁體中文](CONTRIBUTING.zh-TW.md)

Thanks for helping. This project moves real money when someone runs it live, so correctness and
safety come before features.

## Before you start

- Read [docs/architecture.md](docs/architecture.md): the strategy driver (`OpportunitySource`), the
  two funding models (`FlashLoan`, `FlashAccounting`) and the arb contract's security model.
- For a larger change (a new strategy, a new funding source, a contract change), open an issue first
  to agree on the approach.
- The chain plumbing (RPC pool, executor, gates, simulation) lives in `onchain-core`, a separate
  repository checked out next to this one; a change there is a pull request there.

## Development setup

See [docs/setup.md](docs/setup.md). In short: Rust (stable), Foundry, `git submodule update --init`,
optionally an RPC endpoint for fork tests (a public one is used by default).

## Checks every pull request must pass

```
cd bot && cargo check --all-targets     # 0 warnings
cd bot && cargo test
cd contracts && forge build && forge test -j 1   # fork tests: public endpoint by default, BASE_RPC_URL overrides
```

## Guidelines

- **Extend through the traits.** A new strategy implements `OpportunitySource`; a new capital source
  implements `FlashLoan` or `FlashAccounting` and gets a contract entry point with the same arguments
  as the existing ones.
- **Contracts**: a new funding callback must only run inside an arb funded by that very source, and
  only once. Add the cases to `AeroMigrationArb.funding.t.sol` (wrong source, second callback, silent
  funder) and fork tests for the real source. Keep the minimum-profit check and the target allowlist
  on every path.
- **Hooks return `Err` only for fatal errors**; log and swallow recoverable ones.
- **Config, not code**, for addresses of venues that can migrate.
- **Comments in English**, matching the surrounding style. Docs come in English and Traditional
  Chinese (`*.zh-TW.md`): update both, or say in the PR that a translation is pending.
- **No secrets and no private strategy data** in commits: no keys, no RPC URLs with API keys, no
  personal wallet addresses, no tuned parameters you do not want to publish.
- Never send mainnet transactions from tests or tooling.

## License

By contributing you agree that your contribution is licensed under the
[GNU AGPL v3.0 or later](LICENSE), like the rest of the project.
