//! Fixed-size sample history, for sparklines and trend arrows.
//!
//! Samples are pushed on a timer rather than per event, so a node that goes
//! quiet still advances through the buffer instead of freezing its graph. A
//! sample we could not take is stored as `NaN`, which renders as a gap: a node
//! whose agent was down for ten seconds must not look like a node that was idle
//! for ten seconds.

use std::collections::VecDeque;

/// Two minutes at 1 Hz. Enough to see a build ramp up and drain, small enough
/// that a 200-node cluster costs well under a megabyte.
pub const DEFAULT_CAPACITY: usize = 120;

/// Which way a series is heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trend {
    Rising,
    Falling,
    Steady,
}

impl Trend {
    /// A word, because an arrow alone does not answer "is the queue growing?".
    pub fn label(&self) -> &'static str {
        match self {
            Self::Rising => "rising",
            Self::Falling => "draining",
            Self::Steady => "steady",
        }
    }

    pub fn arrow(&self) -> char {
        match self {
            Self::Rising => '↑',
            Self::Falling => '↓',
            Self::Steady => '→',
        }
    }
}

#[derive(Debug, Clone)]
pub struct History {
    samples: VecDeque<f32>,
    capacity: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
        }
    }

    /// Record a sample. `None` means "not measured", kept as a gap.
    pub fn push(&mut self, value: Option<f32>) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(value.unwrap_or(f32::NAN));
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Most recent measured value, skipping trailing gaps.
    pub fn last(&self) -> Option<f32> {
        self.samples.iter().rev().find(|v| v.is_finite()).copied()
    }

    /// Largest measured value, for scaling a graph whose range is not 0..100.
    /// Mean of the newest `n` measured samples.
    ///
    /// For a figure that is a *rate*: one tick of a completion counter is a
    /// small integer, and a headline that steps 0, 3, 0, 5 reads as noise
    /// rather than as throughput. The graph underneath still has every tick.
    pub fn mean_recent(&self, n: usize) -> Option<f32> {
        let measured: Vec<f32> = self
            .samples
            .iter()
            .rev()
            .take(n)
            .copied()
            .filter(|v| v.is_finite())
            .collect();
        (!measured.is_empty()).then(|| measured.iter().sum::<f32>() / measured.len() as f32)
    }

    pub fn max(&self) -> Option<f32> {
        self.samples
            .iter()
            .filter(|v| v.is_finite())
            .copied()
            .fold(None, |acc: Option<f32>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
    }

    /// The whole retained window drawn across `width` columns, oldest first.
    ///
    /// Unlike [`Self::window`], which gives one column per sample, this keeps
    /// the **time axis fixed**: a full buffer always spans the full width, so a
    /// wide graph shows the same two minutes as a narrow one, drawn larger. A
    /// buffer that is a third full occupies only the right-hand third, which
    /// keeps the promise that a young series grows in from the right rather
    /// than stretching a handful of samples across the whole box.
    ///
    /// No value is invented between samples: a column that covers several
    /// samples averages them, and a column covering one repeats it.
    pub fn stretched(&self, width: usize) -> Vec<f32> {
        if width == 0 {
            return Vec::new();
        }
        let have = self.samples.len();
        if have == 0 {
            return vec![f32::NAN; width];
        }
        // How much of the axis this much history has earned.
        let filled = ((width * have) as f64 / self.capacity as f64).round() as usize;
        let filled = filled.clamp(1, width);

        let mut out = vec![f32::NAN; width - filled];
        for i in 0..filled {
            let start = (i * have) / filled;
            let end = ((((i + 1) * have) / filled).max(start + 1)).min(have);
            let mut sum = 0.0f32;
            let mut n = 0u32;
            for v in self.samples.iter().skip(start).take(end - start) {
                if v.is_finite() {
                    sum += *v;
                    n += 1;
                }
            }
            out.push(if n > 0 { sum / n as f32 } else { f32::NAN });
        }
        out
    }

    /// The most recent `width` samples, oldest first, padded at the front with
    /// gaps so a young series draws at the right-hand edge and grows leftwards
    /// rather than stretching to fill the space.
    pub fn window(&self, width: usize) -> Vec<f32> {
        if width == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(width);
        let have = self.samples.len();
        if have < width {
            out.extend(std::iter::repeat(f32::NAN).take(width - have));
            out.extend(self.samples.iter().copied());
        } else {
            // More samples than pixels: average each bucket rather than
            // dropping samples, so a spike is not silently skipped.
            let per = have as f64 / width as f64;
            for i in 0..width {
                let start = (i as f64 * per).floor() as usize;
                let end = (((i + 1) as f64 * per).ceil() as usize).min(have);
                let bucket: Vec<f32> = self
                    .samples
                    .iter()
                    .skip(start)
                    .take(end.saturating_sub(start).max(1))
                    .copied()
                    .filter(|v| v.is_finite())
                    .collect();
                out.push(if bucket.is_empty() {
                    f32::NAN
                } else {
                    bucket.iter().sum::<f32>() / bucket.len() as f32
                });
            }
        }
        out
    }

    /// Direction of travel, comparing the mean of the newest third against the
    /// mean of the oldest third.
    ///
    /// `min_delta` is the change worth calling a trend, in the series' own
    /// units — without it, a queue oscillating between 3 and 4 jobs would
    /// alternate between "rising" and "draining" every second.
    pub fn trend(&self, min_delta: f32) -> Trend {
        let measured: Vec<f32> = self
            .samples
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .collect();
        // Too short to say anything; claiming a trend from two samples is noise.
        if measured.len() < 6 {
            return Trend::Steady;
        }
        let third = measured.len() / 3;
        let mean = |s: &[f32]| s.iter().sum::<f32>() / s.len() as f32;
        let old = mean(&measured[..third]);
        let new = mean(&measured[measured.len() - third..]);

        if (new - old).abs() < min_delta {
            Trend::Steady
        } else if new > old {
            Trend::Rising
        } else {
            Trend::Falling
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled_with(values: &[f32]) -> History {
        let mut h = History::new(120);
        for v in values {
            h.push(Some(*v));
        }
        h
    }

    #[test]
    fn a_full_buffer_spans_the_whole_width() {
        let h = filled_with(&vec![50.0; 120]);
        let out = h.stretched(40);
        assert_eq!(out.len(), 40);
        assert!(out.iter().all(|v| v.is_finite()), "{out:?}");
    }

    #[test]
    fn the_time_axis_does_not_change_with_the_width() {
        // The promise a wide graph makes: it shows the same two minutes as a
        // narrow one, drawn larger. Half a buffer is half the axis at every
        // width, so two graphs side by side stay comparable.
        let h = filled_with(&vec![50.0; 60]);
        for width in [20usize, 40, 100, 250] {
            let out = h.stretched(width);
            let drawn = out.iter().filter(|v| v.is_finite()).count();
            let ratio = drawn as f64 / width as f64;
            assert!(
                (ratio - 0.5).abs() < 0.05,
                "width {width}: {drawn}/{width} drawn, expected about half"
            );
        }
    }

    #[test]
    fn a_young_series_stays_at_the_right_hand_edge() {
        // Same promise window() makes, kept differently: gaps at the front, so
        // a handful of samples is never stretched into history that does not
        // exist.
        let h = filled_with(&[1.0, 2.0, 3.0]);
        let out = h.stretched(40);
        let first_drawn = out.iter().position(|v| v.is_finite()).unwrap();
        assert!(first_drawn > 30, "drawn from column {first_drawn} of 40");
        assert!(out.last().unwrap().is_finite(), "the newest sample is last");
    }

    #[test]
    fn widening_repeats_samples_rather_than_inventing_values() {
        // Two samples across eight columns must be those two values, not six
        // interpolated ones that were never measured.
        let h = filled_with(&vec![0.0; 118]);
        let mut h = h;
        h.push(Some(10.0));
        h.push(Some(20.0));
        let out = h.stretched(240);
        let seen: std::collections::BTreeSet<String> = out
            .iter()
            .filter(|v| v.is_finite())
            .map(|v| format!("{v}"))
            .collect();
        assert!(
            seen.iter().all(|v| v == "0" || v == "10" || v == "20"),
            "invented values: {seen:?}"
        );
    }

    #[test]
    fn narrowing_averages_rather_than_dropping_samples() {
        // A spike must not vanish just because the graph is narrow.
        let mut values = vec![0.0f32; 120];
        values[60] = 100.0;
        let out = filled_with(&values).stretched(12);
        assert!(
            out.iter().any(|v| v.is_finite() && *v > 0.0),
            "the spike was dropped: {out:?}"
        );
    }

    #[test]
    fn gaps_survive_being_resampled() {
        // The whole point of NaN: a bucket with no measurement stays a gap.
        let mut h = History::new(120);
        for _ in 0..120 {
            h.push(None);
        }
        let out = h.stretched(30);
        assert!(out.iter().all(|v| !v.is_finite()), "{out:?}");
    }

    #[test]
    fn an_empty_or_degenerate_request_cannot_panic() {
        let empty = History::new(120);
        assert!(empty.stretched(10).iter().all(|v| !v.is_finite()));
        assert!(empty.stretched(0).is_empty());
        assert!(filled_with(&[1.0]).stretched(0).is_empty());
        // One column is still one column, however much history there is.
        assert_eq!(filled_with(&vec![1.0; 120]).stretched(1).len(), 1);
    }

    fn filled(values: &[f32]) -> History {
        let mut h = History::new(8);
        for v in values {
            h.push(Some(*v));
        }
        h
    }

    #[test]
    fn keeps_only_the_most_recent_samples() {
        let h = filled(&[1., 2., 3., 4., 5., 6., 7., 8., 9., 10.]);
        assert_eq!(h.len(), 8);
        assert_eq!(h.last(), Some(10.0));
        assert_eq!(h.max(), Some(10.0));
    }

    #[test]
    fn a_young_series_draws_at_the_right_edge() {
        let h = filled(&[5., 6., 7.]);
        let w = h.window(6);
        assert_eq!(w.len(), 6);
        // Gaps at the front, data at the end: the graph grows leftwards.
        assert!(w[0].is_nan() && w[1].is_nan() && w[2].is_nan());
        assert_eq!(&w[3..], &[5., 6., 7.]);
    }

    #[test]
    fn an_empty_series_is_all_gaps() {
        let h = History::new(8);
        assert!(h.window(4).iter().all(|v| v.is_nan()));
        assert_eq!(h.last(), None);
        assert_eq!(h.max(), None);
        assert!(h.is_empty());
    }

    #[test]
    fn more_samples_than_pixels_are_averaged_not_dropped() {
        let h = filled(&[0., 0., 100., 100., 0., 0., 100., 100.]);
        let w = h.window(4);
        assert_eq!(w, vec![0., 100., 0., 100.]);
    }

    #[test]
    fn a_gap_is_not_a_zero() {
        let mut h = History::new(4);
        h.push(Some(50.0));
        h.push(None); // agent was down
        h.push(Some(60.0));
        let w = h.window(3);
        assert_eq!(w[0], 50.0);
        assert!(w[1].is_nan(), "a missing sample must not read as idle");
        assert_eq!(w[2], 60.0);
        // A gap at the end must not hide the last real value either.
        h.push(None);
        assert_eq!(h.last(), Some(60.0));
    }

    #[test]
    fn zero_width_windows_are_empty_rather_than_panicking() {
        assert!(filled(&[1., 2.]).window(0).is_empty());
    }

    #[test]
    fn trend_needs_enough_samples_before_it_will_commit() {
        assert_eq!(filled(&[1., 2., 3.]).trend(0.5), Trend::Steady);
    }

    #[test]
    fn detects_a_growing_and_a_draining_series() {
        assert_eq!(
            filled(&[1., 1., 2., 3., 6., 8., 9., 10.]).trend(0.5),
            Trend::Rising
        );
        assert_eq!(
            filled(&[10., 9., 8., 6., 3., 2., 1., 1.]).trend(0.5),
            Trend::Falling
        );
    }

    #[test]
    fn small_oscillation_is_steady_not_a_trend() {
        // A queue bouncing between 3 and 4 must not flip its label every tick.
        assert_eq!(
            filled(&[3., 4., 3., 4., 3., 4., 3., 4.]).trend(1.0),
            Trend::Steady
        );
    }

    #[test]
    fn trend_words_answer_the_question_being_asked() {
        assert_eq!(Trend::Rising.label(), "rising");
        assert_eq!(Trend::Falling.label(), "draining");
        assert_eq!(Trend::Steady.label(), "steady");
    }
}
