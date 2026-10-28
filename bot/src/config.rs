//! Config for the arbitrage bot: RPC endpoints, the hot-wallet executor, the flashblocks stream and
//! the strategies' own sections (`[aero]`). Only what the arb code reads lives here, so a config for
//! a new chain needs no liquidation-era keys.

use std::path::PathBuf;

use alloy::primitives::Address;
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub chain_id: u64,
    /// Single endpoint (legacy/simple setup). Ignored when `[[rpc]]` entries are present.
    #[serde(default)]
    pub rpc_http: Option<String>,
    /// WebSocket endpoint for `eth_subscribe newHeads` (legacy); `[[rpc]].ws` supersedes it.
    #[serde(default)]
    pub rpc_ws: Option<String>,
    /// .env file holding endpoint URLs with API keys (referenced by `http_env`).
    #[serde(default)]
    pub env_file: Option<String>,
    /// Load-balanced endpoints (see rpc.rs).
    #[serde(default)]
    pub rpc: Vec<RpcEntry>,
    pub exec: ExecConfig,
    #[serde(default)]
    pub router: RouterConfig,
    #[serde(default)]
    pub flashblocks: FlashblocksConfig,
    #[serde(default)]
    pub aero: AeroConfig,
}

/// `[aero]`: Aero merger arbitrage, the first strategy. Old AERO converts 1:1 into new AERO (permissionless
/// Migration.depositLiquid, from 2026-10-22 00:00 UTC); when old trades below new, the
/// AeroMigrationArb contract flash-borrows the loan token, buys old on a legacy Aerodrome pool,
/// migrates and sells new on an Aero pool. Addresses live here, not in code (Aero is brand new).
#[derive(Debug, Deserialize, Clone, Default)]
pub struct AeroConfig {
    /// Deployed AeroMigrationArb. Unset = observe only (spreads and estimated profits journaled).
    #[serde(default)]
    pub contract: Option<Address>,
    /// Where the contract's capital comes from (see flash.rs).
    #[serde(default)]
    pub funding: AeroFunding,
    /// Morpho Blue (the `arb()` flash loan); the contract's own MORPHO must match. Read it through
    /// `morpho()`: unset = Morpho on Base, the same whether the config came from TOML or Default.
    #[serde(default)]
    pub morpho: Option<Address>,
    /// Uniswap V4 PoolManager (`arbFlashAccounting()`); needed for funding = "v4" / "auto", and the
    /// contract must have been deployed with it.
    #[serde(default)]
    pub pool_manager: Option<Address>,
    #[serde(default)]
    pub migration: Address,
    #[serde(default)]
    pub old_token: Address,
    #[serde(default)]
    pub new_token: Address,
    /// Legacy Aerodrome V2 PoolFactory (buy side: old AERO volatile pools).
    #[serde(default)]
    pub legacy_factory: Address,
    /// Legacy Aerodrome Slipstream (CL) factories: more buy side, swapped straight on their pools
    /// (kind 3); each must be allowlisted in the contract.
    #[serde(default)]
    pub legacy_cl_factories: Vec<Address>,
    /// Aero pool factories, V2 and CL (sell side; CL ones answer tickSpacings()); each must be
    /// allowlisted in the contract.
    #[serde(default)]
    pub new_factories: Vec<Address>,
    /// Aero FactoryRegistry: its targetFactories() (re-read every few dozen blocks) join
    /// new_factories, so factories Aero adds after launch are picked up without a restart.
    #[serde(default)]
    pub factory_registry: Option<Address>,
    /// Flash-loan tokens; profit is counted in them, so only USD stablecoins.
    #[serde(default)]
    pub loan_tokens: Vec<AeroLoan>,
    #[serde(default = "default_aero_min_profit")]
    pub min_profit_usd: f64,
    /// Also net >= this share of the loan (bps): a thin margin does not survive the ~1-2 s of
    /// trades ahead of our tx in the next block (observed reverts lost ~0.1% to them).
    #[serde(default = "default_aero_min_bps")]
    pub min_profit_bps: f64,
    #[serde(default = "default_aero_max_loan")]
    pub max_loan_usd: f64,
    /// Kept low on purpose: Aero pools tax priority fees above a retail threshold (up to 10%).
    #[serde(default = "default_aero_priority")]
    pub priority_fee_wei: u128,
    #[serde(default = "default_aero_gas_limit")]
    pub gas_limit: u64,
    #[serde(default = "default_aero_poll_ms")]
    pub poll_ms: u64,
    /// On-chain minProfit = exact simulated profit x this.
    #[serde(default = "default_aero_keep")]
    pub min_profit_keep: f64,
    /// Reverts are normal here (someone took the spread first, deadline passed): no permanent halt.
    /// When max_failures_in_window failed / dropped txs fall within failure_window_secs, sending
    /// pauses for failure_cooldown_secs, then resumes by itself.
    #[serde(default = "default_aero_max_fail")]
    pub max_failures_in_window: u32,
    #[serde(default = "default_aero_fail_window")]
    pub failure_window_secs: u64,
    #[serde(default = "default_aero_fail_window")]
    pub failure_cooldown_secs: u64,
    /// Gas model for the profit check: base + per swap leg (measured 2026-10-22: 2 legs ~0.73-0.86M,
    /// 4 legs ~1.1M).
    #[serde(default = "default_aero_gas_base")]
    pub gas_base: u64,
    #[serde(default = "default_aero_gas_leg")]
    pub gas_per_leg: u64,
    /// [[rpc]] names this strategy uses, in strict preference order (empty = all endpoints).
    /// Keeps its per-block quotes off endpoints another bot sharing the same keys depends on.
    #[serde(default)]
    pub rpc: Vec<String>,
    /// Same-block mode: also quote (and send) on the in-progress block, from the flashblocks stream
    /// ([flashblocks] ws), replaying its txs that move our pools (aero.rs `At::Pending`). Until
    /// Denim; without the stream only the per-head path runs.
    #[serde(default)]
    pub same_block: bool,
    /// Same-block passes only for flashblocks up to this index: a send lands some flashblocks after
    /// its trigger (how many depends on your latency to the sequencer), so a later one can only
    /// reach the next block, where the per-head pass already is. Measure it from your host.
    #[serde(default = "default_aero_last_fb")]
    pub same_block_last_index: u64,
    /// Bidding for big arbs: when the exact profit is at least bid_min_profit_usd, up to bid_share
    /// of it goes into the priority fee (capped at bid_max_priority_wei, and never below the
    /// floor's net). Aero pools tax priority fees (MEV tax multiplier): check it before raising the
    /// cap. A bid is paid on a revert too.
    #[serde(default = "default_aero_bid_min")]
    pub bid_min_profit_usd: f64,
    #[serde(default = "default_aero_bid_share")]
    pub bid_share: f64,
    #[serde(default = "default_aero_bid_max")]
    pub bid_max_priority_wei: u128,
    /// Journal of this strategy (kind "aero_*").
    #[serde(default = "default_aero_journal")]
    pub journal_path: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AeroLoan {
    pub token: Address,
    pub decimals: u8,
    /// Legacy Solidly pool token/USDC that prices a non-USD loan token (e.g. WETH); unset = 1 USD.
    #[serde(default)]
    pub usd_pool: Option<Address>,
}

/// `[aero] funding`.
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AeroFunding {
    /// Morpho Blue flash loan only (`arb()`), the deepest source on Base.
    #[default]
    Morpho,
    /// Uniswap V4 flash accounting only (`arbFlashAccounting()`).
    V4,
    /// Exact-check both, send the higher net after each source's own gas (V4 is ~21k cheaper, so
    /// it wins when both work; Morpho takes what V4 cannot fund). Doubles the exact eth_calls.
    Auto,
}

impl AeroConfig {
    /// The Morpho Blue to borrow from (`[aero] morpho`, Base by default).
    pub fn morpho(&self) -> Address {
        self.morpho.unwrap_or(crate::flash::BASE_MORPHO)
    }
}

fn default_aero_min_profit() -> f64 {
    1.0
}
fn default_aero_min_bps() -> f64 {
    20.0
}
fn default_aero_last_fb() -> u64 {
    6
}
fn default_aero_bid_min() -> f64 {
    10.0
}
fn default_aero_bid_share() -> f64 {
    0.1
}
fn default_aero_bid_max() -> u128 {
    100_000_000
}
fn default_aero_max_loan() -> f64 {
    10_000.0
}
fn default_aero_priority() -> u128 {
    1_000_000
}
fn default_aero_gas_limit() -> u64 {
    1_500_000
}
fn default_aero_poll_ms() -> u64 {
    150
}
fn default_aero_keep() -> f64 {
    0.5
}
fn default_aero_max_fail() -> u32 {
    5
}
fn default_aero_fail_window() -> u64 {
    600
}
fn default_aero_gas_base() -> u64 {
    450_000
}
fn default_aero_gas_leg() -> u64 {
    160_000
}
fn default_aero_journal() -> String {
    "data/aero_journal.jsonl".into()
}


/// Simulation helpers shared by strategies.
#[derive(Debug, Deserialize)]
pub struct RouterConfig {
    /// forge artifact of GridQuoter, injected by state override for grid quotes (`forge build`).
    #[serde(default = "default_quoter_artifact")]
    pub quoter_artifact: String,
}
impl Default for RouterConfig {
    fn default() -> Self {
        Self { quoter_artifact: default_quoter_artifact() }
    }
}
fn default_quoter_artifact() -> String {
    "../contracts/out/GridQuoter.sol/GridQuoter.json".into()
}

#[derive(Debug, Deserialize, Clone)]
pub struct FlashblocksConfig {
    /// Flashblocks stream (brotli-compressed JSON over WebSocket), for same-block strategies.
    #[serde(default = "default_fb_ws")]
    pub ws: String,
}
impl Default for FlashblocksConfig {
    fn default() -> Self {
        Self { ws: default_fb_ws() }
    }
}
fn default_fb_ws() -> String {
    "wss://mainnet.flashblocks.base.org/ws".into()
}

#[derive(Debug, Deserialize, Clone)]
pub struct ExecConfig {
    /// Hot wallet address (only the address; the key never goes in config). Used as `from` for
    /// simulations in observe-only mode.
    pub executor: Address,
    /// Encrypted keystore of the hot wallet (`cast wallet import`). Never a raw key.
    #[serde(default)]
    pub keystore: Option<String>,
    /// Env var holding the keystore password (removed right after reading); else a prompt.
    #[serde(default = "default_pw_env")]
    pub keystore_password_env: String,
    /// Extra endpoints the raw tx is broadcast to, besides every [[rpc]] endpoint. Opt-in (default
    /// empty): a config that forgets it must not send a fork's test txs to mainnet.
    #[serde(default)]
    pub extra_broadcast: Vec<String>,
    /// ETH/USD used to value gas and WETH-priced loans when no on-chain price is available.
    pub eth_usd_fallback: f64,
    /// If this file exists, nothing is sent.
    #[serde(default = "default_kill_file")]
    pub kill_switch_file: String,
    /// File every process signing for the hot wallet locks around sign + broadcast, recording the
    /// next nonce (executor::SharedNonce). Empty (default) = one file per wallet under the user's
    /// state dir (see `nonce_path`), so bots in different repos/directories still share it.
    #[serde(default)]
    pub nonce_file: String,
}

impl ExecConfig {
    /// Where the shared nonce file for `wallet` lives. Default: `$XDG_STATE_HOME` (or
    /// `~/.local/state`) `/onchain-bots/nonce-<wallet>`, the same for every bot using this wallet
    /// whatever its working directory. An explicit `nonce_file` is used as given; a relative one only
    /// coordinates with processes started from the same directory (warned at startup).
    pub fn nonce_path(&self, wallet: Address) -> PathBuf {
        if !self.nonce_file.is_empty() {
            let p = PathBuf::from(&self.nonce_file);
            if p.is_relative() {
                tracing::warn!(nonce_file = %self.nonce_file, "relative exec.nonce_file: only bots started from this directory share it; leave it empty to share per wallet");
            }
            return p;
        }
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))
            .unwrap_or_else(|| PathBuf::from("data"));
        state.join("onchain-bots").join(format!("nonce-{}", wallet.to_string().to_lowercase()))
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct RpcEntry {
    pub name: String,
    /// URL written in the config (public endpoints only).
    #[serde(default)]
    pub http: Option<String>,
    /// Name of the env var (in `env_file` or the process env) holding the URL with its key.
    #[serde(default)]
    pub http_env: Option<String>,
    /// Lower = preferred. Equal priorities are tie-broken by measured latency.
    #[serde(default)]
    pub priority: u32,
    /// newHeads source: "derive" (from the HTTP URL), a wss:// URL, or absent for none.
    #[serde(default)]
    pub ws: Option<String>,
    /// JSON-RPC methods never routed here (unsupported or too slow on this endpoint), e.g.
    /// ["eth_simulateV1"]. Methods answered with "method not found" are also learned at runtime.
    #[serde(default)]
    pub skip_methods: Vec<String>,
    /// Largest eth_getLogs block range this endpoint accepts (e.g. 10 on Alchemy's free plan);
    /// wider queries go to other endpoints instead of being refused here.
    #[serde(default)]
    pub max_log_blocks: Option<u64>,
}

fn default_pw_env() -> String {
    "BOT_KEYSTORE_PASSWORD".into()
}
fn default_kill_file() -> String {
    "data/KILL".into()
}

/// Resolved endpoints. URLs contain secrets: keep them out of logs.
pub struct ResolvedRpc {
    pub http: Vec<crate::rpc::EndpointSpec>,
    /// (endpoint name, wss url)
    pub ws: Vec<(String, String)>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        toml::from_str(&text).with_context(|| format!("parsing {path}"))
    }

