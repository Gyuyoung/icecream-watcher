//! Braille-dot graphs, in the style `btop` uses.
//!
//! A block sparkline gets eight levels out of one character cell. A braille
//! cell carries two dot columns by four dot rows, so an `n`-row graph has `4n`
//! vertical levels and twice the horizontal resolution — which is why btop's
//! graphs read as curves rather than as bar charts, and why this only earns its
//! keep where there is vertical room to spend. One row of braille would be
//! *four* levels, worse than the blocks it replaced, so the node table keeps
//! its block sparklines and this is used where the layout can give a series
//! two rows or more.

use ratatui::style::Color;

use crate::ui::widgets;

/// Dot columns in one braille character.
pub const CELL_COLS: usize = 2;
/// Dot rows in one braille character.
pub const CELL_ROWS: usize = 4;

/// The bit for each dot, indexed `[column][row]` with row 0 at the top.
///
/// Not a plain grid: braille was six dots before the eight-dot extension, so
/// the bottom row was bolted on as the two high bits rather than continuing the
/// sequence.
const DOT_BITS: [[u8; CELL_ROWS]; CELL_COLS] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

/// U+2800 BRAILLE PATTERN BLANK. Adding the dot bits to it gives the glyph.
const BRAILLE_BASE: u32 = 0x2800;

/// Render `values` as a filled area graph `width` characters wide and `rows`
/// tall, topmost row first.
///
/// `values` is indexed by **dot** column, so it should hold `width * CELL_COLS`
/// samples; the caller decides how history maps onto the axis. A non-finite
/// sample draws nothing at all, so a gap in measurement stays visibly different
/// from a measured zero — which draws the bottom dot, exactly as the block
/// sparkline draws its lowest glyph.
pub fn area(values: &[f32], max: f32, width: usize, rows: usize) -> Vec<String> {
    if width == 0 || rows == 0 {
        return Vec::new();
    }
    let max = if max.is_finite() && max > 0.0 {
        max
    } else {
        1.0
    };
    let dot_rows = rows * CELL_ROWS;
    let mut cells = vec![vec![0u8; width]; rows];

    for (x, &value) in values.iter().enumerate().take(width * CELL_COLS) {
        if !value.is_finite() {
            continue; // a gap: no dots, not a zero
        }
        let scaled = (value / max).clamp(0.0, 1.0);
        // At least one dot for any measured value, so zero is information
        // rather than an empty column that reads as "no data".
        let level = ((scaled * dot_rows as f32).round() as usize).clamp(1, dot_rows);

        let cell_col = x / CELL_COLS;
        let sub_col = x % CELL_COLS;
        for filled in 0..level {
            let dot_row = dot_rows - 1 - filled; // fill upward from the baseline
            cells[dot_row / CELL_ROWS][cell_col] |= DOT_BITS[sub_col][dot_row % CELL_ROWS];
        }
    }

    cells
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|bits| char::from_u32(BRAILLE_BASE + bits as u32).unwrap_or(' '))
                .collect()
        })
        .collect()
}

/// Render `pct` as a horizontal bar `width` characters wide, in braille dots.
///
/// Two dot columns to a character, so this resolves twice as finely as a block
/// bar of the same width. The unfilled part keeps a baseline row of dots rather
/// than going blank, so the bar's full extent — and therefore what the filled
/// part is a fraction *of* — stays visible.
/// A bar cut where the fill ends, so the two parts can be coloured apart.
///
/// A cell the fill only half covers belongs to the filled part: it is the fill
/// that it is showing, and colouring it as trough would round the reading down
/// by half a cell on every bar.
pub fn bar_split(pct: f32, width: usize) -> (String, String) {
    if width == 0 {
        return (String::new(), String::new());
    }
    let dot_cols = width * CELL_COLS;
    let filled = (pct.clamp(0.0, 100.0) / 100.0 * dot_cols as f32).round() as usize;

    let mut cells = vec![0u8; width];
    for x in 0..dot_cols {
        let column = DOT_BITS[x % CELL_COLS];
        cells[x / CELL_COLS] |= if x < filled {
            column.iter().fold(0u8, |acc, bit| acc | bit)
        } else {
            column[CELL_ROWS - 1] // baseline only
        };
    }
    let glyphs: Vec<char> = cells
        .into_iter()
        .map(|bits| char::from_u32(BRAILLE_BASE + bits as u32).unwrap_or(' '))
        .collect();
    let cut = filled.div_ceil(CELL_COLS).min(width);
    (
        glyphs[..cut].iter().collect(),
        glyphs[cut..].iter().collect(),
    )
}

