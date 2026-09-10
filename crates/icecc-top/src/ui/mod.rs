//! Phase 4 rendering.
//!
//! The organising idea is **visual hierarchy**, not more columns. Nine
//! questions should be answerable in a second or two — which nodes are busy,
//! which are idle, which are CPU- or memory-constrained, how full the cluster
//! is, whether the queue is growing, whether one node is slow, whether anything
//! is unhealthy — and a grid of equally-weighted numbers cannot do that however
//! many numbers it has.
//!
//! So:
//!
//! * a **cluster band** answers the whole-cluster questions before the table is
//!   read at all: slot occupancy as a bar, queue depth as a sparkline with the
//!   trend spelled out in a word, and throughput;
//! * **three bars per node** (CPU, memory, slots) carry the metrics that matter,
//!   comparable across rows without reading a single figure;
//! * **colour carries state**, ramping only as a metric becomes a problem, and a
//!   `!` marks the metric that makes a node a bottleneck, so CPU-bound and
//!   memory-bound are distinguishable at a glance;
//! * **idle is quiet, unhealthy is loud** — idle rows dim, offline rows sink and
//!   strike through;
//! * everything else — IP, platform, protocol, features, per-core detail,
//!   network — is deliberately absent, and belongs in the detail view.
//!
//! A value we do not have renders as `—`, never as zero: an idle node and an
//! unmeasured node must not look the same.

mod widgets;

use icecc_model::{Bottleneck, Cluster, ConnectionState, Node, ResourceState, Summary, Trend};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::app::App;

/// Shown when a metric is not available. Distinct from `0`.
const UNKNOWN: &str = "—";

/// Persistent widget state, so scrolling and selection survive between frames.
#[derive(Default)]
pub struct Ui {
    table: TableState,
}

pub fn draw(frame: &mut Frame, app: &App, ui: &mut Ui) {
    let area = frame.area();
    let cluster = &app.cluster;
    let summary = cluster.summary();

    // The cluster band is the first thing to go when the terminal is short:
    // without room for the table it would answer questions about rows nobody
    // can see.
    let band_height = if area.height >= 12 { 5 } else { 0 };
    let areas = Layout::vertical([
        Constraint::Length(1),           // header
        Constraint::Length(band_height), // cluster band
        Constraint::Min(3),              // node table
        Constraint::Length(1),           // footer
    ])
    .split(area);

    frame.render_widget(Paragraph::new(header_line(app, &summary)), areas[0]);
    if band_height > 0 {
        cluster_band(frame, areas[1], cluster, &summary);
    }
    node_table(frame, areas[2], app, ui);
    frame.render_widget(Paragraph::new(footer_line(app, &summary)), areas[3]);

    if app.show_help {
        help_overlay(frame, area);
    }
}

// ---------------------------------------------------------------- header