    pub fn resolve_rpc(&self) -> Result<ResolvedRpc> {
        let env = match &self.env_file {
            Some(p) if std::path::Path::new(p).exists() => crate::rpc::load_env_file(p)?,
            Some(p) => {
                tracing::warn!(env_file = %p, "env file not found; endpoints that need it are skipped");
                Default::default()
            }
            None => Default::default(),
        };
        let mut out = ResolvedRpc { http: Vec::new(), ws: Vec::new() };
        if self.rpc.is_empty() {
            let url = self.rpc_http.clone().context("config needs [[rpc]] entries or rpc_http")?;
            out.http.push(crate::rpc::EndpointSpec { name: "default".into(), url, priority: 0, skip_methods: vec![], max_log_blocks: None });
            if let Some(ws) = &self.rpc_ws {
                out.ws.push(("default".into(), ws.clone()));
            }
            return Ok(out);
        }
        for e in &self.rpc {
            let url = match (&e.http, &e.http_env) {
                (Some(u), _) => u.clone(),
                (None, Some(var)) => match env.get(var).cloned().or_else(|| std::env::var(var).ok()) {
                    Some(u) => u,
                    None => {
                        // Missing key (e.g. .env not copied yet on a new machine): run on the
                        // remaining endpoints instead of refusing to start.
                        tracing::warn!(rpc = %e.name, var = %var, "env var not found; endpoint skipped");
                        continue;
                    }
                },
                (None, None) => anyhow::bail!("rpc {}: needs http or http_env", e.name),
            };
            match e.ws.as_deref() {
                Some("derive") => {
                    let ws = crate::rpc::ws_from_http(&url).with_context(|| format!("rpc {}: cannot derive ws url", e.name))?;
                    out.ws.push((e.name.clone(), ws));
                }
                Some(u) => out.ws.push((e.name.clone(), u.to_string())),
                None => {}
            }
            out.http.push(crate::rpc::EndpointSpec { name: e.name.clone(), url, priority: e.priority, skip_methods: e.skip_methods.clone(), max_log_blocks: e.max_log_blocks });
        }
        anyhow::ensure!(!out.http.is_empty(), "no usable RPC endpoint (check .env / [[rpc]])");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        chain_id = 8453
        [[rpc]]
        name = "public"
        http = "https://example.invalid"
        [exec]
        executor = "0x000000000000000000000000000000000000dEaD"
        eth_usd_fallback = 1.0
    "#;

    #[test]
    fn minimal_arb_config_loads_without_liquidation_keys() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert_eq!(cfg.chain_id, 8453);
        assert_eq!(cfg.router.quoter_artifact, default_quoter_artifact());
        assert_eq!(cfg.flashblocks.ws, default_fb_ws());
    }

