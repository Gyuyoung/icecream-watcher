//! Phase 3 rendering: a header, a node table with real resource metrics, a footer.
//!
//! Still numbers rather than bars and graphs — those are Phase 4. What changed
//! from Phase 2 is that CPU, memory and temperature are now real, because they
//! come from `icecc-top-agent` rather than from the scheduler's change-driven
//! composite `Load` (ARCHITECTURE.md §2.3, §3).
//!
//! The rule throughout: a value we do not have renders as `—`, never as zero.
//! An idle node and an unmeasured node must not look the same.

use std::time::Duration;

use icecc_model::{Cluster, ConnectionState, Node, ResourceState, Summary};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::app::App;

/// Shown when a metric is not available. Distinct from `0`.
const UNKNOWN: &str = "—";

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
        } => spans.push(Span::raw(format!("{target}  proto {protocol}"))),
        ConnectionState::Connecting { what } => spans.push(Span::raw(format!("{what}…"))),
        ConnectionState::Disconnected { reason } => {
            spans.push(Span::raw(format!("disconnected: {reason}")))
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
        spans.push(Span::raw(format!("   {}", describe_agents(summary))));
    }

    Line::from(spans)
}

fn describe_nodes(summary: &Summary) -> String {
    let mut s = summary.nodes_online.to_string();
    if summary.nodes_offline > 0 {
        s.push_str(&format!(" (+{} offline)", summary.nodes_offline));
    }
    s
}

/// Agent coverage, so a half-finished rollout is obvious rather than looking
/// like a cluster of idle nodes.
fn describe_agents(summary: &Summary) -> String {
    let mut s = format!(
        "agents {}/{}",
        summary.nodes_with_metrics, summary.nodes_online
    );
    if summary.nodes_metrics_stale > 0 {
        s.push_str(&format!(" ({} stale)", summary.nodes_metrics_stale));
    }
    s
}

fn node_table(cluster: &Cluster) -> Table<'static> {
    let stale_after = cluster.metrics_stale_after;

    let header = Row::new(vec![
        Cell::from("NODE"),
        Cell::from("CPU"),
        Cell::from("MEM"),
        Cell::from("LOAD"),
        Cell::from("JOBS"),
        Cell::from("SPEED"),
        Cell::from("TEMP"),
        Cell::from("IP"),
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
                Cell::from(node_label(node, stale_after)),
                Cell::from(cpu_cell(node)),
                Cell::from(mem_cell(node)),
                Cell::from(load_cell(node)),
                Cell::from(jobs_cell(node)),
                Cell::from(speed_cell(node)),
                Cell::from(temp_cell(node)),
                Cell::from(node.ip().to_owned()),
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
            Constraint::Min(16),    // NODE
            Constraint::Length(6),  // CPU
            Constraint::Length(6),  // MEM
            Constraint::Length(6),  // LOAD
            Constraint::Length(9),  // JOBS
            Constraint::Length(7),  // SPEED
            Constraint::Length(6),  // TEMP
            Constraint::Length(16), // IP
        ],
    )
    .header(header)
    .block(Block::bordered().title(title))
}

/// Node name plus the states worth seeing at a glance. Spelled out rather than
/// encoded in colour, which Phase 4 adds.
fn node_label(node: &Node, stale_after: Duration) -> String {
    let mut s = node.name().to_owned();
    if node.offline {
        s.push_str(" [offline]");
        return s;
    }
    if node.suspect() {
        // The scheduler negated MaxJobs: it has pinged this node and is waiting.
        s.push_str(" [no reply]");
    }
    if !node.accepts_remote() {
        s.push_str(" [local only]");
    }
    if node.identity_mismatch {
        // We reached a machine that calls itself something else, so the metrics
        // on this row may belong to a different host.
        s.push_str(" [wrong host?]");
    }
    match &node.resource_state {
        ResourceState::Error { .. } => s.push_str(" [agent error]"),
        // Stale only matters once an agent has ever answered; a node that never
        // had one is a rollout gap, shown by the header's agent count.
        _ if node.has_agent() && node.metrics_stale(stale_after) => s.push_str(" [stale]"),
        _ => {}
    }
    s
}

fn cpu_cell(node: &Node) -> String {
    if node.offline {
        return UNKNOWN.into();
    }
    node.cpu_pct()
        .map(|p| format!("{p:.0}%"))
        .unwrap_or_else(|| UNKNOWN.into())
}

fn mem_cell(node: &Node) -> String {
    if node.offline {
        return UNKNOWN.into();
    }
    node.mem_pct()
        .map(|p| format!("{p:.0}%"))
        .unwrap_or_else(|| UNKNOWN.into())
}

/// Load average. Prefers the agent's 1 Hz reading; falls back to the
/// scheduler's, which only updates when load shifts by 10 %.
fn load_cell(node: &Node) -> String {
    if node.offline {
        return UNKNOWN.into();
    }
    node.load_avg_1()
        .map(|l| format!("{l:.1}"))
        .unwrap_or_else(|| UNKNOWN.into())
}

