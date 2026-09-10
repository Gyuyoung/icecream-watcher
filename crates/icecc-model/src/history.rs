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
    pub fn max(&self) -> Option<f32> {
        self.samples
            .iter()
            .filter(|v| v.is_finite())
            .copied()
            .fold(None, |acc: Option<f32>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
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