    #[test]
    fn missing_env_skips_keyed_endpoints_but_keeps_public_ones() {
        let cfg: Config = toml::from_str(&format!(
            "env_file = \"definitely-missing.env\"\n{MINIMAL}\n[[rpc]]\nname = \"keyed\"\nhttp_env = \"BOT_TEST_NO_SUCH_VAR\"\n"
        ))
        .unwrap();
        let r = cfg.resolve_rpc().unwrap();
        assert_eq!(r.http.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["public"]);
    }

    #[test]
    fn shipped_example_config_parses() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.example.toml")).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(cfg.aero.contract.is_none(), "the example stays observe-only");
        assert!(!cfg.aero.new_factories.is_empty());
        assert!(cfg.exec.nonce_file.is_empty(), "example keeps the shared per-wallet nonce file");
        assert_eq!(cfg.aero.funding, AeroFunding::Morpho, "the example keeps today's funding");
        assert_eq!(cfg.aero.morpho(), crate::flash::BASE_MORPHO);
        assert!(cfg.aero.pool_manager.is_some());
    }

    #[test]
    fn funding_defaults_to_morpho_and_parses_v4_auto() {
        let parse = |extra: &str| toml::from_str::<Config>(&format!("{MINIMAL}\n[aero]\n{extra}")).unwrap().aero;
        let a = parse("");
        assert_eq!((a.funding, a.morpho(), a.pool_manager), (AeroFunding::Morpho, crate::flash::BASE_MORPHO, None));
        assert_eq!(AeroConfig::default().morpho(), a.morpho(), "Default and an empty [aero] agree");
        assert_eq!(parse("funding = \"v4\"").funding, AeroFunding::V4);
        assert_eq!(parse("funding = \"auto\"").funding, AeroFunding::Auto);
        assert!(toml::from_str::<Config>(&format!("{MINIMAL}\n[aero]\nfunding = \"balancer\"")).is_err(), "typos are rejected");
    }

    #[test]
    fn default_nonce_path_is_per_wallet_and_cwd_independent() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        let w: Address = "0x1111111111111111111111111111111111111111".parse().unwrap();
        let p = cfg.exec.nonce_path(w);
        assert!(p.is_absolute() || std::env::var_os("HOME").is_none(), "{p:?}");
        assert!(p.ends_with("onchain-bots/nonce-0x1111111111111111111111111111111111111111"), "{p:?}");
    }
}
