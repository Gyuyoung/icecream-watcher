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

mod detail;
mod graph;
mod widgets;

use std::time::{Duration, Instant};

use icecc_model::{Bottleneck, Cluster, ConnectionState, Node, ResourceState, Summary, Trend};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::app::App;

/// Shown when a metric is not available. Distinct from `0`.
pub(crate) const UNKNOWN: &str = "—";

/// Persistent widget state, so scrolling and selection survive between frames.
#[derive(Default)]
pub struct Ui {
    table: TableState,
    /// How far the detail view could be scrolled at the last paint. Recorded
    /// here because only rendering knows the viewport size, and the caller uses
    /// it to clamp the stored offset — without that, holding a scroll key past
    /// the end means pressing back the same number of times to return.
    pub detail_max_scroll: u16,
}

pub fn draw(frame: &mut Frame, app: &App, ui: &mut Ui) {
    let area = frame.area();
    let cluster = &app.cluster;
    let summary = cluster.summary();

    // The cluster band is the first thing to go when the terminal is short:
    // without room for the table it would answer questions about rows nobody
    // can see. Given room, each series gets extra rows and its graph is drawn
    // in braille dots, which only pay off with height (ui::graph).
    let band_height = band_height(area.height);
    let areas = Layout::vertical([
        Constraint::Length(1),           // header
        Constraint::Length(band_height), // cluster band
        Constraint::Min(3),              // node table
        Constraint::Length(1),           // footer
    ])
    .split(area);

    frame.render_widget(Paragraph::new(header_line(app, &summary)), areas[0]);

    match app.detail_node() {
        // The detail view takes the band's space as well as the table's: it has
        // per-core bars to show, and the cluster summary is one Esc away.
        Some(node) => {
            let body = areas[1].union(areas[2]);
            ui.detail_max_scroll = detail::max_scroll(node, cluster, body);
            detail::draw(frame, body, node, cluster, app.detail_scroll);
        }
        None => {
            if band_height > 0 {
                cluster_band(frame, areas[1], cluster, &summary);
            }
            node_table(frame, areas[2], app, ui);
        }
    }

    frame.render_widget(Paragraph::new(footer_line(app, &summary)), areas[3]);

    if app.show_help {
        help_overlay(frame, area);
    }
}

/// Silence longer than this is worth stating in the header. Chosen above the
/// keepalive detection window (~35 s) so anything shown here is a genuinely
/// idle cluster rather than a link that is about to be declared dead.
const QUIET_AFTER: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------- header

