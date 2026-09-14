//! Cluster state, rebuilt from the scheduler's event stream.
//!
//! The scheduler does not report job counts; it reports job *transitions*. Every
//! "pending / active / IN / OUT / LOCAL" number a monitor shows is an
//! accumulator maintained here, the same way `icecream-sundae` does it
//! (`src/main.cpp:125-178`). Two consequences the UI has to respect:
//!
//! * cumulative counters are **since connect**, not since boot — there is no way
//!   to ask the scheduler for history, so a reconnect resets them ([`Cluster::reset`]);
//! * `MON_STATS` records are partial, so node fields are merged rather than
//!   replaced (see [`icecc_proto::StatsRecord::merge_into`]).

pub mod history;

use std::collections::VecDeque;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

pub use history::{History, Trend};

use icecc_metrics::Snapshot;
use icecc_proto::msg::JobDone;
use icecc_proto::{Event, SchedulerTarget, StatsRecord, Update};

/// How a node's resource metrics are doing. Distinguishing "no agent" from
/// "agent broken" from "stale" matters: the first is a deployment gap, the
/// others are faults.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ResourceState {
    /// Never polled yet.
    #[default]
    Unknown,
    /// Fresh metrics in hand.
    Ok,
    /// Nothing listening. Almost always means the agent is not installed.
    NoAgent { reason: String },
    /// Something answered but could not be used — wrong service on the port,
    /// an unsupported schema, a timeout.
    Error { reason: String },
}

impl ResourceState {
    /// One short word for the UI.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unknown => "?",
            Self::Ok => "ok",
            Self::NoAgent { .. } => "no agent",
            Self::Error { .. } => "error",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::NoAgent { reason } | Self::Error { reason } => Some(reason),
            _ => None,
        }
    }
}

/// Outcome of one attempt to poll an agent.
#[derive(Debug, Clone)]
pub enum ResourceResult {
    /// Boxed because a snapshot with per-core data is large next to the other
    /// variants, and this type is moved through a channel.
    Ok(Box<Snapshot>),
    /// Could not reach anything.
    Unreachable(String),
    /// Reached something unusable.
    Bad(String),
}

/// A compile node as the scheduler describes it, plus what we have counted.
#[derive(Debug, Clone)]
pub struct Node {
    pub host_id: u32,
    /// Merged view of every `MON_STATS` record seen for this node.
    pub stats: StatsRecord,
    /// Remote jobs currently compiling here.
    pub active_jobs: BTreeSet<u32>,
    /// Local (non-distributed) jobs currently running here.
    pub local_jobs: BTreeSet<u32>,
    /// Remote jobs this node has compiled for others, since connect.
    pub jobs_in: u64,
    /// Jobs submitted from this node, since connect.
    ///
    /// Not "compiled elsewhere": the scheduler may well place a job back on the
    /// machine that asked for it, and that job counts in this node's `jobs_in`
    /// *and* its `jobs_out`.
    pub jobs_out: u64,
    /// Jobs this node compiled locally, since connect.
    pub jobs_local: u64,
    /// When we last heard anything about this node.
    pub last_update: Instant,
    /// Last good metrics from this node's agent, kept even once stale so a row
    /// shows its last known values rather than going blank.
    pub resources: Option<Snapshot>,
    pub resource_state: ResourceState,
    /// When the last *successful* poll landed, by the monitor's clock. The
    /// agent's own clock is not trusted for staleness.
    pub resources_at: Option<Instant>,
    /// The agent's hostname does not match the name the scheduler reports,
    /// which means we are probably talking to the wrong host (NAT, a reused
    /// address, a stale DNS entry).
    pub identity_mismatch: bool,
    /// Recent CPU busy percentage, one sample per history tick.
    pub cpu_history: History,
    /// Recent memory used percentage.
    pub mem_history: History,
    /// Recent compile-slot occupancy, as a percentage of this node's slots.
    pub slots_history: History,
    /// What this node's finished jobs say about how fast it actually is.
    pub throughput: Throughput,
}

impl Node {
    fn new(host_id: u32) -> Self {
        Self {
            host_id,
            stats: StatsRecord::default(),
            active_jobs: BTreeSet::new(),
            local_jobs: BTreeSet::new(),
            jobs_in: 0,
            jobs_out: 0,
            jobs_local: 0,
            last_update: Instant::now(),
            resources: None,
            resource_state: ResourceState::default(),
            resources_at: None,
            identity_mismatch: false,
            cpu_history: History::default(),
            throughput: Throughput::default(),
            mem_history: History::default(),
            slots_history: History::default(),
        }
    }

    pub fn name(&self) -> &str {
        self.stats.name.as_deref().unwrap_or("?")
    }

    pub fn ip(&self) -> &str {
        self.stats.ip.as_deref().unwrap_or("?")
    }

    pub fn platform(&self) -> &str {
        self.stats.platform.as_deref().unwrap_or("?")
    }

    pub fn max_jobs(&self) -> u32 {
        self.stats.max_jobs.unwrap_or(0)
    }

    /// Remote compiles running here right now.
    pub fn current_jobs(&self) -> u32 {
        self.active_jobs.len() as u32
    }

    /// `server_speed()`. `None` until the node has compiled something, which is
    /// different from "slow" and must be rendered differently.
    pub fn speed(&self) -> Option<f64> {
        match self.stats.speed {
            Some(s) if s > 0.0 => Some(s),
            _ => None,
        }
    }

    /// Composite 0..1000 scheduling weight. Not CPU utilisation.
    pub fn load(&self) -> Option<u32> {
        self.stats.load
    }

    /// The scheduler is pinging this node and has not heard back
    /// (it negates `MaxJobs` while waiting).
    pub fn suspect(&self) -> bool {
        self.stats.suspect.unwrap_or(false)
    }

    pub fn accepts_remote(&self) -> bool {
        !self.stats.no_remote.unwrap_or(false)
    }

    /// Nothing heard from the *scheduler* about this node for longer than
    /// `limit`. Because scheduler stats are change-driven, a quiet node is
    /// normal, so this is a weak signal — prefer [`Self::metrics_stale`].
    pub fn is_stale(&self, limit: Duration) -> bool {
        self.last_update.elapsed() > limit
    }

    /// Whether an agent has ever answered for this node.
    pub fn has_agent(&self) -> bool {
        self.resources.is_some()
    }

    /// Metrics we hold are older than `limit`. Unlike scheduler stats, agent
    /// metrics *are* expected on a fixed interval, so silence here is a fault.
    pub fn metrics_stale(&self, limit: Duration) -> bool {
        match self.resources_at {
            Some(at) => at.elapsed() > limit,
            None => self.resources.is_some(),
        }
    }

    /// Metrics that can be shown as current: present, and not stale.
    pub fn fresh_metrics(&self, limit: Duration) -> Option<&Snapshot> {
        if self.metrics_stale(limit) {
            return None;
        }
        self.resources.as_ref()
    }

    /// CPU busy percentage from the agent. `None` without one — deliberately
    /// not derived from the scheduler's `Load`, which is a scheduling weight
    /// and not utilisation.
    pub fn cpu_pct(&self) -> Option<f32> {
        self.resources.as_ref().map(|r| r.cpu.total_busy_pct)
    }

    /// Memory in use as a percentage of total. Needs an agent: the protocol
    /// carries no memory total at all.
    pub fn mem_pct(&self) -> Option<f32> {
        self.resources.as_ref().and_then(|r| r.mem.used_pct())
    }

    pub fn temp_c(&self) -> Option<f32> {
        self.resources.as_ref().and_then(|r| r.thermal.cpu_celsius)
    }

    /// Cores as the agent counts them. The scheduler's `MaxJobs` is a
    /// configured slot count and can differ from the real core count.
    pub fn cores(&self) -> Option<usize> {
        self.resources.as_ref().map(|r| r.cpu.cores)
    }

    /// Load average, preferring the agent's 1 Hz reading over the scheduler's
    /// change-driven one.
    pub fn load_avg_1(&self) -> Option<f32> {
        self.resources
            .as_ref()
            .map(|r| r.load.one)
            .or(self.stats.load_avg_1.map(|v| v as f32))
    }

    /// Load per core, which is what makes differently sized nodes comparable.
    pub fn load_per_core(&self) -> Option<f32> {
        let cores = self.cores()?;
        self.resources.as_ref()?.load.per_core(cores)
    }

    /// Compile slots in use, as a percentage. `None` when the node offers none.
    pub fn slot_pct(&self) -> Option<f32> {
        let max = self.max_jobs();
        (max > 0).then(|| (self.current_jobs() as f32 * 100.0 / max as f32).min(100.0))
    }

    /// Whether this node is doing anything at all, for dimming idle rows.
    pub fn is_busy(&self) -> bool {
        self.current_jobs() > 0
            || !self.local_jobs.is_empty()
            || self.cpu_pct().is_some_and(|c| c >= BUSY_CPU_PCT)
    }

