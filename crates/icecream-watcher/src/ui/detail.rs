//! Phase 5: the per-node detail view.
//!
//! The overview answers "which node should I look at" and its slot meter says
//! how many slots are busy and whose work is in them. This answers what follows:
//! *which file* each slot is compiling and for how long, and everything the
//! scheduler reports about the node itself.
//!
//! Rendered as a flat list of lines and scrolled, rather than as a fixed
//! layout, so a 128-core machine and a 2-core one both work and nothing is
//! silently cut off.

use icecc_model::{Cluster, Job, JobState, Node, ResourceState};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use super::widgets;
use super::UNKNOWN;

pub fn draw(frame: &mut Frame, area: Rect, node: &Node, cluster: &Cluster, scroll: u16) {
    let block = Block::bordered().title(title(node));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = lines(node, cluster, inner.width as usize);
    // Clamp so scrolling past the end cannot leave a blank pane.
    let max_scroll = (lines.len() as u16).saturating_sub(inner.height.max(1));
    frame.render_widget(
        Paragraph::new(lines).scroll((scroll.min(max_scroll), 0)),
        inner,
    );
}

/// How far the detail view can be scrolled, so the caller can clamp its state.
pub fn max_scroll(node: &Node, cluster: &Cluster, area: Rect) -> u16 {
    let inner_height = area.height.saturating_sub(2).max(1);
    let inner_width = area.width.saturating_sub(2) as usize;
    (lines(node, cluster, inner_width).len() as u16).saturating_sub(inner_height)
}

fn title(node: &Node) -> Line<'static> {
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            node.name().to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    // Identity the overview drops: address, platform, protocol.
    spans.push(Span::styled(
        format!(
            "  {}  {}  proto {} ",
            node.ip(),
            node.platform(),
            node.stats
                .protocol
                .map(|p| p.to_string())
                .unwrap_or_else(|| UNKNOWN.into())
        ),
        Style::default().add_modifier(Modifier::DIM),
    ));
    Line::from(spans)
}

/// Build the whole view. Split out from rendering so it can be asserted on.
pub fn lines(node: &Node, cluster: &Cluster, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();

    out.extend(status_lines(node));
    out.push(Line::raw(""));
    out.extend(jobs_lines(node, cluster, width));
    out.push(Line::raw(""));
    out.extend(node_lines(node, cluster));
    out.push(Line::raw(""));
    out.extend(agent_lines(node));

    out
}

/// Anything wrong with the node, stated before the numbers it would explain.
fn status_lines(node: &Node) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if node.identity_mismatch {
        if let Some(agent) = node.resources.as_ref().map(|r| r.hostname.clone()) {
            out.push(warn(
                "WRONG HOST?",
                &format!(
                    "the agent at {} calls itself {agent:?}, so its readings may be another machine's",
                    node.ip()
                ),
            ));
        }
    }
    if node.suspect() {
        out.push(warn(
            "NO ACK",
            "the scheduler has pinged this node and is still waiting for an answer",
        ));
    }
    // A missing agent is a deployment gap, not a fault, and nothing on this
    // panel depends on it any more — it belongs in the agent footnote, not at
    // the top as though it explained a blank screen.
    if matches!(node.resource_state, ResourceState::Error { .. }) {
        if let Some(reason) = node.resource_state.reason() {
            out.push(warn("AGENT ERROR", reason));
        }
    }
    if !node.accepts_remote() {
        out.push(note(
            "LOCAL ONLY",
            "this node refuses remote jobs and offers no compile slots",
        ));
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "  healthy",
            Style::default().fg(Color::Green),
        )));
    }
    out
}

