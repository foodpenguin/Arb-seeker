//! Arbitrage bot — entry point. The framework is the `arbitrage_framework` library (see lib.rs);
//! this binary runs its example strategy, Aero merger arbitrage.

use alloy::providers::ProviderBuilder;
use anyhow::{Context, Result};
use arbitrage_framework::{aero, config::Config, executor, flashblocks, heads, rpc};
use clap::Parser;
use tracing::{info, warn};
#[derive(Parser)]
#[command(about = "Arbitrage bot (Aero merger arbitrage; observe-only unless --live)")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: String,
    /// Actually send transactions (keystore = exec.keystore). Without it, observe only: spreads and
    /// estimated profits are journaled, nothing is sent.
    #[arg(long)]
    live: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    // Live: load the key BEFORE the async runtime exists. The password may come from an env var that
    // is removed after reading, and mutating the environment is only sound while single-threaded.
    let live_signer = if cli.live {
        anyhow::ensure!(cfg.exec.is_live(), "--live needs exec.mode = \"live\" in the config too (it is {:?})", cfg.exec.mode);
        let ks = cfg.exec.keystore.clone().context("live mode needs exec.keystore")?;
        Some(executor::load_signer(&ks, &cfg.exec.keystore_password_env)?)
    } else {
        None
    };
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(cfg, live_signer))
}

async fn run(cfg: Config, live_signer: Option<alloy::signers::local::PrivateKeySigner>) -> Result<()> {
    let resolved = cfg.resolve_rpc()?;
    info!(
        http = ?resolved.http.iter().map(|e| format!("{}(p{})", e.name, e.priority)).collect::<Vec<_>>(),
        ws = ?resolved.ws.iter().map(|w| w.0.as_str()).collect::<Vec<_>>(),
        "rpc endpoints (urls hidden)"
    );
    // Raw txs go to every RPC endpoint plus the extra broadcast URLs (e.g. the sequencer). A local
    // fork (every endpoint on loopback) never broadcasts elsewhere, so test txs cannot reach mainnet.
    let local_only = resolved.http.iter().all(|e| is_loopback_url(&e.url));
    if local_only && !cfg.exec.extra_broadcast.is_empty() {
        warn!("all rpc endpoints are local: exec.extra_broadcast ignored");
    }
    let extra: &[String] = if local_only { &[] } else { &cfg.exec.extra_broadcast };
    let broadcast: Vec<(String, String)> = resolved
        .http
        .iter()
        .map(|e| (e.name.clone(), e.url.clone()))
        .chain(extra.iter().enumerate().map(|(i, u)| (format!("extra{i}"), u.clone())))
        .collect();
    // `[aero] rpc`: use only those endpoints, in that strict order, for the per-block quotes.
    // Unknown names are an error, not a silent fallback. Empty = use all resolved endpoints.
    let provider = {
        let specs = if cfg.aero.rpc.is_empty() {
            resolved.http.clone()
        } else {
            let mut v = vec![];
            for (i, n) in cfg.aero.rpc.iter().enumerate() {
                let e = resolved.http.iter().find(|e| &e.name == n).with_context(|| format!("[aero] rpc names unknown endpoint {n}"))?;
                v.push(rpc::EndpointSpec { name: e.name.clone(), url: e.url.clone(), priority: i as u32, skip_methods: e.skip_methods.clone(), max_log_blocks: e.max_log_blocks });
            }
            info!(rpc = ?v.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), "aero: own rpc subset");
            v
        };
        ProviderBuilder::new().connect_client(alloy::rpc::client::RpcClient::new(rpc::RpcPool::new(specs)?, false))
    };
    let heads = heads::spawn_ws_many(resolved.ws.clone());
    let exec = match live_signer {
        Some(signer) => {
            let b = executor::Broadcaster::new(executor::Broadcaster::client()?, broadcast);
            b.spawn_keepalive(executor::KEEPALIVE_EVERY);
            let nonce_file = cfg.exec.nonce_path(signer.address());
            info!(nonce_file = %nonce_file.display(), "shared hot-wallet nonce file");
            // One arb in flight at a time: each is exact-checked on a state without the previous one.
            let mut e = executor::Executor::new(provider.clone(), signer, cfg.chain_id, b, cfg.exec.failure_policy()).with_shared_nonce(nonce_file).with_max_inflight(1);
            e.sync_nonce().await?;
            // Nonce and balance read in the background: the send path pays no RPC call for them.
            e.spawn_nonce_watch(executor::NONCE_WATCH_EVERY);
            warn!(address = %e.address, "AERO LIVE MODE: profitable arbs WILL be sent");
            Some(e)
        }
        None => None,
    };
    let fb = cfg.aero.same_block.then(|| flashblocks::spawn_raw(cfg.flashblocks.ws.clone()));
    aero::run(&cfg, provider, exec, heads, fb).await
}

/// http://127.0.0.1:8545, http://localhost:..., ws://[::1]... (anvil forks).
fn is_loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |x| x.1);
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |x| x.1);
    let host = if host.starts_with('[') { host.split(']').next().unwrap_or("").trim_start_matches('[') } else { host.split(':').next().unwrap_or("") };
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}