    /// Which single metric is holding this node back, if any.
    ///
    /// The point is to separate "CPU constrained" from "memory constrained" at
    /// a glance; without it both just look like a busy node.
    pub fn bottleneck(&self) -> Option<Bottleneck> {
        let cpu = self.cpu_pct().unwrap_or(0.0);
        let mem = self.mem_pct().unwrap_or(0.0);

        // Memory first: a node that is out of memory will thrash rather than
        // compile, so it is the more actionable of the two.
        if mem >= MEM_PRESSURE_PCT {
            return Some(Bottleneck::Memory);
        }
        if cpu >= CPU_SATURATED_PCT {
            return Some(Bottleneck::Cpu);
        }
        if self.slot_pct().is_some_and(|s| s >= 100.0) {
            return Some(Bottleneck::Slots);
        }
        None
    }
}

/// A node's limiting factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bottleneck {
    Cpu,
    Memory,
    /// Every compile slot is taken, but the machine itself is not saturated —
    /// this node could take more work if `MaxJobs` allowed it.
    Slots,
}

/// Above this, a node counts as doing work even with no compile jobs assigned
/// (it may be running a local build, or something unrelated).
const BUSY_CPU_PCT: f32 = 15.0;
/// Above this, CPU is the limiting factor.
const CPU_SATURATED_PCT: f32 = 85.0;
/// Above this, memory pressure will hurt compile throughput.
const MEM_PRESSURE_PCT: f32 = 90.0;

/// What a job is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// A node has been requested but not yet assigned (`MON_GET_CS`).
    Pending,
    /// Compiling remotely on `host_id`.
    Active,
    /// Compiling locally on the submitter.
    Local,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u32,
    pub state: JobState,
    /// Node doing the work, once known.
    pub host_id: Option<u32>,
    /// Node that submitted the job, when the scheduler told us.
    pub client_id: Option<u32>,
    pub filename: String,
    pub since: Instant,
}

/// Connection status, for the header line.
#[derive(Debug, Clone)]
pub enum ConnectionState {
    Connecting {
        what: String,
    },
    Connected {
        target: SchedulerTarget,
        protocol: u32,
        /// The network this scheduler answered for, when it was found by
        /// broadcast. `None` when it was named outright or is being replayed.
        netname: Option<String>,
        since: Instant,
    },
    Disconnected {
        reason: String,
        /// Consecutive failed attempts; 0 right after a working session ended.
        attempt: u32,
        /// When the connection task will try again.
        retry_at: Instant,
    },
}

/// Compile throughput measured from results, not estimated.
///
/// Every `MON_JOB_DONE` carries what the compiling node actually spent and
/// produced, so a node's real rate can be had without anything installed on it.
/// One job says almost nothing — on this cluster the same machine ran at 17,
/// 114 and 284 KB per CPU-second on three consecutive files, because a
/// template-heavy source grinds a long time for a small object and a plain one
/// does not — so the figure is the *ratio of the sums*, never the mean of the
/// ratios, and it is withheld until enough jobs have gone through to mean
/// something.
#[derive(Debug, Clone, Default)]
pub struct Throughput {
    samples: VecDeque<(u64, u32)>,
    out_bytes: u64,
    user_ms: u64,
}

/// Jobs kept. Enough to average out what a file happens to be, short enough
/// that a node which has just started throttling shows it within a few seconds
/// of work rather than a few minutes.
const THROUGHPUT_SAMPLES: usize = 32;
/// Below this the figure is noise, and noise presented as a measurement is
/// worse than no measurement at all.
const THROUGHPUT_MIN: usize = 5;

impl Throughput {
    fn push(&mut self, out_bytes: u64, user_ms: u32) {
        if self.samples.len() == THROUGHPUT_SAMPLES {
            if let Some((b, u)) = self.samples.pop_front() {
                self.out_bytes -= b;
                self.user_ms -= u64::from(u);
            }
        }
        self.samples.push_back((out_bytes, user_ms));
        self.out_bytes += out_bytes;
        self.user_ms += u64::from(user_ms);
    }

    /// Bytes of compiled output per second of CPU time, over the kept jobs.
    pub fn rate(&self) -> Option<f64> {
        (self.samples.len() >= THROUGHPUT_MIN && self.user_ms > 0)
            .then(|| self.out_bytes as f64 / (self.user_ms as f64 / 1000.0))
    }

    pub fn jobs_measured(&self) -> usize {
        self.samples.len()
    }
}

/// A stretch of work with no idle gap in it — one build, as far as a monitor
/// can tell.
///
/// The protocol has no notion of a build: the scheduler learns a file exists
/// when a client asks for a node for it, and never learns how many more are
/// coming. So a build is inferred from the only thing visible, which is whether
/// the cluster is doing anything, and what can be said about it is what has
/// happened — not how much is left.
#[derive(Debug, Clone)]
pub struct BuildRun {
    /// When work first appeared.
    pub started: Instant,
    /// When work stopped, for a build that has finished.
    pub ended: Option<Instant>,
    /// Completions counted before this build began.
    completed_before: u64,
    /// Jobs finished during it.
    pub done: u64,
    /// When the cluster last went quiet, while waiting to see whether it stays
    /// quiet long enough to call the build over.
    quiet_since: Option<Instant>,
}

impl BuildRun {
    /// How long the work took, excluding the silence that ended it.
    pub fn elapsed(&self) -> Duration {
        match self.ended {
            Some(end) => end.saturating_duration_since(self.started),
            None => self.started.elapsed(),
        }
    }

    /// Jobs per second across the whole run, which is not the instantaneous
    /// rate the graph draws.
    pub fn average_rate(&self) -> Option<f64> {
        let secs = self.elapsed().as_secs_f64();
        (secs >= 1.0 && self.done > 0).then(|| self.done as f64 / secs)
    }
}

/// A cluster quiet for this long has finished whatever it was doing. Long
/// enough to ride out the gap between a link step and the compiles after it,
/// short enough that the figure is still on screen when someone looks up.
pub const BUILD_IDLE: Duration = Duration::from_secs(20);

/// Counters over completed work, since connect.
#[derive(Debug, Clone, Default)]
pub struct Totals {
    pub completed_remote: u64,
    pub completed_local: u64,
    pub failed: u64,
    /// `MON_JOB_DONE` for a job we never saw begin — normal right after connect.
    pub unmatched_done: u64,
    /// Jobs dropped because their end never arrived. See
    /// [`Cluster::job_timeout`]; a non-zero count here means the queue figure
    /// would otherwise have been overstated.
    pub expired_jobs: u64,
    /// Nodes that have left the cluster since connect. Their rows are gone, so
    /// this is all that is left of the fact that they were ever here.
    pub nodes_left: u64,
}

/// One-glance cluster figures, computed on demand.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub nodes_online: usize,
    /// Online nodes that accept remote jobs.
    pub nodes_available: usize,
    pub total_slots: u32,
    pub used_slots: u32,
    pub pending_jobs: usize,
    pub active_jobs: usize,
    pub local_jobs: usize,
    /// Nodes that have left the cluster since connect. Their rows are gone, so
    /// this is the only thing left saying they were ever here.
    pub nodes_left: u64,
    /// Online nodes reporting fresh metrics.
    pub nodes_with_metrics: usize,
    /// Online nodes with no agent answering, i.e. a deployment gap.
    pub nodes_without_agent: usize,
    /// Online nodes whose agent has gone quiet or is failing.
    pub nodes_metrics_stale: usize,
}

impl Summary {
    /// Slot occupancy in percent, or `None` when the cluster has no slots.
    pub fn slot_usage(&self) -> Option<f64> {
        (self.total_slots > 0).then(|| self.used_slots as f64 * 100.0 / self.total_slots as f64)
    }
}

/// The whole monitored cluster.
#[derive(Debug)]
pub struct Cluster {
    pub nodes: BTreeMap<u32, Node>,
    pub jobs: HashMap<u32, Job>,
    pub totals: Totals,
    pub connection: ConnectionState,
    /// When the current counters started accumulating.
    pub counters_since: Instant,
    /// Message types we could not model, counted once per type for the log.
    pub unknown_messages: BTreeMap<u32, u64>,
    /// Agent metrics older than this count as stale. Agents report on a fixed
    /// interval, so this can be tight — unlike scheduler stats, which are
    /// change-driven and legitimately silent for minutes.
    pub metrics_stale_after: Duration,
    /// Jobs waiting for a node, sampled per tick. This is the series that
    /// answers "is the scheduler queue growing?".
    pub pending_history: History,
    /// Cluster-wide slot occupancy percentage per tick.
    pub slots_history: History,
    /// Jobs completed during each tick, i.e. throughput.
    pub rate_history: History,
    /// Completion total at the previous tick, to turn a counter into a rate.
    last_completed: u64,
    /// The build the cluster is working on now, if it is working on one.
    pub build: Option<BuildRun>,
    /// The one before it, kept so the band has something to say the moment a
    /// build ends — which is exactly when someone looks at it.
    pub last_build: Option<BuildRun>,
    /// Quiet for this long ends a build. A field rather than a constant so a
    /// test need not wait out the real one.
    pub build_idle: Duration,
    /// Drop a job we were told about but never told the end of, after this.
    ///
    /// The protocol gives no guarantee that every `MON_GET_CS` or
    /// `MON_JOB_BEGIN` is followed by a `MON_JOB_DONE`: upstream's
    /// `handle_job_done` returns without notifying monitors when it cannot find
    /// the job, and its cancellation lookup only matches jobs that have not yet
    /// been assigned. A lost end would otherwise inflate the queue depth for
    /// the rest of the session and grow this map without bound.
    ///
    /// Generous on purpose. Expiring early understates the queue, which is
    /// exactly as wrong as overstating it, and a saturated cluster can keep a
    /// job queued legitimately for a long time. `None` disables expiry.
    pub job_timeout: Option<Duration>,
    /// The first scheduler this session attached to. A later session attaching
    /// to a *different* address means the whole cluster on screen changed
    /// identity, which the user has to be told rather than left to notice.
    pub first_target: Option<SchedulerTarget>,
}