/// What each compile slot is actually doing.
///
/// The overview's meter says how many slots are busy and whose work is in them;
/// this says *which file*, which is the question that follows.
fn jobs_lines(node: &Node, cluster: &Cluster, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![heading("JOBS")];

    let mut running: Vec<&Job> = cluster
        .jobs
        .values()
        .filter(|job| job.state == JobState::Active && job.host_id == Some(node.host_id))
        .collect();
    // Longest-running first: "what is taking so long" is why this list is read.
    running.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.id.cmp(&b.id)));

    if running.is_empty() {
        out.push(Line::from(Span::styled(
            format!("    no remote jobs running ({} slots free)", node.max_jobs()),
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        for (n, job) in running.iter().enumerate() {
            out.push(job_line(n + 1, job, cluster, width));
        }
        let free = (node.max_jobs() as usize).saturating_sub(running.len());
        out.push(Line::from(Span::styled(
            format!("    {} of {} slots free", free, node.max_jobs()),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    // Only jobs this monitor saw start are here: the scheduler replays node
    // stats on login but not jobs, so anything already compiling when we
    // attached stays invisible until it finishes.
    if cluster.totals.unmatched_done > 0 {
        out.push(Line::from(Span::styled(
            format!(
                "    ({} job{} elsewhere finished that began before this monitor attached)",
                cluster.totals.unmatched_done,
                if cluster.totals.unmatched_done == 1 { "" } else { "s" }
            ),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    if !node.local_jobs.is_empty() {
        out.push(Line::raw(""));
        out.push(field(
            "local jobs",
            format!(
                "{} compiling here for itself, occupying no scheduler slot",
                node.local_jobs.len()
            ),
        ));
    }

    let waiting = cluster.pending_from(node.host_id);
    if waiting > 0 {
        out.push(Line::raw(""));
        out.push(field(
            "queued from here",
            format!("{waiting} waiting for a compile host"),
        ));
    }
    out
}

/// `Job  3  ( 32.5s)  path/to/file.cc  · from build02`
fn job_line(n: usize, job: &Job, cluster: &Cluster, width: usize) -> Line<'static> {
    let client = job
        .client_id
        .and_then(|id| cluster.nodes.get(&id))
        .map(|node| node.name().to_owned());

    let mut spans = vec![
        Span::styled(
            format!("    Job {n:>3}  "),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(
            format!("({:>7})  ", elapsed(job.since.elapsed())),
            Style::default().fg(Color::Cyan),
        ),
    ];

    // Budget the trailing attribution before eliding the name, so the answer to
    // "whose job is this" is not the part that gets cut.
    let tail = client.as_ref().map_or(0, |c| c.chars().count() + 9);
    let room = width.saturating_sub(4 + 9 + 11 + tail);
    let name = if job.filename.is_empty() {
        // We saw MON_JOB_BEGIN but not the MON_GET_CS that carries the name.
        "(name not seen)".to_owned()
    } else {
        super::elide(&job.filename, room.max(8))
    };
    spans.push(Span::raw(name));

    if let Some(client) = client {
        spans.push(Span::styled(
            "  · from ".to_owned(),
            Style::default().add_modifier(Modifier::DIM),
        ));
        spans.push(Span::styled(
            client.clone(),
            Style::default().fg(widgets::node_colour(&client)),
        ));
    }
    Line::from(spans)
}

/// How long a job has been running, to the precision a compile deserves.
fn elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}m {:02}s", (secs / 60.0) as u64, (secs % 60.0) as u64)
    }
}

/// Everything the scheduler reports about this node, plus what we have counted.
fn node_lines(node: &Node, cluster: &Cluster) -> Vec<Line<'static>> {
    let mut out = vec![heading("NODE")];

    out.push(field("name", node.name().to_owned()));
    out.push(field("IP", node.ip().to_owned()));
    out.push(field("platform", node.platform().to_owned()));
    out.push(field(
        "protocol",
        node.stats
            .protocol
            .map(|p| p.to_string())
            .unwrap_or_else(|| UNKNOWN.into()),
    ));
    out.push(field(
        "features",
        node.stats
            .features
            .clone()
            .unwrap_or_else(|| UNKNOWN.into()),
    ));
    out.push(field("max jobs", node.max_jobs().to_string()));
    out.push(field(
        "accepts remote",
        if node.accepts_remote() { "yes" } else { "no" }.to_owned(),
    ));
    out.push(field(
        "speed",
        match node.speed() {
            Some(s) => {
                let mut text = format!("{s:.1} output bytes per user-second");
                if cluster.is_slow_outlier(node) {
                    if let Some(median) = cluster.median_speed() {
                        text.push_str(&format!("  — well below the cluster median of {median:.0}"));
                    }
                }
                text
            }
            None => "unknown until this node compiles something".to_owned(),
        },
    ));
    out.push(field(
        "load",
        match node.stats.load {
            // Spelled out because the name invites the wrong reading: this is
            // the scheduler's placement weight, not CPU utilisation.
            Some(load) => format!("{load} of 1000 — the scheduler's placement weight"),
            None => UNKNOWN.to_owned(),
        },
    ));
    out.push(field("load average", load_averages(node)));
    out.push(field("free memory", free_memory(node)));

    out.push(Line::raw(""));
    out.push(field(
        "jobs in",
        format!("{} compiled here for others", node.jobs_in),
    ));
    out.push(field(
        "jobs out",
        format!(
            "{} submitted from here — a node that submits none is a pure compile server",
            node.jobs_out
        ),
    ));
    out.push(field(
        "jobs local",
        format!("{} compiled here for itself", node.jobs_local),
    ));
    out.push(Line::from(Span::styled(
        "    (job counts are since this monitor connected)",
        Style::default().add_modifier(Modifier::DIM),
    )));
    out
}

fn load_averages(node: &Node) -> String {
    let stats = &node.stats;
    match (stats.load_avg_1, stats.load_avg_5, stats.load_avg_10) {
        (Some(one), Some(five), Some(ten)) => {
            format!("{one:.2}  {five:.2}  {ten:.2}   (1 / 5 / 10 min)")
        }
        _ => UNKNOWN.to_owned(),
    }
}

/// `FreeMem`, which the protocol documents as MiB but not every daemon sends
/// that way.
///
/// A Linux daemon's figure matches `free -m`; a macOS daemon in the test cluster
/// sends what can only be KiB — 5 647 912 would be 5.4 TiB as MiB and is a
/// plausible 5.4 GiB as KiB (ARCHITECTURE §9). Rendering the documented unit
/// regardless would put terabytes of free memory on a laptop, so an implausible
/// figure is labelled rather than converted: guessing the unit silently is how
/// the trap was set in the first place.
fn free_memory(node: &Node) -> String {
    let Some(mib) = node.stats.free_mem_mib else {
        return UNKNOWN.to_owned();
    };
    // A machine with more than a terabyte of free memory is not what this is.
    if mib > 1024 * 1024 {
        format!(
            "{mib} as reported — implausible as MiB, likely KiB ({} MiB)",
            mib / 1024
        )
    } else {
        format!("{mib} MiB available")
    }
}

/// A footnote, not a section: the agent no longer feeds this panel. It is what
/// the `cpu!` and `mem!` badges on the overview are computed from, so there has
/// to be somewhere to see the figures behind them.
fn agent_lines(node: &Node) -> Vec<Line<'static>> {
    let mut out = vec![heading("AGENT")];

    match node.resources.as_ref() {
        Some(res) => {
            out.push(field(
                "reported",
                format!(
                    "{:.0}% CPU, {} memory in use — what the overview badges read",
                    res.cpu.total_busy_pct,
                    node.mem_pct()
                        .map(|p| format!("{p:.0}%"))
                        .unwrap_or_else(|| UNKNOWN.into()),
                ),
            ));
            out.push(field(
                "agent",
                format!(
                    "icecream-watcher-agent {} on {}, last answered {}",
                    res.agent_version,
                    res.hostname,
                    match node.resources_at {
                        Some(at) => format!("{:.1} s ago", at.elapsed().as_secs_f32()),
                        None => UNKNOWN.to_owned(),
                    }
                ),
            ));
        }
        None => {
            out.push(Line::from(Span::styled(
                "    no agent here — the overview cannot badge this node as CPU- or memory-bound",
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
    }
    out
}

// ---------------------------------------------------------------- helpers

fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ))
}

/// Width of a field's label column.
const FIELD_LABEL: usize = 17;

fn field(name: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            // The trailing space is not decoration: a label longer than the
            // column would otherwise run straight into its value.
            format!("    {name:<FIELD_LABEL$} "),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::raw(value),
    ])
}

fn warn(label: &str, detail: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {label}  "),
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(detail.to_owned(), Style::default().fg(Color::LightRed)),
    ])
}

fn note(label: &str, detail: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label}  "), Style::default().fg(Color::Yellow)),
        Span::styled(
            detail.to_owned(),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ])
}

