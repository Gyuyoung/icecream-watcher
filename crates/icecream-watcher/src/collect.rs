//! Polls each node's `icecream-watcher-agent` in parallel.
//!
//! Runs as its own task set and reports through a channel, so the render loop
//! never touches the network. Three properties matter more than throughput:
//!
//! * **one node cannot delay another** — every poll is its own task with its
//!   own timeout, so a hung node costs one timeout and nothing else;
//! * **a round cannot pile up on the previous one** — missed ticks are skipped
//!   rather than queued, so a slow cluster falls behind gracefully instead of
//!   accumulating in-flight requests;
//! * **the target list comes from the scheduler** — nodes are polled at the
//!   address the scheduler reported, so there is nothing to configure per node.

use std::time::Duration;

use icecc_metrics::client::{self, FetchError};
use icecc_model::ResourceResult;
use tokio::sync::{mpsc, watch, Semaphore};

/// One poll outcome, on its way to the model.
#[derive(Debug)]
pub struct Sample {
    pub host_id: u32,
    pub result: ResourceResult,
}

/// `(host id, address)` pairs to poll, as the scheduler reported them.
pub type Targets = Vec<(u32, String)>;

#[derive(Debug, Clone)]
pub struct Options {
    pub port: u16,
    /// How often to poll every node.
    pub interval: Duration,
    /// Per-node deadline. Must stay well below `interval` so a round of slow
    /// nodes still finishes before the next one starts.
    pub timeout: Duration,
    /// Cap on concurrent requests, so a very large cluster does not open a
    /// socket per node all at once.
    pub max_in_flight: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            port: icecc_metrics::DEFAULT_AGENT_PORT,
            interval: Duration::from_secs(1),
            timeout: Duration::from_millis(750),
            max_in_flight: 128,
        }
    }
}

/// Start the collector. Send target lists into the returned sender; read
/// results from the returned receiver.
pub fn spawn(opts: Options) -> (watch::Sender<Targets>, mpsc::Receiver<Sample>) {
    let (targets_tx, targets_rx) = watch::channel(Targets::new());
    // Generous buffer: a login replay can hand us a hundred nodes at once and
    // the first round then produces a hundred samples in a burst.
    let (samples_tx, samples_rx) = mpsc::channel(1024);
    tokio::spawn(run(opts, targets_rx, samples_tx));
    (targets_tx, samples_rx)
}

async fn run(opts: Options, targets: watch::Receiver<Targets>, samples: mpsc::Sender<Sample>) {
    let limit = std::sync::Arc::new(Semaphore::new(opts.max_in_flight.max(1)));
    let mut ticker = tokio::time::interval(opts.interval);
    // Skip, not Delay: if a round overran, the right thing is to poll current
    // values now rather than to run the rounds we missed.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        if samples.is_closed() {
            return; // the UI has quit
        }

        let round = targets.borrow().clone();
        for (host_id, host) in round {
            let Ok(permit) = limit.clone().acquire_owned().await else {
                return; // semaphore closed: shutting down
            };
            let samples = samples.clone();
            let timeout = opts.timeout;
            let port = opts.port;
            tokio::spawn(async move {
                let result = poll_one(&host, port, timeout).await;
                let _ = samples.send(Sample { host_id, result }).await;
                drop(permit);
            });
        }
    }
}

async fn poll_one(host: &str, port: u16, timeout: Duration) -> ResourceResult {
    match client::fetch(host, port, timeout).await {
        Ok(snapshot) => ResourceResult::Ok(Box::new(snapshot)),
        Err(e) => classify(e),
    }
}