impl Default for Cluster {
    fn default() -> Self {
        Self::new()
    }
}

impl Cluster {
    pub fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            jobs: HashMap::new(),
            totals: Totals::default(),
            connection: ConnectionState::Connecting {
                what: "starting".into(),
            },
            counters_since: Instant::now(),
            unknown_messages: BTreeMap::new(),
            metrics_stale_after: Duration::from_secs(5),
            pending_history: History::default(),
            slots_history: History::default(),
            rate_history: History::default(),
            last_completed: 0,
            build: None,
            last_build: None,
            build_idle: BUILD_IDLE,
            job_timeout: Some(Duration::from_secs(30 * 60)),
            first_target: None,
        }
    }

    /// Drop all derived state. Called on connect, because the scheduler replays
    /// the node list and we cannot reconcile old job ids against a new session.
    ///
    /// Agent metrics go with it: host ids are assigned by the scheduler and a
    /// new session may hand the same id to a different machine, so keeping the
    /// old readings could attach one node's CPU graph to another.
    pub fn reset(&mut self) {
        self.nodes.clear();
        self.jobs.clear();
        self.totals = Totals::default();
        self.counters_since = Instant::now();
        self.pending_history = History::default();
        self.slots_history = History::default();
        self.rate_history = History::default();
        self.last_completed = 0;
        // A build is a run of work seen on *this* connection: the counters it
        // is measured against restart here, so it cannot straddle a reconnect.
        self.build = None;
        self.last_build = None;
    }

    /// Advance every history series by one sample.
    ///
    /// Called on a fixed timer rather than per event, so a quiet cluster's
    /// graphs still scroll instead of freezing — and so the horizontal axis
    /// means time rather than "however many events happened".
    pub fn tick_history(&mut self) {
        // Before the summary: an expired job must not be counted into the very
        // sample that its expiry was meant to correct.
        self.expire();

        let summary = self.summary();

        self.pending_history.push(Some(summary.pending_jobs as f32));
        self.slots_history
            .push(summary.slot_usage().map(|p| p as f32));

        let completed = self.totals.completed_remote + self.totals.completed_local;
        // Saturating, because a reconnect resets the counter.
        let done_this_tick = completed.saturating_sub(self.last_completed);
        self.last_completed = completed;
        self.rate_history.push(Some(done_this_tick as f32));

        self.track_build(&summary, completed);

        let stale_after = self.metrics_stale_after;
        for node in self.nodes.values_mut() {
            // Stale metrics are a gap too, so a graph cannot flatline on a
            // value that stopped being true a minute ago. Values are read out
            // before pushing, because the push borrows the node mutably.
            let (cpu, mem) = match node.fresh_metrics(stale_after) {
                Some(r) => (Some(r.cpu.total_busy_pct), r.mem.used_pct()),
                None => (None, None),
            };
            let slots = node.slot_pct();
            node.cpu_history.push(cpu);
            node.mem_history.push(mem);
            node.slots_history.push(slots);
        }
    }

    /// Drop state the scheduler has stopped telling us about.
    ///
    /// A monitor is a long-running process fed by a remote one, so every
    /// collection it keeps needs a bound that does not depend on the remote
    /// behaving. Both bounds here are opt-out rather than opt-in because an
    /// unbounded map is not a safe default for a process meant to run for days.
    fn expire(&mut self) {
        if let Some(limit) = self.job_timeout {
            let mut lost: Vec<u32> = Vec::new();
            for job in self.jobs.values() {
                if job.since.elapsed() > limit {
                    lost.push(job.id);
                }
            }
            for id in lost {
                let Some(job) = self.jobs.remove(&id) else {
                    continue;
                };
                // Whatever node was holding it must release the slot too, or
                // the row keeps showing work that is not happening.
                if let Some(host_id) = job.host_id {
                    if let Some(node) = self.nodes.get_mut(&host_id) {
                        node.active_jobs.remove(&id);
                        node.local_jobs.remove(&id);
                    }
                }
                self.totals.expired_jobs += 1;
                tracing::warn!(
                    "job {id} ({:?}, {}) expired after {limit:?} with no MON_JOB_DONE",
                    job.state,
                    if job.filename.is_empty() {
                        "<unnamed>"
                    } else {
                        &job.filename
                    }
                );
            }
        }
    }

    /// The scheduler we attached to first, when the current one is not it.
    ///
    /// Non-`None` means the cluster on screen is not the cluster the session
    /// started on — a different scheduler answered discovery, so every host id,
    /// node and counter belongs to somewhere else.
    pub fn moved_from(&self) -> Option<&SchedulerTarget> {
        let ConnectionState::Connected { target, .. } = &self.connection else {
            return None;
        };
        match &self.first_target {
            Some(first) if first != target => Some(first),
            _ => None,
        }
    }

    /// Jobs this node has submitted that are still waiting for a compile host.
    ///
    /// Pending jobs have no host yet, so they belong to whoever submitted them —
    /// which is how `icemon` and `icecream-sundae` attribute them too.
    pub fn pending_from(&self, host_id: u32) -> usize {
        self.jobs
            .values()
            .filter(|j| j.state == JobState::Pending && j.client_id == Some(host_id))
            .count()
    }

    /// Median compile speed across nodes that have one, for spotting outliers.
    /// The middle measured rate among the nodes that do the same work as
    /// `node` — the ones on its platform.
    ///
    /// Per platform, because icecream only sends a job to a node whose
    /// environment matches it: a `Darwin25_arm64` node compiles the macOS build
    /// and an `x86_64` node compiles the Linux one, so the two are not doing the
    /// same work and the ratio between them measures nothing.
    ///
    /// Measured on a live four-node cluster: the macOS node read 3.8x the
    /// cluster-wide median while running 891 jobs averaging 359 ms against the
    /// x86 nodes' two to four seconds — different work, not a faster machine —
    /// and in doing so dragged all three x86 nodes below 1.0x, which was the
    /// only thing on the screen anyone could have acted on.
    ///
    /// `None` for a platform with nothing to compare against: one node is its
    /// own median, and `1.0x` would claim a comparison that was never made.
    pub fn median_rate_among_peers(&self, node: &Node) -> Option<f64> {
        let mut rates: Vec<f64> = self
            .nodes
            .values()
            .filter(|n| n.platform() == node.platform())
            .filter_map(|n| n.throughput.rate())
            .collect();
        if rates.len() < 2 {
            return None;
        }
        rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(rates[rates.len() / 2])
    }

    /// Whether a node is markedly slower than the rest of its platform.
    ///
    /// Answers "is one node significantly slower than the others?" without
    /// making the user compare numbers by eye. Needs at least three nodes of
    /// that platform with a measured rate, or the median is not meaningful —
    /// and the rate is the measured one, so a node that is slow because it is
    /// throttling or because somebody else is using it counts as slow, which
    /// the scheduler's own estimate of the machine never would.
    pub fn is_slow_outlier(&self, node: &Node) -> bool {
        if self
            .nodes
            .values()
            .filter(|n| n.platform() == node.platform())
            .filter(|n| n.throughput.rate().is_some())
            .count()
            < 3
        {
            return false;
        }
        match (node.throughput.rate(), self.median_rate_among_peers(node)) {
            (Some(rate), Some(median)) if median > 0.0 => rate < median * SLOW_OUTLIER_FRACTION,
            _ => false,
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.connection, ConnectionState::Connected { .. })
    }

    /// Feed one update from the connection task.
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Connecting { what } => {
                self.connection = ConnectionState::Connecting { what };
            }
            Update::Connected {
                target,
                protocol,
                netname,
            } => {
                self.reset();
                // Recorded before the move so the *first* scheduler of the
                // session stays the reference point.
                if self.first_target.is_none() {
                    self.first_target = Some(target.clone());
                }
                self.connection = ConnectionState::Connected {
                    target,
                    protocol,
                    netname,
                    since: Instant::now(),
                };
            }
            Update::Disconnected {
                reason,
                attempt,
                retry_at,
            } => {
                self.connection = ConnectionState::Disconnected {
                    reason,
                    attempt,
                    retry_at,
                };
                // Jobs cannot make progress without the scheduler, and their
                // ids are only meaningful within a session.
                self.jobs.clear();
                for node in self.nodes.values_mut() {
                    node.active_jobs.clear();
                    node.local_jobs.clear();
                }
            }
            Update::Event(ev) => self.apply_event(ev),
        }
    }

    fn apply_event(&mut self, ev: Event) {
        match ev {
            Event::Stats { host_id, stats } => self.apply_stats(host_id, &stats),

            Event::GetCs {
                job_id,
                client_id,
                filename,
                ..
            } => {
                self.jobs.insert(
                    job_id,
                    Job {
                        id: job_id,
                        state: JobState::Pending,
                        host_id: None,
                        client_id: Some(client_id),
                        filename,
                        since: Instant::now(),
                    },
                );
            }

            Event::JobBegin {
                job_id, host_id, ..
            } => {
                // The submitter is only known if we saw the MON_GET_CS; after a
                // mid-flight connect we will not have.
                let client_id = self.jobs.get(&job_id).and_then(|j| j.client_id);
                let filename = self
                    .jobs
                    .get(&job_id)
                    .map(|j| j.filename.clone())
                    .unwrap_or_default();

                self.jobs.insert(
                    job_id,
                    Job {
                        id: job_id,
                        state: JobState::Active,
                        host_id: Some(host_id),
                        client_id,
                        filename,
                        since: Instant::now(),
                    },
                );

                if let Some(node) = self.nodes.get_mut(&host_id) {
                    node.active_jobs.insert(job_id);
                    node.jobs_in += 1;
                }
                if let Some(node) = client_id.and_then(|id| self.nodes.get_mut(&id)) {
                    node.jobs_out += 1;
                }
            }

            Event::LocalJobBegin {
                job_id,
                host_id,
                file,
                ..
            } => {
                self.jobs.insert(
                    job_id,
                    Job {
                        id: job_id,
                        state: JobState::Local,
                        host_id: Some(host_id),
                        client_id: Some(host_id),
                        filename: file,
                        since: Instant::now(),
                    },
                );
                if let Some(node) = self.nodes.get_mut(&host_id) {
                    node.local_jobs.insert(job_id);
                    node.jobs_local += 1;
                }
            }

            Event::JobDone(done) => self.finish_remote(done),

            Event::LocalJobDone { job_id } => match self.jobs.remove(&job_id) {
                Some(job) => {
                    if let Some(node) = job.host_id.and_then(|id| self.nodes.get_mut(&id)) {
                        node.local_jobs.remove(&job_id);
                    }
                    self.totals.completed_local += 1;
                }
                None => self.totals.unmatched_done += 1,
            },

            Event::End | Event::Ping => {}

            Event::Other { ty } => {
                *self.unknown_messages.entry(ty).or_default() += 1;
            }
        }
    }

    fn apply_stats(&mut self, host_id: u32, blob: &str) {
        let record = StatsRecord::parse(blob);

        // A node that has gone leaves the list rather than sitting in it struck
        // through. Host ids are per *connection* — the scheduler builds a new
        // `CompileServer`, and so a new id, every time a daemon attaches — so a
        // laptop that sleeps and wakes would leave a fresh corpse behind on
        // every cycle, and a morning of that is a screen of one machine's
        // remains crowding out the nodes that are running. It reappears by
        // itself when its daemon reattaches.
        if record.offline {
            let Some(gone) = self.nodes.remove(&host_id) else {
                return;
            };
            for id in gone.active_jobs.iter().chain(gone.local_jobs.iter()) {
                self.jobs.remove(id);
            }
            self.totals.nodes_left += 1;
            tracing::info!("{} (host id {host_id}) left the cluster", gone.name());
            return;
        }

        let node = self.node_mut(host_id);
        record.merge_into(&mut node.stats);
        node.last_update = Instant::now();

    }

    fn finish_remote(&mut self, done: JobDone) {
        match self.jobs.remove(&done.job_id) {
            Some(job) => {
                if let Some(node) = job.host_id.and_then(|id| self.nodes.get_mut(&id)) {
                    node.active_jobs.remove(&done.job_id);
                    node.local_jobs.remove(&done.job_id);
                }
                if job.state == JobState::Local {
                    self.totals.completed_local += 1;
                } else {
                    self.totals.completed_remote += 1;
                }
                if done.exit_code != 0 {
                    self.totals.failed += 1;
                }
                // Only successful jobs measure anything: a compile that failed
                // stopped when the error was found, which is not how long the
                // file takes.
                if done.exit_code == 0 && done.user_msec > 0 {
                    if let Some(node) = job.host_id.and_then(|id| self.nodes.get_mut(&id)) {
                        node.throughput.push(u64::from(done.out_uncompressed), done.user_msec);
                    }
                }
            }
            None => {
                // Either the job started before we connected, or the scheduler
                // had already forgotten it. Both are expected, not errors.
                self.totals.unmatched_done += 1;
            }
        }
    }

    /// Apply the outcome of one agent poll.
    pub fn apply_resource(&mut self, host_id: u32, result: ResourceResult) {
        // Only for nodes the scheduler told us about: an agent answering for a
        // host that has left the cluster is not something to display.
        let Some(node) = self.nodes.get_mut(&host_id) else {
            return;
        };

        match result {
            ResourceResult::Ok(snapshot) => {
                node.identity_mismatch = !hostnames_agree(node.name(), &snapshot.hostname);
                if node.identity_mismatch {
                    tracing::warn!(
                        "node {} (host id {host_id}) answered with hostname {:?}",
                        node.name(),
                        snapshot.hostname
                    );
                }
                node.resources = Some(*snapshot);
                node.resources_at = Some(Instant::now());
                node.resource_state = ResourceState::Ok;
            }
            ResourceResult::Unreachable(reason) => {
                // Keep any previous snapshot: staleness is shown separately, and
                // the last known values are more useful than a blank row.
                node.resource_state = ResourceState::NoAgent { reason };
            }
            ResourceResult::Bad(reason) => {
                node.resource_state = ResourceState::Error { reason };
            }
        }
    }

    /// Nodes worth polling, as `(host id, address)`.
    ///
    /// Offline nodes are skipped — polling a machine the scheduler has already
    /// lost wastes a timeout every interval.
    pub fn poll_targets(&self) -> Vec<(u32, String)> {
        self.nodes
            .values()
            .filter_map(|n| {
                let ip = n.stats.ip.as_deref()?;
                (!ip.is_empty() && ip != "?").then(|| (n.host_id, ip.to_owned()))
            })
            .collect()
    }

    /// The node a `MON_STATS` record belongs to, created if this is the first
    /// we have heard of it.
    ///
    /// Only stats may introduce a node. Job events name host ids too, and
    /// letting those create one conjures a row with no name, no address and no
    /// slot count out of a `MON_JOB_DONE` that arrived after its node left —
    /// which is exactly what happens now that a departing node is removed. The
    /// scheduler announces a node with stats before it can appear in any job,
    /// so nothing real is lost by ignoring the rest.
    fn node_mut(&mut self, host_id: u32) -> &mut Node {
        self.nodes
            .entry(host_id)
            .or_insert_with(|| Node::new(host_id))
    }

    pub fn summary(&self) -> Summary {
        let mut s = Summary {
            nodes_left: self.totals.nodes_left,
            ..Summary::default()
        };
        for node in self.nodes.values() {
            s.nodes_online += 1;
            if node.accepts_remote() {
                s.nodes_available += 1;
                s.total_slots += node.max_jobs();
            }
            s.used_slots += node.current_jobs();

            if node.fresh_metrics(self.metrics_stale_after).is_some() {
                s.nodes_with_metrics += 1;
            } else if node.has_agent() {
                s.nodes_metrics_stale += 1;
            } else {
                s.nodes_without_agent += 1;
            }
        }
        for job in self.jobs.values() {
            match job.state {
                JobState::Pending => s.pending_jobs += 1,
                JobState::Active => s.active_jobs += 1,
                JobState::Local => s.local_jobs += 1,
            }
        }
        s
    }

    /// Follow the run of work the cluster is in, if it is in one.
    ///
    /// Anything in flight or waiting counts as work, local jobs included: a
    /// build that fell back to compiling on the submitter is still a build, and
    /// a monitor that called it finished would be wrong in exactly the case
    /// someone is watching to understand.
    fn track_build(&mut self, summary: &Summary, completed: u64) {
        let busy = summary.active_jobs + summary.pending_jobs + summary.local_jobs > 0;
        let now = Instant::now();

        match &mut self.build {
            None => {
                if busy {
                    self.build = Some(BuildRun {
                        started: now,
                        ended: None,
                        completed_before: completed,
                        done: 0,
                        quiet_since: None,
                    });
                }
            }
            Some(run) => {
                run.done = completed.saturating_sub(run.completed_before);
                if busy {
                    run.quiet_since = None;
                    return;
                }
                match run.quiet_since {
                    None => run.quiet_since = Some(now),
                    Some(since) if now.saturating_duration_since(since) >= self.build_idle => {
                        // Ended when the work stopped, not when we decided it
                        // had: the wait is ours, and charging it to the build
                        // would inflate every elapsed time by BUILD_IDLE.
                        run.ended = Some(since);
                        self.last_build = self.build.take();
                    }
                    Some(_) => {}
                }
            }
        }
    }

    /// Nodes in a stable display order: name, then host id as a tiebreak so
    /// unnamed nodes do not jitter between frames.
    pub fn nodes_sorted(&self) -> Vec<&Node> {
        let mut v: Vec<&Node> = self.nodes.values().collect();
        v.sort_by(|a, b| a.name().cmp(b.name()).then(a.host_id.cmp(&b.host_id)));
        v
    }
}