/// Colour for character row `r` of an `rows`-tall graph whose axis is a 0..100
/// utilisation.
///
/// Colour comes from the row's height rather than from the value, so a graph
/// reads the same way as the bars beside it: the band near the top is the
/// alarming one wherever the curve happens to be. Series with no natural
/// maximum — a queue, a completion rate — must *not* use this: at peak scaling
/// the top row means "the most we have seen", which is not the same as "full".
pub fn row_colour(r: usize, rows: usize) -> Color {
    if rows == 0 {
        return Color::Green;
    }
    // The same ramp the node meters use, so a band row and a row in the table
    // at the same height mean the same thing.
    let midpoint = ((rows - r) as f32 - 0.5) / rows as f32;
    widgets::heat(midpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole bar as one string. The renderer colours the two parts
    /// differently and so keeps them apart; these tests read the shape.
    fn bar(pct: f32, width: usize) -> String {
        let (filled, trough) = bar_split(pct, width);
        filled + &trough
    }

    fn flat(value: f32, dots: usize) -> Vec<f32> {
        vec![value; dots]
    }

    #[test]
    fn the_grid_is_the_size_asked_for() {
        for (w, h) in [(1usize, 1usize), (4, 2), (30, 3), (60, 6)] {
            let out = area(&flat(50.0, w * CELL_COLS), 100.0, w, h);
            assert_eq!(out.len(), h, "{w}x{h} rows");
            for line in &out {
                assert_eq!(line.chars().count(), w, "{w}x{h} width");
            }
        }
    }

    #[test]
    fn full_is_solid_and_empty_is_blank() {
        let full = area(&flat(100.0, 8), 100.0, 4, 2);
        assert!(full.iter().all(|l| l.chars().all(|c| c == '⣿')), "{full:?}");

        // No samples at all: every cell is the blank braille pattern, so the
        // graph area keeps its shape without implying a measured zero.
        let empty = area(&[f32::NAN; 8], 100.0, 4, 2);
        assert!(
            empty.iter().all(|l| l.chars().all(|c| c == '⠀')),
            "{empty:?}"
        );
    }

    #[test]
    fn a_measured_zero_is_not_a_gap() {
        // The trap this whole codebase keeps guarding against: a node whose
        // agent was down must not look like a node that was idle.
        let zero = area(&flat(0.0, 2), 100.0, 1, 1);
        let gap = area(&[f32::NAN; 2], 100.0, 1, 1);
        assert_ne!(zero, gap);
        assert_eq!(zero[0], "⣀", "a zero draws the baseline dots");
    }

    #[test]
    fn the_area_fills_upward_from_the_baseline() {
        // Half height over a 2-row graph means the bottom row solid and the
        // top row blank, not the other way up.
        let out = area(&flat(50.0, 2), 100.0, 1, 2);
        assert_eq!(out[0], "⠀", "top row should be empty");
        assert_eq!(out[1], "⣿", "bottom row should be full");
    }

    #[test]
    fn taller_graphs_resolve_finer_differences() {
        // The whole reason to spend vertical space. One row is four levels —
        // 25% each — so two readings 7% apart land on the same dot; four rows
        // is sixteen levels and tells them apart.
        assert_eq!(
            area(&flat(50.0, 2), 100.0, 1, 1),
            area(&flat(57.0, 2), 100.0, 1, 1),
            "one row cannot resolve 7%"
        );
        assert_ne!(
            area(&flat(50.0, 2), 100.0, 1, 4),
            area(&flat(57.0, 2), 100.0, 1, 4),
            "four rows should resolve 7%"
        );
    }

    #[test]
    fn out_of_range_values_are_clamped_not_wrapped() {
        let over = area(&flat(500.0, 2), 100.0, 1, 1);
        assert_eq!(over, area(&flat(100.0, 2), 100.0, 1, 1));
        let under = area(&flat(-20.0, 2), 100.0, 1, 1);
        assert_eq!(under, area(&flat(0.0, 2), 100.0, 1, 1));
    }

    #[test]
    fn a_nonsense_maximum_cannot_panic_or_blank_the_graph() {
        for max in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let out = area(&flat(1.0, 2), max, 1, 1);
            assert_eq!(out.len(), 1);
        }
    }

    #[test]
    fn the_two_dot_columns_of_a_cell_are_independent() {
        // Half the horizontal resolution would be silently lost if both dot
        // columns of a cell shared a value.
        let rising = area(&[0.0, 100.0], 100.0, 1, 1);
        let falling = area(&[100.0, 0.0], 100.0, 1, 1);
        assert_ne!(rising, falling, "a cell must carry two samples, not one");
        assert_eq!(rising[0], "⣸", "left baseline only, right full: {rising:?}");
        assert_eq!(
            falling[0], "⣇",
            "left full, right baseline only: {falling:?}"
        );
    }

    #[test]
    fn a_dot_bar_is_exactly_the_width_asked_for() {
        for pct in [0.0, 1.0, 33.3, 50.0, 99.9, 100.0] {
            for width in [1usize, 4, 10, 20] {
                let b = bar(pct, width);
                assert_eq!(b.chars().count(), width, "{pct} at {width}: {b:?}");
            }
        }
        assert!(bar(50.0, 0).is_empty());
    }

    #[test]
    fn a_dot_bar_shows_its_full_extent_when_empty() {
        // A blank trough would leave no way to see what the filled part is a
        // fraction of.
        assert_eq!(bar(0.0, 4), "⣀⣀⣀⣀");
        assert_eq!(bar(100.0, 4), "⣿⣿⣿⣿");
    }

    #[test]
    fn a_dot_bar_resolves_half_a_character() {
        // The reason to use dots here: a block bar of this width could only
        // round 50% to a whole cell either way.
        assert_eq!(bar(50.0, 1), "⣇", "half of one cell is its left column");
        assert_eq!(bar(12.5, 4), "⣇⣀⣀⣀");
    }

    #[test]
    fn a_dot_bar_clamps_rather_than_overflowing() {
        assert_eq!(bar(-10.0, 4), bar(0.0, 4));
        assert_eq!(bar(400.0, 4), bar(100.0, 4));
    }

    #[test]
    fn colour_follows_height_not_the_curve() {
        let rows = 4;
        assert_eq!(row_colour(0, rows), widgets::heat(0.875));
        assert_eq!(row_colour(rows - 1, rows), widgets::heat(0.125));
        assert_eq!(row_colour(0, 0), Color::Green, "must not divide by zero");
    }
}
