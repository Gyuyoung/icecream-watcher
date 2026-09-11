//! Block-character bars, sparklines and the colour ramp.
//!
//! These are what turn the node table from a grid of numbers into something
//! readable in one glance: a bar's length is comparable across rows without
//! reading it, and colour carries state rather than decoration.

use ratatui::style::Color;

/// Partial block glyphs, one eighth apart. Index 0 is empty.
const EIGHTHS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Sparkline glyphs, low to high. Index 0 is reserved for "zero", which is
/// distinct from a gap (rendered as a space).
const SPARKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// What an unfilled bar cell looks like. Light enough not to compete with the
/// filled portion, solid enough to show the bar's full extent.
const TROUGH: char = '░';

/// Render `pct` (0..100) as a bar `width` cells wide.
///
/// The leading edge uses partial blocks, so a 3 % difference between two nodes
/// is visible instead of rounding to the same number of whole cells.
pub fn bar(pct: f32, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let pct = pct.clamp(0.0, 100.0);
    let eighths_total = (pct / 100.0 * (width * 8) as f32).round() as usize;
    let full = eighths_total / 8;
    let remainder = eighths_total % 8;

    let mut out = String::with_capacity(width * 3);
    for _ in 0..full.min(width) {
        out.push('█');
    }
    if full < width {
        if remainder > 0 {
            out.push(EIGHTHS[remainder]);
        }
        let drawn = full + usize::from(remainder > 0);
        for _ in drawn..width {
            out.push(TROUGH);
        }
    }
    out
}

/// A bar for a value that has no measurement, so a row keeps its shape.
pub fn empty_bar(width: usize) -> String {
    "·".repeat(width)
}

/// Render a series as a sparkline `values.len()` cells wide.
///
/// `max` scales the graph; pass 100.0 for percentages, or the series maximum
/// for unbounded things like a job queue. Non-finite samples render as spaces:
/// a gap in measurement must not look like a measured zero.
pub fn sparkline(values: &[f32], max: f32) -> String {
    let max = if max.is_finite() && max > 0.0 {
        max
    } else {
        1.0
    };
    values
        .iter()
        .map(|&v| {
            if !v.is_finite() {
                return ' ';
            }
            if v <= 0.0 {
                // Lowest glyph, not a space: a real zero is information.
                return SPARKS[0];
            }
            let scaled = (v / max).clamp(0.0, 1.0);
            // 1..=7, so any non-zero value is visibly above the baseline.
            let idx =
                ((scaled * (SPARKS.len() - 1) as f32).ceil() as usize).clamp(1, SPARKS.len() - 1);
            SPARKS[idx]
        })
        .collect()
}

/// Colour for a 0..100 utilisation figure: calm when there is headroom, loud
/// when there is not.
pub fn ramp(pct: f32) -> Color {
    match pct {
        p if p >= 90.0 => Color::LightRed,
        p if p >= 75.0 => Color::Yellow,
        p if p >= 40.0 => Color::LightGreen,
        _ => Color::Green,
    }
}

/// Colour for a CPU package temperature. Thresholds are deliberately high:
/// build machines run hot, and colouring 70 °C as alarming would cry wolf.
pub fn temp_ramp(celsius: f32) -> Color {
    match celsius {
        t if t >= 90.0 => Color::LightRed,
        t if t >= 80.0 => Color::Yellow,
        _ => Color::Gray,
    }
}

/// Human-readable byte rate, for network throughput.
pub fn rate(bytes_per_sec: u64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    let mut value = bytes_per_sec as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0}{}/s", UNITS[unit])
    } else {
        format!("{value:.1}{}/s", UNITS[unit])
    }
}

