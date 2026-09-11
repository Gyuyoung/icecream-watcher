//! `icecream-watcher` — terminal monitor for an Icecream compile cluster.
//!
//! Phase 2: scheduler data only. See `ARCHITECTURE.md` for the roadmap and for
//! why per-node CPU/memory needs an agent rather than the scheduler.

mod app;
mod collect;
mod ui;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use icecc_proto::conn::{self, Options, Source};
use icecc_proto::discover;
use ratatui::crossterm::event::{self as term_event, Event as TermEvent, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::app::{classify, App, FRAME_INTERVAL, HISTORY_INTERVAL};
use crate::ui::Ui;

#[derive(Parser, Debug)]
#[command(
    name = "icecream-watcher",
    version,
    about = "Terminal monitor for Icecream (icecc) distributed compile clusters"
)]
struct Cli {
    /// Scheduler as `host[:port]`. Defaults to $ICECC_SCHEDULER, then
    /// $USE_SCHEDULER, then UDP broadcast discovery.
    #[arg(short, long, value_name = "HOST[:PORT]")]
    scheduler: Option<String>,

    /// Icecream network name for broadcast discovery. Ignored when a scheduler
    /// host is named explicitly, matching upstream behaviour.
    #[arg(short, long, value_name = "NAME", env = "ICECC_NETNAME")]
    netname: Option<String>,

    /// Seconds to wait for the TCP connect and version handshake. An idle
    /// scheduler can take ~36 s to answer (ARCHITECTURE.md §2.4), so do not
    /// lower this to "make it fail faster".
    #[arg(long, value_name = "SECS", default_value_t = 45)]
    handshake_timeout: u64,

    /// Seconds to wait for answers to a discovery broadcast.
    #[arg(long, value_name = "SECS", default_value_t = 2)]
    discover_timeout: u64,

    /// Record the raw protocol stream here for offline replay.
    #[arg(long, value_name = "FILE")]
    record: Option<PathBuf>,

    /// Replay a recorded stream instead of connecting.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["scheduler", "record"])]
    replay: Option<PathBuf>,

    /// Replay using the recorded inter-event delays rather than as fast as
    /// possible.
    #[arg(long, requires = "replay")]
    replay_realtime: bool,

    /// Port each node's `icecream-watcher-agent` listens on.
    #[arg(long, value_name = "PORT", default_value_t = icecc_metrics::DEFAULT_AGENT_PORT)]
    agent_port: u16,

    /// How often to poll node agents.
    #[arg(long, value_name = "MS", default_value_t = 1000)]
    poll_interval: u64,

    /// Per-node deadline for an agent poll. Kept below the interval so a round
    /// of slow nodes still finishes before the next one starts.
    #[arg(long, value_name = "MS", default_value_t = 750)]
    poll_timeout: u64,

    /// Treat agent metrics older than this as stale.
    #[arg(long, value_name = "MS", default_value_t = 5000)]
    stale_after: u64,

    /// Do not poll node agents at all; show scheduler data only.
    #[arg(long)]
    no_agents: bool,

    /// Print events as text instead of drawing a TUI. Useful for checking a
    /// cluster over ssh, or in CI.
    #[arg(long)]
    dump: bool,

    /// Write diagnostics here. Logging to stderr would corrupt the display, so
    /// there is no console logging in TUI mode.
    #[arg(long, value_name = "FILE")]
    log_file: Option<PathBuf>,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    init_logging(&cli)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        // The workload is one socket plus a render loop; a big pool would only
        // add idle wakeups to a tool that must not disturb builds.
        .worker_threads(2)
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let source = match &cli.replay {
            Some(path) => Source::Replay {
                path: path.clone(),
                realtime: cli.replay_realtime,
            },
            None => {
                let discovery = discover::resolve(cli.scheduler.as_deref(), cli.netname.as_deref());
                tracing::info!("discovery plan: {discovery:?}");
                Source::Live(discovery)
            }
        };

        let opts = Options {
            discover_timeout: Duration::from_secs(cli.discover_timeout),
            handshake_timeout: Duration::from_secs(cli.handshake_timeout),
            record: cli.record.clone(),
            ..Options::default()
        };

        let rx = conn::spawn(source, opts);

        // The collector is optional, so a cluster with no agents deployed pays
        // nothing for the feature.
        let collector = (!cli.no_agents).then(|| {
            collect::spawn(collect::Options {
                port: cli.agent_port,
                interval: Duration::from_millis(cli.poll_interval.max(100)),
                timeout: Duration::from_millis(cli.poll_timeout.max(50)),
                ..collect::Options::default()
            })
        });

        let stale_after = Duration::from_millis(cli.stale_after);
        if cli.dump {
            run_dump(rx, collector, stale_after).await
        } else {
            run_tui(rx, collector, stale_after).await
        }
    })
}

/// Text mode: echo every update. No terminal state to restore, so this is also
/// the safe way to debug the protocol.
async fn run_dump(
    mut rx: mpsc::Receiver<conn::Update>,
    collector: Option<Collector>,
    stale_after: Duration,
) -> io::Result<()> {
    let mut app = App::new();
    app.cluster.metrics_stale_after = stale_after;
    let (targets_tx, mut samples) = split(collector);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            update = rx.recv() => {
                let Some(update) = update else { break };
                println!("{update:?}");
                app.apply(update);
                publish_targets(&app, targets_tx.as_ref());
            }
            sample = recv_sample(&mut samples) => {
                if let Some(sample) = sample {
                    println!("Resource {{ host_id: {}, result: {:?} }}", sample.host_id, sample.result);
                    app.apply_resource(sample.host_id, sample.result);
                }
            }
        }
    }

    let s = app.cluster.summary();
    println!(
        "\n{} nodes online ({} offline), slots {}/{}, {} active / {} pending jobs, \
         agents {}/{} ({} stale, {} missing)",
        s.nodes_online,
        s.nodes_offline,
        s.used_slots,
        s.total_slots,
        s.active_jobs,
        s.pending_jobs,
        s.nodes_with_metrics,
        s.nodes_online,
        s.nodes_metrics_stale,
        s.nodes_without_agent
    );
    Ok(())
}

