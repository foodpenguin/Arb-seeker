//! Generic strategy driver. A strategy implements `OpportunitySource` and gets the whole loop:
//!
//! - the new-block signal: WS newHeads (heads::spawn_ws_many); when it is quiet for `ws_quiet`,
//!   HTTP polling every `poll_ms` takes over until WS speaks again;
//! - the flashblocks stream (same-block mode), handed to the strategy in batches;
//! - the executor as its own task (`onchain_core::actor`): it polls receipts on every head the
//!   driver sees, and each settled tx reaches the strategy's `on_outcome` with the gas spent so
//!   far, without the block loop ever waiting for a receipt.
//!
//! Hooks return `Err` only for fatal errors (e.g. the journal cannot be written); a strategy logs
//! and swallows recoverable ones (a failed quote pass, a flaky endpoint) itself.

use std::{future::Future, time::Duration};

use alloy::{eips::BlockNumberOrTag, primitives::U256, providers::Provider};
use anyhow::Result;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::{
    actor::{self, ExecHandle, Settled},
    executor::{Executor, Inflight, Outcome},
    flashblocks::Flashblock,
    heads::{Head, http_head},
};

/// What a strategy does on each event of the driver loop.
pub trait OpportunitySource<P: Provider + Clone> {
    /// Short name, used as the log prefix ("aero: ...").
    fn name(&self) -> &'static str;

    /// A new sealed head (deduplicated, strictly increasing). `exec` is `None` in observe-only mode.
    fn on_block(&mut self, p: &P, head: &Head, exec: Option<&ExecHandle>) -> impl Future<Output = Result<()>> + Send;

    /// Flashblocks of the in-progress block: `first`, plus whatever is queued behind it in `rx`.
    /// `last_block` is the latest sealed head the driver has seen.
    fn on_flashblocks(&mut self, p: &P, first: Flashblock, rx: &mut mpsc::Receiver<Flashblock>, exec: Option<&ExecHandle>, last_block: u64) -> impl Future<Output = Result<()>> + Send;

    /// The tx in flight resolved (included, reverted, dropped or expired). `gas_spent_total_wei`
    /// counts every included tx so far; `cooldown_started` is true when this outcome put the
    /// executor into its failure cooldown.
    fn on_outcome(&mut self, block: u64, inflight: &Inflight, outcome: &Outcome, gas_spent_total_wei: U256, cooldown_started: bool) -> Result<()>;
}

#[derive(Debug, Clone, Copy)]
pub struct RunnerOpts {
    /// HTTP head polling interval while WS heads are quiet.
    pub poll_ms: u64,
    /// No WS head for this long switches to HTTP polling.
    pub ws_quiet: Duration,
}

/// Runs `strategy` until a hook returns a fatal error. `exec` (live mode) is started as its own
/// task here.
pub async fn run<P: Provider + Clone + Send + Sync + 'static, S: OpportunitySource<P>>(
    strategy: &mut S,
    p: P,
    exec: Option<Executor<P>>,
    mut heads: watch::Receiver<Option<Head>>,
    mut fb: Option<mpsc::Receiver<Flashblock>>,
    opts: RunnerOpts,
) -> Result<()> {
    let name = strategy.name();
    let (blocks_tx, blocks_rx) = watch::channel(0u64);
    let (exec, mut settled) = match exec {
        Some(e) => {
            let (h, s) = actor::spawn(e, blocks_rx);
            (Some(h), Some(s))
        }
        None => (None, None),
    };
    let mut last_block = 0u64;
    let mut gas_spent_wei = U256::ZERO;
    let (mut ws_quiet, mut heads_open) = (false, true);
    let mut quiet_at = tokio::time::Instant::now() + opts.ws_quiet;
    let mut next_poll = tokio::time::Instant::now();
    loop {
        // Heads first, then the timers (ready only when due), then flashblocks: a same-block pass
        // takes longer than the 200 ms between flashblocks, so one is nearly always waiting, and
        // ahead of the timers it would keep the quiet switch and the HTTP poll from ever firing.
        let head = tokio::select! {
            biased;
            r = heads.changed(), if heads_open => {
                if r.is_err() {
                    heads_open = false;
                    continue;
                }
                let Some(h) = *heads.borrow_and_update() else { continue };
                if ws_quiet {
                    info!("{}: ws heads back", name);
                    ws_quiet = false;
                }
                quiet_at = tokio::time::Instant::now() + opts.ws_quiet;
                h
            }
            _ = tokio::time::sleep_until(quiet_at), if !ws_quiet => {
                warn!("{}: ws heads quiet; polling over http", name);
                ws_quiet = true;
                continue;
            }
            _ = tokio::time::sleep_until(next_poll), if ws_quiet => {
                next_poll = tokio::time::Instant::now() + Duration::from_millis(opts.poll_ms);
                match http_head(&p, BlockNumberOrTag::Latest).await {
                    Ok(h) => h,
                    Err(e) => {
                        warn!(error = %e, "{}: head fetch failed", name);
                        continue;
                    }
                }
            }
            Some(s) = next_settled(&mut settled) => {
                let Settled { inflight, outcome, cooldown_started } = s;
                if let Outcome::Included { gas_used, effective_gas_price, l1_fee_wei, .. } = &outcome {
                    gas_spent_wei += U256::from(*gas_used) * U256::from(*effective_gas_price) + U256::from(*l1_fee_wei);
                }
                // Lost races / expiries are normal; a burst pauses sending for a while
                // (executor::FailurePolicy), then it resumes by itself.
                strategy.on_outcome(last_block, &inflight, &outcome, gas_spent_wei, cooldown_started)?;
                continue;
            }
            Some(first) = next_fb(&mut fb) => {
                let rx = fb.as_mut().expect("a flashblock came from it");
                strategy.on_flashblocks(&p, first, rx, exec.as_ref(), last_block).await?;
                continue;
            }
        };
        if head.number <= last_block {
            continue;
        }
        last_block = head.number;
        // The executor task polls its receipts on this head.
        blocks_tx.send_replace(head.number);
        strategy.on_block(&p, &head, exec.as_ref()).await?;
        // A long pass must not read as quiet WS heads.
        quiet_at = quiet_at.max(tokio::time::Instant::now() + opts.ws_quiet);
    }
}

