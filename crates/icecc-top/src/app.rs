//! Application state and input handling.
//!
//! Phase 2 keeps this deliberately small: the scheduler stream is the only data
//! source, so there is nothing to sort by that would not be misleading yet
//! (`Load` is a scheduling weight, not CPU; `Speed` is 0 until a node compiles).
//! Sorting and navigation arrive with real per-node metrics in Phase 4.

use std::time::{Duration, Instant};

use icecc_model::Cluster;
use icecc_proto::Update;

/// How often the screen may repaint. Events can burst — a login replay of 100
/// nodes arrives at once — so redraws are coalesced rather than done per event.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(100);

pub struct App {
    pub cluster: Cluster,
    /// Set when state changed since the last paint.
    dirty: bool,
    pub should_quit: bool,
    /// Events applied since start, shown in the footer as a liveness hint —
    /// a quiet cluster legitimately produces none.
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
        self.dirty = true;
    }

    /// Take the dirty flag; true means repaint.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Force a repaint (terminal resize, manual refresh).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn on_key(&mut self, key: Key) {
        match key {
            Key::Quit => self.should_quit = true,
            Key::Refresh => self.mark_dirty(),
            Key::Ignored => {}
        }
    }
}

/// The keys Phase 2 understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Quit,
    /// `r` — repaint now. It cannot pull from the scheduler: the scheduler
    /// pushes, and there is no request a monitor may send (ARCHITECTURE.md §1.2).
    Refresh,
    Ignored,
}

/// Map a terminal key event onto an action.
pub fn classify(
    code: ratatui::crossterm::event::KeyCode,
    mods: ratatui::crossterm::event::KeyModifiers,
) -> Key {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    match (code, mods) {
        (KeyCode::Char('q') | KeyCode::Esc, _) => Key::Quit,
        (KeyCode::Char('c') | KeyCode::Char('C'), KeyModifiers::CONTROL) => Key::Quit,
        (KeyCode::Char('r'), _) => Key::Refresh,
        _ => Key::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use icecc_proto::{Event, SchedulerTarget};
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn q_and_ctrl_c_quit() {
        assert_eq!(classify(KeyCode::Char('q'), KeyModifiers::NONE), Key::Quit);
        assert_eq!(classify(KeyCode::Esc, KeyModifiers::NONE), Key::Quit);
        assert_eq!(
            classify(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Key::Quit
        );
        // A bare 'c' must not quit; it is reserved for sort-by-CPU later.
        assert_eq!(
            classify(KeyCode::Char('c'), KeyModifiers::NONE),
            Key::Ignored
        );
    }

    #[test]
    fn quit_sets_the_flag() {
        let mut app = App::new();
        assert!(!app.should_quit);
        app.on_key(Key::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn first_paint_is_dirty_then_settles() {
        let mut app = App::new();
        assert!(app.take_dirty());
        assert!(!app.take_dirty());
    }

    #[test]
    fn applying_an_update_requests_a_repaint() {
        let mut app = App::new();
        app.take_dirty();
        app.apply(Update::Connected {
            target: SchedulerTarget {
                host: "sched".into(),
                port: 8765,
            },
            protocol: 43,
        });
        assert!(app.take_dirty());
    }

    #[test]
    fn only_protocol_events_count_toward_liveness() {
        let mut app = App::new();
        app.apply(Update::Connecting { what: "x".into() });
        assert_eq!(app.events_seen, 0);
        assert!(app.last_event.is_none());

        app.apply(Update::Event(Event::Ping));
        assert_eq!(app.events_seen, 1);
        assert!(app.last_event.is_some());
    }
}
