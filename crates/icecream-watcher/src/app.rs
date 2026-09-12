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
///
/// Icecream figures only, matching the table. Sorting by a node's CPU or memory
/// was dropped with the columns that showed them: a list that reorders itself
/// by a number the reader cannot see is worse than one that offers fewer ways
/// to order it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    /// Icecream compile slots in use.
    Jobs,
    /// The scheduler's placement weight, not CPU utilisation.
    Load,
    Speed,
}

impl SortKey {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Jobs => "jobs",
            Self::Load => "load",
            Self::Speed => "speed",
        }
    }

    /// Order for the `s` key to cycle through.
    pub fn next(&self) -> Self {
        match self {
            Self::Name => Self::Jobs,
            Self::Jobs => Self::Load,
            Self::Load => Self::Speed,
            Self::Speed => Self::Name,
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
    /// Whether the per-node detail view is open, for [`Self::selected`].
    pub detail: bool,
    /// Scroll offset within the detail view.
    pub detail_scroll: u16,
    pub show_help: bool,
    /// Whether the "really quit?" prompt is up. A monitor is something people
    /// leave running for a day, and `q` is one key away from every other key.
    pub confirm_quit: bool,
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
            detail: false,
            detail_scroll: 0,
            show_help: false,
            confirm_quit: false,
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

    /// How long since the scheduler last said anything, once connected.
    ///
    /// `None` before the first event of a session, because "silent for 0 s"
    /// and "has never spoken" are different things and only the first is
    /// reassuring.
    pub fn quiet_for(&self) -> Option<Duration> {
        if !self.cluster.is_connected() {
            return None;
        }
        Some(self.last_event?.elapsed())
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Nodes in display order, by the current sort key. A node that has left
    /// the cluster is not here to order: it leaves the list entirely.
    pub fn sorted_nodes(&self) -> Vec<&Node> {
        let mut nodes: Vec<&Node> = self.cluster.nodes.values().collect();
        let key = self.sort;
        nodes.sort_by(|a, b| {
            compare(a, b, key)
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
                // The detail view has nothing left to show; falling back to the
                // list beats an empty pane.
                self.detail = false;
            }
        }
    }

    /// The node the detail view is showing, if it is open.
    pub fn detail_node(&self) -> Option<&Node> {
        if !self.detail {
            return None;
        }
        self.cluster.nodes.get(&self.selected?)
    }

    /// Open the detail view for the selection, selecting the first row if
    /// nothing is selected yet — pressing Enter on a fresh screen should show
    /// something rather than nothing.
    fn open_detail(&mut self) {
        if self.selected.is_none() {
            self.move_selection(1);
        }
        if self.selected.is_some() {
            self.detail = true;
            self.detail_scroll = 0;
            self.dirty = true;
        }
    }

    /// Clamp the detail scroll to what the view can actually show.
    pub fn clamp_detail_scroll(&mut self, max: u16) {
        if self.detail_scroll > max {
            self.detail_scroll = max;
            self.dirty = true;
        }
    }

    fn scroll_detail(&mut self, delta: isize) {
        let next = (self.detail_scroll as isize + delta).max(0);
        self.detail_scroll = next as u16;
        self.dirty = true;
    }

    pub fn on_key(&mut self, key: Key) {
        // The prompt takes every key while it is up: behind it is a list whose
        // selection and sort order must not change under an answer to a
        // question about quitting.
        if self.confirm_quit {
            match key {
                // `y` and nothing else. Enter is how a node is opened, and a
                // prompt that exists to catch a stray keystroke must not be
                // dismissable by the most reflexive one there is.
                Key::ForceQuit | Key::Confirm => self.should_quit = true,
                // `n` reaches here as the sort key it is everywhere else;
                // at a yes/no question it is the answer it looks like.
                Key::Quit | Key::Back | Key::Sort(SortKey::Name) => {
                    self.confirm_quit = false;
                    self.dirty = true;
                }
                _ => {}
            }
            return;
        }

        match key {
            Key::ForceQuit => self.should_quit = true,
            Key::Quit | Key::Back => {
                // Both unwind one layer at a time: overlay, then detail, then
                // the session. Quitting straight from a detail view would be a
                // surprise — the way out of a view is the same key twice, not
                // the end of the session.
                if self.show_help {
                    self.show_help = false;
                    self.dirty = true;
                } else if self.detail {
                    self.detail = false;
                    self.dirty = true;
                } else {
                    self.confirm_quit = true;
                    self.dirty = true;
                }
            }
            Key::Refresh => self.mark_dirty(),
            Key::ToggleHelp => {
                self.show_help = !self.show_help;
                self.dirty = true;
            }
            // In the detail view the arrows scroll the pane rather than moving
            // a selection that is not visible, and sorting a hidden list would
            // change the screen you cannot see.
            Key::Up if self.detail => self.scroll_detail(-1),
            Key::Down if self.detail => self.scroll_detail(1),
            Key::PageUp if self.detail => self.scroll_detail(-10),
            Key::PageDown if self.detail => self.scroll_detail(10),
            Key::Sort(_) | Key::CycleSort if self.detail => {}
            Key::Enter if self.detail => {
                self.detail = false;
                self.dirty = true;
            }

            Key::Up => self.move_selection(-1),
            Key::Down => self.move_selection(1),
            Key::PageUp => self.move_selection(-10),
            Key::PageDown => self.move_selection(10),
            Key::Sort(key) => self.set_sort(key),
            Key::CycleSort => self.set_sort(self.sort.next()),
            Key::Enter => self.open_detail(),
            // Only the quit prompt asks a yes/no question, and it answers this
            // one itself above.
            Key::Confirm | Key::Ignored => {}
        }
    }
}

/// Compare two nodes by one metric. A node with no measurement always sorts
/// after one that has a value, whichever direction we are sorting.
fn compare(a: &Node, b: &Node, key: SortKey) -> Ordering {
    let ordering = match key {
        SortKey::Name => return a.name().cmp(b.name()),
        SortKey::Jobs => opt_cmp(a.slot_pct(), b.slot_pct()),
        // The scheduler's own figure, so the order matches the LOAD column.
        SortKey::Load => opt_cmp(a.stats.load, b.stats.load),
        SortKey::Speed => opt_cmp(a.speed(), b.speed()),
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
    /// `q`: the same as [`Key::Back`]. It reads as "quit" and it does quit —
    /// but from a detail view, what the reader wants out of is the detail view,
    /// and a keystroke that ends the session instead is a bad surprise.
    Quit,
    /// Esc: leave the current overlay, or quit at the top level.
    Back,
    /// Ctrl-C: quit from wherever you are, no unwinding and no prompt. The one
    /// key that always means what it says.
    ForceQuit,
    /// `y` — yes, at the quit prompt. Bound nowhere else.
    Confirm,
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
        (KeyCode::Char('c') | KeyCode::Char('C'), KeyModifiers::CONTROL) => Key::ForceQuit,
        (KeyCode::Char('q'), _) => Key::Quit,
        (KeyCode::Char('y') | KeyCode::Char('Y'), _) => Key::Confirm,
        (KeyCode::Esc, _) => Key::Back,
        (KeyCode::Up | KeyCode::Char('k'), _) => Key::Up,
        (KeyCode::Down | KeyCode::Char('j'), _) => Key::Down,
        (KeyCode::PageUp, _) => Key::PageUp,
        (KeyCode::PageDown, _) => Key::PageDown,
        (KeyCode::Enter, _) => Key::Enter,
        (KeyCode::Char('r'), _) => Key::Refresh,
        (KeyCode::Char('s'), _) => Key::CycleSort,
        (KeyCode::Char('n'), _) => Key::Sort(SortKey::Name),
        (KeyCode::Char('i'), _) => Key::Sort(SortKey::Jobs),
        (KeyCode::Char('l'), _) => Key::Sort(SortKey::Load),
        // `p` because `s` already cycles the sort.
        (KeyCode::Char('p'), _) => Key::Sort(SortKey::Speed),
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
            netname: Some("ICECREAM".into()),
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
            "Name:build01\nIP:10.0.0.1\nMaxJobs:8\nSpeed:3000\nLoad:200\n",
        ));
        app.apply(stats(
            2,
            "Name:build02\nIP:10.0.0.2\nMaxJobs:8\nSpeed:1000\nLoad:900\n",
        ));
        app.apply(stats(
            3,
            "Name:build03\nIP:10.0.0.3\nMaxJobs:8\nSpeed:2000\nLoad:500\n",
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

        app.set_sort(SortKey::Load);
        assert_eq!(names(&app), ["build02", "build03", "build01"]);

        app.set_sort(SortKey::Speed);
        assert_eq!(names(&app), ["build01", "build03", "build02"]);
    }

    #[test]
    fn sorting_offers_only_figures_the_table_shows() {
        // A list that reorders itself by a number the reader cannot see is
        // worse than one with fewer ways to order it. CPU and memory left the
        // table, so they left the sort keys with it.
        let mut seen = vec![SortKey::Name];
        while seen.last().unwrap().next() != SortKey::Name {
            seen.push(seen.last().unwrap().next());
        }
        let labels: Vec<&str> = seen.iter().map(|k| k.label()).collect();
        assert_eq!(labels, ["name", "jobs", "load", "speed"]);
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
        app.apply(stats(4, "Name:build04\nIP:10.0.0.4\nMaxJobs:8\n")); // no speed yet
        app.set_sort(SortKey::Speed);
        assert_eq!(
            names(&app).last().unwrap(),
            "build04",
            "a node with no measurement must not top a busiest-first sort"
        );
    }

    #[test]
    fn a_node_that_leaves_is_not_sorted_at_all() {
        // It is no longer in the list to place: an offline node leaves rather
        // than sinking to the bottom struck through.
        let mut app = app_with_three();
        app.apply(stats(2, "State:Offline\n")); // the busiest node
        app.set_sort(SortKey::Load);
        assert_eq!(names(&app), ["build03", "build01"]);
    }

    #[test]
    fn cycling_sort_visits_every_key_and_returns() {
        let mut app = App::new();
        let mut seen = vec![app.sort];
        for _ in 0..3 {
            app.on_key(Key::CycleSort);
            seen.push(app.sort);
        }
        assert_eq!(seen, [SortKey::Name, SortKey::Jobs, SortKey::Load, SortKey::Speed]);
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
        app.on_key(Key::Down); // build01, the least loaded
        let picked = app.selected;
        app.set_sort(SortKey::Load);
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
        assert!(!app.should_quit, "the last layer asks before it goes");
        assert!(app.confirm_quit);
        app.on_key(Key::Confirm);
        assert!(app.should_quit);
    }

    #[test]
    fn leaving_a_detail_view_does_not_end_the_session() {
        // `q` read as "quit the program" from inside a node's detail view,
        // which is not what someone pressing it there wants: they want out of
        // the view they opened. Ctrl-C is the key that still ends the session
        // wherever it is pressed.
        for leave in [Key::Quit, Key::Back] {
            let mut app = App::new();
            app.apply(connected());
            app.apply(stats(1, "Name:build01\nIP:10.0.0.1\nMaxJobs:8\n"));
            app.on_key(Key::Down);
            app.on_key(Key::Enter);
            assert!(app.detail, "{leave:?} needs a detail view to leave");

            app.on_key(leave);
            assert!(!app.detail, "{leave:?} should close the view");
            assert!(!app.should_quit, "{leave:?} should not end the session");

            // ...and then it means quit, once the prompt has been answered.
            app.on_key(leave);
            assert!(app.confirm_quit, "{leave:?} at the top level asks");
            app.on_key(Key::Confirm);
            assert!(app.should_quit, "{leave:?} then y quits");
        }
    }

    #[test]
    fn the_quit_prompt_takes_every_key_until_it_is_answered() {
        // Behind it is a list whose selection and sort order must not change
        // under an answer to a question about quitting — and `n`, the obvious
        // way to say no, is the key that sorts by name.
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(1, "Name:build01\nIP:10.0.0.1\nMaxJobs:8\n"));
        app.apply(stats(2, "Name:build02\nIP:10.0.0.2\nMaxJobs:8\n"));
        app.on_key(Key::Down);
        let selected = app.selected;
        app.set_sort(SortKey::Speed);

        app.on_key(Key::Quit);
        assert!(app.confirm_quit);

        for ignored in [Key::Down, Key::Up, Key::Enter, Key::CycleSort] {
            app.on_key(ignored);
        }
        assert_eq!(app.selected, selected, "the list moved under the prompt");
        assert_eq!(app.sort, SortKey::Speed, "the order changed under the prompt");
        assert!(!app.detail, "a view opened under the prompt");
        assert!(app.confirm_quit, "the prompt should still be up");

        app.on_key(Key::Sort(SortKey::Name));
        assert!(!app.confirm_quit, "n answers no");
        assert!(!app.should_quit);
        assert_eq!(app.sort, SortKey::Speed, "...and does not also sort");
    }

    #[test]
    fn ctrl_c_skips_the_prompt() {
        let mut app = App::new();
        app.on_key(Key::Quit);
        assert!(app.confirm_quit);
        app.on_key(Key::ForceQuit);
        assert!(app.should_quit, "Ctrl-C does not stop to ask");
    }

    #[test]
    fn ctrl_c_quits_from_inside_a_view() {
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(1, "Name:build01\nIP:10.0.0.1\nMaxJobs:8\n"));
        app.on_key(Key::Down);
        app.on_key(Key::Enter);
        app.on_key(Key::ForceQuit);
        assert!(app.should_quit, "Ctrl-C does not unwind, it quits");
    }

    #[test]
    fn key_bindings_match_the_documented_set() {
        let n = KeyModifiers::NONE;
        assert_eq!(classify(KeyCode::Char('q'), n), Key::Quit);
        assert_eq!(
            classify(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Key::ForceQuit
        );
        assert_eq!(classify(KeyCode::Esc, n), Key::Back);
        assert_eq!(classify(KeyCode::Char('k'), n), Key::Up);
        assert_eq!(classify(KeyCode::Up, n), Key::Up);
        assert_eq!(classify(KeyCode::Char('j'), n), Key::Down);
        assert_eq!(classify(KeyCode::Down, n), Key::Down);
        assert_eq!(classify(KeyCode::Enter, n), Key::Enter);
        assert_eq!(classify(KeyCode::Char('r'), n), Key::Refresh);
        assert_eq!(classify(KeyCode::Char('s'), n), Key::CycleSort);
        assert_eq!(classify(KeyCode::Char('n'), n), Key::Sort(SortKey::Name));
        assert_eq!(classify(KeyCode::Char('i'), n), Key::Sort(SortKey::Jobs));
        assert_eq!(classify(KeyCode::Char('l'), n), Key::Sort(SortKey::Load));
        assert_eq!(classify(KeyCode::Char('p'), n), Key::Sort(SortKey::Speed));
        // Freed when CPU and memory sorting went; they must do nothing rather
        // than quietly reorder by something invisible.
        assert_eq!(classify(KeyCode::Char('c'), n), Key::Ignored);
        assert_eq!(classify(KeyCode::Char('m'), n), Key::Ignored);
        assert_eq!(classify(KeyCode::Char('?'), n), Key::ToggleHelp);
        assert_eq!(classify(KeyCode::Char('z'), n), Key::Ignored);
    }

    #[test]
    fn ctrl_c_quits_but_a_bare_c_does_nothing() {
        // The two must not be confused: one ends the session, the other is now
        // an unbound key and must stay harmless.
        assert_eq!(
            classify(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Key::ForceQuit
        );
        assert_eq!(classify(KeyCode::Char('c'), KeyModifiers::NONE), Key::Ignored);
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
            Key::Sort(SortKey::Load),
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
