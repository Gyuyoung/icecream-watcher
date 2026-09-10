//! Application state: sorting, selection, and key handling.

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use icecc_model::{Cluster, Node, ResourceResult};
use icecc_proto::Update;

/// How often the screen may repaint. Events can burst — a login replay of 100
/// nodes arrives at once — so redraws are coalesced rather than done per event.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(100);

/// How often history series advance. One sample per second, matching the
/// agent's own sampling rate.
pub const HISTORY_INTERVAL: Duration = Duration::from_secs(1);

/// What the node table is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Cpu,
    Mem,
    Load,
    /// Icecream compile slots in use.
    Jobs,
    Speed,
    Temp,
}

impl SortKey {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Cpu => "cpu",
            Self::Mem => "mem",
            Self::Load => "load",
            Self::Jobs => "jobs",
            Self::Speed => "speed",
            Self::Temp => "temp",
        }
    }

    /// Order for the `s` key to cycle through.
    pub fn next(&self) -> Self {
        match self {
            Self::Name => Self::Cpu,
            Self::Cpu => Self::Mem,
            Self::Mem => Self::Load,
            Self::Load => Self::Jobs,
            Self::Jobs => Self::Speed,
            Self::Speed => Self::Temp,
            Self::Temp => Self::Name,
        }
    }

    /// Name sorts A→Z; every metric sorts busiest first, because the reason to
    /// sort by CPU is to see what is hot, not what is idle.
    fn descending(&self) -> bool {
        !matches!(self, Self::Name)
    }
}

pub struct App {
    pub cluster: Cluster,
    pub sort: SortKey,
    /// Selected node, tracked by host id so it survives a re-sort.
    pub selected: Option<u32>,
    pub show_help: bool,
    /// Set when state changed since the last paint.
    dirty: bool,
    pub should_quit: bool,
    /// Events applied since start, shown as a liveness hint — a quiet cluster
    /// legitimately produces none.
    pub events_seen: u64,
    pub last_event: Option<Instant>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            cluster: Cluster::new(),
            sort: SortKey::Name,
            selected: None,
            show_help: false,
            dirty: true,
            should_quit: false,
            events_seen: 0,
            last_event: None,
        }
    }

    pub fn apply(&mut self, update: Update) {
        if matches!(update, Update::Event(_)) {
            self.events_seen += 1;
            self.last_event = Some(Instant::now());
        }
        self.cluster.apply(update);
        self.prune_selection();
        self.dirty = true;
    }

    pub fn apply_resource(&mut self, host_id: u32, result: ResourceResult) {
        self.cluster.apply_resource(host_id, result);
        self.dirty = true;
    }

    /// Advance every history series. Driven by a timer, not by events.
    pub fn tick_history(&mut self) {
        self.cluster.tick_history();
        self.dirty = true;
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Nodes in display order: online first, then offline, each by the current
    /// sort key. Offline nodes sink to the bottom because their metrics are not
    /// comparable with a live node's.
    pub fn sorted_nodes(&self) -> Vec<&Node> {
        let mut nodes: Vec<&Node> = self.cluster.nodes.values().collect();
        let key = self.sort;
        nodes.sort_by(|a, b| {
            a.offline
                .cmp(&b.offline)
                .then_with(|| compare(a, b, key))
                .then_with(|| a.name().cmp(b.name()))
                .then_with(|| a.host_id.cmp(&b.host_id))
        });
        nodes
    }

    /// Index of the selected node within [`Self::sorted_nodes`].
    pub fn selected_index(&self) -> Option<usize> {
        let selected = self.selected?;
        self.sorted_nodes()
            .iter()
            .position(|n| n.host_id == selected)
    }

    pub fn set_sort(&mut self, key: SortKey) {
        self.sort = key;
        self.dirty = true;
    }

    /// Move the selection by `delta` rows, clamped to the ends.
    ///
    /// Clamped rather than wrapping: holding a key at the bottom of a
    /// hundred-node list should stop, not jump back to the top.
    pub fn move_selection(&mut self, delta: isize) {
        let nodes = self.sorted_nodes();
        if nodes.is_empty() {
            self.selected = None;
            return;
        }
        let current = self.selected_index().map(|i| i as isize);
        let next = match current {
            Some(i) => (i + delta).clamp(0, nodes.len() as isize - 1),
            // First keypress selects an end, so both arrows do something useful
            // from a fresh screen.
            None if delta < 0 => nodes.len() as isize - 1,
            None => 0,
        };
        self.selected = Some(nodes[next as usize].host_id);
        self.dirty = true;
    }

    /// Drop a selection whose node has gone away, so the highlight cannot point
    /// at nothing.
    fn prune_selection(&mut self) {
        if let Some(id) = self.selected {
            if !self.cluster.nodes.contains_key(&id) {
                self.selected = None;
            }
        }
    }

    pub fn on_key(&mut self, key: Key) {
        match key {
            Key::Quit => self.should_quit = true,
            Key::Back => {
                // Esc backs out of the help overlay first; only quits when
                // there is nothing to back out of.
                if self.show_help {
                    self.show_help = false;
                    self.dirty = true;
                } else {
                    self.should_quit = true;
                }
            }
            Key::Refresh => self.mark_dirty(),
            Key::Up => self.move_selection(-1),
            Key::Down => self.move_selection(1),
            Key::PageUp => self.move_selection(-10),
            Key::PageDown => self.move_selection(10),
            Key::Sort(key) => self.set_sort(key),
            Key::CycleSort => self.set_sort(self.sort.next()),
            Key::ToggleHelp => {
                self.show_help = !self.show_help;
                self.dirty = true;
            }
            Key::Enter | Key::Ignored => {}
        }
    }
}

