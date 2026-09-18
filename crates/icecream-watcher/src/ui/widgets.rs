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

/// A stretch of work, to the precision the figure beside it deserves.
///
/// Unlike [`brief_duration`], which drops everything below its leading unit:
/// "3m" and "3m59s" are the same answer to "how long has this been running",
/// and a different one to "how long did this build take".
pub fn span(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Colours used to tell one node from another.
///
/// Searched for rather than picked by eye, under four constraints:
///
/// * **no warm hues.** Red, orange, yellow and tan carry *state* in this UI — a
///   problem badge, a saturated metric, a hot sensor — and a healthy node that
///   happened to hash into that range would read as a node in trouble. This is
///   the expensive one: it reserves half the wheel, leaving the 215° from
///   yellow-green round to magenta for everything here.
/// * **nothing too dark**, or the name vanishes on the screen's black. An early
///   version of this list said so and did not hold to it: it contained
///   `(0, 95, 255)`, whose blue channel is at full while its luminance is 86,
///   and that node was reported as unreadable. The floor is on *luminance*,
///   because that is what "dark on black" means — a saturated blue can max a
///   channel and still be dim. It is 155.
/// * **no greys**, which already mean "no measurement": every entry keeps at
///   least 60 between its strongest and weakest channel.
/// * **spread round the wheel**, at least 10° of hue between any two.
///
/// That last rule is the one this list spent a while without, and its absence
/// is what made four nodes on one cluster read as the same green. The rule it
/// replaces asked only that two entries be three steps apart in the 6×6×6
/// cube, which counts lightness as distance: `(135,255,0)`, `(135,175,95)`,
/// `(175,215,135)` and `(215,255,175)` all passed it comfortably, and all four
/// are yellow-green to anyone glancing down a column of names. Eight of the
/// twelve sat between 88° and 180°, and nothing at all sat between 210° and
/// 300°. Hue distance is what "I can tell these apart" means for a word on a
/// dark screen; cube distance was measuring the wrong thing.
///
/// So the arc is divided into twelve 18° slices and each entry comes from the
/// middle of its own slice, taking whatever lightness within it sits furthest
/// from the entries already chosen. That drops the closest pair in RGB terms
/// from about 120 to 43 — the price of the swap — while raising the closest
/// pair in hue from 0° to 11°, and it leaves six of the twelve rather than
/// eight in the green-to-cyan half.
/// The order is not the order they were found in. Names on one cluster are
/// rarely unrelated — `build01` to `build12`, or a `-desktop` and a
/// `-desktop-2` — and near-identical names hash to near-identical values, which
/// land on *neighbouring* slots. Listed by hue, neighbouring slots are
/// neighbouring colours, and a cluster named that way comes out all one shade:
/// four nodes on a real one drew hues 95°, 114°, 133° and 170°, every one of
/// them a green, from a list that was properly spread.
///
/// So the twelve are laid out at a stride of five round the wheel, five being
/// coprime with twelve and the stride that maximises the worst case. Adjacent
/// slots are at least 82° apart, and those same four nodes now draw 95°, 188°,
/// 234° and 272° — a green, a cyan, an indigo and an orchid.
const NODE_COLOURS: [(u8, u8, u8); 12] = [
    (0xa8, 0xff, 0x69), // yellow-green, hue  95
    (0x05, 0xd5, 0xf7), // cyan,         hue 188
    (0xb6, 0x8d, 0xd9), // orchid,       hue 272
    (0x05, 0xff, 0x8c), // spring,       hue 152
    (0x8d, 0x97, 0xf0), // indigo,       hue 234
    (0x1b, 0xe8, 0x05), // green,        hue 114
    (0x78, 0xa2, 0xc2), // slate,        hue 206
    (0xf7, 0x78, 0xff), // magenta,      hue 296
    (0x70, 0xff, 0xe8), // mint,         hue 170
    (0xbf, 0x9e, 0xff), // violet,       hue 260
    (0x4c, 0xba, 0x63), // moss,         hue 133
    (0x9e, 0xc3, 0xff), // sky,          hue 217
];

/// Green-to-red stops for the load ramp, as 24-bit RGB.
///
/// Green through yellow-green, amber and orange to red — the convention for
/// "fine, getting busy, at the limit". Every channel is monotonic along the
/// path (red never falls, green never rises), so the ramp cannot appear to cool
/// as the figure it stands for climbs; a gradient that doubles back reads as
/// noise however pretty the individual colours are.
const HEAT_STOPS: [(u8, u8, u8); 5] = [
    (40, 220, 60),
    (150, 215, 50),
    (255, 210, 40),
    (255, 140, 40),
    (255, 50, 40),
];

/// The ramp as true 24-bit colour. `0.0` is green, `1.0` is red.
fn heat_rgb(fraction: f32) -> (u8, u8, u8) {
    let f = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let last = HEAT_STOPS.len() - 1;
    let pos = f * last as f32;
    let lower = (pos.floor() as usize).min(last);
    let upper = (lower + 1).min(last);
    let t = pos - lower as f32;

    let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    let (ar, ag, ab) = HEAT_STOPS[lower];
    let (br, bg, bb) = HEAT_STOPS[upper];
    (mix(ar, br), mix(ag, bg), mix(ab, bb))
}

static TRUECOLOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Force the ramp onto the 216-colour cube, for a terminal that cannot do
/// better.
///
/// On by default rather than sniffed, because `COLORTERM` is absent far more
/// often than 24-bit colour is: it goes missing over `ssh`, under `sudo`, and
/// in anything that sanitises the environment, and the cost of guessing wrong
/// in that direction is a gradient nobody can see. A terminal that really
/// cannot show RGB approximates it, so the cost of guessing wrong the other way
/// is smaller — and `--colors 256` settles it either way.
pub fn set_truecolor(enabled: bool) {
    TRUECOLOR.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// Whether to emit 24-bit colour. Without it the ramp rounds to the 6×6×6 cube,
/// where each channel has six levels and a green-to-red sweep collapses to about
/// a dozen distinguishable steps — enough to read, not enough to look gradual.
fn truecolor() -> bool {
    TRUECOLOR.load(std::sync::atomic::Ordering::Relaxed)
}

/// Nearest 6×6×6 cube colour, for terminals that cannot do better.
fn nearest_cube(r: u8, g: u8, b: u8) -> Color {
    let level = |v: u8| (f32::from(v) / 255.0 * 5.0).round() as u16;
    Color::Indexed((16 + 36 * level(r) + 6 * level(g) + level(b)) as u8)
}

/// A fixed colour, rounded to the 6x6x6 cube where 24-bit colour is off.
///
/// Use this rather than `Color::Rgb` for anything chosen by hand: on a terminal
/// that cannot show RGB, the cube is what it will approximate to anyway, and
/// rounding here keeps that approximation ours.
pub fn rgb(r: u8, g: u8, b: u8) -> Color {
    if truecolor() {
        Color::Rgb(r, g, b)
    } else {
        nearest_cube(r, g, b)
    }
}

/// The screen's own foreground: real white, not the terminal's colour 15.
///
/// The same argument as [`background`]. Slot 15 is "bright white" only by
/// convention — a theme is free to paint it cream, or a grey a shade off the
/// one this uses for secondary text, and then the two levels of the screen's
/// hierarchy stop being two levels.
pub fn foreground() -> Color {
    if truecolor() {
        Color::Rgb(255, 255, 255)
    } else {
        Color::Indexed(231) // the cube's white, not palette slot 15
    }
}

/// The screen's own background: real black, not the terminal's colour 0.
///
/// Colour 0 is whatever the user's theme says it is — Solarized paints it a
/// dark slate, and a light theme may paint it something else again — so a
/// palette tuned against black would be sitting on an unknown colour. Where
/// 24-bit colour is off, the cube's own black is the closest thing available.
pub fn background() -> Color {
    if truecolor() {
        Color::Rgb(0, 0, 0)
    } else {
        Color::Indexed(16) // the 6x6x6 cube's black, not palette slot 0
    }
}

/// A colour from the green-to-red ramp. `0.0` is green, `1.0` is red.
pub fn heat(fraction: f32) -> Color {
    let (r, g, b) = heat_rgb(fraction);
    if truecolor() {
        Color::Rgb(r, g, b)
    } else {
        nearest_cube(r, g, b)
    }
}

/// A stable colour for a node, derived from its name.
///
/// Keyed by name rather than by row so a node keeps its colour when the list is
/// re-sorted, when other nodes come and go, and between sessions — recognising
/// the same machine across all that is the whole point.
///
/// With more nodes than colours, two will share one. This is a hint for the eye,
/// not an identifier: the name is still the name.
///
/// The colour goes out through [`rgb`], like every other colour chosen by hand
/// here. The list used to be cube indices sent as `Color::Indexed`, and was
/// reported as reading all of one hue on a terminal with a customised palette:
/// a cube index is not the fixed thing it looks like, because the 240 slots
/// above the basic 16 are as remappable as slots 0 and 15 are, and a theme that
/// repaints them repaints this list. It is the argument [`foreground`] and
/// [`background`] already make about white and black, applied to the one list
/// that had been left out of it.
pub fn node_colour(name: &str) -> Color {
    // FNV-1a. Small, and stable in a way `DefaultHasher` does not promise
    // across Rust versions — a colour that changed when the toolchain changed
    // would defeat the point.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Fold the halves together before reducing. `hash % len` on its own reads
    // only the low bits, which FNV-1a barely mixes — two names whose hashes
    // differ by a multiple of the list length then share an entry however long
    // the list is, and on one real cluster `gyuyoung-ThinkPad-P1` and
    // `gyuyoung-desktop-2` did exactly that for 12, 16, 20 and 24 colours
    // alike.
    //
    // The high bits alone are no better, and for these names they are worse.
    // FNV's prime is 2^40 + 0x1b3, so a name differing in its last byte differs
    // by about that byte times 2^40: the change lands around bit 40 and leaves
    // the top twenty bits alone. Taking the top bits put all of `build01` to
    // `build12` on one colour. Folding uses both ends and gives eight colours
    // across those twelve names, against seven for the low bits alone.
    let folded = hash ^ (hash >> 32);
    let (r, g, b) = NODE_COLOURS[(folded % NODE_COLOURS.len() as u64) as usize];
    rgb(r, g, b)
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
    fn the_heat_ramp_runs_green_to_red() {
        assert_eq!(heat_rgb(0.0), HEAT_STOPS[0], "0 should be green");
        assert_eq!(
            heat_rgb(1.0),
            *HEAT_STOPS.last().unwrap(),
            "1 should be red"
        );

        // Monotonic in both channels: a gradient that doubles back reads as
        // noise, however pretty its individual colours are.
        let (mut red, mut green) = (0u8, 255u8);
        for step in 0..=200 {
            let (r, g, _) = heat_rgb(step as f32 / 200.0);
            assert!(r >= red, "red fell at {step}");
            assert!(g <= green, "green rose at {step}");
            red = r;
            green = g;
        }
    }

    #[test]
    fn the_heat_ramp_is_actually_gradual() {
        // The point of true colour here: 100 steps should not collapse into a
        // handful of shades the way the 216-colour cube does.
        let shades: std::collections::BTreeSet<(u8, u8, u8)> =
            (0..=100).map(|i| heat_rgb(i as f32 / 100.0)).collect();
        assert!(shades.len() > 80, "only {} distinct shades", shades.len());

        // And the fallback still spans the ramp rather than flattening it.
        let cube: std::collections::BTreeSet<String> = (0..=100)
            .map(|i| {
                let (r, g, b) = heat_rgb(i as f32 / 100.0);
                format!("{:?}", nearest_cube(r, g, b))
            })
            .collect();
        assert!(
            cube.len() >= 8,
            "fallback collapsed to {} shades",
            cube.len()
        );
    }

    #[test]
    fn the_heat_ramp_cannot_be_broken_by_a_bad_fraction() {
        assert_eq!(heat(-1.0), heat(0.0));
        assert_eq!(heat(5.0), heat(1.0));
        assert_eq!(heat(f32::NAN), heat(0.0));
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

    /// Where a colour sits on the wheel, in degrees. Red is 0, green 120.
    fn hue(colour: (u8, u8, u8)) -> f32 {
        let (r, g, b) = (
            f32::from(colour.0) / 255.0,
            f32::from(colour.1) / 255.0,
            f32::from(colour.2) / 255.0,
        );
        let max = r.max(g).max(b);
        let span = max - r.min(g).min(b);
        if span == 0.0 {
            return 0.0; // grey has no hue; `no_node_colour_is_a_grey` rejects it
        }
        let h = if max == r {
            ((g - b) / span).rem_euclid(6.0)
        } else if max == g {
            (b - r) / span + 2.0
        } else {
            (r - g) / span + 4.0
        };
        (h * 60.0).rem_euclid(360.0)
    }

    fn luminance(colour: (u8, u8, u8)) -> f32 {
        0.2126 * f32::from(colour.0) + 0.7152 * f32::from(colour.1) + 0.0722 * f32::from(colour.2)
    }

    #[test]
    fn node_colours_never_borrow_a_colour_that_means_trouble() {
        // Red, orange and yellow carry state in this UI — a problem badge, a
        // saturated metric, a hot sensor. A healthy node that hashed into that
        // range would read as a node in trouble. Those hues are the ones with
        // almost no blue and plenty of red, so that is what is excluded rather
        // than a hand-drawn list of colours.
        for colour in NODE_COLOURS {
            let (r, g, b) = colour;
            assert!(
                r < g || r <= b,
                "{colour:?} is red-dominant, so it reads as a warning"
            );
            assert!(
                b >= 135 || r < 175 || g < 175,
                "{colour:?} is yellow enough to read as a warning"
            );
        }
    }

    #[test]
    fn no_node_colour_is_a_grey() {
        // Grey already means "no measurement" on this screen.
        for colour in NODE_COLOURS {
            let (r, g, b) = colour;
            let span = r.max(g).max(b) - r.min(g).min(b);
            assert!(
                span >= 60,
                "{colour:?} spans only {span} between its channels, which reads as grey"
            );
        }
    }

    #[test]
    fn the_node_palette_is_spread_round_the_wheel() {
        // The rule this list spent a while without, and the reason four nodes on
        // one cluster all read as the same green. Its predecessor asked only for
        // three steps of cube distance, which counts lightness: (135,255,0),
        // (135,175,95), (175,215,135) and (215,255,175) all passed, and all four
        // are yellow-green to a reader glancing down a column of names.
        for (i, a) in NODE_COLOURS.iter().enumerate() {
            for b in &NODE_COLOURS[i + 1..] {
                let gap = (hue(*a) - hue(*b)).abs();
                let gap = gap.min(360.0 - gap);
                assert!(
                    gap >= 10.0,
                    "{a:?} and {b:?} are {gap:.0}° apart: the same hue to a reader"
                );
            }
        }
    }

    #[test]
    fn the_node_palette_is_actually_distinguishable() {
        // Hue carries the work now, but two entries of the same hue and nearly
        // the same lightness would still be one colour, so the floor stays.
        for (i, a) in NODE_COLOURS.iter().enumerate() {
            for b in &NODE_COLOURS[i + 1..] {
                let d = |x: u8, y: u8| (f32::from(x) - f32::from(y)).powi(2);
                let distance = (d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)).sqrt();
                assert!(
                    distance >= 40.0,
                    "{a:?} and {b:?} are only {distance:.0} apart"
                );
            }
        }
    }

    #[test]
    fn no_node_colour_is_dim_against_the_screen() {
        // The rule the list once claimed and did not keep: it held (0, 95, 255),
        // whose blue channel is at full and whose luminance is 86, and the node
        // wearing it was reported as unreadable. Brightest channel is not the
        // test; luminance is.
        for colour in NODE_COLOURS {
            let l = luminance(colour);
            assert!(
                l >= 155.0,
                "{colour:?} has luminance {l:.0}: too dim to read on black"
            );
        }
    }

    #[test]
    fn neighbouring_slots_are_not_neighbouring_colours() {
        // Names on one cluster are rarely unrelated, and near-identical names
        // hash to near-identical values, which land on neighbouring slots. A
        // list in hue order therefore hands a uniformly-named cluster one shade:
        // four nodes on a real one drew 95°, 114°, 133° and 170°, all green.
        for (i, a) in NODE_COLOURS.iter().enumerate() {
            let b = &NODE_COLOURS[(i + 1) % NODE_COLOURS.len()];
            let gap = (hue(*a) - hue(*b)).abs();
            let gap = gap.min(360.0 - gap);
            assert!(
                gap >= 60.0,
                "slots {i} and {} are {gap:.0}° apart: too close for two names \
                 that differ by a character",
                (i + 1) % NODE_COLOURS.len()
            );
        }
    }

    #[test]
    fn a_uniformly_named_cluster_does_not_come_out_one_colour() {
        // The four names from the cluster that reported this.
        let names = [
            "Gyuyoung-MacBook-Pro.local",
            "gyuyoung-ThinkPad-P1",
            "gyuyoung-desktop",
            "gyuyoung-desktop-2",
        ];
        let seen: std::collections::BTreeSet<String> = names
            .iter()
            .map(|n| format!("{:?}", node_colour(n)))
            .collect();
        assert_eq!(seen.len(), names.len(), "two of {names:?} share a colour");
    }

    #[test]
    fn two_names_whose_hashes_differ_by_a_multiple_of_the_palette_still_differ() {
        // FNV-1a's last act is a multiply, so its low bits are barely mixed and
        // `hash % len` collides for any two names whose hashes differ by a
        // multiple of `len`. These two are from a real four-node cluster, where
        // they shared a colour for 12, 16, 20 and 24 palette entries alike, and
        // the fix was to reduce from the high bits instead.
        assert_ne!(
            node_colour("gyuyoung-ThinkPad-P1"),
            node_colour("gyuyoung-desktop-2"),
            "these two collided under `hash % len`"
        );
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
    fn durations_switch_to_days_when_long() {
        assert_eq!(duration(0), "00:00:00");
        assert_eq!(duration(3661), "01:01:01");
        assert_eq!(duration(275_290), "3d 04:28");
    }
}