fn header_line(app: &App, summary: &Summary) -> Line<'static> {
    let cluster = &app.cluster;
    let mut spans = vec![
        Span::styled(
            "icecc-top",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];

    match &cluster.connection {
        ConnectionState::Connected {
            target,
            protocol,
            since,
        } => {
            spans.push(Span::raw(format!("{target}")));
            spans.push(Span::styled(
                format!(
                    "  proto {protocol}  up {}",
                    widgets::duration(since.elapsed().as_secs())
                ),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        ConnectionState::Connecting { what } => spans.push(Span::styled(
            format!("{what}…"),
            Style::default().fg(Color::Yellow),
        )),
        ConnectionState::Disconnected { reason } => spans.push(Span::styled(
            format!("disconnected: {reason}"),
            Style::default().fg(Color::LightRed),
        )),
    }

    if cluster.is_connected() {
        spans.push(Span::styled(
            format!("   sort {}", app.sort.label()),
            Style::default().add_modifier(Modifier::DIM),
        ));
        let _ = summary;
    }
    spans.push(Span::styled(
        "   [?] help",
        Style::default().add_modifier(Modifier::DIM),
    ));

    Line::from(spans)
}

// ---------------------------------------------------------------- cluster band

/// Three lines that answer the whole-cluster questions on their own.
fn cluster_band(frame: &mut Frame, area: Rect, cluster: &Cluster, summary: &Summary) {
    let block = Block::bordered().title(Span::styled(
        " CLUSTER ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Bars scale with the terminal; the right-hand notes need a fixed budget.
    let bar_width = (inner.width as usize).saturating_sub(58).clamp(10, 40);

    let lines = vec![
        slots_line(summary, bar_width),
        queue_line(cluster, summary, bar_width),
        rate_line(cluster, bar_width),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// "How many compile slots are occupied?" and "is the cluster healthy?"
fn slots_line(summary: &Summary, bar_width: usize) -> Line<'static> {
    let mut spans = vec![
        label("SLOTS"),
        band_value(format!("{}/{}", summary.used_slots, summary.total_slots)),
    ];

    match summary.slot_usage() {
        Some(pct) => {
            spans.push(Span::styled(
                widgets::bar(pct as f32, bar_width),
                Style::default().fg(widgets::ramp(pct as f32)),
            ));
            spans.push(Span::raw(format!(" {pct:>3.0}%  ")));
        }
        None => {
            spans.push(Span::styled(
                widgets::empty_bar(bar_width),
                Style::default().add_modifier(Modifier::DIM),
            ));
            spans.push(Span::raw("   —  "));
        }
    }

    // Health, in the order a reader cares about it.
    spans.push(Span::raw(format!("{} online", summary.nodes_online)));
    if summary.nodes_metrics_stale > 0 {
        spans.push(Span::styled(
            format!(" · {} stale", summary.nodes_metrics_stale),
            Style::default().fg(Color::Yellow),
        ));
    }
    if summary.nodes_offline > 0 {
        spans.push(Span::styled(
            format!(" · {} down", summary.nodes_offline),
            Style::default().fg(Color::LightRed),
        ));
    }
    let missing = summary.nodes_without_agent;
    if missing > 0 {
        spans.push(Span::styled(
            format!(" · {missing} no agent"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    spans.into_iter().collect::<Vec<_>>().into()
}

/// "Is the scheduler queue growing?" — the sparkline shows it, the word says it.
fn queue_line(cluster: &Cluster, summary: &Summary, bar_width: usize) -> Line<'static> {
    let history = &cluster.pending_history;
    let window = history.window(bar_width);
    // Scale to the observed peak: a queue has no natural maximum, and scaling
    // to 100 would flatten a queue of 7 into nothing.
    let peak = history.max().unwrap_or(1.0).max(1.0);
    // One job of movement is noise; two is a trend worth naming.
    let trend = history.trend(1.5);

    let trend_style = match trend {
        Trend::Rising if summary.pending_jobs > 0 => Style::default().fg(Color::Yellow),
        Trend::Falling => Style::default().fg(Color::Green),
        _ => Style::default().add_modifier(Modifier::DIM),
    };

    vec![
        label("QUEUE"),
        band_value(format!("{} wait", summary.pending_jobs)),
        Span::styled(
            widgets::sparkline(&window, peak),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{} {:<9}", trend.arrow(), trend.label()),
            trend_style,
        ),
        // The graph is scaled to its own peak, because a queue has no natural
        // maximum. Saying so is what stops a queue that has sat at seven for
        // the whole window from reading as "full".
        Span::styled(
            format!(" peak {peak:.0}"),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(
            format!(
                "   {} remote · {} local",
                summary.active_jobs, summary.local_jobs
            ),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]
    .into()
}

/// "Is the cluster being used efficiently?" — throughput over time.
fn rate_line(cluster: &Cluster, bar_width: usize) -> Line<'static> {
    let history = &cluster.rate_history;
    let window = history.window(bar_width);
    let peak = history.max().unwrap_or(1.0).max(1.0);
    let now = history.last().unwrap_or(0.0);

    vec![
        label("RATE"),
        band_value(format!("{now:.0}/s")),
        Span::styled(
            widgets::sparkline(&window, peak),
            Style::default().fg(Color::Magenta),
        ),
        Span::styled(
            format!("  peak {peak:.0}/s"),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(
            format!(
                "   {} done since connect",
                cluster.totals.completed_remote + cluster.totals.completed_local
            ),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]
    .into()
}

/// Width of the figure between a band label and its graph.
///
/// Fixed, so all three graphs start in the same column: the band exists to make
/// them comparable at a glance, which a three-cell stagger defeats.
const BAND_VALUE_WIDTH: usize = 10;

fn band_value(text: String) -> Span<'static> {
    Span::raw(format!("{text:<BAND_VALUE_WIDTH$}"))
}

fn label(text: &str) -> Span<'static> {
    Span::styled(
        format!("{text:<7}"),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
}

// ---------------------------------------------------------------- node table

/// Cells a metric column needs beyond its bar: four for the percentage, one
/// for the `%`, and one for the bottleneck `!` — which must be budgeted for
/// even on rows that do not carry it, or it is truncated away on exactly the
/// rows where it matters.
const METRIC_TEXT: usize = 6;

/// Which columns fit, and how wide the bars can be.
///
/// Chosen by breakpoint rather than by proportion: a bar narrower than about
/// four cells conveys nothing, so below each threshold a whole column is
/// dropped instead of every column being squeezed into uselessness.
struct Columns {
    name: u16,
    cpu_bar: usize,
    mem_bar: usize,
    slot_bar: usize,
    load: bool,
    speed: bool,
    temp: bool,
    spark: usize,
}

fn columns(width: u16) -> Columns {
    match width {
        w if w >= 124 => Columns {
            name: 22,
            cpu_bar: 14,
            mem_bar: 12,
            slot_bar: 9,
            load: true,
            speed: true,
            temp: true,
            spark: 12,
        },
        w if w >= 108 => Columns {
            name: 20,
            cpu_bar: 12,
            mem_bar: 10,
            slot_bar: 8,
            load: true,
            speed: true,
            temp: true,
            spark: 0,
        },
        w if w >= 92 => Columns {
            name: 18,
            cpu_bar: 10,
            mem_bar: 8,
            slot_bar: 7,
            load: true,
            speed: false,
            temp: true,
            spark: 0,
        },
        w if w >= 76 => Columns {
            name: 16,
            cpu_bar: 8,
            mem_bar: 7,
            slot_bar: 6,
            load: true,
            speed: false,
            temp: false,
            spark: 0,
        },
        w if w >= 60 => Columns {
            name: 14,
            cpu_bar: 6,
            mem_bar: 5,
            slot_bar: 5,
            load: false,
            speed: false,
            temp: false,
            spark: 0,
        },
        _ => Columns {
            name: 12,
            cpu_bar: 4,
            mem_bar: 4,
            slot_bar: 0,
            load: false,
            speed: false,
            temp: false,
            spark: 0,
        },
    }
}

fn node_table(frame: &mut Frame, area: Rect, app: &App, ui: &mut Ui) {
    let cluster = &app.cluster;
    let cols = columns(area.width);
    let stale_after = cluster.metrics_stale_after;
    let median_speed = cluster.median_speed();

    let mut header = vec![
        Cell::from("NODE"),
        Cell::from(format!(
            "{:^width$}",
            "CPU",
            width = cols.cpu_bar + METRIC_TEXT
        )),
        Cell::from(format!(
            "{:^width$}",
            "MEM",
            width = cols.mem_bar + METRIC_TEXT
        )),
    ];
    if cols.slot_bar > 0 {
        header.push(Cell::from(format!(
            "{:^width$}",
            "SLOTS",
            width = cols.slot_bar + 6
        )));
    }
    if cols.load {
        header.push(Cell::from("LOAD"));
    }
    if cols.speed {
        header.push(Cell::from("SPEED"));
    }
    if cols.temp {
        header.push(Cell::from("TEMP"));
    }
    if cols.spark > 0 {
        header.push(Cell::from("CPU 2min"));
    }

    let rows: Vec<Row> = app
        .sorted_nodes()
        .into_iter()
        .map(|node| node_row(node, &cols, stale_after, median_speed, cluster))
        .collect();

    let mut constraints = vec![
        Constraint::Length(cols.name),
        Constraint::Length(cols.cpu_bar as u16 + METRIC_TEXT as u16),
        Constraint::Length(cols.mem_bar as u16 + METRIC_TEXT as u16),
    ];
    if cols.slot_bar > 0 {
        constraints.push(Constraint::Length(cols.slot_bar as u16 + 6));
    }
    if cols.load {
        constraints.push(Constraint::Length(5));
    }
    if cols.speed {
        constraints.push(Constraint::Length(6));
    }
    if cols.temp {
        constraints.push(Constraint::Length(5));
    }
    if cols.spark > 0 {
        constraints.push(Constraint::Length(cols.spark as u16));
    }

    let title = if cluster.nodes.is_empty() {
        if cluster.is_connected() {
            " no nodes registered with this scheduler ".to_owned()
        } else {
            " waiting for a scheduler ".to_owned()
        }
    } else {
        format!(" {} nodes ", cluster.nodes.len())
    };

    let table = Table::new(rows, constraints)
        .header(
            Row::new(header).style(
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(Block::bordered().title(title))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(1);

    // Setting the selection is what makes the table scroll to keep it visible.
    ui.table.select(app.selected_index());
    frame.render_stateful_widget(table, area, &mut ui.table);
}

fn node_row<'a>(
    node: &Node,
    cols: &Columns,
    stale_after: std::time::Duration,
    median_speed: Option<f64>,
    cluster: &Cluster,
) -> Row<'a> {
    let bottleneck = node.bottleneck();
    let stale = node.has_agent() && node.metrics_stale(stale_after);

    let mut cells = vec![
        Cell::from(name_cell(node, stale, cols.name as usize)),
        Cell::from(metric_cell(
            node.cpu_pct(),
            cols.cpu_bar,
            bottleneck == Some(Bottleneck::Cpu),
            node.offline,
        )),
        Cell::from(metric_cell(
            node.mem_pct(),
            cols.mem_bar,
            bottleneck == Some(Bottleneck::Memory),
            node.offline,
        )),
    ];

    if cols.slot_bar > 0 {
        cells.push(Cell::from(slots_cell(node, cols.slot_bar)));
    }
    if cols.load {
        cells.push(Cell::from(load_cell(node)));
    }
    if cols.speed {
        cells.push(Cell::from(speed_cell(node, median_speed, cluster)));
    }
    if cols.temp {
        cells.push(Cell::from(temp_cell(node)));
    }
    if cols.spark > 0 {
        let window = node.cpu_history.window(cols.spark);
        cells.push(Cell::from(Line::from(Span::styled(
            widgets::sparkline(&window, 100.0),
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let row_style = if node.offline {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::CROSSED_OUT)
    } else if !node.is_busy() {
        // Idle is deliberately quiet, so busy nodes are what the eye lands on.
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };

    Row::new(cells).style(row_style)
}

fn name_cell<'a>(node: &Node, stale: bool, width: usize) -> Line<'a> {
    // At most one badge, in order of how much it should worry the reader.
    let badge = if node.offline {
        Some(("down", Color::LightRed))
    } else if matches!(node.resource_state, ResourceState::Error { .. }) {
        Some(("agent?", Color::LightRed))
    } else if node.identity_mismatch {
        Some(("host?", Color::LightRed))
    } else if node.suspect() {
        Some(("no ack", Color::Yellow))
    } else if stale {
        Some(("stale", Color::Yellow))
    } else if !node.accepts_remote() {
        Some(("local", Color::DarkGray))
    } else {
        None
    };

    // The badge is why the row deserves attention, so the *name* gives up
    // space for it rather than the badge being truncated off the end.
    let badge_width = badge.map_or(0, |(text, _)| text.chars().count() + 1);
    let mut spans = vec![Span::raw(elide(
        node.name(),
        width.saturating_sub(badge_width),
    ))];

    if let Some((text, colour)) = badge {
        spans.push(Span::styled(
            format!(" {text}"),
            Style::default().fg(colour),
        ));
    }
    Line::from(spans)
}

/// Shorten to `max` cells, marking the cut so a truncated name cannot be
/// mistaken for a shorter one.
fn elide(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text.to_owned();
    }
    if max <= 1 {
        return "…".repeat(max);
    }
    text.chars()
        .take(max - 1)
        .chain(std::iter::once('…'))
        .collect()
}

/// A bar plus its percentage, with `!` when this metric is the node's limit.
fn metric_cell<'a>(pct: Option<f32>, width: usize, is_bottleneck: bool, offline: bool) -> Line<'a> {
    match pct {
        Some(pct) if !offline => {
            let mut spans = vec![
                Span::styled(
                    widgets::bar(pct, width),
                    Style::default().fg(widgets::ramp(pct)),
                ),
                Span::raw(format!("{pct:>4.0}%")),
            ];
            if is_bottleneck {
                spans.push(Span::styled(
                    "!",
                    Style::default()
                        .fg(Color::LightRed)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Line::from(spans)
        }
        // No agent, or offline: keep the row's shape without implying a value.
        _ => Line::from(vec![
            Span::styled(
                widgets::empty_bar(width),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("{UNKNOWN:>5}"),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    }
}

fn slots_cell<'a>(node: &Node, width: usize) -> Line<'a> {
    if node.offline {
        return Line::from(Span::styled(
            format!("{:width$}{UNKNOWN:>6}", "", width = width),
            Style::default().fg(Color::DarkGray),
        ));
    }
    let text = format!("{:>2}/{:<2}", node.current_jobs(), node.max_jobs());
    match node.slot_pct() {
        Some(pct) => Line::from(vec![
            Span::styled(
                widgets::bar(pct, width),
                Style::default().fg(if pct >= 100.0 {
                    Color::Cyan
                } else {
                    Color::LightBlue
                }),
            ),
            Span::raw(format!(" {text}")),
        ]),
        None => Line::from(vec![
            Span::styled(
                widgets::empty_bar(width),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(format!(" {text}"), Style::default().fg(Color::DarkGray)),
        ]),
    }
}

/// Load average, coloured by load *per core*, which is what makes a 4-core and
/// a 64-core node comparable.
fn load_cell<'a>(node: &Node) -> Line<'a> {
    let Some(load) = node.load_avg_1() else {
        return dim(format!("{UNKNOWN:>4}"));
    };
    let style = match node.load_per_core() {
        Some(per) if per >= 1.5 => Style::default().fg(Color::LightRed),
        Some(per) if per >= 1.0 => Style::default().fg(Color::Yellow),
        _ => Style::default(),
    };
    Line::from(Span::styled(format!("{load:>4.1}"), style))
}

/// Compile speed, with `!` on a node markedly slower than the cluster median.
fn speed_cell<'a>(node: &Node, median: Option<f64>, cluster: &Cluster) -> Line<'a> {
    // Zero means "has not compiled yet", which is not the same as slow.
    let Some(speed) = node.speed() else {
        return dim(format!("{UNKNOWN:>5}"));
    };
    let slow = cluster.is_slow_outlier(node);
    let _ = median;
    let mut spans = vec![Span::styled(
        format!("{speed:>5.0}"),
        if slow {
            Style::default().fg(Color::LightRed)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        },
    )];
    if slow {
        spans.push(Span::styled(
            "!",
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn temp_cell<'a>(node: &Node) -> Line<'a> {
    match node.temp_c() {
        Some(t) if !node.offline => Line::from(Span::styled(
            format!("{t:>3.0}°"),
            Style::default().fg(widgets::temp_ramp(t)),
        )),
        _ => dim(format!("{UNKNOWN:>4}")),
    }
}

fn dim<'a>(text: String) -> Line<'a> {
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
}

// ---------------------------------------------------------------- footer

fn footer_line(app: &App, summary: &Summary) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let key = Style::default().fg(Color::Cyan);

    let mut spans = vec![
        Span::styled("q", key),
        Span::styled(" quit  ", dim),
        Span::styled("↑↓/jk", key),
        Span::styled(" select  ", dim),
        Span::styled("s", key),
        Span::styled(" sort  ", dim),
        Span::styled("c m l i", key),
        Span::styled(" by cpu/mem/load/jobs  ", dim),
        Span::styled("?", key),
        Span::styled(" help", dim),
    ];

    if let Some(node) = app.selected.and_then(|id| app.cluster.nodes.get(&id)) {
        spans.push(Span::styled(
            format!("   selected {}", node.name()),
            Style::default().fg(Color::Cyan),
        ));
    }
    let _ = summary;
    Line::from(spans)
}

// ---------------------------------------------------------------- help

fn help_overlay(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            "keys",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  q, Ctrl-C    quit"),
        Line::from("  ↑ / k        move up"),
        Line::from("  ↓ / j        move down"),
        Line::from("  PgUp / PgDn  move ten rows"),
        Line::from("  Enter        node detail  (Phase 5)"),
        Line::from("  Esc          close this, or quit"),
        Line::from("  r            redraw"),
        Line::from("  s            cycle sort"),
        Line::from("  c / m / l / i  sort by cpu / mem / load / jobs"),
        Line::from("  ?            toggle this help"),
        Line::from(""),
        Line::from(Span::styled(
            "reading the screen",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  —            not measured; needs icecc-top-agent on that node"),
        Line::from("  !            the metric limiting this node, or a slow outlier"),
        Line::from("  dim row      idle"),
        Line::from("  SPEED        output bytes per user-second; blank until a node compiles"),
        Line::from("  LOAD         coloured by load per core, so node sizes compare"),
    ];

    let width = 74u16.min(area.width.saturating_sub(4)).max(20);
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let [popup] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(" icecc-top help ")
                    .title_alignment(Alignment::Center),
            )
            .style(Style::default().bg(Color::Black)),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use icecc_metrics::Snapshot;
    use icecc_model::ResourceResult;
    use icecc_proto::{Event, SchedulerTarget, Update};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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
            addresses: vec![],
            uptime_secs: 1000,
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
                rx_bytes_per_sec: 0,
                tx_bytes_per_sec: 0,
                interfaces: vec![],
            },
        })
    }

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut ui = Ui::default();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app, &mut ui)).unwrap();
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

    /// A cluster shaped to exercise every visual state.
    fn busy_cluster() -> App {
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(
            1,
            "Name:build01\nIP:10.0.0.1\nMaxJobs:8\nNoRemote:false\nSpeed:3200\n",
        ));
        app.apply(stats(
            2,
            "Name:build02\nIP:10.0.0.2\nMaxJobs:8\nNoRemote:false\nSpeed:2900\n",
        ));
        app.apply(stats(
            3,
            "Name:build03\nIP:10.0.0.3\nMaxJobs:8\nNoRemote:false\nSpeed:3100\n",
        ));
        // build01 CPU-bound, build02 memory-bound, build03 idle.
        app.apply_resource(
            1,
            ResourceResult::Ok(snapshot("build01", 95.0, 500, Some(78.0))),
        );
        app.apply_resource(
            2,
            ResourceResult::Ok(snapshot("build02", 60.0, 950, Some(70.0))),
        );
        app.apply_resource(
            3,
            ResourceResult::Ok(snapshot("build03", 2.0, 100, Some(45.0))),
        );
        for id in [1u32, 2] {
            app.apply(Update::Event(Event::JobBegin {
                job_id: id * 100,
                start_time: 0,
                host_id: id,
            }));
        }
        for _ in 0..10 {
            app.tick_history();
        }
        app
    }

    #[test]
    fn the_cluster_band_answers_the_whole_cluster_questions() {
        let out = render(&busy_cluster(), 130, 24);
        assert!(out.contains("CLUSTER"), "{out}");
        assert!(out.contains("SLOTS"), "{out}");
        assert!(out.contains("QUEUE"), "{out}");
        assert!(out.contains("RATE"), "{out}");
        // Occupancy as a figure and as a bar.
        assert!(out.contains("2/24"), "{out}");
        assert!(out.contains('█'), "expected bars: {out}");
        assert!(out.contains("3 online"), "{out}");
    }

    #[test]
    fn the_queue_trend_is_spelled_out_as_a_word() {
        let mut app = busy_cluster();
        // A queue that grows every tick.
        for i in 0..12 {
            app.apply(Update::Event(Event::GetCs {
                job_id: 9000 + i,
                client_id: 1,
                filename: "x.cpp".into(),
                lang: 1,
            }));
            app.tick_history();
        }
        let out = render(&app, 130, 24);
        assert!(
            out.contains("rising"),
            "a growing queue must say so, not just draw: {out}"
        );
    }

    #[test]
    fn nodes_get_bars_not_just_numbers() {
        let out = render(&busy_cluster(), 130, 24);
        let row = row_for(&out, "build01");
        // A filled bar, a trough, and the figure.
        assert!(row.contains('█'), "{row}");
        assert!(row.contains('░'), "{row}");
        assert!(row.contains("95%"), "{row}");
    }

    #[test]
    fn cpu_bound_and_memory_bound_nodes_are_distinguishable() {
        let out = render(&busy_cluster(), 130, 24);
        let cpu_bound = row_for(&out, "build01");
        let mem_bound = row_for(&out, "build02");

        // Each carries exactly one bottleneck marker, on a different metric.
        assert_eq!(cpu_bound.matches('!').count(), 1, "{cpu_bound}");
        assert_eq!(mem_bound.matches('!').count(), 1, "{mem_bound}");
        // The marker follows the percentage it belongs to.
        assert!(cpu_bound.contains("95%!"), "{cpu_bound}");
        assert!(mem_bound.contains("95%!"), "{mem_bound}");
    }

    #[test]
    fn an_unconstrained_node_carries_no_marker() {
        let out = render(&busy_cluster(), 130, 24);
        assert!(!row_for(&out, "build03").contains('!'));
    }

    #[test]
    fn a_node_history_sparkline_appears_when_there_is_room() {
        let out = render(&busy_cluster(), 130, 24);
        assert!(out.contains("CPU 2min"), "{out}");
        let row = row_for(&out, "build01");
        // Ten ticks of ~95% CPU should draw near the top of the sparkline.
        assert!(
            row.contains('█') && row.chars().filter(|&c| c == '█').count() > 8,
            "{row}"
        );
    }

    #[test]
    fn a_slow_node_is_flagged_against_the_cluster_median() {
        let mut app = busy_cluster();
        // build02 at a fraction of the others' speed.
        app.apply(stats(2, "Speed:300\n"));
        let out = render(&app, 130, 24);
        assert!(
            row_for(&out, "build02").contains('!'),
            "a slow outlier should be marked: {out}"
        );
    }

    #[test]
    fn without_an_agent_the_row_keeps_its_shape_but_claims_nothing() {
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(
            1,
            "Name:build01\nIP:10.0.0.1\nMaxJobs:8\nNoRemote:false\n",
        ));
        let out = render(&app, 130, 24);
        let row = row_for(&out, "build01");
        assert!(row.contains(UNKNOWN), "{row}");
        assert!(row.contains('·'), "expected placeholder bars: {row}");
        assert!(!row.contains('█'), "must not imply a measurement: {row}");
        assert!(out.contains("1 no agent"), "{out}");
    }

    #[test]
    fn health_problems_are_visible_in_the_band_and_on_the_row() {
        let mut app = busy_cluster();
        app.apply(stats(3, "State:Offline\n"));
        let out = render(&app, 130, 24);
        assert!(out.contains("1 down"), "{out}");
        assert!(row_for(&out, "build03").contains("down"), "{out}");
    }

    #[test]
    fn an_offline_node_sinks_below_the_live_ones() {
        let mut app = busy_cluster();
        app.apply(stats(1, "State:Offline\n"));
        let out = render(&app, 130, 24);
        let lines: Vec<&str> = out.lines().collect();
        let offline = lines.iter().position(|l| l.contains("build01")).unwrap();
        let live = lines.iter().position(|l| l.contains("build02")).unwrap();
        assert!(offline > live, "offline node should sink:\n{out}");
    }

    #[test]
    fn the_selected_node_is_named_in_the_footer() {
        let mut app = busy_cluster();
        app.move_selection(1);
        let out = render(&app, 130, 24);
        assert!(out.contains("selected build01"), "{out}");
    }

    #[test]
    fn the_sort_key_is_shown_so_the_order_is_never_a_mystery() {
        let mut app = busy_cluster();
        app.set_sort(crate::app::SortKey::Mem);
        let out = render(&app, 130, 24);
        assert!(out.contains("sort mem"), "{out}");
    }

    #[test]
    fn help_covers_the_screen_and_explains_the_symbols() {
        let mut app = busy_cluster();
        app.show_help = true;
        let out = render(&app, 130, 30);
        assert!(out.contains("icecc-top help"), "{out}");
        assert!(out.contains("cycle sort"), "{out}");
        // The two symbols a newcomer cannot guess.
        assert!(out.contains("not measured"), "{out}");
        assert!(out.contains("slow outlier"), "{out}");
    }

    #[test]
    fn narrow_terminals_drop_columns_rather_than_squeezing_every_bar() {
        let app = busy_cluster();

        let wide = render(&app, 130, 24);
        assert!(wide.contains("SPEED") && wide.contains("TEMP") && wide.contains("CPU 2min"));

        let medium = render(&app, 95, 24);
        assert!(medium.contains("TEMP"), "{medium}");
        assert!(!medium.contains("SPEED"), "{medium}");

        let narrow = render(&app, 70, 24);
        assert!(!narrow.contains("TEMP"), "{narrow}");
        assert!(narrow.contains("CPU"), "{narrow}");
        // Bars survive at every width, because they are the point.
        assert!(narrow.contains('█'), "{narrow}");
    }

    #[test]
    fn a_short_terminal_drops_the_band_to_keep_the_table() {
        let app = busy_cluster();
        let short = render(&app, 130, 9);
        assert!(!short.contains("CLUSTER"), "{short}");
        assert!(short.contains("build01"), "{short}");
    }

    #[test]
    fn renders_at_any_geometry_without_panicking() {
        let mut app = busy_cluster();
        app.show_help = true;
        for (w, h) in [
            (10u16, 3u16),
            (20, 5),
            (40, 8),
            (60, 12),
            (200, 60),
            (300, 100),
        ] {
            let _ = render(&app, w, h);
        }
    }

    #[test]
    fn a_hundred_nodes_render_and_scroll_to_the_selection() {
        let mut app = App::new();
        app.apply(connected());
        for i in 0..120u32 {
            app.apply(stats(
                i,
                &format!(
                    "Name:build{i:03}\nIP:10.0.0.{}\nMaxJobs:16\nNoRemote:false\n",
                    i % 250
                ),
            ));
            app.apply_resource(
                i,
                ResourceResult::Ok(snapshot(&format!("build{i:03}"), 50.0, 500, Some(60.0))),
            );
        }
        app.tick_history();

        let out = render(&app, 130, 30);
        assert!(out.contains("120 nodes"), "{out}");

        // Selecting the last row must bring it into view.
        app.move_selection(-1);
        let out = render(&app, 130, 30);
        assert!(out.contains("build119"), "selection should scroll:\n{out}");
    }

    #[test]
    fn connecting_and_disconnected_states_stay_legible() {
        let mut app = App::new();
        app.apply(Update::Connecting {
            what: "handshaking with build-master:8765".into(),
        });
        let out = render(&app, 130, 24);
        assert!(out.contains("handshaking with build-master:8765"), "{out}");
        assert!(out.contains("waiting for a scheduler"), "{out}");

        app.apply(Update::Disconnected {
            reason: "connection reset".into(),
        });
        let out = render(&app, 130, 24);
        assert!(out.contains("disconnected: connection reset"), "{out}");
    }

    #[test]
    fn stale_metrics_are_badged_but_keep_their_last_values() {
        let mut app = busy_cluster();
        app.cluster.metrics_stale_after = std::time::Duration::from_millis(1);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let out = render(&app, 130, 24);
        assert!(out.contains("stale"), "{out}");
        assert!(row_for(&out, "build01").contains("95%"), "{out}");
    }

    #[test]
    fn a_long_hostname_gives_up_space_rather_than_losing_its_badge() {
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(
            1,
            "Name:a-very-long-build-host-name.corp.example\nIP:10.0.0.1\nMaxJobs:8\nNoRemote:true\n",
        ));
        let out = render(&app, 130, 24);
        // The badge survives; the name is elided and says so.
        assert!(
            out.contains("local"),
            "badge must not be truncated away:\n{out}"
        );
        assert!(out.contains('…'), "an elided name should be marked:\n{out}");
    }

    #[test]
    fn a_short_hostname_is_left_alone() {
        let out = render(&busy_cluster(), 130, 24);
        assert!(row_for(&out, "build01").contains("build01"));
        assert!(!row_for(&out, "build01").contains('…'));
    }

    #[test]
    fn the_three_band_graphs_occupy_the_same_columns() {
        let mut app = busy_cluster();
        // Fill every series, so this checks steady state rather than the
        // right-aligned partial graphs of a freshly started monitor.
        for _ in 0..60 {
            app.tick_history();
        }
        let out = render(&app, 130, 24);
        let lines: Vec<&str> = out.lines().collect();

        // Each band line is "│LABEL  figure    graph…". The graph must span the
        // same columns on all three, or the bars cannot be compared by eye.
        let span_of = |needle: &str| -> (usize, usize) {
            let line = lines.iter().find(|l| l.contains(needle)).unwrap();
            let after_label = line.find(needle).unwrap() + needle.len();
            let glyphs: Vec<usize> = line[after_label..]
                .char_indices()
                .filter(|(_, c)| "░█▁▂▃▄▅▆▇".contains(*c))
                .map(|(i, _)| after_label + i)
                .collect();
            assert!(!glyphs.is_empty(), "no graph on the {needle} line: {line}");
            (*glyphs.first().unwrap(), *glyphs.last().unwrap())
        };

        let slots = span_of("SLOTS");
        assert_eq!(span_of("QUEUE"), slots, "QUEUE is staggered:\n{out}");
        assert_eq!(span_of("RATE"), slots, "RATE is staggered:\n{out}");
    }

    #[test]
    fn a_young_graph_grows_in_from_the_right() {
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(
            1,
            "Name:build01\nIP:10.0.0.1\nMaxJobs:8\nNoRemote:false\n",
        ));
        app.tick_history();

        let out = render(&app, 130, 24);
        let queue = out.lines().find(|l| l.contains("QUEUE")).unwrap();
        let first = queue.find('▁').expect("one sample should be drawn");
        let slots = out.lines().find(|l| l.contains("SLOTS")).unwrap();
        let bar = slots.find('░').expect("slots bar");
        assert!(
            first > bar,
            "a single sample belongs at the right edge, not the left:\n{out}"
        );
    }

    #[test]
    fn eliding_never_exceeds_the_budget_or_panics() {
        for max in 0..8usize {
            let out = elide("build-node-01", max);
            assert!(out.chars().count() <= max, "{max}: {out:?}");
        }
        assert_eq!(elide("abc", 10), "abc");
        assert_eq!(elide("abcdef", 4), "abc…");
    }

    /// Renders a representative cluster, for the README screenshot. Ignored by
    /// default; run it with
    /// `cargo test -p icecc-top screenshot -- --ignored --nocapture`
    /// so the picture in the README is always real output from this code.
    #[test]
    #[ignore]
    fn screenshot() {
        let mut app = App::new();
        app.apply(connected());
        let nodes: [(u32, &str, u32, f32, u64, f32, f64); 6] = [
            (1, "build01", 16, 96.0, 720, 78.0, 3200.0),
            (2, "build02", 16, 61.0, 955, 71.0, 2900.0),
            (3, "build03", 16, 48.0, 410, 63.0, 3100.0),
            (4, "build04", 16, 22.0, 260, 55.0, 940.0),
            (5, "build05", 8, 4.0, 180, 41.0, 3050.0),
            (6, "laptop", 12, 12.0, 620, 52.0, 0.0),
        ];
        for (id, name, max, cpu, mem, temp, speed) in nodes {
            let remote = if name == "laptop" { "true" } else { "false" };
            app.apply(stats(
                id,
                &format!(
                    "Name:{name}\nIP:10.0.0.{id}\nMaxJobs:{max}\nNoRemote:{remote}\nSpeed:{speed}\n"
                ),
            ));
            app.apply_resource(id, ResourceResult::Ok(snapshot(name, cpu, mem, Some(temp))));
        }
        // A node that has dropped out, and one whose agent never answered.
        app.apply(stats(
            7,
            "Name:build06\nIP:10.0.0.7\nMaxJobs:16\nNoRemote:false\n",
        ));
        app.apply(stats(7, "State:Offline\n"));
        app.apply(stats(
            8,
            "Name:build07\nIP:10.0.0.8\nMaxJobs:16\nNoRemote:false\nSpeed:3000\n",
        ));

        // Occupy slots, and leave a few jobs queued.
        let mut job = 1000;
        for (id, _, max, cpu, _, _, _) in nodes {
            let running = (max as f32 * cpu / 100.0).round() as u32;
            for _ in 0..running {
                job += 1;
                app.apply(Update::Event(Event::JobBegin {
                    job_id: job,
                    start_time: 0,
                    host_id: id,
                }));
            }
        }
        for i in 0..7 {
            app.apply(Update::Event(Event::GetCs {
                job_id: 9000 + i,
                client_id: 1,
                filename: "widget.cpp".into(),
                lang: 1,
            }));
        }
        for _ in 0..70 {
            app.tick_history();
        }
        app.move_selection(1);
        app.move_selection(1);

        println!("{}", render(&app, 118, 20));
    }
}