/// Compare two nodes by one metric. A node with no measurement always sorts
/// after one that has a value, whichever direction we are sorting.
fn compare(a: &Node, b: &Node, key: SortKey) -> Ordering {
    let ordering = match key {
        SortKey::Name => return a.name().cmp(b.name()),
        SortKey::Cpu => opt_cmp(a.cpu_pct(), b.cpu_pct()),
        SortKey::Mem => opt_cmp(a.mem_pct(), b.mem_pct()),
        SortKey::Load => opt_cmp(a.load_avg_1(), b.load_avg_1()),
        SortKey::Jobs => opt_cmp(a.slot_pct(), b.slot_pct()),
        SortKey::Speed => opt_cmp(a.speed(), b.speed()),
        SortKey::Temp => opt_cmp(a.temp_c(), b.temp_c()),
    };
    match ordering {
        // Unmeasured nodes stay at the bottom instead of being flipped to the
        // top by the reversal.
        Missing::Unknown(o) => o,
        Missing::Known(o) if key.descending() => o.reverse(),
        Missing::Known(o) => o,
    }
}

/// Result of comparing two optional values, remembering whether the comparison
/// was between two real values or involved a missing one.
enum Missing {
    Known(Ordering),
    Unknown(Ordering),
}

fn opt_cmp<T: PartialOrd>(a: Option<T>, b: Option<T>) -> Missing {
    match (a, b) {
        (Some(a), Some(b)) => Missing::Known(a.partial_cmp(&b).unwrap_or(Ordering::Equal)),
        (Some(_), None) => Missing::Unknown(Ordering::Less),
        (None, Some(_)) => Missing::Unknown(Ordering::Greater),
        (None, None) => Missing::Unknown(Ordering::Equal),
    }
}

/// An action, decoupled from the terminal's key encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Quit,
    /// Esc: leave the current overlay, or quit at the top level.
    Back,
    /// `r` — repaint now. It cannot pull from the scheduler: the scheduler
    /// pushes, and there is no request a monitor may send (ARCHITECTURE.md §1.2).
    Refresh,
    Up,
    Down,
    PageUp,
    PageDown,
    Sort(SortKey),
    CycleSort,
    ToggleHelp,
    /// Node detail view — arrives in Phase 5.
    Enter,
    Ignored,
}

