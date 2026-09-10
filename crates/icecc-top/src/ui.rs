//! Phase 2 rendering: a header, a node table, a footer.
//!
//! Intentionally plain. Bars, graphs and colour arrive in Phase 4, once a
//! node-side collector supplies data that changes every second — drawing a CPU
//! bar from the scheduler's change-driven composite `Load` would be a
//! confidently wrong picture (ARCHITECTURE.md §2.2, §2.3).

use icecc_model::{Cluster, ConnectionState, Node, Summary};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::app::App;

pub fn draw(frame: &mut Frame, app: &App) {
    let areas = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(3),    // table
        Constraint::Length(1), // footer
    ])
    .split(frame.area());

    let cluster = &app.cluster;
    let summary = cluster.summary();

    frame.render_widget(Paragraph::new(header_line(cluster, &summary)), areas[0]);
    frame.render_widget(node_table(cluster), areas[1]);
    frame.render_widget(Paragraph::new(footer_line(app, &summary)), areas[2]);
}

fn header_line(cluster: &Cluster, summary: &Summary) -> Line<'static> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled("icecc-top", bold), Span::raw("  ")];

    match &cluster.connection {
        ConnectionState::Connected {
            target, protocol, ..
        } => {
            spans.push(Span::raw(format!("{target}  proto {protocol}")));
        }
        ConnectionState::Connecting { what } => {
            spans.push(Span::raw(format!("{what}…")));
        }
        ConnectionState::Disconnected { reason } => {
            spans.push(Span::raw(format!("disconnected: {reason}")));
        }
    }

    if cluster.is_connected() {
        spans.push(Span::raw(format!("   nodes {}", describe_nodes(summary))));
        spans.push(Span::raw(format!(
            "   slots {}/{}{}",
            summary.used_slots,
            summary.total_slots,
            summary
                .slot_usage()
                .map(|p| format!(" ({p:.0}%)"))
                .unwrap_or_default()
        )));
    }

    Line::from(spans)
}

fn describe_nodes(summary: &Summary) -> String {
    let mut s = format!("{}", summary.nodes_online);
    if summary.nodes_offline > 0 {
        s.push_str(&format!(" (+{} offline)", summary.nodes_offline));
    }
    s
}