type Collector = (
    watch::Sender<collect::Targets>,
    mpsc::Receiver<collect::Sample>,
);

fn split(
    collector: Option<Collector>,
) -> (
    Option<watch::Sender<collect::Targets>>,
    Option<mpsc::Receiver<collect::Sample>>,
) {
    match collector {
        Some((tx, rx)) => (Some(tx), Some(rx)),
        None => (None, None),
    }
}

/// Await the next sample, or never, when agent polling is switched off.
///
/// `select!` needs a future for every branch, and a branch that can never fire
/// is how the same loop serves both modes.
async fn recv_sample(
    samples: &mut Option<mpsc::Receiver<collect::Sample>>,
) -> Option<collect::Sample> {
    match samples {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Tell the collector which nodes to poll, whenever the scheduler's view
/// changes. Sent only on an actual change, so a busy cluster does not wake the
/// collector on every job event.
fn publish_targets(app: &App, targets: Option<&watch::Sender<collect::Targets>>) {
    let Some(targets) = targets else { return };
    let mut next = app.cluster.poll_targets();
    next.sort();
    targets.send_if_modified(|current| {
        if *current == next {
            false
        } else {
            *current = next;
            true
        }
    });
}

async fn run_tui(
    mut rx: mpsc::Receiver<conn::Update>,
    collector: Option<Collector>,
    stale_after: Duration,
) -> io::Result<()> {
    let mut terminal = setup_terminal()?;
    let mut keys = spawn_input_reader();
    let mut app = App::new();
    app.cluster.metrics_stale_after = stale_after;
    let mut ui = Ui::default();
    let (targets_tx, mut samples) = split(collector);
    let mut ticker = tokio::time::interval(FRAME_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Separate from the frame timer: graphs advance on wall-clock seconds, so
    // their horizontal axis means time rather than "however many frames".
    let mut history = tokio::time::interval(HISTORY_INTERVAL);
    history.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result = loop {
        tokio::select! {
            // Coalesce bursts: a 100-node login replay is one repaint, not 100.
            _ = ticker.tick() => {
                if app.take_dirty() {
                    if let Err(e) = terminal.draw(|f| ui::draw(f, &app, &mut ui)) {
                        break Err(e);
                    }
                    // Only the renderer knows the viewport, so the scroll offset
                    // is clamped after the fact rather than guessed beforehand.
                    app.clamp_detail_scroll(ui.detail_max_scroll);
                }
            }
            _ = history.tick() => app.tick_history(),
            update = rx.recv() => match update {
                Some(update) => {
                    app.apply(update);
                    publish_targets(&app, targets_tx.as_ref());
                }
                // The connection task retries forever, so this only happens if
                // it panicked; without it we would spin on a closed channel.
                None => break Ok(()),
            },
            sample = recv_sample(&mut samples) => {
                if let Some(sample) = sample {
                    app.apply_resource(sample.host_id, sample.result);
                }
            }
            key = keys.recv() => match key {
                Some(InputEvent::Key(code, mods)) => {
                    app.on_key(classify(code, mods));
                    if app.should_quit {
                        break Ok(());
                    }
                }
                Some(InputEvent::Resize) => app.mark_dirty(),
                None => break Ok(()),
            },
            _ = tokio::signal::ctrl_c() => break Ok(()),
        }

        if app.should_quit {
            break Ok(());
        }
    };

    restore_terminal()?;
    result
}

enum InputEvent {
    Key(
        ratatui::crossterm::event::KeyCode,
        ratatui::crossterm::event::KeyModifiers,
    ),
    Resize,
}

/// Terminal input is blocking, so it gets its own OS thread rather than a
/// tokio worker. Keeping it off the runtime is what stops a slow network read
/// from making the UI feel stuck.
fn spawn_input_reader() -> mpsc::Receiver<InputEvent> {
    let (tx, rx) = mpsc::channel(64);
    std::thread::spawn(move || loop {
        match term_event::read() {
            Ok(TermEvent::Key(k)) => {
                // Windows and some terminals report press *and* release.
                if k.kind != KeyEventKind::Release
                    && tx
                        .blocking_send(InputEvent::Key(k.code, k.modifiers))
                        .is_err()
                {
                    return;
                }
            }
            Ok(TermEvent::Resize(_, _)) => {
                if tx.blocking_send(InputEvent::Resize).is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(_) => return,
        }
    });
    rx
}

type Term = ratatui::Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>;

fn setup_terminal() -> io::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // Without this, a panic leaves the user in a raw-mode alternate screen with
    // no echo — effectively a broken terminal.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        previous(info);
    }));

    ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))
}

fn restore_terminal() -> io::Result<()> {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    Ok(())
}

fn init_logging(cli: &Cli) -> io::Result<()> {
    use tracing_subscriber::filter::EnvFilter;
    use tracing_subscriber::fmt;

    let filter =
        EnvFilter::try_from_env("ICECREAM_WATCHER_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    match &cli.log_file {
        Some(path) => {
            let file = std::fs::File::create(path)?;
            fmt()
                .with_env_filter(filter)
                .with_writer(file)
                .with_ansi(false)
                .init();
            Ok(())
        }
        None if cli.dump => {
            // Text mode has no display to corrupt.
            fmt().with_env_filter(filter).with_writer(io::stderr).init();
            Ok(())
        }
        None => Ok(()), // TUI mode with no log file: stay silent.
    }
}
