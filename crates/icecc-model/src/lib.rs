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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use icecc_proto::msg::JobDone;
use icecc_proto::{Event, SchedulerTarget, StatsRecord, Update};

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
    /// Jobs this node submitted that were compiled elsewhere, since connect.
    pub jobs_out: u64,
    /// Jobs this node compiled locally, since connect.
    pub jobs_local: u64,
    /// When we last heard anything about this node.
    pub last_update: Instant,
    /// Set by a `State:Offline` record or a scheduler-side removal.
    pub offline: bool,
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
            offline: false,
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

    /// Nothing heard for longer than `limit`. Because stats are change-driven,
    /// a quiet node is normal — this is only meaningful once a node-side
    /// collector exists, so keep the limit generous.
    pub fn is_stale(&self, limit: Duration) -> bool {
        !self.offline && self.last_update.elapsed() > limit
    }
}

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
        since: Instant,
    },
    Disconnected {
        reason: String,
    },
}

/// Counters over completed work, since connect.
#[derive(Debug, Clone, Default)]
pub struct Totals {
    pub completed_remote: u64,
    pub completed_local: u64,
    pub failed: u64,
    /// `MON_JOB_DONE` for a job we never saw begin — normal right after connect.
    pub unmatched_done: u64,
}

/// One-glance cluster figures, computed on demand.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub nodes_online: usize,
    pub nodes_offline: usize,
    /// Online nodes that accept remote jobs.
    pub nodes_available: usize,
    pub total_slots: u32,
    pub used_slots: u32,
    pub pending_jobs: usize,
    pub active_jobs: usize,
    pub local_jobs: usize,
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
        }
    }

    /// Drop all derived state. Called on connect, because the scheduler replays
    /// the node list and we cannot reconcile old job ids against a new session.
    pub fn reset(&mut self) {
        self.nodes.clear();
        self.jobs.clear();
        self.totals = Totals::default();
        self.counters_since = Instant::now();
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
            Update::Connected { target, protocol } => {
                self.reset();
                self.connection = ConnectionState::Connected {
                    target,
                    protocol,
                    since: Instant::now(),
                };
            }
            Update::Disconnected { reason } => {
                self.connection = ConnectionState::Disconnected { reason };
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

                let node = self.node_mut(host_id);
                node.active_jobs.insert(job_id);
                node.jobs_in += 1;

                if let Some(client_id) = client_id {
                    self.node_mut(client_id).jobs_out += 1;
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
                let node = self.node_mut(host_id);
                node.local_jobs.insert(job_id);
                node.jobs_local += 1;
            }

            Event::JobDone(done) => self.finish_remote(done),

            Event::LocalJobDone { job_id } => match self.jobs.remove(&job_id) {
                Some(job) => {
                    if let Some(host_id) = job.host_id {
                        self.node_mut(host_id).local_jobs.remove(&job_id);
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
        let node = self.node_mut(host_id);
        record.merge_into(&mut node.stats);
        node.last_update = Instant::now();

        if record.offline {
            // Keep the row: "which node just died" is exactly what the user
            // wants to see. Its jobs are gone, though.
            node.offline = true;
            let dead: Vec<u32> = node
                .active_jobs
                .iter()
                .chain(node.local_jobs.iter())
                .copied()
                .collect();
            node.active_jobs.clear();
            node.local_jobs.clear();
            for id in dead {
                self.jobs.remove(&id);
            }
        } else {
            // A node can come back with the same host id after a restart.
            node.offline = false;
        }
    }

    fn finish_remote(&mut self, done: JobDone) {
        match self.jobs.remove(&done.job_id) {
            Some(job) => {
                if let Some(host_id) = job.host_id {
                    let node = self.node_mut(host_id);
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
            }
            None => {
                // Either the job started before we connected, or the scheduler
                // had already forgotten it. Both are expected, not errors.
                self.totals.unmatched_done += 1;
            }
        }
    }

    fn node_mut(&mut self, host_id: u32) -> &mut Node {
        self.nodes
            .entry(host_id)
            .or_insert_with(|| Node::new(host_id))
    }

    pub fn summary(&self) -> Summary {
        let mut s = Summary::default();
        for node in self.nodes.values() {
            if node.offline {
                s.nodes_offline += 1;
                continue;
            }
            s.nodes_online += 1;
            if node.accepts_remote() {
                s.nodes_available += 1;
                s.total_slots += node.max_jobs();
            }
            s.used_slots += node.current_jobs();
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

    /// Nodes in a stable display order: name, then host id as a tiebreak so
    /// unnamed nodes do not jitter between frames.
    pub fn nodes_sorted(&self) -> Vec<&Node> {
        let mut v: Vec<&Node> = self.nodes.values().collect();
        v.sort_by(|a, b| a.name().cmp(b.name()).then(a.host_id.cmp(&b.host_id)));
        v
    }
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
    fn offline_keeps_the_row_but_releases_the_jobs() {
        let mut c = cluster_with_two_nodes();
        c.apply(Update::Event(Event::JobBegin {
            job_id: 1,
            start_time: 0,
            host_id: 1,
        }));
        c.apply(stats(1, "State:Offline\n"));

        let node = &c.nodes[&1];
        assert!(node.offline);
        assert_eq!(node.name(), "build01", "last known identity must survive");
        assert_eq!(node.current_jobs(), 0);

        let s = c.summary();
        assert_eq!(s.nodes_online, 1);
        assert_eq!(s.nodes_offline, 1);
        assert_eq!(s.total_slots, 4, "an offline node offers no slots");
        assert_eq!(s.active_jobs, 0);
    }

    #[test]
    fn a_node_can_come_back_after_going_offline() {
        let mut c = cluster_with_two_nodes();
        c.apply(stats(1, "State:Offline\n"));
        assert!(c.nodes[&1].offline);
        c.apply(stats(1, &node_blob("build01", 8)));
        assert!(!c.nodes[&1].offline);
        assert_eq!(c.summary().total_slots, 12);
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
}
