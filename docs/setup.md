# Setup

[繁體中文](setup.zh-TW.md)

Works on Linux and on WSL2 (Ubuntu 22.04 / 24.04). Nothing in this guide sends a mainnet
transaction; live mode is a separate, explicit step at the end that only you start.

## 1. Tools

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config git curl ca-certificates chrony
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y   # Rust (edition 2024: stable >= 1.85)
curl -L https://foundry.paradigm.xyz | bash && ~/.foundry/bin/foundryup  # forge, cast, anvil
```

## 2. Code

```bash
git clone --recurse-submodules <repo-url> arbitrage-framework
cd arbitrage-framework
git submodule update --init --recursive    # if you cloned without --recurse-submodules (forge-std)
cd contracts && forge build && cd ..        # the bot injects the built GridQuoter for its quotes
```

## 3. RPC endpoints (`.env`)

```bash
cp .env.example .env    # then put your real URLs in it (the file is gitignored)
```

RPC URLs carry API keys, so they live only in `.env`. Each `[[rpc]]` entry in the config names its
variable with `http_env` (public endpoints may use `http` directly); logs and errors show only the
endpoint `name`. `[aero] rpc` can pin the strategy to its own endpoints, in strict order, so it
does not compete for quota with another bot on the same keys. Same-block mode needs an endpoint
that supports `eth_simulateV1`.

## 4. Config

```bash
cd bot
cp config.example.toml config.local.toml   # gitignored
```

Review `[[rpc]]`, `[exec]` (hot wallet address only, never a key), `[flashblocks]` and `[aero]`:
the Aero / legacy Aerodrome addresses (verify them on-chain; protocol migrations change them),
`loan_tokens`, profit floors, priority fees, `funding` and, after deployment, `contract`. The
example values are placeholders to make the bot start, not recommendations: size and price for
your own risk.

## 5. Build and observe

```bash
cargo build --release
./target/release/arb-bot -c config.local.toml     # observe only: venues, spreads, estimates, exact checks are journaled
```

Without `[aero] contract`, or without `--live`, nothing is ever sent. Read the journal
(`[aero] journal_path`) before going further.

## 6. Tests

```bash
cargo test                                      # unit tests, incl. the runner with a mock strategy
cd ../contracts
forge test --no-match-contract Fork -vv         # funding guard tests, no RPC needed
BASE_RPC_URL=<your url> forge test -j 1 -vv     # fork tests; -j 1 keeps free RPC plans under their rate limit
```

Fork tests read the `[aero]` addresses from `bot/config.example.toml` and default to a public
endpoint when `BASE_RPC_URL` is unset. Pass keyed URLs only through the environment.

To try the deploy script or `--live` end to end, use a local anvil fork, a throwaway keystore, a
kill-switch file and `extra_broadcast = []` in that test config, so nothing can reach mainnet.

## 7. Keys

```bash
cast wallet import bot-hot --interactive   # hot wallet: gas only
```

Keys live only in an encrypted keystore; never in code, config, `.env`, shell history or logs. The
bot reads the password from the env var named by `exec.keystore_password_env` (removed from the
process right after reading) or prompts for it. Profit goes to the contract's immutable
`PROFIT_RECEIVER` (a cold wallet), never to the hot wallet.

## 8. Going live (operator only)

1. Deploy: `./scripts/deploy_aero_arb.sh <HOT> <COLD>` (reads addresses from
   `bot/config.local.toml [aero]`, verifies them on-chain, deploys with `profitReceiver = COLD`,
   allowlists the pool factories). Re-run with `ARB=<address>` to allowlist factories added later.
   Hand ownership to COLD when you no longer need fast allowlisting:
   `transferOwnership(COLD)` from the owner, then `./scripts/accept_ownership.sh <ARB> <COLD>`.
   These scripts sign mainnet txs: run them yourself.
2. Put the address in `[aero] contract`; choose `funding` (a contract deployed without a V4
   PoolManager supports `"morpho"` only; the bot checks this at start).
3. Before the first live run, set and double-check: `[aero] priority_fee_wei` and
   `bid_max_priority_wei` (priority-fee caps), `max_loan_usd`, the profit floors, the failure
   cooldown (`max_failures_in_window` / `failure_window_secs` / `failure_cooldown_secs`) and
   `exec.kill_switch_file`. Keep the hot wallet balance small: it caps what failed txs can burn.
4. Start `./target/release/arb-bot -c config.local.toml --live` yourself. The pre-flight check
   refuses to start if the contract's executor, migration, tokens, allowlist or funding sources do
   not match the config, or if it is paused.
5. Emergency stop: create the kill-switch file, or call `pause()` on the contract from the owner or
   executor (only the owner can unpause).

If another bot signs with the same hot wallet, leave `exec.nonce_file` empty in both so they share
the per-wallet nonce lock (`~/.local/state/onchain-bots/nonce-<wallet>`).