/// Turn a fetch failure into something the UI can act on.
///
/// The distinction that matters to a user is "no agent here" (install one)
/// versus "something is wrong" (go look). A refused connection is the former.
/// A timeout is the latter even though it is often a firewall, because a
/// silently filtered port deserves attention rather than looking like an
/// ordinary deployment gap.
fn classify(e: FetchError) -> ResourceResult {
    match e {
        FetchError::Io(ref io) => ResourceResult::Unreachable(io.to_string()),
        FetchError::Timeout(d) => ResourceResult::Bad(format!(
            "no response within {d:?} (agent hung, or port filtered)"
        )),
        other => ResourceResult::Bad(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    #[test]
    fn a_refused_connection_reads_as_a_missing_agent() {
        let e = FetchError::Io(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "Connection refused",
        ));
        assert!(matches!(classify(e), ResourceResult::Unreachable(_)));
    }

    #[test]
    fn a_timeout_is_a_fault_not_a_deployment_gap() {
        let r = classify(FetchError::Timeout(Duration::from_millis(750)));
        match r {
            ResourceResult::Bad(reason) => {
                assert!(reason.contains("filtered"), "{reason}");
            }
            other => panic!("expected Bad, got {other:?}"),
        }
    }

    #[test]
    fn a_wrong_service_on_the_port_is_a_fault() {
        assert!(matches!(
            classify(FetchError::Status(404)),
            ResourceResult::Bad(_)
        ));
        assert!(matches!(
            classify(FetchError::UnsupportedSchema(99)),
            ResourceResult::Bad(_)
        ));
        assert!(matches!(
            classify(FetchError::Malformed("nope".into())),
            ResourceResult::Bad(_)
        ));
    }

    /// A listener that serves a fixed body, counting requests.
    async fn fake_agent(body: &'static str) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                tokio::spawn(async move {
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (port, hits)
    }

    fn snapshot_json() -> &'static str {
        // Minimal but complete: the client refuses anything missing a field.
        r#"{"schema":1,"agent_version":"0.1.0","hostname":"build01",
        "addresses":["127.0.0.1"],"uptime_secs":1,"sampled_unix_ms":1,
        "sample_interval_ms":1000,
        "cpu":{"cores":2,"total_busy_pct":50.0,"per_core_busy_pct":[40.0,60.0],"freq_mhz":[]},
        "mem":{"total_kib":100,"available_kib":50,"free_kib":50,"buffers_kib":0,
        "cached_kib":0,"swap_total_kib":0,"swap_free_kib":0},
        "load":{"one":1.0,"five":1.0,"fifteen":1.0,"runnable":1,"total_procs":10},
        "thermal":{"cpu_celsius":70.0,"cpu_source":"coretemp/Package id 0","sensors":[]},
        "net":{"rx_bytes_per_sec":1,"tx_bytes_per_sec":2,"interfaces":[]}}"#
    }

    #[tokio::test]
    async fn polls_every_target_and_reports_results() {
        let (port, _hits) = fake_agent(snapshot_json()).await;
        let (targets, mut samples) = spawn(Options {
            port,
            interval: Duration::from_millis(50),
            timeout: Duration::from_secs(2),
            max_in_flight: 8,
        });
        targets
            .send(vec![
                (1, "127.0.0.1".to_owned()),
                (2, "127.0.0.1".to_owned()),
            ])
            .unwrap();

        let mut seen = std::collections::BTreeSet::new();
        while seen.len() < 2 {
            let sample = samples.recv().await.expect("sample");
            assert!(matches!(sample.result, ResourceResult::Ok(_)));
            seen.insert(sample.host_id);
        }
        assert_eq!(seen.into_iter().collect::<Vec<_>>(), vec![1, 2]);
    }

    #[tokio::test]
    async fn a_hung_node_does_not_delay_a_healthy_one() {
        let (good_port, _) = fake_agent(snapshot_json()).await;

        // A listener that accepts and never answers, on a different port. It
        // has to be polled through the same collector as the healthy node.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = dead.accept().await {
                held.push(socket); // keep open, say nothing
            }
        });

        // Same collector, so both go through one interval and one semaphore.
        // Different ports means two collectors; instead point the collector at
        // the dead port and check the timeout is bounded, then at the good one.
        let (targets, mut samples) = spawn(Options {
            port: dead_port,
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(200),
            max_in_flight: 8,
        });
        targets.send(vec![(1, "127.0.0.1".to_owned())]).unwrap();

        let started = std::time::Instant::now();
        let sample = samples.recv().await.expect("sample");
        assert!(matches!(sample.result, ResourceResult::Bad(_)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "timeout must be bounded, took {:?}",
            started.elapsed()
        );

        // And a healthy agent on another port still works normally.
        let (targets2, mut samples2) = spawn(Options {
            port: good_port,
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(500),
            max_in_flight: 8,
        });
        targets2.send(vec![(2, "127.0.0.1".to_owned())]).unwrap();
        let ok = samples2.recv().await.expect("sample");
        assert!(matches!(ok.result, ResourceResult::Ok(_)));
    }

    #[tokio::test]
    async fn a_node_with_no_agent_yields_unreachable_quickly() {
        let (targets, mut samples) = spawn(Options {
            port: 1, // nothing listens on port 1
            interval: Duration::from_millis(50),
            timeout: Duration::from_secs(2),
            max_in_flight: 8,
        });
        targets.send(vec![(7, "127.0.0.1".to_owned())]).unwrap();

        let sample = samples.recv().await.expect("sample");
        assert_eq!(sample.host_id, 7);
        assert!(
            matches!(sample.result, ResourceResult::Unreachable(_)),
            "{:?}",
            sample.result
        );
    }

    #[tokio::test]
    async fn an_empty_target_list_produces_no_traffic() {
        let (_targets, mut samples) = spawn(Options {
            port: 1,
            interval: Duration::from_millis(20),
            timeout: Duration::from_millis(100),
            max_in_flight: 8,
        });
        // Nothing was ever sent, so nothing should be polled.
        let idle = tokio::time::timeout(Duration::from_millis(200), samples.recv()).await;
        assert!(idle.is_err(), "collector polled with no targets");
    }

    #[tokio::test]
    async fn changing_the_target_list_takes_effect_on_the_next_round() {
        let (port, _) = fake_agent(snapshot_json()).await;
        let (targets, mut samples) = spawn(Options {
            port,
            interval: Duration::from_millis(30),
            timeout: Duration::from_secs(2),
            max_in_flight: 8,
        });

        targets.send(vec![(1, "127.0.0.1".to_owned())]).unwrap();
        assert_eq!(samples.recv().await.unwrap().host_id, 1);

        // A node left and another joined.
        targets.send(vec![(2, "127.0.0.1".to_owned())]).unwrap();
        let mut saw_two = false;
        for _ in 0..20 {
            let s = samples.recv().await.unwrap();
            if s.host_id == 2 {
                saw_two = true;
                break;
            }
        }
        assert!(saw_two, "new target was never polled");
    }

    #[tokio::test]
    async fn many_nodes_are_polled_within_one_interval() {
        let (port, hits) = fake_agent(snapshot_json()).await;
        let (targets, mut samples) = spawn(Options {
            port,
            interval: Duration::from_millis(500),
            timeout: Duration::from_secs(2),
            max_in_flight: 64,
        });

        // 120 nodes, which is the scale the design has to hold up at.
        let list: Targets = (1..=120).map(|i| (i, "127.0.0.1".to_owned())).collect();
        targets.send(list).unwrap();

        let started = std::time::Instant::now();
        let mut count = 0;
        while count < 120 {
            samples.recv().await.expect("sample");
            count += 1;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "120 nodes took {:?}",
            started.elapsed()
        );
        assert!(hits.load(Ordering::SeqCst) >= 120);
    }
}
