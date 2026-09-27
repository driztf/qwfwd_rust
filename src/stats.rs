//! Rolling per-connection timing statistics over the last few seconds.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const WINDOW: Duration = Duration::from_secs(5);

/// Mean, standard deviation and maximum of the samples in the window, in ms.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    pub count: usize,
    pub mean: f64,
    pub stddev: f64,
    pub max: f64,
}

/// Timestamped samples (in ms) that expire after [`WINDOW`].
#[derive(Default)]
pub struct Series {
    samples: VecDeque<(Instant, f64)>,
}

impl Series {
    pub fn record(&mut self, now: Instant, value: Duration) {
        self.prune(now);
        self.samples.push_back((now, value.as_secs_f64() * 1000.0));
    }

    fn prune(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// Number and mean of the samples in the window, without the cost of
    /// [`summary`](Self::summary).
    pub fn mean(&self, now: Instant) -> (usize, f64) {
        let (count, sum) = self
            .samples
            .iter()
            .filter(|(at, _)| now.saturating_duration_since(*at) <= WINDOW)
            .fold((0, 0.0), |(count, sum), (_, v)| (count + 1, sum + v));
        (count, if count == 0 { 0.0 } else { sum / count as f64 })
    }

    pub fn summary(&self, now: Instant) -> Summary {
        let values: Vec<f64> = self
            .samples
            .iter()
            .filter(|(at, _)| now.saturating_duration_since(*at) <= WINDOW)
            .map(|(_, v)| *v)
            .collect();
        if values.is_empty() {
            return Summary::default();
        }
        let count = values.len();
        let mean = values.iter().sum::<f64>() / count as f64;
        let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / count as f64;
        let max = values.iter().cloned().fold(0.0, f64::max);
        Summary {
            count,
            mean,
            stddev: variance.sqrt(),
            max,
        }
    }
}

/// Timing of one direction of a peer: gaps between arriving packets, gaps
/// between forwarded packets, time spent queued, and drops.
#[derive(Default)]
pub struct LinkStats {
    last_arrival: Option<Instant>,
    last_send: Option<Instant>,
    pub arrival_gap: Series,
    pub send_gap: Series,
    pub wait: Series,
    /// Only the counts of these matter.
    arrivals: Series,
    dupes: Series,
    drops: Series,
}

impl LinkStats {
    /// A packet arrived; returns the gap since the previous one, if any.
    pub fn arrived(&mut self, now: Instant) -> Option<Duration> {
        let gap = self
            .last_arrival
            .map(|prev| now.saturating_duration_since(prev));
        if let Some(gap) = gap {
            self.arrival_gap.record(now, gap);
        }
        self.last_arrival = Some(now);
        self.arrivals.record(now, Duration::ZERO);
        gap
    }

    /// Packets that arrived within the window.
    pub fn recent_arrivals(&self, now: Instant) -> usize {
        self.arrivals.mean(now).0
    }

    /// A copy of the previous packet arrived; it is not a packet in its own
    /// right and leaves the gap statistics alone.
    pub fn duplicate(&mut self, now: Instant) {
        self.dupes.record(now, Duration::ZERO);
    }

    /// Duplicates that arrived within the window.
    pub fn recent_dupes(&self, now: Instant) -> usize {
        self.dupes.mean(now).0
    }

    /// A packet that arrived at `arrived` was forwarded at `now`.
    pub fn sent(&mut self, now: Instant, arrived: Instant) {
        if let Some(prev) = self.last_send {
            self.send_gap
                .record(now, now.saturating_duration_since(prev));
        }
        self.last_send = Some(now);
        self.wait
            .record(now, now.saturating_duration_since(arrived));
    }

    pub fn dropped(&mut self, now: Instant) {
        self.drops.record(now, Duration::ZERO);
    }

    /// Drops within the window.
    pub fn recent_drops(&self, now: Instant) -> usize {
        self.drops.mean(now).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn summary_computes_mean_stddev_and_max() {
        let mut series = Series::default();
        let t0 = Instant::now();
        for (i, v) in [10u32, 12, 14, 16].into_iter().enumerate() {
            series.record(t0 + (i as u32) * MS, v * MS);
        }
        let s = series.summary(t0 + 10 * MS);
        assert_eq!(s.count, 4);
        let (count, mean) = series.mean(t0 + 10 * MS);
        assert_eq!(count, 4);
        assert!((mean - 13.0).abs() < 1e-9);
        assert_eq!(Series::default().mean(t0), (0, 0.0));
        assert!((s.mean - 13.0).abs() < 1e-9);
        assert!((s.stddev - 5f64.sqrt()).abs() < 1e-9);
        assert!((s.max - 16.0).abs() < 1e-9);
        assert_eq!(Series::default().summary(t0), Summary::default());
    }

    #[test]
    fn samples_expire_after_the_window() {
        let mut series = Series::default();
        let t0 = Instant::now();
        series.record(t0, 5 * MS);
        series.record(t0 + WINDOW / 2, 7 * MS);
        assert_eq!(series.summary(t0 + WINDOW).count, 2);
        assert_eq!(series.summary(t0 + WINDOW + MS).count, 1);
        // Recording prunes too, so memory stays bounded.
        series.record(t0 + 2 * WINDOW, 9 * MS);
        assert_eq!(series.samples.len(), 1);
    }

    #[test]
    fn link_stats_track_gaps_wait_and_drops() {
        let mut link = LinkStats::default();
        let t0 = Instant::now();
        link.arrived(t0);
        link.sent(t0, t0);
        link.arrived(t0 + 13 * MS);
        link.arrived(t0 + 14 * MS);
        link.sent(t0 + 13 * MS, t0 + 13 * MS);
        link.sent(t0 + 25 * MS, t0 + 14 * MS);
        link.dropped(t0 + 30 * MS);
        link.duplicate(t0 + 30 * MS);

        let now = t0 + 40 * MS;
        assert_eq!(link.recent_arrivals(now), 3);
        assert_eq!(link.recent_arrivals(now + WINDOW), 0);
        assert_eq!(link.recent_dupes(now), 1);
        assert_eq!(link.recent_dupes(now + WINDOW), 0);
        let arrival = link.arrival_gap.summary(now);
        assert_eq!(arrival.count, 2);
        assert!((arrival.mean - 7.0).abs() < 1e-9);
        let send = link.send_gap.summary(now);
        assert!((send.mean - 12.5).abs() < 1e-9);
        assert!((send.max - 13.0).abs() < 1e-9);
        let wait = link.wait.summary(now);
        assert!((wait.max - 11.0).abs() < 1e-9);
        assert_eq!(link.recent_drops(now), 1);
        assert_eq!(link.recent_drops(now + WINDOW), 0);
    }
}