fn header_line(app: &App, summary: &Summary) -> Line<'static> {
    let cluster = &app.cluster;
    let mut spans = vec![
        Span::styled(
            "icecream-watcher",
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
            // A different scheduler answering discovery means every host id,
            // node and counter now belongs to another cluster. Saying so beats
            // letting the figures change identity behind the user's back.
            if let Some(first) = cluster.moved_from() {
                spans.push(Span::styled(
                    format!("  ⇄ moved from {first}"),
                    Style::default().fg(Color::Yellow),
                ));
            }
            // Scheduler traffic is change-driven, so silence is ambiguous:
            // this distinguishes "nothing is happening" from "nothing is
            // arriving". TCP keepalive turns the latter into a disconnect
            // within ~35 s, so a figure larger than that is a quiet cluster.
            if let Some(quiet) = app.quiet_for() {
                if quiet >= QUIET_AFTER {
                    spans.push(Span::styled(
                        format!("  quiet {}", widgets::brief_duration(quiet.as_secs())),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
            }
        }
        ConnectionState::Connecting { what } => spans.push(Span::styled(
            format!("{what}…"),
            Style::default().fg(Color::Yellow),
        )),
        ConnectionState::Disconnected {
            reason,
            attempt,
            retry_at,
        } => {
            spans.push(Span::styled(
                format!("disconnected: {reason}"),
                Style::default().fg(Color::LightRed),
            ));
            // Without this the screen says only that something is wrong, and
            // gives no sign that anything is still being tried.
            let left = retry_at.saturating_duration_since(Instant::now());
            let when = if left.is_zero() {
                "now".to_owned()
            } else {
                format!("in {}", widgets::duration(left.as_secs().max(1)))
            };
            spans.push(Span::styled(
                format!("  retry {when} (attempt {attempt})"),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
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

/// How tall the whole band should be, borders included.
///
/// Progressive rather than proportional: the node table always keeps priority,
/// so the band only grows once there is room the table does not need. The
/// thresholds are the point at which spending three more rows still leaves a
/// usable list.
fn band_height(total: u16) -> u16 {
    match total {
        h if h >= 40 => 3 * SERIES_ROWS_LARGE as u16 + 2,
        h if h >= 24 => 3 * SERIES_ROWS_TALL as u16 + 2,
        h if h >= 12 => 3 + 2,
        _ => 0,
    }
}

/// Rows per series once the band is tall enough for dot graphs.
const SERIES_ROWS_TALL: usize = 2;
/// Rows per series on a large terminal.
const SERIES_ROWS_LARGE: usize = 3;

/// Width reserved for a series' label, figure and notes in the tall layout.
///
/// Fixed for the same reason [`BAND_VALUE_WIDTH`] is: the band exists so the
/// three graphs can be read against each other, which a ragged left edge
/// defeats. It is *enforced* rather than assumed — a note one character over
/// budget would shunt its graph sideways and push the newest samples off the
/// right-hand edge, which is both a stagger and a silent loss of exactly the
/// data the eye goes to first.
const BAND_LEFT: usize = 42;

/// Below this the graph is too narrow to be worth the rows, so the band falls
/// back to its compact one-line-per-series layout.
const BAND_MIN_GRAPH: usize = 20;

/// Three lines that answer the whole-cluster questions on their own.
fn cluster_band(frame: &mut Frame, area: Rect, cluster: &Cluster, summary: &Summary) {
    let block = Block::bordered().title(Span::styled(
        " CLUSTER ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = inner.height as usize / 3;
    let graph_width = (inner.width as usize).saturating_sub(BAND_LEFT);
    if rows >= SERIES_ROWS_TALL && graph_width >= BAND_MIN_GRAPH {
        let mut lines = Vec::with_capacity(rows * 3);
        lines.extend(slots_block(summary, cluster, graph_width, rows));
        lines.extend(queue_block(cluster, summary, graph_width, rows));
        lines.extend(rate_block(cluster, graph_width, rows));
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    // Compact: one line per series, with block sparklines. A single row of
    // braille would be four levels, which is worse than the eight a block
    // sparkline gives — so height, not style, decides which is drawn.
    let bar_width = (inner.width as usize).saturating_sub(58).clamp(10, 40);
    let lines = vec![
        slots_line(summary, bar_width),
        queue_line(cluster, summary, bar_width),
        rate_line(cluster, bar_width),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Lay one series out as a fixed-width text column beside a dot graph.
///
/// `text` supplies as many lines as it has to say; the graph spans every row,
/// so the reading order is "what is it, then what has it been doing".
fn series_block(
    text: Vec<Vec<Span<'static>>>,
    glyphs: Vec<String>,
    style_for_row: impl Fn(usize) -> Style,
    rows: usize,
) -> Vec<Line<'static>> {
    (0..rows)
        .map(|r| {
            let mut spans = fit(text.get(r).cloned().unwrap_or_default(), BAND_LEFT);
            if let Some(glyphs) = glyphs.get(r) {
                spans.push(Span::styled(glyphs.clone(), style_for_row(r)));
            }
            Line::from(spans)
        })
        .collect()
}

/// Force a run of spans to exactly `width` characters, padding or eliding.
///
/// Eliding marks the cut with `…`, so a shortened note cannot be read as the
/// whole of a shorter one — and nothing downstream of it can be displaced.
fn fit(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if used == width {
        return spans;
    }
    if used < width {
        let mut spans = spans;
        spans.push(Span::raw(" ".repeat(width - used)));
        return spans;
    }

    let mut out = Vec::with_capacity(spans.len());
    let mut left = width.saturating_sub(1); // room for the ellipsis
    for span in spans {
        let len = span.content.chars().count();
        if len <= left {
            left -= len;
            out.push(span);
            continue;
        }
        let kept: String = span.content.chars().take(left).collect();
        let style = span.style;
        out.push(Span::styled(kept, style));
        break;
    }
    out.push(Span::raw("…"));
    out
}

/// Samples for a dot graph `width` characters wide, on a fixed two-minute axis.
fn dots(history: &icecc_model::History, width: usize) -> Vec<f32> {
    history.stretched(width * graph::CELL_COLS)
}

/// "How many compile slots are occupied?", with its two minutes of history.
fn slots_block(
    summary: &Summary,
    cluster: &Cluster,
    width: usize,
    rows: usize,
) -> Vec<Line<'static>> {
    let pct = summary.slot_usage();
    let mut head = vec![
        label("SLOTS"),
        band_value(format!("{}/{}", summary.used_slots, summary.total_slots)),
    ];
    match pct {
        Some(p) => head.push(Span::styled(
            format!("{p:>3.0}%"),
            Style::default().fg(widgets::ramp(p as f32)),
        )),
        None => head.push(Span::raw("  —")),
    }

    let mut health = vec![Span::styled(
        format!("{:<7}{} online", "", summary.nodes_online),
        Style::default().add_modifier(Modifier::DIM),
    )];
    if summary.nodes_metrics_stale > 0 {
        health.push(Span::styled(
            format!(" · {} stale", summary.nodes_metrics_stale),
            Style::default().fg(Color::Yellow),
        ));
    }
    if summary.nodes_offline > 0 {
        health.push(Span::styled(
            format!(" · {} down", summary.nodes_offline),
            Style::default().fg(Color::LightRed),
        ));
    }
    if summary.nodes_without_agent > 0 {
        health.push(Span::styled(
            format!(" · {} no agent", summary.nodes_without_agent),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    // Utilisation has a real maximum, so the gradient by height means what it
    // means on the bars: near the top is the part worth worrying about.
    let glyphs = graph::area(&dots(&cluster.slots_history, width), 100.0, width, rows);
    series_block(
        vec![head, health],
        glyphs,
        |r| Style::default().fg(graph::row_colour(r, rows)),
        rows,
    )
}

/// "Is the scheduler queue growing?" — the shape shows it, the word says it.
fn queue_block(
    cluster: &Cluster,
    summary: &Summary,
    width: usize,
    rows: usize,
) -> Vec<Line<'static>> {
    let history = &cluster.pending_history;
    let peak = history.max().unwrap_or(1.0).max(1.0);
    let trend = history.trend(1.5);
    let trend_style = match trend {
        Trend::Rising if summary.pending_jobs > 0 => Style::default().fg(Color::Yellow),
        Trend::Falling => Style::default().fg(Color::Green),
        _ => Style::default().add_modifier(Modifier::DIM),
    };

    let head = vec![
        label("QUEUE"),
        band_value(format!("{} wait", summary.pending_jobs)),
        Span::styled(format!("{} {}", trend.arrow(), trend.label()), trend_style),
    ];
    let notes = vec![Span::styled(
        format!(
            "{:<7}peak {peak:.0} · {} remote · {} local",
            "", summary.active_jobs, summary.local_jobs
        ),
        Style::default().add_modifier(Modifier::DIM),
    )];

    // Peak-scaled, so a flat colour rather than the utilisation gradient: the
    // top of this graph is "the most we have seen", not "full".
    let glyphs = graph::area(&dots(history, width), peak, width, rows);
    series_block(
        vec![head, notes],
        glyphs,
        |_| Style::default().fg(Color::Cyan),
        rows,
    )
}

/// "Is the cluster being used efficiently?" — throughput over time.
fn rate_block(cluster: &Cluster, width: usize, rows: usize) -> Vec<Line<'static>> {
    let history = &cluster.rate_history;
    let peak = history.max().unwrap_or(1.0).max(1.0);
    let now = history.last().unwrap_or(0.0);

    let head = vec![label("RATE"), band_value(format!("{now:.0}/s"))];
    let notes = vec![Span::styled(
        format!(
            "{:<7}peak {peak:.0}/s · {} done since connect",
            "",
            cluster.totals.completed_remote + cluster.totals.completed_local
        ),
        Style::default().add_modifier(Modifier::DIM),
    )];

    let glyphs = graph::area(&dots(history, width), peak, width, rows);
    series_block(
        vec![head, notes],
        glyphs,
        |_| Style::default().fg(Color::Magenta),
        rows,
    )
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

/// Which columns fit, and how wide the bars can be.
///
/// Chosen by breakpoint rather than by proportion: a bar narrower than about
/// four cells conveys nothing, so below each threshold a whole column is
/// dropped instead of every column being squeezed into uselessness.
struct Columns {
    name: u16,
    /// Slots in use and slots configured, as plain numbers.
    cur_max: bool,
    slot_bar: usize,
    /// `IN` / `OUT`: work actually done here, and work sent from here.
    jobs: bool,
    load: bool,
    speed: bool,
    /// Width of the trailing slot-occupancy graph, 0 to drop it.
    graph: usize,
}

/// Which columns fit, and how wide the bar can be.
///
/// Chosen by breakpoint rather than by proportion: a bar narrower than about
/// four cells conveys nothing, so below each threshold a whole column is
/// dropped instead of every column being squeezed into uselessness. The graph
/// then takes whatever is left, so a wide terminal spends its space on history
/// rather than on padding.
fn columns(width: u16) -> Columns {
    let mut cols = match width {
        w if w >= 108 => Columns {
            name: 24,
            cur_max: true,
            slot_bar: 16,
            jobs: true,
            load: true,
            speed: true,
            graph: 0,
        },
        w if w >= 92 => Columns {
            name: 22,
            cur_max: true,
            slot_bar: 12,
            jobs: true,
            load: true,
            speed: true,
            graph: 0,
        },
        w if w >= 76 => Columns {
            name: 20,
            cur_max: true,
            slot_bar: 10,
            jobs: false,
            load: true,
            speed: true,
            graph: 0,
        },
        w if w >= 60 => Columns {
            name: 16,
            cur_max: true,
            slot_bar: 8,
            jobs: false,
            load: true,
            speed: false,
            graph: 0,
        },
        _ => Columns {
            name: 12,
            cur_max: false,
            slot_bar: 6,
            jobs: false,
            load: false,
            speed: false,
            graph: 0,
        },
    };

    let fixed = cols.name as usize
        + cols.slot_bar
        + if cols.cur_max { (COUNT_WIDTH as usize + 1) * 2 } else { 0 }
        + if cols.jobs { (JOBS_WIDTH as usize + 1) * 2 } else { 0 }
        + if cols.load { LOAD_WIDTH as usize + 1 } else { 0 }
        + if cols.speed { SPEED_WIDTH as usize + 1 } else { 0 };
    // Two for the borders, one for the column gap before the graph, and a
    // little slack so the graph never collides with the right-hand border.
    let mut spare = (width as usize).saturating_sub(fixed + 6);

    // Names come first with space going spare: an elided hostname costs the
    // reader more than a shorter graph does, and build hosts are often named at
    // length. Only once every column already fits, though — on a narrow
    // terminal the same rule hands most of the screen to one column.
    if width >= 108 {
        let widen = (spare / 2).min(NAME_MAX.saturating_sub(cols.name as usize));
        cols.name += widen as u16;
        spare -= widen;
    }

    if spare >= MIN_GRAPH {
        cols.graph = spare.min(MAX_GRAPH);
    }
    cols
}

/// Width of the `ACTIVE` and `MAX` job-count columns.
const COUNT_WIDTH: u16 = 6;
const JOBS_WIDTH: u16 = 6;
const LOAD_WIDTH: u16 = 5;
const SPEED_WIDTH: u16 = 6;
/// Below this a history graph shows too little time to be worth a column.
const MIN_GRAPH: usize = 10;
/// Beyond this the graph stops growing. Two dot columns per character means 60
/// cells already draw the whole two-minute buffer one sample to a dot; wider is
/// upscaling, and a table row is a strip rather than a chart.
const MAX_GRAPH: usize = 60;
/// How wide a hostname column may grow when there is space going spare.
const NAME_MAX: usize = 38;

fn node_table(frame: &mut Frame, area: Rect, app: &App, ui: &mut Ui) {
    let cluster = &app.cluster;
    let cols = columns(area.width);
    let stale_after = cluster.metrics_stale_after;
    let median_speed = cluster.median_speed();

    let mut header = vec![Cell::from("NODE")];
    if cols.cur_max {
        header.push(Cell::from(format!(
            "{:>width$}",
            "ACTIVE",
            width = COUNT_WIDTH as usize
        )));
        header.push(Cell::from(format!(
            "{:>width$}",
            "MAX",
            width = COUNT_WIDTH as usize
        )));
    }
    header.push(Cell::from(format!("{:<width$}", "JOBS", width = cols.slot_bar)));
    if cols.jobs {
        header.push(Cell::from(format!("{:>width$}", "IN", width = JOBS_WIDTH as usize)));
        header.push(Cell::from(format!("{:>width$}", "OUT", width = JOBS_WIDTH as usize)));
    }
    if cols.load {
        header.push(Cell::from("LOAD"));
    }
    if cols.speed {
        header.push(Cell::from("SPEED"));
    }
    if cols.graph > 0 {
        header.push(Cell::from("JOBS 2min"));
    }

    let rows: Vec<Row> = app
        .sorted_nodes()
        .into_iter()
        .map(|node| node_row(node, &cols, stale_after, median_speed, cluster))
        .collect();

    let mut constraints = vec![Constraint::Length(cols.name)];
    if cols.cur_max {
        constraints.push(Constraint::Length(COUNT_WIDTH));
        constraints.push(Constraint::Length(COUNT_WIDTH));
    }
    constraints.push(Constraint::Length(cols.slot_bar as u16));
    if cols.jobs {
        constraints.push(Constraint::Length(JOBS_WIDTH));
        constraints.push(Constraint::Length(JOBS_WIDTH));
    }
    if cols.load {
        constraints.push(Constraint::Length(LOAD_WIDTH));
    }
    if cols.speed {
        constraints.push(Constraint::Length(SPEED_WIDTH));
    }
    if cols.graph > 0 {
        constraints.push(Constraint::Length(cols.graph as u16));
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
    let stale = node.has_agent() && node.metrics_stale(stale_after);

    let mut cells = vec![Cell::from(name_cell(node, stale, cols.name as usize))];
    if cols.cur_max {
        let w = COUNT_WIDTH as usize;
        cells.push(Cell::from(count_cell(node, u64::from(node.current_jobs()), w)));
        cells.push(Cell::from(count_cell(node, u64::from(node.max_jobs()), w)));
    }
    cells.push(Cell::from(slots_cell(node, cluster, cols.slot_bar)));

    if cols.jobs {
        let w = JOBS_WIDTH as usize;
        cells.push(Cell::from(count_cell(node, node.jobs_in, w)));
        cells.push(Cell::from(count_cell(node, node.jobs_out, w)));
    }
    if cols.load {
        cells.push(Cell::from(load_cell(node)));
    }
    if cols.speed {
        cells.push(Cell::from(speed_cell(node, median_speed, cluster)));
    }
    if cols.graph > 0 {
        cells.push(Cell::from(slots_graph_cell(node, cols.graph)));
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

/// A counter since connect, right-aligned. Zero is dimmed rather than hidden:
/// "this node has compiled nothing" is an answer, and a blank is not.
fn count_cell<'a>(node: &Node, value: u64, width: usize) -> Line<'a> {
    if node.offline {
        return dim(format!("{UNKNOWN:>width$}"));
    }
    let text = format!("{value:>width$}");
    if value == 0 {
        dim(text)
    } else {
        Line::from(Span::raw(text))
    }
}

/// Two minutes of this node's slot occupancy, in braille dots.
///
/// One character row is four levels rather than the eight a block sparkline
/// gives, but twice the horizontal resolution — so a row-height strip shows
/// twice the time at half the vertical detail. For "has this node been busy, and
/// is it busier now than a minute ago" that is the better trade; the exact
/// figure is one column to the left.
fn slots_graph_cell<'a>(node: &Node, width: usize) -> Line<'a> {
    if node.offline {
        return dim(" ".repeat(width));
    }
    let samples = node.slots_history.stretched(width * graph::CELL_COLS);
    let glyphs = graph::area(&samples, 100.0, width, 1);
    Line::from(Span::styled(
        glyphs.into_iter().next().unwrap_or_default(),
        Style::default().fg(widgets::node_colour(node.name())),
    ))
}

fn name_cell<'a>(node: &Node, stale: bool, width: usize) -> Line<'a> {
    // At most one badge, in order of how much it should worry the reader.
    let badge: Option<(String, Color)> = if node.offline {
        // How long it has been down, not just that it is: a node that dropped
        // ten seconds ago is a live incident, one down for three hours is
        // furniture, and the reaction to each is different.
        let text = match node.downtime() {
            Some(d) => format!("down {}", widgets::brief_duration(d.as_secs())),
            None => "down".to_owned(),
        };
        Some((text, Color::LightRed))
    } else if matches!(node.resource_state, ResourceState::Error { .. }) {
        Some(("agent?".to_owned(), Color::LightRed))
    } else if node.identity_mismatch {
        Some(("host?".to_owned(), Color::LightRed))
    } else if node.suspect() {
        Some(("no ack".to_owned(), Color::Yellow))
    } else if let Some(b) = node.bottleneck() {
        // The table no longer carries CPU and memory columns — they are not
        // Icecream figures — but "why is this node not taking more work" is,
        // and it is one of the questions this screen exists to answer. The
        // conclusion stays; the raw gauges live in the detail view.
        match b {
            Bottleneck::Memory => Some(("mem!".to_owned(), Color::LightRed)),
            Bottleneck::Cpu => Some(("cpu!".to_owned(), Color::LightRed)),
            // A full slot count is already obvious from the bar beside it.
            Bottleneck::Slots => None,
        }
    } else if stale {
        Some(("stale".to_owned(), Color::Yellow))
    } else if !node.accepts_remote() {
        Some(("local".to_owned(), Color::DarkGray))
    } else {
        None
    };

    // The badge is why the row deserves attention, so the *name* gives up
    // space for it rather than the badge being truncated off the end.
    let badge_width = badge
        .as_ref()
        .map_or(0, |(text, _)| text.chars().count() + 1);
    // The name is where the colour is most useful: it is what the eye looks
    // for when scanning back to a node it was already watching. State still
    // wins — an offline row is grey whatever colour its name would have been,
    // because "which machine is this" matters less than "this one is gone".
    let name_style = if node.offline {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(widgets::node_colour(node.name()))
    };
    let mut spans = vec![Span::styled(
        elide(node.name(), width.saturating_sub(badge_width)),
        name_style,
    )];

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
pub(crate) fn elide(text: &str, max: usize) -> String {
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

/// One cell per compile slot, so slots can be counted rather than estimated.
///
/// A proportional bar answers "how full", which the `CUR`/`MAX` columns already
/// do in figures. What a bar cannot do is show *which* slots are busy and who is
/// using them, and that needs a cell per slot: colour is a property of a
/// character, so two slots sharing one cell cannot be told apart however many
/// dots it has.
///
/// An occupied slot is drawn as a filled left dot-column with the baseline
/// carrying on to its right — a bar with a built-in gap — so a run of busy slots
/// stays countable instead of merging into one block. Each is coloured by the
/// node that *submitted* the job, the way `icecream-sundae` attributes work, so
/// a glance says "eight of these are build02's". Jobs already running when the
/// monitor attached have no known submitter and take the compiling node's own
/// colour.
fn slots_cell<'a>(node: &Node, cluster: &Cluster, width: usize) -> Line<'a> {
    if node.offline || width == 0 {
        return dim(" ".repeat(width));
    }
    let max = node.max_jobs() as usize;
    if max == 0 {
        // No slot count from the scheduler: a dotted placeholder, which is
        // visibly not a meter reading zero.
        return Line::from(Span::styled(
            widgets::empty_bar(width),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // More slots than cells: one per slot would be a smear, so fall back to a
    // proportional bar and let CUR/MAX carry the count.
    if max > width {
        let pct = node.slot_pct().unwrap_or(0.0);
        return Line::from(Span::styled(
            graph::bar(pct, width),
            Style::default().fg(widgets::node_colour(node.name())),
        ));
    }

    let mut spans = Vec::with_capacity(width);
    for job_id in node.active_jobs.iter().take(max) {
        let client = cluster
            .jobs
            .get(job_id)
            .and_then(|job| job.client_id)
            .and_then(|id| cluster.nodes.get(&id))
            .map(|n| n.name().to_owned());
        let colour = widgets::node_colour(client.as_deref().unwrap_or(node.name()));
        spans.push(Span::styled(SLOT_BUSY.to_string(), Style::default().fg(colour)));
    }
    let free = max.saturating_sub(node.active_jobs.len());
    if free > 0 {
        spans.push(Span::styled(
            SLOT_FREE.to_string().repeat(free),
            Style::default().fg(Color::DarkGray),
        ));
    }
    // Pad to the column so the graphs to the right of every row line up.
    if max < width {
        spans.push(Span::raw(" ".repeat(width - max)));
    }
    Line::from(spans)
}

/// An occupied slot: the left dot-column filled, the baseline continuing right.
/// The gap is what keeps a run of busy slots countable.
const SLOT_BUSY: char = '⣇';
/// A free slot: baseline only, so the meter's full extent stays visible.
const SLOT_FREE: char = '⣀';

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

fn dim<'a>(text: String) -> Line<'a> {
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
}

// ---------------------------------------------------------------- footer

fn footer_line(app: &App, summary: &Summary) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let key = Style::default().fg(Color::Cyan);
    let _ = summary;

    // Offer only the keys that do something in the current view; advertising
    // a sort key that the detail view ignores would be a small lie.
    if let Some(node) = app.detail_node() {
        return Line::from(vec![
            Span::styled("Esc", key),
            Span::styled(" back  ", dim),
            Span::styled("↑↓/jk", key),
            Span::styled(" scroll  ", dim),
            Span::styled("q", key),
            Span::styled(" quit", dim),
            Span::styled(
                format!("   {}", node.name()),
                Style::default().fg(Color::Cyan),
            ),
        ]);
    }

    let mut spans = vec![
        Span::styled("q", key),
        Span::styled(" quit  ", dim),
        Span::styled("↑↓/jk", key),
        Span::styled(" select  ", dim),
        Span::styled("Enter", key),
        Span::styled(" detail  ", dim),
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
        Line::from("  Enter        open or close the node detail view"),
        Line::from("  Esc          close this, leave the detail view, or quit"),
        Line::from("  r            redraw; while disconnected, retry now"),
        Line::from("  s            cycle sort"),
        Line::from("  c / m / l / i  sort by cpu / mem / load / jobs"),
        Line::from("  ?            toggle this help"),
        Line::from(""),
        Line::from(Span::styled(
            "reading the screen",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  —            not measured; needs icecream-watcher-agent on that node"),
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
                    .title(" icecream-watcher help ")
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

    fn is_braille(c: char) -> bool {
        ('\u{2800}'..='\u{28FF}').contains(&c)
    }

    fn is_graph_glyph(c: char) -> bool {
        is_braille(c) || "░█▁▂▃▄▅▆▇".contains(c)
    }

    /// The lines one band series occupies: its labelled line plus the rows
    /// under it, up to the next series or the box edge. A tall series draws its
    /// label on the first row and its graph across all of them, so anything
    /// that looks at the labelled line alone misses most of the graph.
    fn series_lines<'a>(out: &'a str, needle: &str) -> Vec<&'a str> {
        let lines: Vec<&str> = out.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no {needle} line in:\n{out}"));
        let mut block = vec![lines[at]];
        for line in lines.iter().skip(at + 1) {
            if ["SLOTS", "QUEUE", "RATE"].iter().any(|l| line.contains(l)) || line.contains('└') {
                break;
            }
            block.push(line);
        }
        block
    }

    /// Character columns a series' graph occupies, checked across every row it
    /// spans.
    ///
    /// Columns, not byte offsets: braille cells and `→` are three bytes each,
    /// so byte positions drift between lines and would report a stagger that is
    /// not there. Every row is checked because the rows that carry the notes
    /// are the ones whose text can overrun its budget and shunt the graph — and
    /// a graph shunted right loses its newest samples off the screen edge,
    /// which is the half the eye goes to first.
    ///
    /// Blank braille cells count, because the question is where the graph
    /// *area* sits, not where the data in it happens to reach.
    fn graph_span(out: &str, needle: &str) -> (usize, usize) {
        let mut span: Option<(usize, usize)> = None;
        for line in series_lines(out, needle) {
            let cols: Vec<usize> = line
                .chars()
                .enumerate()
                .filter(|(_, c)| is_graph_glyph(*c))
                .map(|(i, _)| i)
                .collect();
            let Some(&first) = cols.first() else { continue };
            let here = (first, *cols.last().unwrap());
            match span {
                None => span = Some(here),
                Some(seen) => assert_eq!(
                    here, seen,
                    "the {needle} graph is staggered between its own rows: {line}"
                ),
            }
        }
        span.unwrap_or_else(|| panic!("no graph in the {needle} block of:\n{out}"))
    }

    /// Character columns where a dot graph actually has dots, across the whole
    /// series block and ignoring blank cells.
    fn drawn_span(out: &str, needle: &str) -> Option<(usize, usize)> {
        let cols: Vec<usize> = series_lines(out, needle)
            .iter()
            .flat_map(|line| {
                line.chars()
                    .enumerate()
                    .filter(|(_, c)| is_braille(*c) && *c != '\u{2800}')
                    .map(|(i, _)| i)
            })
            .collect();
        Some((*cols.iter().min()?, *cols.iter().max()?))
    }

    /// Foreground colours used across the row that names `name`.
    ///
    /// The text-only `render` helper cannot see styling, and a colour scheme
    /// that silently stopped being applied would look identical to one that
    /// works.
    fn row_colours(app: &App, width: u16, height: u16, name: &str) -> Vec<Color> {
        let mut ui = Ui::default();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app, &mut ui)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let row = (0..buf.area.height)
            .find(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, *y)].symbol().to_owned())
                    .collect::<String>()
                    .contains(name)
            })
            .unwrap_or_else(|| panic!("no row for {name}"));

        (0..buf.area.width)
            .map(|x| buf[(x, row)].style().fg.unwrap_or(Color::Reset))
            .collect()
    }

    /// The two figures immediately left of the meter: ACTIVE and MAX. Read by
    /// position rather than by exact spacing, so a column width change is not a
    /// test failure.
    fn counts_before_meter(row: &str) -> Vec<String> {
        let head: String = row
            .chars()
            .take_while(|c| *c != SLOT_BUSY && *c != SLOT_FREE && *c != '⣿')
            .collect();
        let tokens: Vec<&str> = head.split_whitespace().collect();
        tokens
            .iter()
            .rev()
            .take(2)
            .rev()
            .map(|t| (*t).to_owned())
            .collect()
    }

    /// The slot meter on a row: the first run of slot glyphs, which stops at
    /// the padding before the next column. Counting these across the whole row
    /// would also count the history graph's baseline dots.
    fn slot_meter(row: &str) -> String {
        row.chars()
            .skip_while(|c| *c != SLOT_BUSY && *c != SLOT_FREE)
            .take_while(|c| *c == SLOT_BUSY || *c == SLOT_FREE)
            .collect()
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
        // Occupancy as a figure, and as two minutes of shape beside it.
        assert!(out.contains("2/24"), "{out}");
        assert!(
            series_lines(&out, "SLOTS").iter().any(|l| l.chars().any(is_braille)),
            "expected a graph beside the figures: {out}"
        );
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
        // One slot busy, the rest drawn but free, and the figures beside them.
        assert_eq!(slot_meter(row), "⣇⣀⣀⣀⣀⣀⣀⣀", "{row}");
        assert_eq!(counts_before_meter(row), ["1", "8"], "ACTIVE and MAX: {row}");
    }

    #[test]
    fn every_slot_gets_its_own_cell_so_they_can_be_counted() {
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(1, "Name:build01\nIP:10.0.0.1\nMaxJobs:8\nNoRemote:false\n"));
        for job in 0..3u32 {
            app.apply(Update::Event(Event::JobBegin {
                job_id: 500 + job,
                start_time: 0,
                host_id: 1,
            }));
        }
        let out = render(&app, 130, 24);
        let row = row_for(&out, "build01");
        assert_eq!(slot_meter(row), "⣇⣇⣇⣀⣀⣀⣀⣀", "{row}");
    }

    #[test]
    fn a_busy_slot_is_coloured_by_whoever_submitted_the_job() {
        // The bar answers "how full" and the figures answer it better; what a
        // per-slot meter adds is *whose* work is running here.
        let mut app = App::new();
        app.apply(connected());
        for id in 1..=3u32 {
            app.apply(stats(
                id,
                &format!("Name:build{id:02}\nIP:10.0.0.{id}\nMaxJobs:8\nNoRemote:false\n"),
            ));
        }
        // build01 compiles one job for build02 and one for build03.
        for (job, client) in [(601u32, 2u32), (602, 3)] {
            app.apply(Update::Event(Event::GetCs {
                job_id: job,
                client_id: client,
                filename: "x.cpp".into(),
                lang: 1,
            }));
            app.apply(Update::Event(Event::JobBegin {
                job_id: job,
                start_time: 0,
                host_id: 1,
            }));
        }

        let colours = row_colours(&app, 130, 30, "build01");
        let busy: Vec<Color> = {
            let mut ui = Ui::default();
            let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
            terminal.draw(|f| draw(f, &app, &mut ui)).unwrap();
            let buf = terminal.backend().buffer().clone();
            let row = (0..buf.area.height)
                .find(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, *y)].symbol().to_owned())
                        .collect::<String>()
                        .contains("build01")
                })
                .unwrap();
            (0..buf.area.width)
                .filter(|x| buf[(*x, row)].symbol() == SLOT_BUSY.to_string())
                .map(|x| buf[(x, row)].style().fg.unwrap_or(Color::Reset))
                .collect()
        };
        assert_eq!(busy.len(), 2, "{colours:?}");
        assert_ne!(
            busy[0], busy[1],
            "two clients' jobs should not look like one client's"
        );
        assert_eq!(busy[0], widgets::node_colour("build02"));
        assert_eq!(busy[1], widgets::node_colour("build03"));
    }

    #[test]
    fn more_slots_than_cells_falls_back_to_a_proportional_bar() {
        // A 128-slot node cannot have a cell each in a 16-column budget; one
        // smeared cell per eight slots would be a lie, so the meter becomes a
        // bar and the figures carry the count.
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(1, "Name:big\nIP:10.0.0.1\nMaxJobs:128\nNoRemote:false\n"));
        for job in 0..64u32 {
            app.apply(Update::Event(Event::JobBegin {
                job_id: 700 + job,
                start_time: 0,
                host_id: 1,
            }));
        }
        let out = render(&app, 130, 24);
        let row = row_for(&out, "big");
        assert!(row.contains('⣿'), "expected a proportional bar: {row}");
        assert_eq!(counts_before_meter(row), ["64", "128"], "{row}");
    }

    #[test]
    fn active_and_max_are_plain_numbers_beside_the_meter() {
        let out = render(&busy_cluster(), 130, 24);
        let header = out.lines().find(|l| l.contains("NODE")).unwrap();
        let active = header.find("ACTIVE").expect("ACTIVE column");
        let max = header.find("MAX").expect("MAX column");
        let jobs = header.find("JOBS").expect("JOBS column");
        assert!(
            active < max && max < jobs,
            "order should read ACTIVE MAX JOBS: {header}"
        );
    }

    #[test]
    fn each_node_is_drawn_in_its_own_colour() {
        // Twelve nodes so the palette is exercised, and so a scheme that
        // collapsed to one colour could not pass.
        let mut app = App::new();
        app.apply(connected());
        for id in 1..=12u32 {
            app.apply(stats(
                id,
                &format!("Name:build{id:02}\nIP:10.0.0.{id}\nMaxJobs:8\nNoRemote:false\n"),
            ));
        }

        let mut seen = std::collections::BTreeSet::new();
        for id in 1..=12u32 {
            let name = format!("build{id:02}");
            let colours = row_colours(&app, 130, 30, &name);
            // The colour a node is drawn in, taken from the first cell of its
            // name rather than from anywhere a badge or figure might sit.
            let first = colours
                .iter()
                .find(|c| **c != Color::Reset)
                .copied()
                .unwrap_or(Color::Reset);
            assert!(
                matches!(first, Color::Indexed(_)),
                "{name} is not drawn in a node colour: {first:?}"
            );
            seen.insert(format!("{first:?}"));
        }
        assert!(
            seen.len() >= 6,
            "twelve nodes should not share two or three colours, saw {seen:?}"
        );
    }

    #[test]
    fn a_node_keeps_its_colour_when_the_list_is_re_sorted() {
        // The colour is an identity, so it must follow the node rather than the
        // row it happens to be in.
        let mut app = busy_cluster();
        let before = row_colours(&app, 130, 30, "build03");
        app.set_sort(crate::app::SortKey::Jobs);
        let after = row_colours(&app, 130, 30, "build03");
        assert_eq!(
            before.iter().find(|c| **c != Color::Reset),
            after.iter().find(|c| **c != Color::Reset),
            "build03 changed colour when the table was re-sorted"
        );
    }

    #[test]
    fn an_offline_node_is_grey_whatever_colour_it_would_have_been() {
        // State beats identity: "this one is gone" matters more than which
        // machine it was.
        let mut app = busy_cluster();
        app.apply(stats(3, "State:Offline\n"));
        let colours = row_colours(&app, 130, 30, "build03");
        let first = colours.iter().find(|c| **c != Color::Reset).copied();
        assert_eq!(first, Some(Color::DarkGray), "{colours:?}");
    }

    #[test]
    fn the_table_carries_icecream_figures_not_machine_ones() {
        // btop is the reference for how the screen reads, not for what it
        // measures. CPU, memory and temperature are a machine's business and
        // belong in the detail view; slots, jobs, scheduling load and compile
        // speed are the cluster's.
        let out = render(&busy_cluster(), 130, 24);
        let header = out
            .lines()
            .find(|l| l.contains("NODE"))
            .expect("header row");
        for icecream in ["ACTIVE", "MAX", "JOBS", "IN", "OUT", "LOAD", "SPEED"] {
            assert!(header.contains(icecream), "missing {icecream}: {header}");
        }
        for machine in ["CPU", "MEM", "TEMP"] {
            assert!(!header.contains(machine), "{machine} should be gone: {header}");
        }
    }

    #[test]
    fn cpu_bound_and_memory_bound_nodes_are_distinguishable() {
        // The gauges left the table with the rest of the machine metrics, but
        // "why is this node not taking more work" is an Icecream question and
        // one of the nine this screen exists to answer, so the conclusion stays
        // as a badge even though the percentages behind it do not.
        let out = render(&busy_cluster(), 130, 24);
        assert!(row_for(&out, "build01").contains("cpu!"), "{out}");
        assert!(row_for(&out, "build02").contains("mem!"), "{out}");
    }

    #[test]
    fn an_unconstrained_node_carries_no_marker() {
        let out = render(&busy_cluster(), 130, 24);
        assert!(!row_for(&out, "build03").contains('!'));
    }

    #[test]
    fn a_node_history_graph_appears_when_there_is_room() {
        let out = render(&busy_cluster(), 130, 24);
        assert!(out.contains("JOBS 2min"), "{out}");

        // A working node has drawn dots; an idle one has a measured zero, not a
        // blank — and both differ from a node that is not drawn at all.
        let busy: String = row_for(&out, "build01").chars().filter(|c| is_braille(*c)).collect();
        assert!(!busy.is_empty(), "no graph on a busy row: {out}");
        assert!(
            busy.chars().any(|c| c != '\u{2800}'),
            "ten ticks of work should leave dots: {busy:?}"
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
        // Everything in this table comes from the scheduler except LOAD, so a
        // missing agent costs one column and nothing else.
        assert_eq!(
            slot_meter(row),
            SLOT_FREE.to_string().repeat(8),
            "slots are scheduler data and must still be drawn: {row}"
        );
        assert!(row.contains(UNKNOWN), "load has no source: {row}");
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
        assert!(out.contains("icecream-watcher help"), "{out}");
        assert!(out.contains("cycle sort"), "{out}");
        // The two symbols a newcomer cannot guess.
        assert!(out.contains("not measured"), "{out}");
        assert!(out.contains("slow outlier"), "{out}");
    }

    #[test]
    fn narrow_terminals_drop_columns_rather_than_squeezing_every_bar() {
        let app = busy_cluster();

        let header_of = |w: u16| -> String {
            render(&app, w, 24)
                .lines()
                .find(|l| l.contains("NODE"))
                .expect("header row")
                .to_owned()
        };

        let wide = header_of(130);
        assert!(wide.contains("OUT") && wide.contains("SPEED"), "{wide}");

        // Job counters go first: they are a tally, and a tally is the easiest
        // thing to read one column to the right in the detail view.
        let medium = header_of(80);
        assert!(!medium.contains("OUT"), "{medium}");
        assert!(medium.contains("SPEED"), "{medium}");

        let narrow = header_of(65);
        assert!(!narrow.contains("SPEED"), "{narrow}");
        assert!(narrow.contains("LOAD"), "{narrow}");

        let tiny = header_of(50);
        assert!(!tiny.contains("LOAD"), "{tiny}");
        // Slots survive every width, because they are the point.
        assert!(tiny.contains("JOBS"), "{tiny}");
        assert!(
            render(&app, 50, 24).chars().any(is_braille),
            "the bar must survive"
        );
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
            // A terminal can report zero during a resize or when detached, and
            // a panic there leaves the user in a raw-mode alternate screen.
            (0u16, 0u16),
            (1, 1),
            (0, 40),
            (40, 0),
            (10, 3),
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
            attempt: 3,
            retry_at: Instant::now() + Duration::from_secs(4),
        });
        let out = render(&app, 130, 24);
        assert!(out.contains("disconnected: connection reset"), "{out}");
        // Saying only that something is wrong leaves the user unable to tell a
        // retrying monitor from a wedged one.
        assert!(out.contains("retry in"), "no retry countdown in:\n{out}");
        assert!(out.contains("attempt 3"), "no attempt count in:\n{out}");
    }

    #[test]
    fn a_due_retry_says_now_rather_than_counting_down_to_nothing() {
        let mut app = App::new();
        app.apply(Update::Disconnected {
            reason: "connection refused".into(),
            attempt: 1,
            retry_at: Instant::now(),
        });
        let out = render(&app, 130, 24);
        assert!(out.contains("retry now"), "{out}");
    }

    #[test]
    fn landing_on_a_different_scheduler_is_said_out_loud() {
        // Otherwise the whole screen quietly changes which cluster it describes.
        let mut app = busy_cluster();
        app.apply(Update::Connected {
            target: icecc_proto::SchedulerTarget {
                host: "backup-sched".into(),
                port: 8765,
            },
            protocol: 43,
        });
        let out = render(&app, 160, 24);
        assert!(out.contains("moved from"), "{out}");
        assert!(out.contains("sched"), "{out}");
    }

    #[test]
    fn a_silent_scheduler_is_labelled_quiet_not_left_ambiguous() {
        // Scheduler stats are change-driven, so silence is normal — but
        // indistinguishable from a dead link without being named.
        let mut app = busy_cluster();
        app.last_event = Some(Instant::now() - Duration::from_secs(300));
        let out = render(&app, 160, 24);
        assert!(out.contains("quiet 5m"), "{out}");
    }

    #[test]
    fn a_busy_scheduler_is_not_labelled_quiet() {
        let mut app = busy_cluster();
        app.last_event = Some(Instant::now());
        let out = render(&app, 160, 24);
        assert!(!out.contains("quiet"), "{out}");
    }

    #[test]
    fn a_down_node_says_how_long_it_has_been_down() {
        // "down" and "down since before this build started" call for different
        // reactions, and only one of them is an incident.
        let mut app = busy_cluster();
        app.apply(stats(1, "State:Offline\n"));
        if let Some(node) = app.cluster.nodes.get_mut(&1) {
            node.offline_since = Some(Instant::now() - Duration::from_secs(3600));
        }
        let out = render(&app, 160, 24);
        let row = row_for(&out, "build01");
        assert!(row.contains("down 1h"), "{row}");
    }

    #[test]
    fn stale_metrics_are_badged_but_keep_their_last_values() {
        let mut app = busy_cluster();
        app.cluster.metrics_stale_after = std::time::Duration::from_millis(1);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let out = render(&app, 130, 24);
        assert!(out.contains("stale"), "{out}");
        // Agent staleness must not blank what the *scheduler* told us: slots
        // and speed have nothing to do with whether an agent answered.
        let row = row_for(&out, "build01");
        assert_eq!(slot_meter(row).matches(SLOT_BUSY).count(), 1, "{row}");
        assert!(row.contains("3200"), "{row}");
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

        // Both layouts: the band exists so the three series can be read against
        // each other, which a stagger of even three columns defeats.
        for height in [20u16, 26, 44] {
            let out = render(&app, 130, height);
            let slots = graph_span(&out, "SLOTS");
            assert_eq!(
                graph_span(&out, "QUEUE"),
                slots,
                "QUEUE is staggered at height {height}:\n{out}"
            );
            assert_eq!(
                graph_span(&out, "RATE"),
                slots,
                "RATE is staggered at height {height}:\n{out}"
            );
        }
    }

    #[test]
    fn the_band_spends_rows_on_dot_graphs_only_when_there_are_rows_to_spend() {
        let mut app = busy_cluster();
        for _ in 0..60 {
            app.tick_history();
        }

        // Short: block sparklines, because one row of braille is four levels —
        // worse than the eight a block gives.
        let band_line = |out: &str| -> String {
            out.lines()
                .find(|l| l.contains("QUEUE"))
                .expect("band")
                .to_owned()
        };

        let short = render(&app, 130, 20);
        let line = band_line(&short);
        assert!(
            line.chars().any(|c| "▁▂▃▄▅▆▇█".contains(c)),
            "expected a block sparkline in the band at height 20: {line}"
        );
        assert!(
            !line.chars().any(is_braille),
            "no room for a dot graph at height 20: {line}"
        );

        // Tall: dot graphs, and the node table still has usable rows left.
        let tall = render(&app, 130, 30);
        assert!(
            band_line(&tall).chars().any(is_braille),
            "expected a dot graph in the band at height 30:\n{tall}"
        );
        assert!(
            tall.lines().filter(|l| l.contains("build0")).count() >= 3,
            "the table must not be squeezed out by the band:\n{tall}"
        );
    }

    #[test]
    fn a_taller_terminal_gives_the_graphs_more_rows() {
        let mut app = busy_cluster();
        for _ in 0..60 {
            app.tick_history();
        }
        // Band lines only: the node rows carry dot graphs of their own now, and
        // counting those would pass for the wrong reason.
        let rows_of = |h: u16| {
            let out = render(&app, 130, h);
            ["SLOTS", "QUEUE", "RATE"]
                .iter()
                .map(|s| series_lines(&out, s).len())
                .sum::<usize>()
        };
        assert!(
            rows_of(44) > rows_of(30),
            "a large terminal should draw taller graphs"
        );
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

        // Compact layout: one block glyph, at the right-hand end.
        let out = render(&app, 130, 20);
        let queue = out.lines().find(|l| l.contains("QUEUE")).unwrap();
        let first = queue.find('▁').expect("one sample should be drawn");
        let slots = out.lines().find(|l| l.contains("SLOTS")).unwrap();
        let bar = slots.find('░').expect("slots bar");
        assert!(
            first > bar,
            "a single sample belongs at the right edge, not the left:\n{out}"
        );

        // Tall layout: the same promise, kept by the fixed time axis. One
        // sample out of two minutes earns a sliver at the right rather than
        // being stretched across the box as history that does not exist.
        let out = render(&app, 130, 30);
        let (start, end) = graph_span(&out, "QUEUE");
        let drawn = drawn_span(&out, "QUEUE").expect("one sample should be drawn");
        let three_quarters = start + (end - start) * 3 / 4;
        assert!(
            drawn.0 >= three_quarters,
            "a single sample belongs at the right edge, not stretched:\n{out}"
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
    /// `cargo test -p icecream-watcher screenshot -- --ignored --nocapture`
    /// so the picture in the README is always real output from this code.
    #[test]
    #[ignore]
    fn screenshot() {
        /// Real-looking translation units, so the detail view shows what it
        /// looks like against a build rather than one filename repeated.
        const SOURCES: [&str; 8] = [
            "mojom/sensor/web_sensor_provider.mojom-blink.cc",
            "mojom/smart_card/smart_card.mojom-blink.cc",
            "renderer/modules/webaudio/audio_worklet_processor.cc",
            "renderer/core/layout/layout_block_flow.cc",
            "mojom/serial/serial.mojom-blink.cc",
            "renderer/platform/graphics/paint/paint_controller.cc",
            "renderer/core/css/resolver/style_resolver.cc",
            "mojom/speculation_rules/speculation_rules.mojom-blink.cc",
        ];
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
                    "Name:{name}\nIP:10.0.0.{id}\nMaxJobs:{max}\nNoRemote:{remote}\nSpeed:{speed}\n\
                     Platform:x86_64\nVersion:43\nFeatures:env_xz env_zstd\n\
                     Load:{load}\nLoadAvg1:{avg}\nLoadAvg5:{avg}\nLoadAvg10:{avg}\nFreeMem:36732\n",
                    load = (cpu * 10.0) as u32,
                    avg = (cpu * 140.0) as u32,
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

        // Occupy slots, and leave a few jobs queued. Submitters are mixed, so
        // the slot meters show whose work is running where.
        let mut job = 1000;
        let clients = [1u32, 4, 6, 8];
        for (id, _, max, cpu, _, _, _) in nodes {
            let running = (max as f32 * cpu / 100.0).round() as u32;
            for n in 0..running {
                job += 1;
                app.apply(Update::Event(Event::GetCs {
                    job_id: job,
                    client_id: clients[(n as usize + id as usize) % clients.len()],
                    filename: SOURCES[(job as usize) % SOURCES.len()].to_owned(),
                    lang: 1,
                }));
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

        println!("{}", render(&app, 118, 30));

        // …and the detail view for the node the overview points at.
        app.on_key(crate::app::Key::Enter);
        println!("\n--- detail ---\n{}", render(&app, 118, 30));
    }

    // ---- Phase 5: the detail view ----

    fn detail_of(app: &mut App, name: &str) -> String {
        // Select the named node, then open its detail view.
        loop {
            app.move_selection(1);
            let picked = app
                .selected
                .and_then(|id| app.cluster.nodes.get(&id))
                .map(|n| n.name().to_owned());
            match picked {
                Some(n) if n == name => break,
                Some(_) => {}
                None => panic!("no node named {name}"),
            }
            if app.selected_index() == Some(app.sorted_nodes().len() - 1) {
                panic!("no node named {name}");
            }
        }
        app.on_key(crate::app::Key::Enter);
        render(app, 118, 30)
    }

    /// The detail view's full text, independent of how much fits on screen.
    fn detail_text(app: &App) -> String {
        let node = app.detail_node().expect("detail view open");
        detail::lines(node, &app.cluster, 116)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn enter_opens_the_detail_view_for_the_selected_node() {
        let mut app = busy_cluster();
        let out = detail_of(&mut app, "build01");
        assert!(app.detail);
        // Identity the overview drops on purpose.
        assert!(out.contains("build01"), "{out}");
        assert!(out.contains("10.0.0.1"), "{out}");
        // And the table is gone while it is open.
        assert!(!out.contains("build02"), "{out}");
    }

    #[test]
    fn the_detail_view_shows_what_the_overview_left_out() {
        let mut app = busy_cluster();
        detail_of(&mut app, "build01");
        let out = detail_text(&app);
        for section in ["JOBS", "NODE", "AGENT"] {
            assert!(out.contains(section), "missing {section}:\n{out}");
        }
        // Everything the scheduler says about the node.
        for field in [
            "name", "IP", "platform", "protocol", "features", "max jobs",
            "speed", "load", "load average", "free memory", "jobs in", "jobs out",
        ] {
            assert!(out.contains(field), "missing {field}:\n{out}");
        }
    }

    #[test]
    fn each_slot_names_the_file_it_is_compiling_and_who_asked() {
        // The overview's meter says how many slots are busy and whose work is in
        // them; this is the question that follows.
        let mut app = App::new();
        app.apply(connected());
        for id in 1..=2u32 {
            app.apply(stats(
                id,
                &format!("Name:build{id:02}\nIP:10.0.0.{id}\nMaxJobs:8\nNoRemote:false\n"),
            ));
        }
        app.apply(Update::Event(Event::GetCs {
            job_id: 900,
            client_id: 2,
            filename: "mojom/sensor/web_sensor_provider.mojom-blink.cc".into(),
            lang: 1,
        }));
        app.apply(Update::Event(Event::JobBegin {
            job_id: 900,
            start_time: 0,
            host_id: 1,
        }));

        let out = detail_of(&mut app, "build01");
        assert!(out.contains("Job   1"), "jobs should be numbered:\n{out}");
        assert!(out.contains("web_sensor_provider"), "{out}");
        assert!(out.contains("from build02"), "the submitter:\n{out}");
        assert!(out.contains("7 of 8 slots free"), "{out}");
    }

    #[test]
    fn a_job_whose_name_was_never_seen_says_so_rather_than_showing_a_blank() {
        // MON_JOB_BEGIN carries no filename; it comes from the MON_GET_CS that
        // precedes it, which we miss if we attach in between.
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(1, "Name:build01\nIP:10.0.0.1\nMaxJobs:8\nNoRemote:false\n"));
        app.apply(Update::Event(Event::JobBegin {
            job_id: 900,
            start_time: 0,
            host_id: 1,
        }));
        let out = detail_of(&mut app, "build01");
        assert!(out.contains("name not seen"), "{out}");
    }

    #[test]
    fn an_idle_node_says_its_slots_are_free_rather_than_listing_nothing() {
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(1, "Name:build01\nIP:10.0.0.1\nMaxJobs:8\nNoRemote:false\n"));
        let out = detail_of(&mut app, "build01");
        assert!(out.contains("no remote jobs running"), "{out}");
        assert!(out.contains("8 slots free"), "{out}");
    }

    #[test]
    fn an_implausible_free_memory_figure_is_labelled_not_converted() {
        // The protocol documents FreeMem as MiB and Linux daemons send MiB, but
        // the macOS daemon in the test cluster sends KiB (ARCHITECTURE §9).
        // Rendering the documented unit regardless would claim terabytes of free
        // memory on a laptop; silently guessing the unit is how the trap was set.
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(
            1,
            "Name:mac\nIP:10.0.0.1\nMaxJobs:8\nNoRemote:false\nFreeMem:5647912\n",
        ));
        let out = detail_of(&mut app, "mac");
        assert!(out.contains("5647912"), "the raw figure must survive:\n{out}");
        assert!(out.contains("KiB"), "the doubt must be stated:\n{out}");

        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(
            1,
            "Name:linux\nIP:10.0.0.2\nMaxJobs:8\nNoRemote:false\nFreeMem:36732\n",
        ));
        let out = detail_of(&mut app, "linux");
        assert!(out.contains("36732 MiB"), "a plausible figure is just shown:\n{out}");
    }

    #[test]
    fn the_load_figure_says_what_it_actually_is() {
        // "Load" in this protocol is the scheduler's placement weight, not CPU
        // utilisation, and the name invites exactly the wrong reading.
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(
            1,
            "Name:build01\nIP:10.0.0.1\nMaxJobs:8\nNoRemote:false\nLoad:772\n",
        ));
        let out = detail_of(&mut app, "build01");
        assert!(out.contains("772 of 1000"), "{out}");
        assert!(out.contains("placement weight"), "{out}");
    }

    #[test]
    fn scrolling_reveals_the_sections_below_the_first_screen() {
        let mut app = busy_cluster();
        // A full slot list pushes the later sections past the fold.
        for job in 0..12u32 {
            app.apply(Update::Event(Event::GetCs {
                job_id: 800 + job,
                client_id: 2,
                filename: format!("src/some/deeply/nested/translation_unit_{job}.cc"),
                lang: 1,
            }));
            app.apply(Update::Event(Event::JobBegin {
                job_id: 800 + job,
                start_time: 0,
                host_id: 1,
            }));
        }
        let first_screen = detail_of(&mut app, "build01");
        // Tall content on a short terminal: the later sections start off-screen.
        assert!(!first_screen.contains("AGENT"), "{first_screen}");

        for _ in 0..40 {
            app.on_key(crate::app::Key::Down);
        }
        let scrolled = render(&app, 118, 30);
        assert!(
            scrolled.contains("AGENT"),
            "scrolling should reach the end:\n{scrolled}"
        );
    }

    #[test]
    fn a_long_field_label_does_not_run_into_its_value() {
        let mut app = busy_cluster();
        detail_of(&mut app, "build01");
        for line in detail_text(&app).lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("queued from here") {
                assert!(rest.starts_with(' '), "label and value collide: {line:?}");
            }
        }
    }

    #[test]
    fn job_counters_say_they_are_since_connect() {
        let mut app = busy_cluster();
        detail_of(&mut app, "build01");
        let out = detail_text(&app);
        assert!(out.contains("since this monitor connected"), "{out}");
    }

    #[test]
    fn a_node_with_no_agent_says_so_instead_of_showing_blanks() {
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(
            1,
            "Name:build01\nIP:10.0.0.1\nMaxJobs:8\nNoRemote:false\nSpeed:3000\n",
        ));
        detail_of(&mut app, "build01");
        let out = detail_text(&app);
        assert!(out.contains("no agent here"), "{out}");
        // Everything this panel shows comes from the scheduler, so a missing
        // agent costs it nothing but the badge it can no longer justify.
        assert!(out.contains("slots free"), "{out}");
        assert!(out.contains("3000"), "{out}");
    }

    #[test]
    fn a_healthy_node_says_so_and_an_unhealthy_one_leads_with_the_problem() {
        let mut app = busy_cluster();
        let healthy = detail_of(&mut app, "build01");
        assert!(healthy.contains("healthy"), "{healthy}");

        app.on_key(crate::app::Key::Back);
        app.apply(stats(2, "State:Offline\n"));
        let sick = detail_of(&mut app, "build02");
        assert!(sick.contains("OFFLINE"), "{sick}");
        assert!(
            sick.contains("last known"),
            "an offline node's numbers must be labelled:\n{sick}"
        );
    }

    #[test]
    fn a_wrong_host_warning_names_both_sides() {
        let mut app = busy_cluster();
        app.apply_resource(
            1,
            ResourceResult::Ok(snapshot("someone-else", 50.0, 500, Some(60.0))),
        );
        let out = detail_of(&mut app, "build01");
        assert!(out.contains("WRONG HOST?"), "{out}");
        assert!(out.contains("someone-else"), "{out}");
    }

    #[test]
    fn esc_unwinds_one_layer_at_a_time() {
        let mut app = busy_cluster();
        detail_of(&mut app, "build01");
        app.show_help = true;

        app.on_key(crate::app::Key::Back);
        assert!(!app.show_help, "esc should close the overlay first");
        assert!(app.detail, "and not the detail view as well");

        app.on_key(crate::app::Key::Back);
        assert!(!app.detail);
        assert!(!app.should_quit, "leaving the detail view must not quit");

        app.on_key(crate::app::Key::Back);
        assert!(app.should_quit);
    }

    #[test]
    fn enter_from_a_fresh_screen_shows_something() {
        let mut app = busy_cluster();
        assert_eq!(app.selected, None);
        app.on_key(crate::app::Key::Enter);
        assert!(app.detail, "Enter should pick a row rather than do nothing");
        assert!(app.selected.is_some());
    }

    #[test]
    fn arrows_scroll_the_detail_view_and_stop_at_the_end() {
        let mut app = busy_cluster();
        detail_of(&mut app, "build01");
        assert_eq!(app.detail_scroll, 0);

        for _ in 0..3 {
            app.on_key(crate::app::Key::Down);
        }
        assert_eq!(app.detail_scroll, 3);
        // The selection must not have moved underneath.
        let selected = app.selected;

        // Scrolling far past the end is clamped by the renderer's measurement,
        // so coming back does not need the same number of keypresses.
        for _ in 0..200 {
            app.on_key(crate::app::Key::Down);
        }
        let mut ui = Ui::default();
        let mut terminal = Terminal::new(TestBackend::new(118, 30)).unwrap();
        terminal.draw(|f| draw(f, &app, &mut ui)).unwrap();
        app.clamp_detail_scroll(ui.detail_max_scroll);
        assert!(app.detail_scroll <= ui.detail_max_scroll);

        app.on_key(crate::app::Key::Up);
        assert_eq!(
            app.selected, selected,
            "scrolling must not change selection"
        );
    }

    #[test]
    fn sorting_keys_are_inert_while_the_detail_view_is_open() {
        let mut app = busy_cluster();
        detail_of(&mut app, "build01");
        let before = app.sort;
        app.on_key(crate::app::Key::CycleSort);
        app.on_key(crate::app::Key::Sort(crate::app::SortKey::Mem));
        assert_eq!(app.sort, before, "a hidden list must not reorder silently");
    }

    #[test]
    fn the_footer_offers_only_the_keys_that_work_here() {
        let mut app = busy_cluster();
        let out = detail_of(&mut app, "build01");
        assert!(out.contains("Esc") && out.contains("scroll"), "{out}");
        assert!(!out.contains("by cpu/mem/load/jobs"), "{out}");
    }

    #[test]
    fn the_detail_view_closes_when_its_node_disappears() {
        let mut app = busy_cluster();
        detail_of(&mut app, "build01");
        assert!(app.detail);
        // A reconnect clears the node list.
        app.apply(connected());
        assert!(!app.detail, "an empty pane is worse than the list");
        let _ = render(&app, 118, 30);
    }

    #[test]
    fn the_detail_view_renders_at_any_geometry() {
        let mut app = busy_cluster();
        detail_of(&mut app, "build01");
        for (w, h) in [(10u16, 3u16), (20, 5), (40, 10), (60, 14), (200, 60)] {
            let _ = render(&app, w, h);
        }
    }

    #[test]
    fn a_long_filename_is_elided_rather_than_overflowing_the_line() {
        let mut app = App::new();
        app.apply(connected());
        app.apply(stats(1, "Name:big\nIP:10.0.0.1\nMaxJobs:64\nNoRemote:false\n"));
        for job in 0..64u32 {
            app.apply(Update::Event(Event::GetCs {
                job_id: 800 + job,
                client_id: 1,
                filename: format!(
                    "third_party/blink/renderer/modules/very/deeply/nested/path/that/keeps/going/unit_{job}.cc"
                ),
                lang: 1,
            }));
            app.apply(Update::Event(Event::JobBegin {
                job_id: 800 + job,
                start_time: 0,
                host_id: 1,
            }));
        }

        detail_of(&mut app, "big");
        let out = detail_text(&app);
        assert!(out.contains('…'), "a cut name should say so:\n{out}");
        for line in out.lines() {
            assert!(line.chars().count() <= 118, "line overflows: {line:?}");
        }
    }
}