pub fn classify(
    code: ratatui::crossterm::event::KeyCode,
    mods: ratatui::crossterm::event::KeyModifiers,
) -> Key {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    match (code, mods) {
        (KeyCode::Char('c') | KeyCode::Char('C'), KeyModifiers::CONTROL) => Key::Quit,
        (KeyCode::Char('q'), _) => Key::Quit,
        (KeyCode::Esc, _) => Key::Back,
        (KeyCode::Up | KeyCode::Char('k'), _) => Key::Up,
        (KeyCode::Down | KeyCode::Char('j'), _) => Key::Down,
        (KeyCode::PageUp, _) => Key::PageUp,
        (KeyCode::PageDown, _) => Key::PageDown,
        (KeyCode::Enter, _) => Key::Enter,
        (KeyCode::Char('r'), _) => Key::Refresh,
        (KeyCode::Char('s'), _) => Key::CycleSort,
        (KeyCode::Char('c'), _) => Key::Sort(SortKey::Cpu),
        (KeyCode::Char('m'), _) => Key::Sort(SortKey::Mem),
        (KeyCode::Char('l'), _) => Key::Sort(SortKey::Load),
        (KeyCode::Char('i'), _) => Key::Sort(SortKey::Jobs),
        (KeyCode::Char('?'), _) => Key::ToggleHelp,
        _ => Key::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use icecc_metrics::Snapshot;
    use icecc_proto::{Event, SchedulerTarget};
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    fn connected() -> Update {
        Update::Connected {
            target: SchedulerTarget {
                host: "sched".into(),
                port: 8765,
            },
            protocol: 43,
        }
    }

    fn stats(host_id: u32, blob: &str) -> Update {
        Update::Event(Event::Stats {
            host_id,
            stats: blob.into(),
        })
    }

    fn snapshot(hostname: &str, cpu: f32, mem_used_kib: u64, temp: f32) -> Box<Snapshot> {
        Box::new(Snapshot {
            schema: icecc_metrics::SCHEMA_VERSION,
            agent_version: "0.1.0".into(),
            hostname: hostname.into(),
            addresses: vec![],
            uptime_secs: 1,
            sampled_unix_ms: 1,
            sample_interval_ms: 1000,
            cpu: icecc_metrics::Cpu {
                cores: 8,
                total_busy_pct: cpu,
                per_core_busy_pct: vec![cpu; 8],
                freq_mhz: vec![],
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
                one: cpu / 10.0,
                five: 1.0,
                fifteen: 1.0,
                runnable: 1,
                total_procs: 10,
            },
            thermal: icecc_metrics::Thermal {
                cpu_celsius: Some(temp),
                cpu_source: Some("coretemp/Package id 0".into()),
                sensors: vec![],
            },
            net: icecc_metrics::Net {
                rx_bytes_per_sec: 0,
                tx_bytes_per_sec: 0,
                interfaces: vec![],
            },
        })
    }

    /// Three nodes with distinct metrics, so every sort key has a clear answer.
    fn app_with_three() -> App {
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(
            1,
            "Name:build01\nIP:10.0.0.1\nMaxJobs:8\nSpeed:3000\n",
        ));
        app.apply(stats(
            2,
            "Name:build02\nIP:10.0.0.2\nMaxJobs:8\nSpeed:1000\n",
        ));
        app.apply(stats(
            3,
            "Name:build03\nIP:10.0.0.3\nMaxJobs:8\nSpeed:2000\n",
        ));
        app.apply_resource(1, ResourceResult::Ok(snapshot("build01", 20.0, 300, 50.0)));
        app.apply_resource(2, ResourceResult::Ok(snapshot("build02", 90.0, 100, 80.0)));
        app.apply_resource(3, ResourceResult::Ok(snapshot("build03", 50.0, 900, 65.0)));
        app
    }

    fn names(app: &App) -> Vec<String> {
        app.sorted_nodes()
            .iter()
            .map(|n| n.name().to_owned())
            .collect()
    }

    #[test]
    fn default_order_is_by_name() {
        let app = app_with_three();
        assert_eq!(names(&app), ["build01", "build02", "build03"]);
    }

    #[test]
    fn metric_sorts_put_the_busiest_first() {
        let mut app = app_with_three();

        app.set_sort(SortKey::Cpu);
        assert_eq!(names(&app), ["build02", "build03", "build01"]);

        app.set_sort(SortKey::Mem);
        assert_eq!(names(&app), ["build03", "build01", "build02"]);

        app.set_sort(SortKey::Temp);
        assert_eq!(names(&app), ["build02", "build03", "build01"]);

        app.set_sort(SortKey::Speed);
        assert_eq!(names(&app), ["build01", "build03", "build02"]);
    }

    #[test]
    fn sorting_by_jobs_uses_slot_occupancy() {
        let mut app = app_with_three();
        app.apply(Update::Event(Event::JobBegin {
            job_id: 1,
            start_time: 0,
            host_id: 3,
        }));
        app.set_sort(SortKey::Jobs);
        assert_eq!(names(&app)[0], "build03");
    }

    #[test]
    fn nodes_without_metrics_sort_last_not_first() {
        let mut app = app_with_three();
        app.apply(stats(4, "Name:build04\nIP:10.0.0.4\nMaxJobs:8\n")); // no agent
        app.set_sort(SortKey::Cpu);
        assert_eq!(
            names(&app).last().unwrap(),
            "build04",
            "an unmeasured node must not top a busiest-first sort"
        );
    }

    #[test]
    fn offline_nodes_sink_below_live_ones() {
        let mut app = app_with_three();
        app.apply(stats(2, "State:Offline\n")); // the busiest node
        app.set_sort(SortKey::Cpu);
        assert_eq!(names(&app).last().unwrap(), "build02");
    }

    #[test]
    fn cycling_sort_visits_every_key_and_returns() {
        let mut app = App::new();
        let mut seen = vec![app.sort];
        for _ in 0..6 {
            app.on_key(Key::CycleSort);
            seen.push(app.sort);
        }
        assert_eq!(seen.len(), 7);
        app.on_key(Key::CycleSort);
        assert_eq!(app.sort, SortKey::Name, "cycle should wrap");
    }

    #[test]
    fn selection_moves_and_clamps_at_both_ends() {
        let mut app = app_with_three();
        assert_eq!(app.selected_index(), None);

        app.on_key(Key::Down);
        assert_eq!(app.selected_index(), Some(0));
        app.on_key(Key::Down);
        assert_eq!(app.selected_index(), Some(1));
        app.on_key(Key::Up);
        assert_eq!(app.selected_index(), Some(0));

        // Clamped, not wrapped.
        app.on_key(Key::Up);
        assert_eq!(app.selected_index(), Some(0));
        app.on_key(Key::PageDown);
        assert_eq!(app.selected_index(), Some(2));
        app.on_key(Key::PageDown);
        assert_eq!(app.selected_index(), Some(2));
    }

    #[test]
    fn up_from_nothing_selects_the_last_row() {
        let mut app = app_with_three();
        app.on_key(Key::Up);
        assert_eq!(app.selected_index(), Some(2));
    }

    #[test]
    fn selection_follows_the_node_through_a_re_sort() {
        let mut app = app_with_three();
        app.on_key(Key::Down); // build01, the least busy
        let picked = app.selected;
        app.set_sort(SortKey::Cpu);
        assert_eq!(app.selected, picked, "selection must track the node");
        assert_eq!(app.selected_index(), Some(2), "which is now last");
    }

    #[test]
    fn selection_is_dropped_when_its_node_disappears() {
        let mut app = app_with_three();
        app.on_key(Key::Down);
        assert!(app.selected.is_some());
        // A reconnect clears the node list.
        app.apply(connected());
        assert_eq!(app.selected, None);
        assert_eq!(app.selected_index(), None);
    }

    #[test]
    fn moving_the_selection_on_an_empty_cluster_is_harmless() {
        let mut app = App::new();
        app.on_key(Key::Down);
        app.on_key(Key::Up);
        assert_eq!(app.selected, None);
    }

    #[test]
    fn esc_closes_help_before_it_quits() {
        let mut app = App::new();
        app.on_key(Key::ToggleHelp);
        assert!(app.show_help);

        app.on_key(Key::Back);
        assert!(!app.show_help);
        assert!(!app.should_quit, "esc should close the overlay first");

        app.on_key(Key::Back);
        assert!(app.should_quit);
    }

    #[test]
    fn key_bindings_match_the_documented_set() {
        let n = KeyModifiers::NONE;
        assert_eq!(classify(KeyCode::Char('q'), n), Key::Quit);
        assert_eq!(
            classify(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Key::Quit
        );
        assert_eq!(classify(KeyCode::Esc, n), Key::Back);
        assert_eq!(classify(KeyCode::Char('k'), n), Key::Up);
        assert_eq!(classify(KeyCode::Up, n), Key::Up);
        assert_eq!(classify(KeyCode::Char('j'), n), Key::Down);
        assert_eq!(classify(KeyCode::Down, n), Key::Down);
        assert_eq!(classify(KeyCode::Enter, n), Key::Enter);
        assert_eq!(classify(KeyCode::Char('r'), n), Key::Refresh);
        assert_eq!(classify(KeyCode::Char('s'), n), Key::CycleSort);
        assert_eq!(classify(KeyCode::Char('c'), n), Key::Sort(SortKey::Cpu));
        assert_eq!(classify(KeyCode::Char('m'), n), Key::Sort(SortKey::Mem));
        assert_eq!(classify(KeyCode::Char('l'), n), Key::Sort(SortKey::Load));
        assert_eq!(classify(KeyCode::Char('i'), n), Key::Sort(SortKey::Jobs));
        assert_eq!(classify(KeyCode::Char('?'), n), Key::ToggleHelp);
        assert_eq!(classify(KeyCode::Char('z'), n), Key::Ignored);
    }

    #[test]
    fn ctrl_c_quits_but_a_bare_c_sorts() {
        // The two must not be confused: one ends the session, one reorders.
        assert_eq!(
            classify(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Key::Quit
        );
        assert_eq!(
            classify(KeyCode::Char('c'), KeyModifiers::NONE),
            Key::Sort(SortKey::Cpu)
        );
    }

    #[test]
    fn first_paint_is_dirty_then_settles() {
        let mut app = App::new();
        assert!(app.take_dirty());
        assert!(!app.take_dirty());
    }

    #[test]
    fn every_input_that_changes_the_view_requests_a_repaint() {
        let mut app = app_with_three();
        for key in [
            Key::Down,
            Key::Up,
            Key::CycleSort,
            Key::Sort(SortKey::Mem),
            Key::ToggleHelp,
            Key::Refresh,
        ] {
            app.take_dirty();
            app.on_key(key);
            assert!(app.take_dirty(), "{key:?} did not request a repaint");
        }
    }

    #[test]
    fn a_history_tick_advances_the_series() {
        let mut app = app_with_three();
        assert!(app.cluster.pending_history.is_empty());
        app.tick_history();
        assert_eq!(app.cluster.pending_history.len(), 1);
        assert_eq!(app.cluster.nodes[&1].cpu_history.last(), Some(20.0));
    }

    #[test]
    fn only_protocol_events_count_toward_liveness() {
        let mut app = App::new();
        app.apply(Update::Connecting { what: "x".into() });
        assert_eq!(app.events_seen, 0);
        app.apply(Update::Event(Event::Ping));
        assert_eq!(app.events_seen, 1);
        assert!(app.last_event.is_some());
    }
}
