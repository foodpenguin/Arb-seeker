//! Config for the arbitrage bot: RPC endpoints, the hot-wallet executor, the flashblocks stream and
//! the strategies' own sections (`[aero]`). Only what the arb code reads lives here, so a config for
//! a new chain needs no liquidation-era keys.

use alloy::primitives::Address;
use anyhow::{Context, Result};
pub use onchain_core::config::{ResolvedRpc, RpcEntry, RpcSettings, SendSettings as ExecConfig, anchored};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub chain_id: u64,
    /// `env_file`, `[[rpc]]` (or `rpc_http` / `rpc_ws`): see onchain_core::config::RpcSettings.
    #[serde(flatten)]
    pub rpc: RpcSettings,
    /// The hot wallet, keystore, broadcast, gates and failure policy (onchain_core SendSettings).
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
    /// Flash-loan tokens; profit is counted in them. A USD stablecoin, or a token with `usd_pool`
    /// (its USD price is read from that pool each pass).
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
    /// The arb tx is valid this long; converted to blocks with the observed block time (2 s
    /// blocks: 2; 200 ms blocks: 20), so a block-time change needs no config edit.
    #[serde(default = "default_aero_deadline")]
    pub deadline_secs: f64,
    /// A `usd_feed` answer older than this is not used (the pool, or the last price, is).
    #[serde(default = "default_usd_feed_age")]
    pub usd_feed_max_age_secs: u64,
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
    /// Chainlink USD feed (8 decimals, `latestAnswer`) that prices a non-USD loan token (e.g.
    /// ETH/USD for WETH). Preferred over `usd_pool`: a pool can drain or be pushed.
    #[serde(default)]
    pub usd_feed: Option<Address>,
    /// Solidly pool token/USDC (`getAmountOut`) that prices it when there is no feed. Neither set
    /// = 1 USD.
    #[serde(default)]
    pub usd_pool: Option<Address>,
}

impl AeroLoan {
    /// Priced in USD by a feed or a pool (not a USD stablecoin itself).
    pub fn is_priced(&self) -> bool {
        self.usd_feed.is_some() || self.usd_pool.is_some()
    }
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

fn default_usd_feed_age() -> u64 {
    3600
}
fn default_aero_deadline() -> f64 {
    4.0
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        let mut cfg: Self = toml::from_str(&text).with_context(|| format!("parsing {path}"))?;
        cfg.exec.anchor(path);
        cfg.rpc.anchor(path);
        cfg.exec.validate().with_context(|| format!("checking {path}"))?;
        Ok(cfg)
    }

    pub fn resolve_rpc(&self) -> Result<ResolvedRpc> {
        self.rpc.resolve()
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
    fn relative_kill_switch_paths_follow_the_config_file() {
        assert_eq!(anchored("/srv/bot/config.local.toml", "data/KILL"), "/srv/bot/data/KILL");
        assert_eq!(anchored("config.toml", "data/KILL"), "./data/KILL");
        assert_eq!(anchored("/srv/bot/config.toml", "/var/KILL"), "/var/KILL", "absolute stays");
        assert_eq!(anchored("/srv/bot/config.toml", ""), "", "unset stays unset");
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
