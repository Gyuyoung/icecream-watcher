//! `icecc-top-agent` — serves this host's resource metrics to `icecc-top`.
//!
//! Exists because the Icecream scheduler cannot provide them: `iceccd` reports
//! stats only when its composite load moves by ≥10 %, and even then carries no
//! CPU utilisation, no memory total, no temperature and no network counters
//! (`ARCHITECTURE.md` §2.3, §3).
//!
//! Read-only by construction: it samples `/proc` and `/sys` on a timer and
//! serves the last snapshot over one `GET` endpoint. There is no write path, no
//! shell, and no way to ask it for an arbitrary file.

mod parse;
mod sampler;
mod server;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use icecc_metrics::{DEFAULT_AGENT_PORT, METRICS_PATH};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::sampler::Sampler;

/// Time between the first two samples at startup, so the agent can serve a
/// valid snapshot straight away instead of returning 503 for a whole interval.
const WARMUP: Duration = Duration::from_millis(200);

#[derive(Parser, Debug)]
#[command(
    name = "icecc-top-agent",
    version,
    about = "Serves node resource metrics to icecc-top"
)]
struct Cli {
    /// Address to bind. The default accepts connections from the build network;
    /// restrict it, or firewall the port, if that network is not trusted.
    #[arg(
        long,
        value_name = "ADDR",
        default_value = "0.0.0.0",
        env = "ICECC_TOP_AGENT_BIND"
    )]
    bind: IpAddr,

    /// Port to listen on.
    #[arg(short, long, default_value_t = DEFAULT_AGENT_PORT, env = "ICECC_TOP_AGENT_PORT")]
    port: u16,

    /// Sampling interval. Rates are averaged over this window, so it also sets
    /// how responsive CPU and network figures look.
    #[arg(long, value_name = "MS", default_value_t = 1000)]
    interval_ms: u64,

    /// Print one snapshot as JSON and exit, without listening. For checking
    /// what a node would report.
    #[arg(long)]
    once: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ICECC_TOP_AGENT_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let interval = Duration::from_millis(cli.interval_ms.max(100));
    let mut sampler = Sampler::new();

    if cli.once {
        // Two samples, because rates need a delta.
        sampler.sample();
        tokio::time::sleep(WARMUP).await;
        match sampler.sample() {
            Some(snap) => {
                println!("{}", serde_json::to_string_pretty(&snap)?);
                return Ok(());
            }
            None => {
                return Err(std::io::Error::other("could not take a sample"));
            }
        }
    }

    // Bind before the first sample, so a port clash fails immediately rather
    // than after a warmup delay.
    let addr = SocketAddr::new(cli.bind, cli.port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| std::io::Error::new(e.kind(), format!("cannot bind {addr}: {e}")))?;

    let (tx, rx) = watch::channel(None);
    tokio::spawn(sample_loop(sampler, tx, interval));

    tracing::info!(
        "icecc-top-agent {} serving http://{addr}{METRICS_PATH} every {:?} (read-only)",
        env!("CARGO_PKG_VERSION"),
        interval
    );

    tokio::select! {
        result = server::serve(listener, rx) => result,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

/// Sample on a timer and publish the serialised body.
///
/// Serialising here rather than per request is what keeps the cost independent
/// of how many monitors are watching.
async fn sample_loop(
    mut sampler: Sampler,
    tx: watch::Sender<Option<Arc<String>>>,
    interval: Duration,
) {
    tracing::info!("sampling as {}", sampler.hostname());

    // Prime the deltas.
    sampler.sample();
    tokio::time::sleep(WARMUP).await;

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // /proc reads are blocking file I/O. At one sweep per second the cost
        // is negligible, but keeping it off the runtime thread means a slow
        // sysfs read can never delay a request.
        let sampled = tokio::task::spawn_blocking(move || {
            let snapshot = sampler.sample();
            (sampler, snapshot)
        })
        .await;

        let snapshot = match sampled {
            Ok((returned, snapshot)) => {
                sampler = returned;
                snapshot
            }
            Err(e) => {
                tracing::error!("sampling task failed: {e}");
                return;
            }
        };

        if let Some(snapshot) = snapshot {
            match serde_json::to_string(&snapshot) {
                Ok(json) => {
                    // Nobody listening means every monitor has gone away; the
                    // agent keeps sampling so the next one gets fresh data.
                    let _ = tx.send(Some(Arc::new(json)));
                }
                Err(e) => tracing::error!("cannot serialise snapshot: {e}"),
            }
        }

        ticker.tick().await;
    }
}