fn node_table(cluster: &Cluster) -> Table<'static> {
    let header = Row::new(vec![
        Cell::from("NODE"),
        Cell::from("IP"),
        Cell::from("JOBS"),
        Cell::from("LOAD"),
        Cell::from("SPEED"),
        Cell::from("PLATFORM"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = cluster
        .nodes_sorted()
        .into_iter()
        .map(|node| {
            let style = if node.offline {
                Style::default().add_modifier(Modifier::DIM | Modifier::CROSSED_OUT)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(node_label(node)),
                Cell::from(node.ip().to_owned()),
                Cell::from(jobs_cell(node)),
                Cell::from(load_cell(node)),
                Cell::from(speed_cell(node)),
                Cell::from(node.platform().to_owned()),
            ])
            .style(style)
        })
        .collect();

    let title = if cluster.nodes.is_empty() {
        if cluster.is_connected() {
            " no nodes registered with this scheduler "
        } else {
            " waiting for a scheduler "
        }
    } else {
        " nodes "
    };

    Table::new(
        rows,
        [
            Constraint::Min(16),
            Constraint::Length(17),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .block(Block::bordered().title(title))
}

/// Node name, with the two states the scheduler tells us about spelled out
/// rather than encoded in a colour we do not use yet.
fn node_label(node: &Node) -> String {
    let mut s = node.name().to_owned();
    if node.offline {
        s.push_str(" [offline]");
    } else if node.suspect() {
        // The scheduler negated MaxJobs: it has pinged this node and is waiting.
        s.push_str(" [no reply]");
    } else if !node.accepts_remote() {
        s.push_str(" [local only]");
    }
    s
}

fn jobs_cell(node: &Node) -> String {
    if node.offline {
        return "—".into();
    }
    let mut s = format!("{}/{}", node.current_jobs(), node.max_jobs());
    if !node.local_jobs.is_empty() {
        s.push_str(&format!(" +{}L", node.local_jobs.len()));
    }
    s
}

/// The scheduler's 0..1000 composite weight, shown as a fraction of 1000 so it
/// is not mistaken for a percentage of anything real.
fn load_cell(node: &Node) -> String {
    if node.offline {
        return "—".into();
    }
    node.load()
        .map(|l| l.to_string())
        .unwrap_or_else(|| "—".into())
}

fn speed_cell(node: &Node) -> String {
    if node.offline {
        return "—".into();
    }
    // Zero means "has not compiled yet", which is not the same as slow.
    node.speed()
        .map(|s| format!("{s:.0}"))
        .unwrap_or_else(|| "—".into())
}

fn footer_line(app: &App, summary: &Summary) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let cluster = &app.cluster;

    let jobs = format!(
        "jobs {} active · {} pending · {} local",
        summary.active_jobs, summary.pending_jobs, summary.local_jobs
    );
    // "since connect" is not pedantry: these counters cannot survive a
    // reconnect, because job ids are only meaningful within a session.
    let done = format!(
        "done {} remote / {} local{} (since connect)",
        cluster.totals.completed_remote,
        cluster.totals.completed_local,
        if cluster.totals.failed > 0 {
            format!(", {} failed", cluster.totals.failed)
        } else {
            String::new()
        }
    );

    Line::from(vec![
        Span::styled("[q]", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit  "),
        Span::styled("[r]", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" redraw   "),
        Span::styled(jobs, dim),
        Span::styled("   ", dim),
        Span::styled(done, dim),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use icecc_proto::{Event, SchedulerTarget, Update};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn app_with(updates: Vec<Update>) -> App {
        let mut app = App::new();
        for u in updates {
            app.apply(u);
        }
        app
    }

    fn connected() -> Update {
        Update::Connected {
            target: SchedulerTarget {
                host: "build-master".into(),
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

    /// Render and flatten the buffer to text, so assertions read like the screen.
    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shows_the_scheduler_and_a_node() {
        let app = app_with(vec![
            connected(),
            stats(
                1,
                "Name:build01\nIP:10.0.0.11\nMaxJobs:8\nNoRemote:false\nPlatform:x86_64\nSpeed:3200\nLoad:426\n",
            ),
        ]);
        let out = render(&app, 100, 10);
        assert!(out.contains("build-master:8765"), "{out}");
        assert!(out.contains("proto 43"), "{out}");
        assert!(out.contains("build01"), "{out}");
        assert!(out.contains("10.0.0.11"), "{out}");
        assert!(out.contains("0/8"), "{out}");
        assert!(out.contains("3200"), "{out}");
        assert!(out.contains("x86_64"), "{out}");
    }

    #[test]
    fn a_fresh_node_shows_no_speed_rather_than_zero() {
        let app = app_with(vec![
            connected(),
            stats(1, "Name:build01\nMaxJobs:8\nSpeed:0.000000\nLoad:100\n"),
        ]);
        let out = render(&app, 100, 10);
        let row = out
            .lines()
            .find(|l| l.contains("build01"))
            .expect("node row");
        // The SPEED cell must read as unknown, not as the number zero.
        assert!(row.contains('—'), "{row}");
        assert!(
            !row.contains(" 0 "),
            "zero speed must not read as slow: {row}"
        );
    }

    #[test]
    fn slots_reflect_running_jobs() {
        let app = app_with(vec![
            connected(),
            stats(1, "Name:build01\nMaxJobs:8\nNoRemote:false\nLoad:100\n"),
            Update::Event(Event::JobBegin {
                job_id: 1,
                start_time: 0,
                host_id: 1,
            }),
        ]);
        let out = render(&app, 100, 10);
        assert!(out.contains("slots 1/8"), "{out}");
        assert!(out.contains("1/8"), "{out}");
    }

    #[test]
    fn connecting_state_is_visible_because_it_can_last_36_seconds() {
        let app = app_with(vec![Update::Connecting {
            what: "handshaking with build-master:8765".into(),
        }]);
        let out = render(&app, 100, 8);
        assert!(out.contains("handshaking with build-master:8765"), "{out}");
        assert!(out.contains("waiting for a scheduler"), "{out}");
    }

    #[test]
    fn disconnect_reason_is_shown() {
        let app = app_with(vec![
            connected(),
            Update::Disconnected {
                reason: "connection reset".into(),
            },
        ]);
        let out = render(&app, 100, 8);
        assert!(out.contains("disconnected: connection reset"), "{out}");
    }

    #[test]
    fn offline_node_keeps_its_row_and_is_marked() {
        let app = app_with(vec![
            connected(),
            stats(1, "Name:build01\nIP:10.0.0.11\nMaxJobs:8\nNoRemote:false\n"),
            stats(1, "State:Offline\n"),
        ]);
        let out = render(&app, 100, 10);
        assert!(out.contains("build01 [offline]"), "{out}");
        assert!(out.contains("(+1 offline)"), "{out}");
    }

    #[test]
    fn local_only_and_unresponsive_nodes_are_labelled() {
        let app = app_with(vec![
            connected(),
            stats(1, "Name:laptop\nMaxJobs:12\nNoRemote:true\n"),
            stats(2, "Name:build02\nMaxJobs:-8\nNoRemote:false\n"),
        ]);
        let out = render(&app, 100, 10);
        assert!(out.contains("laptop [local only]"), "{out}");
        assert!(out.contains("build02 [no reply]"), "{out}");
    }

    #[test]
    fn counters_are_labelled_since_connect() {
        let app = app_with(vec![connected()]);
        let out = render(&app, 120, 8);
        assert!(out.contains("since connect"), "{out}");
    }

    #[test]
    fn empty_cluster_says_so_instead_of_looking_broken() {
        let app = app_with(vec![connected()]);
        let out = render(&app, 100, 8);
        assert!(out.contains("no nodes registered"), "{out}");
    }

    #[test]
    fn renders_at_a_narrow_terminal_without_panicking() {
        let app = app_with(vec![
            connected(),
            stats(1, "Name:build01\nIP:10.0.0.11\nMaxJobs:8\nNoRemote:false\n"),
        ]);
        // Resize handling means any geometry has to be safe to draw.
        for (w, h) in [(20u16, 5u16), (40, 8), (200, 60), (10, 3)] {
            let _ = render(&app, w, h);
        }
    }

    #[test]
    fn many_nodes_render_within_the_viewport() {
        let mut updates = vec![connected()];
        for i in 0..120 {
            updates.push(stats(
                i,
                &format!(
                    "Name:build{i:03}\nIP:10.0.0.{}\nMaxJobs:16\nNoRemote:false\n",
                    i % 250
                ),
            ));
        }
        let app = app_with(updates);
        let out = render(&app, 100, 30);
        assert!(out.contains("nodes 120"), "{out}");
        assert!(out.contains("build000"), "{out}");
    }
}