/// A node slower than this fraction of the cluster median is called out.
const SLOW_OUTLIER_FRACTION: f64 = 0.5;

/// Whether a scheduler-reported node name and an agent hostname plausibly
/// describe the same machine.
///
/// Compared case-insensitively and on the first label only, because one side
/// routinely has a domain the other lacks (`build01` vs `build01.corp.example`).
pub fn hostnames_agree(scheduler_name: &str, agent_hostname: &str) -> bool {
    // An unknown name is not evidence of a mismatch.
    if scheduler_name.is_empty() || scheduler_name == "?" || agent_hostname.is_empty() {
        return true;
    }
    let short = |s: &str| s.split('.').next().unwrap_or(s).to_ascii_lowercase();
    short(scheduler_name) == short(agent_hostname)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(host_id: u32, blob: &str) -> Update {
        Update::Event(Event::Stats {
            host_id,
            stats: blob.into(),
        })
    }

    fn connected() -> Update {
        Update::Connected {
            target: SchedulerTarget {
                host: "sched".into(),
                port: 8765,
            },
            protocol: 43,
            netname: Some("ICECREAM".into()),
        }
    }

    fn node_blob(name: &str, max_jobs: u32) -> String {
        format!("Name:{name}\nIP:10.0.0.1\nMaxJobs:{max_jobs}\nNoRemote:false\nPlatform:x86_64\nLoad:100\n")
    }

    fn cluster_with_two_nodes() -> Cluster {
        let mut c = Cluster::new();
        c.apply(connected());
        c.apply(stats(1, &node_blob("build01", 8)));
        c.apply(stats(2, &node_blob("build02", 4)));
        c
    }

    #[test]
    fn a_node_is_measured_against_its_own_platform() {
        // icecream only sends a job to a node whose environment matches, so a
        // macOS node and a Linux node are compiling different builds. Measured
        // on a live cluster, the macOS node read 3.8x the cluster-wide median
        // while running jobs a tenth the length of the Linux ones — and pushed
        // every Linux node below 1.0x in the process.
        let mut c = Cluster::new();
        c.apply(connected());
        for (id, name, platform) in [
            (1u32, "linux01", "x86_64"),
            (2, "linux02", "x86_64"),
            (3, "mac", "Darwin25_arm64"),
        ] {
            c.apply(stats(
                id,
                &format!("Name:{name}\nIP:10.0.0.{id}\nMaxJobs:8\nPlatform:{platform}\n"),
            ));
        }
        // The mac produces ten times the object per CPU-second of either Linux
        // node, because it is compiling something else entirely.
        for (host, out_bytes) in [(1u32, 100_000u32), (2, 120_000), (3, 1_100_000)] {
            for n in 0..6u32 {
                let job_id = host * 1000 + n;
                c.apply(get_cs(job_id, host));
                c.apply(job_begin(job_id, host));
                c.apply(Update::Event(Event::JobDone(JobDone {
                    job_id,
                    exit_code: 0,
                    user_msec: 1_000,
                    out_uncompressed: out_bytes,
                    ..Default::default()
                })));
            }
        }

        let linux = &c.nodes[&1];
        let mac = &c.nodes[&3];
        let linux_median = c.median_rate_among_peers(linux).expect("two x86 peers");
        assert!(
            (linux.throughput.rate().unwrap() / linux_median - 0.83).abs() < 0.02,
            "the x86 node is measured against the other x86 node"
        );
        // The lone macOS node has nothing to compare against and says so,
        // rather than reporting itself as exactly average.
        assert!(
            c.median_rate_among_peers(mac).is_none(),
            "one node is its own median, which is not a comparison"
        );
    }

    #[test]
    fn throughput_is_the_ratio_of_sums_not_the_mean_of_ratios() {
        // Measured on this cluster: the same machine ran at 17, 114 and 284 KB
        // per CPU-second on three consecutive files, because what a file is
        // decides how much object a CPU-second buys. Averaging those three
        // ratios describes the files. Dividing the totals describes the node.
        let mut t = Throughput::default();
        for (bytes, ms) in [(7_776u64, 451u32), (139_104, 1_188), (8_728, 30), (55_016, 589)] {
            t.push(bytes, ms);
        }
        t.push(100_000, 1_000);
        let rate = t.rate().expect("five jobs is enough to speak");
        let total_bytes = 7_776 + 139_104 + 8_728 + 55_016 + 100_000;
        let total_secs = (451 + 1_188 + 30 + 589 + 1_000) as f64 / 1000.0;
        assert!((rate - total_bytes as f64 / total_secs).abs() < 1.0);

        let mean_of_ratios = [(7_776.0, 0.451), (139_104.0, 1.188), (8_728.0, 0.030),
                              (55_016.0, 0.589), (100_000.0, 1.0)]
            .iter()
            .map(|(b, s): &(f64, f64)| b / s)
            .sum::<f64>()
            / 5.0;
        // On these five jobs the mean of the ratios overstates the node by
        // about thirty per cent — one 30 ms job that happened to emit a lot of
        // object counts as much as a job a hundred times its length.
        assert!(
            mean_of_ratios > rate * 1.2,
            "mean of ratios {mean_of_ratios:.0} vs ratio of sums {rate:.0}"
        );
    }

    #[test]
    fn a_rate_is_withheld_until_it_means_something() {
        let mut t = Throughput::default();
        for _ in 0..4 {
            t.push(100_000, 1_000);
            assert!(t.rate().is_none(), "four jobs is still noise");
        }
        t.push(100_000, 1_000);
        assert!(t.rate().is_some(), "five is the point it starts speaking");

        // And it forgets, so a node that slows down says so rather than being
        // averaged against an hour of when it was fast.
        for _ in 0..THROUGHPUT_SAMPLES {
            t.push(10_000, 1_000);
        }
        assert_eq!(t.jobs_measured(), THROUGHPUT_SAMPLES);
        assert!(
            (t.rate().unwrap() - 10_000.0).abs() < 1.0,
            "the fast jobs should have aged out: {:?}",
            t.rate()
        );
    }

    #[test]
    fn a_failed_compile_does_not_count_as_speed() {
        // It stopped when the error was found, which is not how long the file
        // takes — and a node hitting errors fast would look like the quickest
        // machine in the cluster.
        let mut c = cluster_with_two_nodes();
        for job_id in 0..8u32 {
            c.apply(get_cs(job_id, 1));
            c.apply(job_begin(job_id, 1));
            c.apply(Update::Event(Event::JobDone(JobDone {
                job_id,
                exit_code: 1,
                user_msec: 10,
                out_uncompressed: 100_000,
                ..Default::default()
            })));
        }
        assert!(c.nodes[&1].throughput.rate().is_none(), "nothing measured");
        assert_eq!(c.totals.failed, 8);
    }

    #[test]
    fn a_build_is_the_run_of_work_between_two_silences() {
        // There is no build in the protocol: the scheduler is told a file
        // exists when a client asks for a node for it, and never how many more
        // are coming. So one is inferred from the cluster going busy and then
        // staying quiet, and only what happened can be reported.
        let mut c = cluster_with_two_nodes();
        c.build_idle = Duration::from_millis(20);

        c.tick_history();
        assert!(c.build.is_none(), "an idle cluster is not building");

        c.apply(get_cs(1, 1));
        c.apply(job_begin(1, 1));
        c.tick_history();
        let run = c.build.as_ref().expect("work means a build is on");
        assert_eq!(run.done, 0, "nothing has finished yet");

        c.apply(Update::Event(Event::JobDone(JobDone {
            job_id: 1,
            ..Default::default()
        })));
        c.tick_history();
        assert_eq!(c.build.as_ref().unwrap().done, 1);

        // Quiet, but not yet long enough to call it over.
        c.tick_history();
        assert!(c.build.is_some(), "one quiet tick is not the end of a build");

        pass(c.build_idle);
        c.tick_history();
        let finished = c.last_build.as_ref().expect("the build should have ended");
        assert!(c.build.is_none());
        assert_eq!(finished.done, 1);
        // The wait that detected the end is ours, not the build's.
        assert!(
            finished.elapsed() < c.build_idle * 5,
            "the idle wait was charged to the build: {:?}",
            finished.elapsed()
        );
    }

    #[test]
    fn a_local_only_build_is_still_a_build() {
        // Work that fell back to the submitter is the case someone is watching
        // to understand, so a monitor that called the cluster idle would be
        // wrong exactly then.
        let mut c = cluster_with_two_nodes();
        c.apply(Update::Event(Event::LocalJobBegin {
            job_id: 5,
            start_time: 0,
            host_id: 1,
            file: "main.cc".into(),
        }));
        c.tick_history();
        assert!(c.build.is_some(), "local jobs are work too");
    }

    #[test]
    fn a_reconnect_does_not_leave_a_build_straddling_it() {
        let mut c = cluster_with_two_nodes();
        c.apply(get_cs(1, 1));
        c.apply(job_begin(1, 1));
        c.tick_history();
        assert!(c.build.is_some());

        c.apply(connected());
        assert!(c.build.is_none(), "the counters it was measured against reset");
        assert!(c.last_build.is_none());
    }

    fn get_cs(job_id: u32, client_id: u32) -> Update {
        Update::Event(Event::GetCs {
            job_id,
            client_id,
            filename: "lost.cpp".into(),
            lang: 0,
        })
    }

    fn job_begin(job_id: u32, host_id: u32) -> Update {
        Update::Event(Event::JobBegin {
            job_id,
            start_time: 0,
            host_id,
        })
    }

    /// Advance past a deliberately tiny retention limit. Real durations rather
    /// than a fake clock, because the code under test reads the monotonic clock
    /// directly and a fake one would test the fake.
    fn pass(limit: Duration) {
        std::thread::sleep(limit * 5);
    }

    // -------------------------------------------------- job expiry

    #[test]
    fn a_job_whose_end_never_arrives_is_dropped_and_counted() {
        // Upstream does not guarantee a MON_JOB_DONE for every job it announces:
        // handle_job_done returns without notifying monitors when it cannot find
        // the job, and its cancellation lookup only matches unassigned ones. A
        // lost end would otherwise overstate the queue for the whole session.
        let mut c = cluster_with_two_nodes();
        c.job_timeout = Some(Duration::from_millis(2));
        c.apply(get_cs(7, 1));
        assert_eq!(c.summary().pending_jobs, 1);

        pass(Duration::from_millis(2));
        c.tick_history();

        assert_eq!(c.summary().pending_jobs, 0, "the stuck job must be dropped");
        assert_eq!(
            c.totals.expired_jobs, 1,
            "an expiry is counted, never silent"
        );
    }

    #[test]
    fn expiring_a_running_job_releases_the_slot_it_was_holding() {
        // Dropping the job but leaving the node's slot marked busy would swap
        // one wrong number for another.
        let mut c = cluster_with_two_nodes();
        c.job_timeout = Some(Duration::from_millis(2));
        c.apply(get_cs(7, 1));
        c.apply(job_begin(7, 2));
        assert_eq!(c.nodes[&2].current_jobs(), 1);

        pass(Duration::from_millis(2));
        c.tick_history();

        assert_eq!(c.nodes[&2].current_jobs(), 0);
        assert_eq!(c.summary().used_slots, 0);
    }

    #[test]
    fn a_job_within_its_timeout_is_left_alone() {
        let mut c = cluster_with_two_nodes();
        c.job_timeout = Some(Duration::from_secs(3600));
        c.apply(get_cs(7, 1));
        c.tick_history();
        assert_eq!(c.summary().pending_jobs, 1);
        assert_eq!(c.totals.expired_jobs, 0);
    }

    #[test]
    fn job_expiry_can_be_switched_off() {
        let mut c = cluster_with_two_nodes();
        c.job_timeout = None;
        c.apply(get_cs(7, 1));
        pass(Duration::from_millis(2));
        c.tick_history();
        assert_eq!(c.summary().pending_jobs, 1);
        assert_eq!(c.totals.expired_jobs, 0);
    }

    #[test]
    fn the_queue_graph_records_the_corrected_figure_not_the_stale_one() {
        // expire() runs before the sample is taken, so the tick that drops a
        // job does not also record it as still queued.
        let mut c = cluster_with_two_nodes();
        c.job_timeout = Some(Duration::from_millis(2));
        c.apply(get_cs(7, 1));
        pass(Duration::from_millis(2));
        c.tick_history();
        assert_eq!(c.pending_history.last(), Some(0.0));
    }

    #[test]
    fn a_job_scheduled_back_onto_its_submitter_counts_in_both_directions() {
        // OUT is "submitted from here", not "compiled elsewhere": the scheduler
        // may well place a job on the machine that asked for it, and the label
        // used to claim otherwise.
        let mut c = cluster_with_two_nodes();
        c.apply(get_cs(7, 1));
        c.apply(job_begin(7, 1)); // submitted by node 1, compiled on node 1

        assert_eq!(c.nodes[&1].jobs_out, 1);
        assert_eq!(c.nodes[&1].jobs_in, 1);
        assert_eq!(c.nodes[&2].jobs_out, 0);
    }

    #[test]
    fn every_submitted_job_is_counted_against_exactly_one_compiler() {
        // The property that makes the IN and OUT columns readable together: a
        // cluster's INs sum to its OUTs, so a mismatch means an attribution was
        // lost rather than that a node is idle.
        let mut c = cluster_with_two_nodes();
        for job in 0..20u32 {
            c.apply(get_cs(job, 1));
            c.apply(job_begin(job, if job % 3 == 0 { 1 } else { 2 }));
        }
        let total_in: u64 = c.nodes.values().map(|n| n.jobs_in).sum();
        let total_out: u64 = c.nodes.values().map(|n| n.jobs_out).sum();
        assert_eq!(total_in, 20);
        assert_eq!(total_in, total_out);
    }

    #[test]
    fn a_job_event_never_conjures_a_node() {
        // Job events name host ids, and a MON_JOB_DONE can arrive after its
        // node has left. Creating a node from one produced a row with no name,
        // no address and no slot count — a "?" sitting above the real cluster.
        let mut c = cluster_with_two_nodes();
        c.apply(get_cs(7, 1));
        c.apply(job_begin(7, 1));
        c.apply(stats(1, "State:Offline\n")); // the compiler leaves mid-job

        // Its completion still arrives.
        c.apply(Update::Event(Event::JobDone(JobDone {
            job_id: 7,
            ..Default::default()
        })));

        assert_eq!(c.nodes.len(), 1, "{:?}", c.nodes.keys().collect::<Vec<_>>());
        assert!(
            c.nodes.values().all(|n| n.name() != "?"),
            "a nameless node was invented"
        );
    }

    #[test]
    fn a_job_for_an_unknown_host_is_ignored_rather_than_invented() {
        let mut c = cluster_with_two_nodes();
        c.apply(get_cs(7, 99)); // a client we have never been told about
        c.apply(job_begin(7, 98)); // and a compiler we have never been told about
        assert_eq!(c.nodes.len(), 2);
        assert!(c.nodes.values().all(|n| n.name() != "?"));
    }

    // -------------------------------------------------- nodes leaving

    #[test]
    fn a_node_that_goes_offline_leaves_the_list() {
        // Host ids are per connection, so a laptop that sleeps and wakes would
        // otherwise leave a corpse behind every cycle until the screen was all
        // remains. It comes back by itself when its daemon reattaches.
        let mut c = cluster_with_two_nodes();
        c.apply(stats(1, "State:Offline\n"));

        assert!(!c.nodes.contains_key(&1));
        assert_eq!(c.nodes.len(), 1);
        let s = c.summary();
        assert_eq!(s.nodes_online, 1);
        assert_eq!(s.total_slots, 4, "its slots go with it");
    }

    #[test]
    fn a_departure_releases_the_jobs_it_was_holding() {
        // The scheduler has already given up on them; leaving them in the queue
        // would inflate the depth for the rest of the session.
        let mut c = cluster_with_two_nodes();
        c.apply(get_cs(7, 2));
        c.apply(job_begin(7, 1));
        assert_eq!(c.summary().active_jobs, 1);

        c.apply(stats(1, "State:Offline\n"));
        assert_eq!(c.summary().active_jobs, 0);
        assert!(c.jobs.is_empty());
    }

    #[test]
    fn a_departure_is_counted_even_though_the_row_is_gone() {
        // Removing the row removes the only trace that the node was ever here,
        // so the count is what is left of it.
        let mut c = cluster_with_two_nodes();
        c.apply(stats(1, "State:Offline\n"));
        c.apply(stats(2, "State:Offline\n"));
        assert_eq!(c.totals.nodes_left, 2);
    }

    #[test]
    fn a_node_reappears_when_its_daemon_comes_back() {
        // Under a new host id, because the scheduler issues one per connection.
        let mut c = cluster_with_two_nodes();
        c.apply(stats(1, "State:Offline\n"));
        c.apply(stats(9, &node_blob("build01", 8)));

        assert_eq!(c.nodes.len(), 2);
        assert_eq!(c.nodes[&9].name(), "build01");
        assert!(!c.nodes.contains_key(&1), "no duplicate left behind");
    }

    #[test]
    fn an_offline_record_for_a_node_we_never_saw_is_harmless() {
        let mut c = cluster_with_two_nodes();
        c.apply(stats(99, "State:Offline\n"));
        assert_eq!(c.nodes.len(), 2);
        assert_eq!(c.totals.nodes_left, 0, "nothing left, so nothing to count");
    }

    // -------------------------------------------------- scheduler identity

    #[test]
    fn reconnecting_to_the_same_scheduler_is_not_a_move() {
        let mut c = cluster_with_two_nodes();
        c.apply(Update::Disconnected {
            reason: "reset".into(),
            attempt: 1,
            retry_at: Instant::now(),
        });
        c.apply(connected());
        assert_eq!(c.moved_from(), None);
    }

    #[test]
    fn landing_on_a_different_scheduler_is_reported() {
        // Broadcast discovery can find a different scheduler after the first
        // dies. Every host id, node and counter then belongs somewhere else,
        // and the figures must not change identity behind the user's back.
        let mut c = cluster_with_two_nodes();
        c.apply(Update::Connected {
            target: SchedulerTarget {
                host: "other-sched".into(),
                port: 8765,
            },
            protocol: 43,
            netname: Some("ICECREAM".into()),
        });
        let moved = c.moved_from().expect("a move must be visible");
        assert_eq!(moved.host, "sched");
    }

    #[test]
    fn a_move_is_judged_against_the_first_scheduler_not_the_previous_one() {
        // Otherwise bouncing A → B → A would still claim to have moved.
        let mut c = cluster_with_two_nodes();
        c.apply(Update::Connected {
            target: SchedulerTarget {
                host: "other-sched".into(),
                port: 8765,
            },
            protocol: 43,
            netname: Some("ICECREAM".into()),
        });
        c.apply(connected());
        assert_eq!(c.moved_from(), None, "back where it started is not a move");
    }

    #[test]
    fn a_move_is_not_claimed_while_disconnected() {
        let mut c = cluster_with_two_nodes();
        c.apply(Update::Disconnected {
            reason: "reset".into(),
            attempt: 1,
            retry_at: Instant::now(),
        });
        assert_eq!(c.moved_from(), None);
    }

    // -------------------------------------------------- bounded state

    #[test]
    fn a_long_session_of_churn_does_not_accumulate_state() {
        // The scale check that matters for a process meant to run for days:
        // state must track what is happening now, not everything that ever did.
        let mut c = cluster_with_two_nodes();
        for job_id in 0..20_000u32 {
            c.apply(get_cs(job_id, 1));
            c.apply(job_begin(job_id, 2));
            c.apply(Update::Event(Event::JobDone(JobDone {
                job_id,
                exit_code: 0,
                ..Default::default()
            })));
        }
        assert!(c.jobs.is_empty(), "{} jobs left behind", c.jobs.len());
        assert_eq!(c.nodes[&2].active_jobs.len(), 0);
        assert_eq!(c.totals.completed_remote, 20_000);
    }

    #[test]
    fn login_replay_builds_the_node_list() {
        let c = cluster_with_two_nodes();
        assert_eq!(c.nodes.len(), 2);
        assert_eq!(c.nodes[&1].name(), "build01");
        assert_eq!(c.nodes[&1].max_jobs(), 8);
        let s = c.summary();
        assert_eq!(s.nodes_online, 2);
        assert_eq!(s.total_slots, 12);
        assert_eq!(s.used_slots, 0);
    }

    #[test]
    fn partial_updates_merge_instead_of_replacing() {
        let mut c = cluster_with_two_nodes();
        // A stats update carrying resource fields must not blank the identity,
        // and a later identity-only record must not blank the resources.
        c.apply(stats(1, "Load:839\nLoadAvg1:2197\nFreeMem:36732\n"));
        assert_eq!(c.nodes[&1].name(), "build01");
        assert_eq!(c.nodes[&1].stats.free_mem_mib, Some(36732));

        c.apply(stats(1, &node_blob("build01", 8)));
        assert_eq!(c.nodes[&1].stats.free_mem_mib, Some(36732));
        assert_eq!(c.nodes[&1].stats.load_avg_1, Some(2.197));
    }

    #[test]
    fn a_remote_job_flows_pending_to_active_to_done() {
        let mut c = cluster_with_two_nodes();

        c.apply(Update::Event(Event::GetCs {
            job_id: 100,
            client_id: 2,
            filename: "widget.cpp".into(),
            lang: 1,
        }));
        assert_eq!(c.summary().pending_jobs, 1);
        assert_eq!(c.summary().used_slots, 0);

        c.apply(Update::Event(Event::JobBegin {
            job_id: 100,
            start_time: 0,
            host_id: 1,
        }));
        let s = c.summary();
        assert_eq!(s.pending_jobs, 0);
        assert_eq!(s.active_jobs, 1);
        assert_eq!(s.used_slots, 1);
        // IN counts against the compiling node, OUT against the submitter.
        assert_eq!(c.nodes[&1].jobs_in, 1);
        assert_eq!(c.nodes[&2].jobs_out, 1);
        assert_eq!(c.nodes[&1].current_jobs(), 1);

        c.apply(Update::Event(Event::JobDone(JobDone {
            job_id: 100,
            ..Default::default()
        })));
        let s = c.summary();
        assert_eq!(s.active_jobs, 0);
        assert_eq!(s.used_slots, 0);
        assert_eq!(c.totals.completed_remote, 1);
        assert_eq!(c.totals.failed, 0);
    }

    #[test]
    fn a_failed_job_is_counted_as_failed() {
        let mut c = cluster_with_two_nodes();
        c.apply(Update::Event(Event::JobBegin {
            job_id: 5,
            start_time: 0,
            host_id: 1,
        }));
        c.apply(Update::Event(Event::JobDone(JobDone {
            job_id: 5,
            exit_code: 1,
            ..Default::default()
        })));
        assert_eq!(c.totals.completed_remote, 1);
        assert_eq!(c.totals.failed, 1);
    }

    #[test]
    fn local_jobs_are_tracked_separately_from_slots() {
        let mut c = cluster_with_two_nodes();
        c.apply(Update::Event(Event::LocalJobBegin {
            job_id: 7,
            start_time: 0,
            host_id: 1,
            file: "main.cpp".into(),
        }));
        let s = c.summary();
        assert_eq!(s.local_jobs, 1);
        // A local compile occupies no scheduler slot.
        assert_eq!(s.used_slots, 0);
        assert_eq!(c.nodes[&1].jobs_local, 1);

        c.apply(Update::Event(Event::LocalJobDone { job_id: 7 }));
        assert_eq!(c.summary().local_jobs, 0);
        assert_eq!(c.totals.completed_local, 1);
    }

    #[test]
    fn a_job_that_began_before_we_connected_still_completes() {
        let mut c = cluster_with_two_nodes();
        // No GetCs and no JobBegin: this is the mid-flight connect case.
        c.apply(Update::Event(Event::JobDone(JobDone {
            job_id: 999,
            ..Default::default()
        })));
        assert_eq!(c.totals.unmatched_done, 1);
        assert_eq!(c.totals.completed_remote, 0);
    }

    #[test]
    fn job_begin_without_a_prior_get_cs_still_occupies_a_slot() {
        let mut c = cluster_with_two_nodes();
        c.apply(Update::Event(Event::JobBegin {
            job_id: 42,
            start_time: 0,
            host_id: 1,
        }));
        assert_eq!(c.summary().used_slots, 1);
        assert_eq!(c.nodes[&1].jobs_in, 1);
        // Submitter unknown, so nobody's OUT counter moved.
        assert_eq!(c.nodes[&2].jobs_out, 0);
    }

    #[test]
    fn no_remote_nodes_contribute_no_slots() {
        let mut c = Cluster::new();
        c.apply(connected());
        c.apply(stats(1, "Name:laptop\nMaxJobs:12\nNoRemote:true\n"));
        let s = c.summary();
        assert_eq!(s.nodes_online, 1);
        assert_eq!(s.nodes_available, 0);
        assert_eq!(s.total_slots, 0);
    }

    #[test]
    fn reconnect_resets_derived_counters() {
        let mut c = cluster_with_two_nodes();
        c.apply(Update::Event(Event::JobBegin {
            job_id: 1,
            start_time: 0,
            host_id: 1,
        }));
        c.apply(Update::Event(Event::JobDone(JobDone {
            job_id: 1,
            ..Default::default()
        })));
        assert_eq!(c.totals.completed_remote, 1);

        c.apply(Update::Disconnected {
            reason: "test".into(),
            attempt: 1,
            retry_at: Instant::now(),
        });
        assert!(!c.is_connected());
        // Nodes stay visible while disconnected, but their work does not.
        assert_eq!(c.nodes.len(), 2);
        assert_eq!(c.summary().active_jobs, 0);

        c.apply(connected());
        assert_eq!(c.totals.completed_remote, 0, "counters are since-connect");
        assert!(c.nodes.is_empty(), "scheduler replays the node list");
    }

    #[test]
    fn speed_of_zero_reads_as_unknown_not_slow() {
        let mut c = Cluster::new();
        c.apply(connected());
        c.apply(stats(1, "Name:fresh\nMaxJobs:8\nSpeed:0.000000\n"));
        assert_eq!(c.nodes[&1].speed(), None);
        c.apply(stats(1, "Speed:3200.5\n"));
        assert_eq!(c.nodes[&1].speed(), Some(3200.5));
    }

    #[test]
    fn negative_max_jobs_marks_the_node_suspect_without_hiding_capacity() {
        let mut c = Cluster::new();
        c.apply(connected());
        c.apply(stats(1, "Name:build01\nMaxJobs:-8\nNoRemote:false\n"));
        assert!(c.nodes[&1].suspect());
        assert_eq!(c.nodes[&1].max_jobs(), 8);
    }

    #[test]
    fn unknown_message_types_are_counted_not_dropped_silently() {
        let mut c = Cluster::new();
        c.apply(Update::Event(Event::Other { ty: 200 }));
        c.apply(Update::Event(Event::Other { ty: 200 }));
        assert_eq!(c.unknown_messages[&200], 2);
    }

    #[test]
    fn slot_usage_is_none_for_an_empty_cluster() {
        assert_eq!(Cluster::new().summary().slot_usage(), None);
    }

    #[test]
    fn node_order_is_stable_by_name() {
        let mut c = Cluster::new();
        c.apply(connected());
        c.apply(stats(3, &node_blob("build03", 4)));
        c.apply(stats(1, &node_blob("build01", 4)));
        c.apply(stats(2, &node_blob("build02", 4)));
        let names: Vec<&str> = c.nodes_sorted().iter().map(|n| n.name()).collect();
        assert_eq!(names, ["build01", "build02", "build03"]);
    }

    // ---- Phase 3: agent metrics ----

    fn snapshot(hostname: &str, cpu_pct: f32, mem_used_kib: u64) -> Box<Snapshot> {
        Box::new(Snapshot {
            schema: icecc_metrics::SCHEMA_VERSION,
            agent_version: "0.1.0".into(),
            hostname: hostname.into(),
            addresses: vec!["10.0.0.1".into()],
            uptime_secs: 100,
            sampled_unix_ms: 1,
            sample_interval_ms: 1000,
            cpu: icecc_metrics::Cpu {
                cores: 8,
                total_busy_pct: cpu_pct,
                per_core_busy_pct: vec![cpu_pct; 8],
                freq_mhz: vec![3200; 8],
            },
            mem: icecc_metrics::Mem {
                total_kib: 1000,
                available_kib: 1000 - mem_used_kib,
                free_kib: 1000 - mem_used_kib,
                buffers_kib: 0,
                cached_kib: 0,
                swap_total_kib: 0,
                swap_free_kib: 0,
            },
            load: icecc_metrics::Load {
                one: 4.0,
                five: 3.0,
                fifteen: 2.0,
                runnable: 4,
                total_procs: 500,
            },
            thermal: icecc_metrics::Thermal {
                cpu_celsius: Some(71.0),
                cpu_source: Some("coretemp/Package id 0".into()),
                sensors: vec![],
            },
            net: icecc_metrics::Net {
                rx_bytes_per_sec: 1_200_000,
                tx_bytes_per_sec: 4_800_000,
                interfaces: vec![],
            },
        })
    }

    #[test]
    fn a_good_poll_populates_the_metrics_the_scheduler_cannot_provide() {
        let mut c = cluster_with_two_nodes();
        // The scheduler gave us no CPU or memory percentage, by construction.
        assert_eq!(c.nodes[&1].cpu_pct(), None);
        assert_eq!(c.nodes[&1].mem_pct(), None);

        c.apply_resource(1, ResourceResult::Ok(snapshot("build01", 82.0, 700)));

        let node = &c.nodes[&1];
        assert_eq!(node.cpu_pct(), Some(82.0));
        assert_eq!(node.mem_pct(), Some(70.0));
        assert_eq!(node.temp_c(), Some(71.0));
        assert_eq!(node.cores(), Some(8));
        assert_eq!(node.load_avg_1(), Some(4.0));
        assert_eq!(node.load_per_core(), Some(0.5));
        assert_eq!(node.resource_state, ResourceState::Ok);
        assert!(node.has_agent());
        assert!(!node.identity_mismatch);
    }

    #[test]
    fn a_node_without_an_agent_is_a_deployment_gap_not_an_error() {
        let mut c = cluster_with_two_nodes();
        c.apply_resource(1, ResourceResult::Unreachable("Connection refused".into()));

        let node = &c.nodes[&1];
        assert!(!node.has_agent());
        assert_eq!(node.cpu_pct(), None);
        assert_eq!(node.resource_state.label(), "no agent");
        assert_eq!(node.resource_state.reason(), Some("Connection refused"));

        let s = c.summary();
        assert_eq!(s.nodes_without_agent, 2, "neither node has answered");
        assert_eq!(s.nodes_with_metrics, 0);
    }

    #[test]
    fn an_unusable_agent_is_distinguished_from_a_missing_one() {
        let mut c = cluster_with_two_nodes();
        c.apply_resource(1, ResourceResult::Bad("agent returned HTTP 404".into()));
        assert_eq!(c.nodes[&1].resource_state.label(), "error");
        assert_eq!(
            c.nodes[&1].resource_state.reason(),
            Some("agent returned HTTP 404")
        );
    }

    #[test]
    fn a_failed_poll_keeps_the_last_known_values() {
        let mut c = cluster_with_two_nodes();
        c.apply_resource(1, ResourceResult::Ok(snapshot("build01", 82.0, 700)));
        c.apply_resource(1, ResourceResult::Unreachable("timed out".into()));

        // Last known reading survives so the row does not go blank; the state
        // field is what tells the user it is no longer current.
        assert_eq!(c.nodes[&1].cpu_pct(), Some(82.0));
        assert_eq!(c.nodes[&1].resource_state.label(), "no agent");
    }

    #[test]
    fn metrics_go_stale_on_the_monitors_clock() {
        let mut c = cluster_with_two_nodes();
        c.metrics_stale_after = Duration::from_millis(1);
        c.apply_resource(1, ResourceResult::Ok(snapshot("build01", 82.0, 700)));
        assert!(c.nodes[&1].fresh_metrics(c.metrics_stale_after).is_some());

        std::thread::sleep(Duration::from_millis(5));
        assert!(c.nodes[&1].metrics_stale(c.metrics_stale_after));
        assert!(c.nodes[&1].fresh_metrics(c.metrics_stale_after).is_none());
        // But the values are still there to display as last-known.
        assert_eq!(c.nodes[&1].cpu_pct(), Some(82.0));

        let s = c.summary();
        assert_eq!(s.nodes_metrics_stale, 1);
        assert_eq!(s.nodes_with_metrics, 0);
        assert_eq!(s.nodes_without_agent, 1, "the other node never answered");
    }

    #[test]
    fn an_agent_answering_for_the_wrong_host_is_flagged() {
        let mut c = cluster_with_two_nodes();
        c.apply_resource(1, ResourceResult::Ok(snapshot("someone-else", 50.0, 500)));
        assert!(
            c.nodes[&1].identity_mismatch,
            "reaching the wrong machine must be visible, not silently graphed"
        );
    }

    #[test]
    fn a_domain_suffix_is_not_a_mismatch() {
        assert!(hostnames_agree("build01", "build01.corp.example"));
        assert!(hostnames_agree("build01.corp.example", "build01"));
        assert!(hostnames_agree("BUILD01", "build01"));
        assert!(!hostnames_agree("build01", "build02"));
        // An unknown scheduler name is not evidence of anything.
        assert!(hostnames_agree("?", "build01"));
        assert!(hostnames_agree("", "build01"));
    }

    #[test]
    fn polls_go_to_the_scheduler_reported_address_skipping_offline_nodes() {
        let mut c = Cluster::new();
        c.apply(connected());
        c.apply(stats(1, "Name:build01\nIP:10.0.0.11\nMaxJobs:8\n"));
        c.apply(stats(2, "Name:build02\nIP:10.0.0.12\nMaxJobs:8\n"));
        c.apply(stats(3, "Name:build03\nMaxJobs:8\n")); // no IP yet

        let mut targets = c.poll_targets();
        targets.sort();
        assert_eq!(
            targets,
            vec![(1, "10.0.0.11".to_owned()), (2, "10.0.0.12".to_owned())]
        );

        c.apply(stats(2, "State:Offline\n"));
        assert_eq!(c.poll_targets(), vec![(1, "10.0.0.11".to_owned())]);
    }

    #[test]
    fn metrics_for_an_unknown_host_id_are_ignored() {
        let mut c = cluster_with_two_nodes();
        c.apply_resource(999, ResourceResult::Ok(snapshot("ghost", 50.0, 500)));
        assert!(!c.nodes.contains_key(&999));
    }

    #[test]
    fn reconnect_drops_metrics_because_host_ids_are_reassigned() {
        let mut c = cluster_with_two_nodes();
        c.apply_resource(1, ResourceResult::Ok(snapshot("build01", 82.0, 700)));
        c.apply(connected());
        assert!(c.nodes.is_empty());
    }

    #[test]
    fn a_departed_node_is_not_counted_in_agent_coverage() {
        // Agent coverage is a rollout question — "how many of the machines
        // that are here are reporting" — so a machine that has left is not a
        // gap in it.
        let mut c = cluster_with_two_nodes();
        c.apply_resource(1, ResourceResult::Ok(snapshot("build01", 82.0, 700)));
        c.apply(stats(2, "State:Offline\n"));

        let s = c.summary();
        assert_eq!(s.nodes_online, 1);
        assert_eq!(s.nodes_with_metrics, 1);
        assert_eq!(s.nodes_without_agent, 0);
    }
}