/// Human-readable size from kibibytes, which is the unit `/proc/meminfo` uses.
pub fn size_kib(kib: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = kib as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // Whole kibibytes need no decimal; anything larger reads better with one.
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `HH:MM:SS` for an uptime or connection duration.
pub fn duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h >= 24 {
        format!("{}d {:02}:{:02}", h / 24, h % 24, m)
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// Colours used to tell one node from another.
///
/// Chosen by searching the 6×6×6 colour cube for twelve entries that are as far
/// apart as possible under three constraints, rather than picked by eye:
///
/// * **no warm hues.** Red, orange, yellow and tan carry *state* in this UI — a
///   problem badge, a saturated metric, a hot sensor — and a healthy node that
///   happened to hash into that range would read as a node in trouble.
/// * **nothing too dark**, or the name vanishes on a dark terminal.
/// * **no greys**, which already mean "no measurement".
///
/// The first attempt at this list was picked by hand and paired 39 with 45 —
/// one step apart in the cube, and the same colour to anyone glancing at a row.
const NODE_COLOURS: [u8; 12] = [27, 38, 42, 67, 87, 93, 118, 127, 141, 151, 201, 225];

/// A stable colour for a node, derived from its name.
///
/// Keyed by name rather than by row so a node keeps its colour when the list is
/// re-sorted, when other nodes come and go, and between sessions — recognising
/// the same machine across all that is the whole point.
///
/// With more nodes than colours, two will share one. This is a hint for the eye,
/// not an identifier: the name is still the name.
pub fn node_colour(name: &str) -> Color {
    // FNV-1a. Small, and stable in a way `DefaultHasher` does not promise
    // across Rust versions — a colour that changed when the toolchain changed
    // would defeat the point.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Color::Indexed(NODE_COLOURS[(hash % NODE_COLOURS.len() as u64) as usize])
}

/// A duration at a glance: one unit, no padding.
///
/// `HH:MM:SS` is right where figures are read against each other or against a
/// clock, but a badge inside a node's name column is neither — there it is
/// thirteen cells spent on precision nobody needs, taken from the hostname.
/// "roughly an hour" is the whole message.
pub fn brief_duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(s: &str, c: char) -> usize {
        s.chars().filter(|&x| x == c).count()
    }

    #[test]
    fn a_node_keeps_its_colour() {
        // The colour is only useful if it is the same one next time.
        assert_eq!(node_colour("build01"), node_colour("build01"));
        assert_ne!(
            node_colour("build01"),
            node_colour("build02"),
            "adjacent names should not collide"
        );
        // Not sensitive to anything but the name.
        assert_eq!(node_colour(""), node_colour(""));
    }

    /// Decode a 256-colour cube index into its 0..5 red, green and blue levels.
    fn cube_rgb(index: u8) -> (u8, u8, u8) {
        assert!(
            (16..232).contains(&index),
            "colour {index} is outside the 6x6x6 cube: the basic 16 are\n\
             theme-dependent and the greyscale ramp is not a hue"
        );
        let n = index - 16;
        (n / 36, (n % 36) / 6, n % 6)
    }

    #[test]
    fn node_colours_never_borrow_a_colour_that_means_trouble() {
        // Red, orange and yellow carry state in this UI — a problem badge, a
        // saturated metric, a hot sensor. A healthy node that hashed into that
        // range would read as a node in trouble. Those hues are the ones with
        // almost no blue and plenty of red, so that is what is excluded rather
        // than a hand-drawn list of indices.
        for index in NODE_COLOURS {
            let (r, g, b) = cube_rgb(index);
            assert!(
                r < g || r <= b,
                "colour {index} is ({r},{g},{b}) — red-dominant, so it reads as a warning"
            );
            assert!(
                b > 1 || r < 3 || g < 3,
                "colour {index} is ({r},{g},{b}) — yellow enough to read as a warning"
            );
            assert!(
                r.max(g).max(b) >= 3 && r + g + b >= 6,
                "colour {index} is ({r},{g},{b}) — too dark to read on a dark terminal"
            );
            assert!(
                !(r == g && g == b),
                "colour {index} is grey, which already means \"no measurement\""
            );
        }
    }

    #[test]
    fn the_node_palette_is_actually_distinguishable() {
        // Two colours a few cube steps apart are the same colour to a reader
        // glancing at a row.
        for (i, a) in NODE_COLOURS.iter().enumerate() {
            for b in &NODE_COLOURS[i + 1..] {
                let (ar, ag, ab) = cube_rgb(*a);
                let (br, bg, bb) = cube_rgb(*b);
                let distance = ar.abs_diff(br) + ag.abs_diff(bg) + ab.abs_diff(bb);
                assert!(distance >= 3, "{a} and {b} are too close to tell apart");
            }
        }
    }

    #[test]
    fn the_node_palette_has_no_duplicates() {
        let mut seen = NODE_COLOURS.to_vec();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "duplicate colours waste palette slots");
    }

    #[test]
    fn a_brief_duration_uses_one_unit_and_stays_short() {
        assert_eq!(brief_duration(0), "0s");
        assert_eq!(brief_duration(59), "59s");
        assert_eq!(brief_duration(60), "1m");
        assert_eq!(brief_duration(3599), "59m");
        assert_eq!(brief_duration(3600), "1h");
        assert_eq!(brief_duration(86_399), "23h");
        assert_eq!(brief_duration(86_400), "1d");
        // The point is the width: a badge must not push the hostname out.
        for secs in [0, 59, 3600, 86_400, 86_400 * 365] {
            assert!(brief_duration(secs).len() <= 4, "{secs}");
        }
    }

    #[test]
    fn a_bar_is_exactly_the_width_asked_for() {
        for pct in [0.0, 1.0, 33.3, 50.0, 99.9, 100.0] {
            for width in [1usize, 4, 10, 20] {
                let b = bar(pct, width);
                assert_eq!(
                    b.chars().count(),
                    width,
                    "pct {pct} width {width} gave {b:?}"
                );
            }
        }
    }

    #[test]
    fn empty_and_full_bars_are_unambiguous() {
        assert_eq!(bar(0.0, 5), "░░░░░");
        assert_eq!(bar(100.0, 5), "█████");
    }

    #[test]
    fn half_is_half_filled() {
        let b = bar(50.0, 10);
        assert_eq!(count(&b, '█'), 5);
        assert_eq!(count(&b, TROUGH), 5);
    }

    #[test]
    fn small_differences_survive_thanks_to_partial_blocks() {
        // Without eighth-blocks these would both round to one whole cell.
        let a = bar(12.0, 10);
        let b = bar(15.0, 10);
        assert_ne!(a, b, "{a:?} vs {b:?}");
    }

    #[test]
    fn a_tiny_nonzero_value_is_still_visible() {
        let b = bar(1.0, 10);
        assert_ne!(b, bar(0.0, 10), "1% must not look like 0%");
    }

    #[test]
    fn out_of_range_percentages_are_clamped_not_wrapped() {
        assert_eq!(bar(-50.0, 4), bar(0.0, 4));
        assert_eq!(bar(500.0, 4), bar(100.0, 4));
        assert_eq!(bar(f32::NAN, 4).chars().count(), 4);
    }

    #[test]
    fn zero_width_is_empty_rather_than_a_panic() {
        assert_eq!(bar(50.0, 0), "");
        assert_eq!(sparkline(&[], 100.0), "");
    }

    #[test]
    fn a_sparkline_is_one_glyph_per_sample() {
        let s = sparkline(&[0.0, 25.0, 50.0, 75.0, 100.0], 100.0);
        assert_eq!(s.chars().count(), 5);
    }

    #[test]
    fn a_sparkline_rises_monotonically_with_its_input() {
        let s: Vec<char> = sparkline(&[0.0, 20.0, 40.0, 60.0, 80.0, 100.0], 100.0)
            .chars()
            .collect();
        let idx = |c: char| SPARKS.iter().position(|&x| x == c).unwrap();
        for w in s.windows(2) {
            assert!(idx(w[1]) >= idx(w[0]), "not monotonic: {s:?}");
        }
        assert_eq!(s[5], '█');
    }

    #[test]
    fn a_measured_zero_and_a_gap_look_different() {
        let s = sparkline(&[0.0, f32::NAN], 100.0);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars[0], '▁', "a real zero is information");
        assert_eq!(chars[1], ' ', "a gap must not read as zero");
    }

    #[test]
    fn any_nonzero_value_sits_above_the_baseline() {
        // 0.5% of the scale would otherwise round down to the zero glyph.
        let s = sparkline(&[0.5], 100.0);
        assert_ne!(s.chars().next().unwrap(), '▁');
    }

    #[test]
    fn a_zero_or_absurd_max_does_not_divide_by_zero() {
        assert_eq!(sparkline(&[5.0], 0.0).chars().count(), 1);
        assert_eq!(sparkline(&[5.0], f32::NAN).chars().count(), 1);
    }

    #[test]
    fn the_colour_ramp_escalates_with_pressure() {
        assert_eq!(ramp(10.0), Color::Green);
        assert_eq!(ramp(50.0), Color::LightGreen);
        assert_eq!(ramp(80.0), Color::Yellow);
        assert_eq!(ramp(95.0), Color::LightRed);
    }

    #[test]
    fn build_machines_are_allowed_to_be_warm() {
        // 70C on a compiling node is normal and must not be coloured as alarm.
        assert_eq!(temp_ramp(70.0), Color::Gray);
        assert_eq!(temp_ramp(85.0), Color::Yellow);
        assert_eq!(temp_ramp(95.0), Color::LightRed);
    }

    #[test]
    fn byte_rates_are_readable() {
        assert_eq!(rate(0), "0B/s");
        assert_eq!(rate(999), "999B/s");
        assert_eq!(rate(1024), "1.0K/s");
        assert_eq!(rate(1_200_000), "1.1M/s");
        assert_eq!(rate(5_368_709_120), "5.0G/s");
    }

    #[test]
    fn sizes_come_from_kibibytes_because_meminfo_does() {
        assert_eq!(size_kib(0), "0 KiB");
        assert_eq!(size_kib(512), "512 KiB");
        assert_eq!(size_kib(1024), "1.0 MiB");
        // The development machine's 65540720 KiB of RAM.
        assert_eq!(size_kib(65_540_720), "62.5 GiB");
    }

    #[test]
    fn durations_switch_to_days_when_long() {
        assert_eq!(duration(0), "00:00:00");
        assert_eq!(duration(3661), "01:01:01");
        assert_eq!(duration(275_290), "3d 04:28");
    }
}