fn jobs_cell(node: &Node) -> String {
    if node.offline {
        return UNKNOWN.into();
    }
    let mut s = format!("{}/{}", node.current_jobs(), node.max_jobs());
    if !node.local_jobs.is_empty() {
        s.push_str(&format!(" +{}L", node.local_jobs.len()));
    }
    s
}

fn speed_cell(node: &Node) -> String {
    if node.offline {
        return UNKNOWN.into();
    }
    // Zero means "has not compiled yet", which is not the same as slow.
    node.speed()
        .map(|s| format!("{s:.0}"))
        .unwrap_or_else(|| UNKNOWN.into())
}

fn temp_cell(node: &Node) -> String {
    if node.offline {
        return UNKNOWN.into();
    }
    node.temp_c()
        .map(|t| format!("{t:.0}°"))
        .unwrap_or_else(|| UNKNOWN.into())
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
    use icecc_metrics::Snapshot;
    use icecc_model::ResourceResult;
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

    fn snapshot(hostname: &str, cpu: f32, mem_used_kib: u64, temp: Option<f32>) -> Box<Snapshot> {
        Box::new(Snapshot {
            schema: icecc_metrics::SCHEMA_VERSION,
            agent_version: "0.1.0".into(),
            hostname: hostname.into(),
            addresses: vec!["10.0.0.11".into()],
            uptime_secs: 1000,
            sampled_unix_ms: 1,
            sample_interval_ms: 1000,
            cpu: icecc_metrics::Cpu {
                cores: 8,
                total_busy_pct: cpu,
                per_core_busy_pct: vec![cpu; 8],
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
                one: 14.2,
                five: 12.0,
                fifteen: 9.0,
                runnable: 8,
                total_procs: 500,
            },
            thermal: icecc_metrics::Thermal {
                cpu_celsius: temp,
                cpu_source: temp.map(|_| "coretemp/Package id 0".to_owned()),
                sensors: vec![],
            },
            net: icecc_metrics::Net {
                rx_bytes_per_sec: 1,
                tx_bytes_per_sec: 2,
                interfaces: vec![],
            },
        })
    }

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

    fn row_for<'a>(out: &'a str, name: &str) -> &'a str {
        out.lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("no row for {name} in:\n{out}"))
    }

    fn one_node_app() -> App {
        app_with(vec![
            connected(),
            stats(
                1,
                "Name:build01\nIP:10.0.0.11\nMaxJobs:8\nNoRemote:false\nPlatform:x86_64\nSpeed:3200\nLoad:426\n",
            ),
        ])
    }

    #[test]
    fn without_an_agent_cpu_memory_and_temperature_read_as_unknown() {
        let app = one_node_app();
        let out = render(&app, 110, 10);
        let row = row_for(&out, "build01");
        // Three dashes: CPU, MEM, TEMP. Not zeroes.
        assert_eq!(row.matches(UNKNOWN).count(), 3, "{row}");
        assert!(out.contains("agents 0/1"), "{out}");
    }

    #[test]
    fn a_polled_node_shows_real_cpu_memory_and_temperature() {
        let mut app = one_node_app();
        app.apply_resource(
            1,
            ResourceResult::Ok(snapshot("build01", 82.4, 710, Some(71.0))),
        );

        let out = render(&app, 110, 10);
        let row = row_for(&out, "build01");
        assert!(row.contains("82%"), "{row}");
        assert!(row.contains("71%"), "{row}");
        assert!(row.contains("71°"), "{row}");
        // Load now comes from the agent's 1 Hz reading, not the scheduler's.
        assert!(row.contains("14.2"), "{row}");
        assert!(out.contains("agents 1/1"), "{out}");
    }

    #[test]
    fn a_node_without_a_temperature_sensor_shows_a_dash_not_zero_degrees() {
        let mut app = one_node_app();
        app.apply_resource(1, ResourceResult::Ok(snapshot("build01", 50.0, 500, None)));
        let out = render(&app, 110, 10);
        let row = row_for(&out, "build01");
        assert!(!row.contains("0°"), "{row}");
        assert!(row.contains(UNKNOWN), "{row}");
    }

    #[test]
    fn an_idle_node_reads_as_zero_percent_which_is_not_the_same_as_unknown() {
        let mut app = one_node_app();
        app.apply_resource(
            1,
            ResourceResult::Ok(snapshot("build01", 0.0, 100, Some(40.0))),
        );
        let out = render(&app, 110, 10);
        let row = row_for(&out, "build01");
        assert!(row.contains("0%"), "an idle node must show 0%: {row}");
    }

    #[test]
    fn a_failing_agent_is_labelled_on_the_row() {
        let mut app = one_node_app();
        app.apply_resource(1, ResourceResult::Bad("agent returned HTTP 404".into()));
        let out = render(&app, 110, 10);
        assert!(out.contains("[agent error]"), "{out}");
    }

    #[test]
    fn stale_metrics_keep_the_last_values_but_say_so() {
        let mut app = one_node_app();
        app.cluster.metrics_stale_after = Duration::from_millis(1);
        app.apply_resource(
            1,
            ResourceResult::Ok(snapshot("build01", 82.0, 700, Some(71.0))),
        );
        std::thread::sleep(Duration::from_millis(5));

        let out = render(&app, 110, 10);
        let row = row_for(&out, "build01");
        assert!(row.contains("[stale]"), "{row}");
        assert!(row.contains("82%"), "last known value should remain: {row}");
        assert!(out.contains("agents 0/1 (1 stale)"), "{out}");
    }

    #[test]
    fn reaching_the_wrong_host_is_called_out() {
        let mut app = one_node_app();
        app.apply_resource(
            1,
            ResourceResult::Ok(snapshot("some-other-box", 82.0, 700, Some(71.0))),
        );
        assert!(render(&app, 110, 10).contains("[wrong host?]"));
    }

    #[test]
    fn an_offline_node_shows_no_metrics_at_all() {
        let mut app = one_node_app();
        app.apply_resource(
            1,
            ResourceResult::Ok(snapshot("build01", 82.0, 700, Some(71.0))),
        );
        app.apply(stats(1, "State:Offline\n"));

        let out = render(&app, 110, 10);
        let row = row_for(&out, "build01");
        assert!(row.contains("[offline]"), "{row}");
        assert!(!row.contains("82%"), "stale metrics of a dead node: {row}");
    }

    // ---- carried over from Phase 2: these must not regress ----

    #[test]
    fn shows_the_scheduler_and_a_node() {
        let out = render(&one_node_app(), 110, 10);
        assert!(out.contains("build-master:8765"), "{out}");
        assert!(out.contains("proto 43"), "{out}");
        assert!(out.contains("10.0.0.11"), "{out}");
        assert!(out.contains("0/8"), "{out}");
        assert!(out.contains("3200"), "{out}");
    }

    #[test]
    fn a_fresh_node_shows_no_speed_rather_than_zero() {
        let app = app_with(vec![
            connected(),
            stats(1, "Name:build01\nMaxJobs:8\nSpeed:0.000000\nLoad:100\n"),
        ]);
        let out = render(&app, 110, 10);
        let row = row_for(&out, "build01");
        assert!(row.contains(UNKNOWN), "{row}");
        assert!(!row.contains(" 0 "), "{row}");
    }

    #[test]
    fn slots_reflect_running_jobs() {
        let mut app = one_node_app();
        app.apply(Update::Event(Event::JobBegin {
            job_id: 1,
            start_time: 0,
            host_id: 1,
        }));
        let out = render(&app, 110, 10);
        assert!(out.contains("slots 1/8"), "{out}");
    }

    #[test]
    fn connecting_state_is_visible_because_it_can_last_36_seconds() {
        let app = app_with(vec![Update::Connecting {
            what: "handshaking with build-master:8765".into(),
        }]);
        let out = render(&app, 110, 8);
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
        assert!(render(&app, 110, 8).contains("disconnected: connection reset"));
    }

    #[test]
    fn local_only_and_unresponsive_nodes_are_labelled() {
        let app = app_with(vec![
            connected(),
            stats(1, "Name:laptop\nMaxJobs:12\nNoRemote:true\n"),
            stats(2, "Name:build02\nMaxJobs:-8\nNoRemote:false\n"),
        ]);
        let out = render(&app, 110, 10);
        assert!(out.contains("laptop [local only]"), "{out}");
        assert!(out.contains("build02 [no reply]"), "{out}");
    }

    #[test]
    fn counters_are_labelled_since_connect() {
        assert!(render(&app_with(vec![connected()]), 120, 8).contains("since connect"));
    }

    #[test]
    fn empty_cluster_says_so_instead_of_looking_broken() {
        assert!(render(&app_with(vec![connected()]), 110, 8).contains("no nodes registered"));
    }

    #[test]
    fn renders_at_a_narrow_terminal_without_panicking() {
        let mut app = one_node_app();
        app.apply_resource(
            1,
            ResourceResult::Ok(snapshot("build01", 82.0, 700, Some(71.0))),
        );
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
        let mut app = app_with(updates);
        for i in 0..120 {
            app.apply_resource(
                i,
                ResourceResult::Ok(snapshot(&format!("build{i:03}"), 50.0, 500, Some(60.0))),
            );
        }
        let out = render(&app, 110, 30);
        assert!(out.contains("nodes 120"), "{out}");
        assert!(out.contains("agents 120/120"), "{out}");
    }
}