async fn next_settled(rx: &mut Option<mpsc::UnboundedReceiver<Settled>>) -> Option<Settled> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn next_fb(rx: &mut Option<mpsc::Receiver<Flashblock>>) -> Option<Flashblock> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use alloy::providers::ProviderBuilder;

    use super::*;

    #[derive(Debug, PartialEq)]
    enum Ev {
        Block(u64),
        Flash { block: u64, index: u64, last_block: u64 },
    }

    /// Records what the driver hands it and signals each event; a fatal error on `stop_at` ends
    /// the run.
    struct Probe {
        seen: Vec<Ev>,
        stop_at: u64,
        signal: mpsc::UnboundedSender<()>,
    }

    impl<P: Provider + Clone> OpportunitySource<P> for Probe {
        fn name(&self) -> &'static str {
            "probe"
        }
        async fn on_block(&mut self, _p: &P, head: &Head, _exec: Option<&ExecHandle>) -> Result<()> {
            self.seen.push(Ev::Block(head.number));
            let _ = self.signal.send(());
            anyhow::ensure!(head.number != self.stop_at, "stop");
            Ok(())
        }
        async fn on_flashblocks(&mut self, _p: &P, first: Flashblock, _rx: &mut mpsc::Receiver<Flashblock>, _exec: Option<&ExecHandle>, last_block: u64) -> Result<()> {
            self.seen.push(Ev::Flash { block: first.block, index: first.index, last_block });
            let _ = self.signal.send(());
            Ok(())
        }
        fn on_outcome(&mut self, _block: u64, _inflight: &Inflight, _outcome: &Outcome, _gas: U256, _cooldown: bool) -> Result<()> {
            Ok(())
        }
    }

    fn head(n: u64) -> Option<Head> {
        Some(Head { number: n, timestamp: 0, base_fee: 0, seen_at: Instant::now(), source: 0 })
    }

    #[tokio::test]
    async fn driver_dedupes_heads_feeds_flashblocks_and_stops_on_fatal_error() {
        // Never contacted: WS "heads" never go quiet within the test, so no HTTP polling.
        let provider = ProviderBuilder::new().connect_http("http://127.0.0.1:1".parse().unwrap());
        let (htx, hrx) = watch::channel(None);
        let (ftx, frx) = mpsc::channel(8);
        let (sig_tx, mut sig) = mpsc::unbounded_channel();
        let probe = Probe { seen: vec![], stop_at: 7, signal: sig_tx };
        let opts = RunnerOpts { poll_ms: 50, ws_quiet: Duration::from_secs(60) };
        // Spawned: this also checks at compile time that the driver future is Send, i.e. a process
        // can run several strategies side by side.
        let driver = tokio::spawn(async move {
            let mut probe = probe;
            let r = run(&mut probe, provider, None, hrx, Some(frx), opts).await;
            (probe, r)
        });
        htx.send(head(5)).unwrap();
        sig.recv().await.unwrap(); // Block(5) handled
        // A repeat and an older head: both skipped. Heads are polled before flashblocks, so they
        // are consumed before the flashblock below, whatever the scheduling.
        htx.send(head(5)).unwrap();
        htx.send(head(3)).unwrap();
        ftx.send(Flashblock { block: 6, index: 1, txs: vec![], timestamp: None, base_fee: None, seen: Instant::now() }).await.unwrap();
        sig.recv().await.unwrap(); // Flash handled
        htx.send(head(7)).unwrap();
        let (probe, r) = tokio::time::timeout(Duration::from_secs(10), driver).await.expect("driver stops").unwrap();
        let err = r.unwrap_err();
        assert!(err.to_string().contains("stop"), "{err}");
        assert_eq!(probe.seen, vec![Ev::Block(5), Ev::Flash { block: 6, index: 1, last_block: 5 }, Ev::Block(7)]);
        drop((htx, ftx));
    }
}
