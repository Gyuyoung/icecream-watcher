//! Phase 5: the per-node detail view.
//!
//! Everything the overview deliberately leaves out lives here — per-core CPU,
//! frequency, swap, uptime, network, every thermal sensor, the node's Icecream
//! identity and its job ledger. The overview answers "which node should I look
//! at"; this answers "what is going on with it".
//!
//! Rendered as a flat list of lines and scrolled, rather than as a fixed
//! layout, so a 128-core machine and a 2-core one both work and nothing is
//! silently cut off.

use icecc_model::{Cluster, Node, ResourceState};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use super::widgets;
use super::UNKNOWN;

/// Width of one per-core entry: `C12 ███████▌ 98%`.
const CORE_BAR: usize = 8;
const CORE_ENTRY: usize = 4 + CORE_BAR + 5 + 2;

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
    out.extend(cpu_lines(node, width));
    out.push(Line::raw(""));
    out.extend(memory_lines(node));
    out.push(Line::raw(""));
    out.extend(icecream_lines(node, cluster));
    out.push(Line::raw(""));
    out.extend(network_lines(node));
    out.push(Line::raw(""));
    out.extend(sensor_lines(node, width));
    out.push(Line::raw(""));
    out.extend(agent_lines(node));

    out
}

/// Anything wrong with the node, stated before the numbers it would explain.
fn status_lines(node: &Node) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if node.offline {
        out.push(warn(
            "OFFLINE",
            "the scheduler has lost this node; the values below are its last known ones",
        ));
    }
    if node.identity_mismatch {
        if let Some(agent) = node.resources.as_ref().map(|r| r.hostname.clone()) {
            out.push(warn(
                "WRONG HOST?",
                &format!(
                    "the agent at {} calls itself {agent:?}, so these metrics may be another machine's",
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
    if let Some(reason) = node.resource_state.reason() {
        let label = match node.resource_state {
            ResourceState::NoAgent { .. } => "NO AGENT",
            _ => "AGENT ERROR",
        };
        out.push(warn(label, reason));
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

fn cpu_lines(node: &Node, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![heading("CPU")];

    let Some(res) = node.resources.as_ref() else {
        out.push(unavailable());
        return out;
    };

    let mut summary = vec![
        Span::raw("  "),
        Span::styled(
            widgets::bar(res.cpu.total_busy_pct, 20),
            Style::default().fg(widgets::ramp(res.cpu.total_busy_pct)),
        ),
        Span::raw(format!(" {:>3.0}%   ", res.cpu.total_busy_pct)),
        Span::raw(format!("{} cores", res.cpu.cores)),
    ];
    if let Some(mhz) = res.cpu.mean_freq_mhz() {
        summary.push(Span::styled(
            format!("  @ {mhz} MHz avg"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    out.push(Line::from(summary));
    out.push(Line::raw(""));

    // Per-core bars, laid out in as many columns as fit.
    let per_row = (width.saturating_sub(2) / CORE_ENTRY).max(1);
    for chunk_start in (0..res.cpu.per_core_busy_pct.len()).step_by(per_row) {
        let mut spans = vec![Span::raw("  ")];
        for (offset, pct) in res.cpu.per_core_busy_pct[chunk_start..]
            .iter()
            .take(per_row)
            .enumerate()
        {
            let index = chunk_start + offset;
            spans.push(Span::styled(
                format!("C{index:<2} "),
                Style::default().add_modifier(Modifier::DIM),
            ));
            spans.push(Span::styled(
                widgets::bar(*pct, CORE_BAR),
                Style::default().fg(widgets::ramp(*pct)),
            ));
            spans.push(Span::raw(format!(" {pct:>3.0}%  ")));
        }
        out.push(Line::from(spans));
    }

    out.push(Line::raw(""));
    out.push(field(
        "load average",
        format!(
            "{:.2}  {:.2}  {:.2}",
            res.load.one, res.load.five, res.load.fifteen
        ),
    ));
    if let Some(per_core) = node.load_per_core() {
        out.push(field(
            "per core",
            format!(
                "{per_core:.2}   ({} runnable of {} processes)",
                res.load.runnable, res.load.total_procs
            ),
        ));
    }
    out
}

fn memory_lines(node: &Node) -> Vec<Line<'static>> {
    let mut out = vec![heading("MEMORY")];

    let Some(res) = node.resources.as_ref() else {
        out.push(unavailable());
        return out;
    };
    let mem = &res.mem;

    if let Some(pct) = mem.used_pct() {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                widgets::bar(pct, 20),
                Style::default().fg(widgets::ramp(pct)),
            ),
            Span::raw(format!(" {pct:>3.0}%   ")),
            Span::raw(format!(
                "{} used of {}",
                widgets::size_kib(mem.used_kib()),
                widgets::size_kib(mem.total_kib)
            )),
        ]));
    }
    out.push(field(
        "available",
        format!(
            "{}   (free {}, buffers {}, cached {})",
            widgets::size_kib(mem.available_kib),
            widgets::size_kib(mem.free_kib),
            widgets::size_kib(mem.buffers_kib),
            widgets::size_kib(mem.cached_kib),
        ),
    ));

    // No swap at all is a fact worth stating, not a zero to imply.
    match mem.swap_used_pct() {
        Some(pct) => out.push(field(
            "swap",
            format!(
                "{} of {}   ({pct:.0}%)",
                widgets::size_kib(mem.swap_used_kib()),
                widgets::size_kib(mem.swap_total_kib)
            ),
        )),
        None => out.push(field("swap", "none configured".to_owned())),
    }
    out
}

fn icecream_lines(node: &Node, cluster: &Cluster) -> Vec<Line<'static>> {
    let mut out = vec![heading("ICECREAM")];

    out.push(field(
        "compile slots",
        format!("{} of {} in use", node.current_jobs(), node.max_jobs()),
    ));
    if !node.local_jobs.is_empty() {
        out.push(field(
            "local jobs",
            format!(
                "{} running here, occupying no scheduler slot",
                node.local_jobs.len()
            ),
        ));
    }
    out.push(field(
        "queued from here",
        cluster.pending_from(node.host_id).to_string(),
    ));

    out.push(field(
        "speed",
        match node.speed() {
            Some(s) => {
                let mut text = format!("{s:.0} output bytes per user-second");
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

    // Since connect, because job ids only mean anything within one session.
    out.push(field(
        "jobs in",
        format!("{} compiled here for others", node.jobs_in),
    ));
    out.push(field(
        "jobs out",
        format!("{} submitted and compiled elsewhere", node.jobs_out),
    ));
    out.push(field(
        "jobs local",
        format!("{} compiled here for itself", node.jobs_local),
    ));
    out.push(Line::from(Span::styled(
        "    (job counts are since this monitor connected)",
        Style::default().add_modifier(Modifier::DIM),
    )));

    if let Some(features) = node.stats.features.as_deref() {
        out.push(field("features", features.to_owned()));
    }
    out
}

fn network_lines(node: &Node) -> Vec<Line<'static>> {
    let mut out = vec![heading("NETWORK")];

    let Some(res) = node.resources.as_ref() else {
        out.push(unavailable());
        return out;
    };

    out.push(field(
        "total",
        format!(
            "rx {}   tx {}",
            widgets::rate(res.net.rx_bytes_per_sec),
            widgets::rate(res.net.tx_bytes_per_sec)
        ),
    ));
    for iface in &res.net.interfaces {
        out.push(field(
            &iface.name,
            format!(
                "rx {}   tx {}",
                widgets::rate(iface.rx_bytes_per_sec),
                widgets::rate(iface.tx_bytes_per_sec)
            ),
        ));
    }
    out.push(field("uptime", widgets::duration(res.uptime_secs)));
    out
}

fn sensor_lines(node: &Node, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![heading("SENSORS")];

    let Some(res) = node.resources.as_ref() else {
        out.push(unavailable());
        return out;
    };

    match (&res.thermal.cpu_celsius, &res.thermal.cpu_source) {
        (Some(t), Some(source)) => out.push(Line::from(vec![
            Span::raw("    cpu package    "),
            Span::styled(
                format!("{t:>3.0}°C"),
                Style::default().fg(widgets::temp_ramp(*t)),
            ),
            // Naming the sensor matters: hosts disagree between their own by
            // more than 20 °C, so an unattributed number is not evidence.
            Span::styled(
                format!("   from {source}"),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])),
        _ => out.push(field("cpu package", "no sensor identified".to_owned())),
    }

    if res.thermal.sensors.is_empty() {
        return out;
    }
    out.push(Line::raw(""));

    // Two per line where there is room; the list can be long.
    let entry = 34usize;
    let per_row = (width.saturating_sub(4) / entry).max(1);
    for chunk in res.thermal.sensors.chunks(per_row) {
        let mut spans = vec![Span::raw("    ")];
        for sensor in chunk {
            spans.push(Span::styled(
                format!("{:<24}", super::elide(&sensor.label, 24)),
                Style::default().add_modifier(Modifier::DIM),
            ));
            spans.push(Span::styled(
                format!("{:>3.0}°  ", sensor.celsius),
                Style::default().fg(widgets::temp_ramp(sensor.celsius)),
            ));
        }
        out.push(Line::from(spans));
    }
    out
}

fn agent_lines(node: &Node) -> Vec<Line<'static>> {
    let mut out = vec![heading("AGENT")];

    match node.resources.as_ref() {
        Some(res) => {
            out.push(field(
                "version",
                format!("icecream-watcher-agent {}", res.agent_version),
            ));
            out.push(field(
                "sample window",
                format!("{} ms", res.sample_interval_ms),
            ));
            out.push(field(
                "last reply",
                match node.resources_at {
                    Some(at) => format!("{:.1} s ago", at.elapsed().as_secs_f32()),
                    None => UNKNOWN.to_owned(),
                },
            ));
            out.push(field("reported hostname", res.hostname.clone()));
            out.push(field("reported addresses", res.addresses.join(", ")));
        }
        None => {
            out.push(Line::from(Span::styled(
                "    no agent has answered on this node",
                Style::default().add_modifier(Modifier::DIM),
            )));
            out.push(Line::from(Span::styled(
                "    install icecream-watcher-agent for CPU, memory, temperature and network",
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

fn unavailable() -> Line<'static> {
    Line::from(Span::styled(
        "    not measured — needs icecream-watcher-agent on this node",
        Style::default().fg(Color::DarkGray),
    ))
}
