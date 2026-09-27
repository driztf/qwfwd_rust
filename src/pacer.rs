//! Connection smoothing: a per-peer jitter buffer that enforces a minimum interval
//! between forwarded game packets.
//!
//! QuakeWorld sends a fixed number of packets per second (77, one every 13 ms),
//! and links such as cellular uplinks deliver them in clumps. Each packet is
//! released no earlier than one interval after the previous one, so packets
//! that arrive on time pass straight through and only the ones arriving early
//! (right behind a late one) wait. The interval is the client's own send
//! interval, measured from its arrivals over the last few seconds (the
//! configured interval serves until enough have been seen), so the releases
//! run at the client's rate and the queue holds only what the clumping needs.
//! Whatever slack builds up beyond that, because the estimate is a touch long
//! or a stall let the queue grow, is found as the smallest wait over the last
//! second and shaved off the following slots a little at a time. Once a
//! backlog grows past a threshold the queue drains at double rate to recover
//! from a latency burst, and packets that have waited past a hard limit are
//! discarded, oldest first, so the remote end skips ahead rather than falling
//! ever further behind.
//!
//! Clients may send every packet twice for loss protection (`cl_c2sdupe`).
//! A packet that repeats the netchan sequence number of the one before it is
//! such a duplicate: it rides with its original, going straight through when
//! the original did and leaving in the same slot when the original was
//! queued, and it is counted separately from real packets.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::cvar::Cvars;
use crate::stats::{LinkStats, Series};
use crate::{info, parse};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PacerConfig {
    /// Spacing between releases until the client's own rate has been measured.
    pub interval: Duration,
    /// Backlog age above which the queue drains at double rate.
    pub catchup_delay: Duration,
    /// Packets that have waited longer than this are dropped.
    pub max_delay: Duration,
    /// The most a slot may be shortened to drain slack, as a fraction of the interval.
    pub drain: f64,
}

impl PacerConfig {
    pub fn new(interval_ms: f64, catchup_ms: i32, max_delay_ms: i32, drain_percent: f64) -> Self {
        let interval_ms = if interval_ms.is_finite() {
            interval_ms.clamp(0.1, 1000.0)
        } else {
            1.0
        };
        let drain = if drain_percent.is_finite() {
            (drain_percent / 100.0).clamp(0.0, 1.0)
        } else {
            0.0
        };
        PacerConfig {
            interval: Duration::from_secs_f64(interval_ms / 1000.0),
            catchup_delay: Duration::from_millis(catchup_ms.max(0) as u64),
            max_delay: Duration::from_millis(max_delay_ms.max(0) as u64),
            drain,
        }
    }
}

/// What to do with a packet offered to the pacer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Push {
    /// Send it now; the pacer took no copy.
    Forward,
    /// The pacer keeps a copy until its slot comes.
    Queued,
}

struct Queued {
    arrived: Instant,
    data: Vec<u8>,
    /// Repeats the packet before it; it is never at the front of the queue
    /// on its own, its original precedes it.
    dupe: bool,
}

/// The netchan sequence number a game packet starts with, without the
/// reliable flag.
fn sequence(data: &[u8]) -> Option<u32> {
    let head: [u8; 4] = data.get(..4)?.try_into().ok()?;
    Some(u32::from_le_bytes(head) & 0x7fff_ffff)
}

/// Arrival gaps needed before the measured rate replaces the configured
/// interval, and the time they must span: a burst of packets arriving at
/// once says nothing about the rate.
const MIN_RATE_SAMPLES: usize = 32;
const MIN_RATE_SPAN_MS: f64 = 1000.0;
/// The measured interval is stretched by this much: a slightly long interval
/// only builds slack, which is drained, while a short one runs the queue dry
/// and re-syncs the schedule to a clumped arrival.
const RATE_MARGIN: f64 = 1.002;
/// How often the slack in the queue is measured and scheduled for draining.
const SLACK_PERIOD: Duration = Duration::from_millis(500);
/// Slack is shaved off each slot by this share of what is left, so a lot of
/// slack goes quickly and the last of it gently, between a floor (as a
/// fraction of the interval) and the configured cap.
const DRAIN_SHARE: f64 = 0.25;
const DRAIN_FLOOR: f64 = 0.02;

#[derive(Default)]
pub struct Pacer {
    queue: VecDeque<Queued>,
    next_release: Option<Instant>,
    last_sequence: Option<u32>,
    /// Arrival gaps the rate is measured from; stalls are left out.
    rate: Series,
    /// The smallest wait of a release since `period_start`; `None` before the first.
    slack: Option<Duration>,
    period_start: Option<Instant>,
    /// Slack still to be shaved off coming slots.
    drain: Duration,
    pub dropped: u64,
    pub stats: LinkStats,
}

impl Pacer {
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Whether `data` repeats the sequence number of the previous packet.
    fn is_duplicate(&mut self, data: &[u8]) -> bool {
        let sequence = sequence(data);
        let dupe = sequence.is_some() && sequence == self.last_sequence;
        self.last_sequence = sequence;
        dupe
    }

    /// The spacing between releases: the client's measured send interval once
    /// enough arrivals have been seen, the configured one before that.
    pub fn interval(&self, now: Instant, config: &PacerConfig) -> Duration {
        let (count, mean_ms) = self.rate.mean(now);
        if count < MIN_RATE_SAMPLES || mean_ms * (count as f64) < MIN_RATE_SPAN_MS {
            return config.interval;
        }
        Duration::from_secs_f64((mean_ms * RATE_MARGIN / 1000.0).clamp(0.001, 1.0))
    }

    /// A packet was released having waited `wait`; keeps track of the slack
    /// and, once a period is over, schedules it for draining.
    fn released(&mut self, now: Instant, wait: Duration, config: &PacerConfig) {
        let start = *self.period_start.get_or_insert(now);
        self.slack = Some(self.slack.map_or(wait, |slack| slack.min(wait)));
        if now.saturating_duration_since(start) >= SLACK_PERIOD {
            // The slack still present is what remains to drain, whatever was
            // scheduled before; adding would count the same slack twice.
            self.drain = self.slack.unwrap_or_default().min(config.catchup_delay);
            self.slack = None;
            self.period_start = Some(now);
        }
    }

    /// Offers a packet. It comes straight back when the line is idle and the
    /// current slot is free; otherwise it is queued for [`release`](Self::release).
    /// Without a config the packet always comes straight back, so timing is
    /// still recorded for connections that are not being smoothed.
    pub fn push(&mut self, now: Instant, data: &[u8], config: Option<&PacerConfig>) -> Push {
        if self.is_duplicate(data) {
            self.stats.duplicate(now);
            if self.queue.is_empty() {
                return Push::Forward;
            }
            self.queue.push_back(Queued {
                arrived: now,
                data: data.to_vec(),
                dupe: true,
            });
            return Push::Queued;
        }
        let gap = self.stats.arrived(now);
        let Some(config) = config else {
            self.stats.sent(now, now);
            return Push::Forward;
        };
        if let Some(gap) = gap.filter(|gap| *gap <= config.max_delay) {
            self.rate.record(now, gap);
        }
        if self.queue.is_empty() && self.next_release.is_none_or(|due| now >= due) {
            // The queue ran dry, so there is no slack left to drain.
            self.drain = Duration::ZERO;
            self.next_release = Some(now + self.interval(now, config));
            self.released(now, Duration::ZERO, config);
            self.stats.sent(now, now);
            return Push::Forward;
        }
        self.queue.push_back(Queued {
            arrived: now,
            data: data.to_vec(),
            dupe: false,
        });
        Push::Queued
    }

    /// Packets whose slot has come, oldest first. Stale packets are dropped first.
    pub fn release(&mut self, now: Instant, config: &PacerConfig) -> Vec<Vec<u8>> {
        while self
            .queue
            .front()
            .is_some_and(|queued| now.saturating_duration_since(queued.arrived) > config.max_delay)
        {
            self.queue.pop_front();
            self.dropped += 1;
            self.stats.dropped(now);
            // Its copies go with it: they arrived a moment later, so they may
            // not be stale by age, but a dropped packet is not to be sent.
            while self.queue.front().is_some_and(|queued| queued.dupe) {
                self.queue.pop_front();
            }
        }

        let mut out = Vec::new();
        while let Some(front) = self.queue.front() {
            if front.dupe {
                let dupe = self.queue.pop_front().expect("queue is not empty");
                out.push(dupe.data);
                continue;
            }
            let due = self.next_release.unwrap_or(now);
            if now < due {
                break;
            }
            let queued = self.queue.pop_front().expect("queue is not empty");
            let wait = now.saturating_duration_since(queued.arrived);
            self.stats.sent(now, queued.arrived);
            self.released(now, wait, config);
            out.push(queued.data);

            let backlog = self
                .queue
                .iter()
                .find(|queued| !queued.dupe)
                .map_or(Duration::ZERO, |next| {
                    now.saturating_duration_since(next.arrived)
                });
            let interval = self.interval(now, config);
            let slot = if backlog > config.catchup_delay {
                interval / 2
            } else {
                interval
            };
            let shave = self
                .drain
                .mul_f64(DRAIN_SHARE)
                .max(interval.mul_f64(DRAIN_FLOOR))
                .min(interval.mul_f64(config.drain))
                .min(slot / 2)
                .min(self.drain);
            self.drain -= shave;
            // Slots are counted from when the last one was due, not from now,
            // so a late wakeup catches up on the slots it missed.
            self.next_release = Some(due + slot - shave);
        }
        out
    }

    /// Everything still queued, in order, with no further pacing.
    pub fn drain(&mut self, now: Instant) -> Vec<Vec<u8>> {
        self.queue
            .drain(..)
            .map(|queued| {
                if !queued.dupe {
                    self.stats.sent(now, queued.arrived);
                }
                queued.data
            })
            .collect()
    }

    /// When the next queued packet is due, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.queue.front().and(self.next_release)
    }

    /// Discards whatever is queued and forgets the current slot; the
    /// statistics are kept.
    pub fn clear(&mut self) {
        self.queue.clear();
        self.next_release = None;
        self.slack = None;
        self.period_start = None;
        self.drain = Duration::ZERO;
        self.drain = Duration::ZERO;
    }
}

/// Who gets smoothed, from the `smooth` cvar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmoothPolicy {
    Off,
    /// Clients that connect with `setinfo smooth 1`.
    OptIn,
    /// Everyone except clients that connect with `setinfo smooth 0`.
    All,
}

impl SmoothPolicy {
    fn from_cvar(value: i32) -> Self {
        match value {
            0 => SmoothPolicy::Off,
            1 => SmoothPolicy::OptIn,
            _ => SmoothPolicy::All,
        }
    }

    /// Whether a client with this userinfo should be smoothed.
    pub fn applies_to(self, userinfo: &[u8]) -> bool {
        let key = info::value_for_key(userinfo, SMOOTH_KEY);
        match self {
            SmoothPolicy::Off => false,
            SmoothPolicy::OptIn => parse::atoi(key) != 0,
            SmoothPolicy::All => key.is_empty() || parse::atoi(key) != 0,
        }
    }
}

/// Userinfo key clients set with `setinfo smooth 1`.
pub const SMOOTH_KEY: &[u8] = b"smooth";

/// The smoothing settings, read from the cvars.
pub struct Smoothing {
    pub policy: SmoothPolicy,
    pub config: PacerConfig,
}

/// Packets per second a QuakeWorld client sends.
pub const CLIENT_RATE: f64 = 77.0;

impl Smoothing {
    pub fn register_cvars(cvars: &mut Cvars) {
        cvars.get("smooth", "1", 0);
        cvars.get("smooth_interval", &format!("{}", 1000.0 / CLIENT_RATE), 0);
        cvars.get("smooth_catchup", "50", 0);
        cvars.get("smooth_maxdelay", "200", 0);
        cvars.get("smooth_drain", "10", 0);
    }

    /// Whether any smoothing cvar changed since the last call; clears the flags.
    pub fn cvars_modified(cvars: &mut Cvars) -> bool {
        let mut modified = false;
        for name in [
            "smooth",
            "smooth_interval",
            "smooth_catchup",
            "smooth_maxdelay",
            "smooth_drain",
        ] {
            modified |= cvars.take_modified(name);
        }
        modified
    }

    pub fn from_cvars(cvars: &Cvars) -> Self {
        Smoothing {
            policy: SmoothPolicy::from_cvar(cvars.int("smooth")),
            config: PacerConfig::new(
                cvars.float("smooth_interval"),
                cvars.int("smooth_catchup"),
                cvars.int("smooth_maxdelay"),
                cvars.float("smooth_drain"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn config() -> PacerConfig {
        PacerConfig::new(10.0, 50, 200, 10.0)
    }

    fn packet(id: u8) -> Vec<u8> {
        vec![id]
    }

    /// A game packet with a netchan sequence number; `tag` tells copies apart.
    fn numbered(sequence: u32, tag: u8) -> Vec<u8> {
        let mut data = sequence.to_le_bytes().to_vec();
        data.push(tag);
        data
    }

    #[test]
    fn duplicates_ride_with_their_original() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        let cfg = config();
        // Straight through: the duplicate follows without claiming a slot.
        assert!(pacer.push(t0, &numbered(1, 0), Some(&cfg)) == Push::Forward);
        assert_eq!(pacer.push(t0, &numbered(1, 1), Some(&cfg)), Push::Forward);
        assert_eq!(pacer.next_deadline(), None);
        // Queued: the duplicate leaves in the same slot as the original.
        assert_eq!(
            pacer.push(t0 + 6 * MS, &numbered(2, 0), Some(&cfg)),
            Push::Queued
        );
        assert_eq!(
            pacer.push(t0 + 6 * MS, &numbered(2, 1), Some(&cfg)),
            Push::Queued
        );
        assert_eq!(pacer.queued(), 2);
        assert!(pacer.release(t0 + 9 * MS, &cfg).is_empty());
        assert_eq!(
            pacer.release(t0 + 10 * MS, &cfg),
            vec![numbered(2, 0), numbered(2, 1)]
        );
        assert_eq!(pacer.next_deadline(), None);
        assert_eq!(
            pacer.push(t0 + 12 * MS, &numbered(3, 0), Some(&cfg)),
            Push::Queued
        );
        assert_eq!(pacer.release(t0 + 20 * MS, &cfg), vec![numbered(3, 0)]);
        // The reliable flag does not hide a duplicate.
        assert!(
            pacer.push(t0 + 30 * MS, &numbered(4 | 0x8000_0000, 0), Some(&cfg)) == Push::Forward
        );
        assert_eq!(
            pacer.push(t0 + 30 * MS, &numbered(4, 1), Some(&cfg)),
            Push::Forward
        );
        // Only originals count as packets, in the gap statistics and as drops.
        let now = t0 + 30 * MS;
        assert_eq!(pacer.stats.recent_arrivals(now), 4);
        assert_eq!(pacer.stats.recent_dupes(now), 3);
        assert_eq!(pacer.stats.arrival_gap.summary(now).count, 3);
        assert_eq!(pacer.stats.send_gap.summary(now).count, 3);
        assert_eq!(
            pacer.push(t0 + 35 * MS, &numbered(5, 0), Some(&cfg)),
            Push::Queued
        );
        assert_eq!(
            pacer.push(t0 + 35 * MS, &numbered(5, 1), Some(&cfg)),
            Push::Queued
        );
        assert!(pacer.release(t0 + 300 * MS, &cfg).is_empty());
        assert_eq!(pacer.dropped, 1);
        assert_eq!(pacer.stats.recent_drops(t0 + 300 * MS), 1);
    }

    #[test]
    fn a_dropped_packet_takes_its_copies_with_it() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        let cfg = config();
        assert_eq!(pacer.push(t0, &numbered(1, 0), Some(&cfg)), Push::Forward);
        // Original and copy a moment apart, both queued behind the slot.
        assert_eq!(
            pacer.push(t0 + 5 * MS, &numbered(2, 0), Some(&cfg)),
            Push::Queued
        );
        let copy_at = t0 + 5 * MS + Duration::from_micros(50);
        assert_eq!(
            pacer.push(copy_at, &numbered(2, 1), Some(&cfg)),
            Push::Queued
        );
        // Nothing runs until the original is just past the age limit; the
        // copy is not, but it must not be sent on its own.
        let late = t0 + 5 * MS + cfg.max_delay + Duration::from_micros(20);
        assert!(pacer.release(late, &cfg).is_empty());
        assert_eq!(pacer.queued(), 0);
        assert_eq!(pacer.dropped, 1);
        assert_eq!(pacer.next_deadline(), None);
    }

    #[test]
    fn draining_never_moves_a_release_earlier_than_the_last() {
        // A large pending drain and a drain cap of a whole slot: in catch-up
        // mode the shave is still held under half a slot.
        let cfg = PacerConfig::new(10.0, 20, 1000, 100.0);
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        assert_eq!(pacer.push(t0, &packet(0), Some(&cfg)), Push::Forward);
        for id in 1..=20 {
            assert_eq!(pacer.push(t0, &packet(id), Some(&cfg)), Push::Queued);
        }
        pacer.drain = 100 * MS;

        let mut previous = t0;
        let mut t = t0;
        while pacer.queued() > 0 {
            t += Duration::from_micros(500);
            let released = pacer.release(t, &cfg);
            if released.len() > 1 {
                panic!(
                    "{} packets released in one go at {:?}",
                    released.len(),
                    t - t0
                );
            }
            if !released.is_empty() {
                assert!(
                    t - previous >= Duration::from_micros(2500),
                    "gap {:?}",
                    t - previous
                );
                previous = t;
            }
        }
    }

    #[test]
    fn slack_is_replaced_each_period_and_forgotten_when_the_queue_runs_dry() {
        let cfg = config();
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        // Two periods each seeing 5 ms of slack schedule 5 ms, not 10.
        pacer.released(t0, 5 * MS, &cfg);
        pacer.released(t0 + SLACK_PERIOD, 5 * MS, &cfg);
        assert_eq!(pacer.drain, 5 * MS);
        pacer.released(t0 + SLACK_PERIOD + MS, 5 * MS, &cfg);
        pacer.released(t0 + 2 * SLACK_PERIOD + MS, 5 * MS, &cfg);
        assert_eq!(pacer.drain, 5 * MS);
        // A packet passing straight through means there is no slack left.
        let t = t0 + 3 * SLACK_PERIOD;
        assert_eq!(pacer.push(t, &packet(1), Some(&cfg)), Push::Forward);
        assert_eq!(pacer.drain, Duration::ZERO);
    }

    #[test]
    fn duplicates_pass_unsmoothed_and_are_counted() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        assert_eq!(pacer.push(t0, &numbered(7, 0), None), Push::Forward);
        assert_eq!(pacer.push(t0, &numbered(7, 1), None), Push::Forward);
        assert_eq!(
            pacer.push(t0 + 13 * MS, &numbered(8, 0), None),
            Push::Forward
        );
        let now = t0 + 13 * MS;
        assert_eq!(pacer.stats.recent_arrivals(now), 2);
        assert_eq!(pacer.stats.recent_dupes(now), 1);
        assert!((pacer.stats.arrival_gap.summary(now).mean - 13.0).abs() < 1e-9);
        assert!((pacer.stats.send_gap.summary(now).mean - 13.0).abs() < 1e-9);
    }

    #[test]
    fn packets_on_schedule_pass_straight_through() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        assert_eq!(pacer.push(t0, &packet(1), Some(&config())), Push::Forward);
        assert_eq!(
            pacer.push(t0 + 10 * MS, &packet(2), Some(&config())),
            Push::Forward
        );
        assert_eq!(
            pacer.push(t0 + 25 * MS, &packet(3), Some(&config())),
            Push::Forward
        );
        assert_eq!(pacer.queued(), 0);
        assert_eq!(pacer.next_deadline(), None);
    }

    #[test]
    fn early_packet_waits_for_its_slot() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        assert!(pacer.push(t0, &packet(1), Some(&config())) == Push::Forward);
        // Arrives 4 ms early: held until the slot at t0 + 10 ms.
        assert_eq!(
            pacer.push(t0 + 6 * MS, &packet(2), Some(&config())),
            Push::Queued
        );
        assert_eq!(pacer.next_deadline(), Some(t0 + 10 * MS));
        assert!(pacer.release(t0 + 9 * MS, &config()).is_empty());
        assert_eq!(pacer.release(t0 + 10 * MS, &config()), vec![packet(2)]);
        // The line stays paced relative to the previous release.
        assert_eq!(
            pacer.push(t0 + 12 * MS, &packet(3), Some(&config())),
            Push::Queued
        );
        assert_eq!(pacer.release(t0 + 20 * MS, &config()), vec![packet(3)]);
    }

    #[test]
    fn burst_is_spread_and_catch_up_doubles_the_rate() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        assert!(pacer.push(t0, &packet(0), Some(&config())) == Push::Forward);
        for id in 1..=12 {
            assert_eq!(pacer.push(t0, &packet(id), Some(&config())), Push::Queued);
        }

        let mut releases = Vec::new();
        let mut t = t0;
        while pacer.queued() > 0 {
            t += MS;
            for data in pacer.release(t, &config()) {
                releases.push((t - t0, data[0]));
            }
        }
        // 10 ms spacing until the backlog is over 50 ms old, then 5 ms.
        let expected: Vec<(Duration, u8)> = [
            (10, 1),
            (20, 2),
            (30, 3),
            (40, 4),
            (50, 5),
            (60, 6),
            (65, 7),
            (70, 8),
            (75, 9),
            (80, 10),
            (85, 11),
            (90, 12),
        ]
        .into_iter()
        .map(|(ms, id)| (ms * MS, id))
        .collect();
        assert_eq!(releases, expected);
        assert_eq!(pacer.dropped, 0);
    }

    #[test]
    fn stale_packets_are_dropped_oldest_first() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        assert!(pacer.push(t0, &packet(0), Some(&config())) == Push::Forward);
        for id in 1..=3 {
            assert_eq!(pacer.push(t0, &packet(id), Some(&config())), Push::Queued);
        }
        assert_eq!(
            pacer.push(t0 + 100 * MS, &packet(4), Some(&config())),
            Push::Queued
        );

        // Nothing was released for 250 ms: the t0 packets are stale, the later one is not.
        assert_eq!(pacer.release(t0 + 250 * MS, &config()), vec![packet(4)]);
        assert_eq!(pacer.dropped, 3);
        assert_eq!(pacer.queued(), 0);
    }

    #[test]
    fn drain_returns_everything_in_order() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        assert!(pacer.push(t0, &packet(0), Some(&config())) == Push::Forward);
        pacer.push(t0, &packet(1), Some(&config()));
        pacer.push(t0, &packet(2), Some(&config()));
        assert_eq!(pacer.drain(t0), vec![packet(1), packet(2)]);
        assert_eq!(pacer.next_deadline(), None);
    }

    #[test]
    fn clear_discards_queue_and_slot_but_keeps_stats() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        assert!(pacer.push(t0, &packet(0), Some(&config())) == Push::Forward);
        assert_eq!(pacer.push(t0, &packet(1), Some(&config())), Push::Queued);
        pacer.clear();
        assert_eq!(pacer.queued(), 0);
        assert_eq!(pacer.next_deadline(), None);
        assert_eq!(
            pacer.push(t0 + MS, &packet(2), Some(&config())),
            Push::Forward
        );
        assert_eq!(pacer.stats.arrival_gap.summary(t0 + MS).count, 2);
    }

    #[test]
    fn config_clamps_nonsense_values() {
        let config = PacerConfig::new(0.0, -5, -1, -1.0);
        assert_eq!(config.interval, Duration::from_micros(100));
        assert_eq!(config.catchup_delay, Duration::ZERO);
        assert_eq!(config.max_delay, Duration::ZERO);
        assert_eq!(config.drain, 0.0);
        assert_eq!(
            PacerConfig::new(f64::NAN, 0, 0, f64::NAN).interval,
            Duration::from_millis(1)
        );
        let config = PacerConfig::new(12.5, 50, 200, 10.0);
        assert_eq!(config.interval, Duration::from_micros(12_500));
        assert!((config.drain - 0.1).abs() < 1e-12);
        assert_eq!(PacerConfig::new(13.0, 0, 0, 500.0).drain, 1.0);
    }

    #[test]
    fn policy_honours_client_opt_in_and_opt_out() {
        assert!(!SmoothPolicy::Off.applies_to(b"\\smooth\\1"));
        assert!(!SmoothPolicy::OptIn.applies_to(b"\\name\\x"));
        assert!(!SmoothPolicy::OptIn.applies_to(b"\\smooth\\0"));
        assert!(SmoothPolicy::OptIn.applies_to(b"\\name\\x\\smooth\\1"));
        assert!(SmoothPolicy::All.applies_to(b"\\name\\x"));
        assert!(SmoothPolicy::All.applies_to(b"\\smooth\\1"));
        assert!(!SmoothPolicy::All.applies_to(b"\\smooth\\0"));
        assert_eq!(SmoothPolicy::from_cvar(0), SmoothPolicy::Off);
        assert_eq!(SmoothPolicy::from_cvar(1), SmoothPolicy::OptIn);
        assert_eq!(SmoothPolicy::from_cvar(2), SmoothPolicy::All);
    }

    #[test]
    fn unsmoothed_packets_pass_through_but_are_measured() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        assert_eq!(pacer.push(t0, &packet(1), None), Push::Forward);
        assert_eq!(pacer.push(t0, &packet(2), None), Push::Forward);
        assert_eq!(pacer.push(t0 + 20 * MS, &packet(3), None), Push::Forward);
        let gaps = pacer.stats.arrival_gap.summary(t0 + 20 * MS);
        assert_eq!(gaps.count, 2);
        assert!((gaps.max - 20.0).abs() < 1e-9);
        assert!((pacer.stats.wait.summary(t0 + 20 * MS).max).abs() < 1e-9);
    }

    /// Runs `seconds` of a 77 packets/s client whose uplink only transmits
    /// every `slot` (zero: straight away), releasing exactly when due.
    /// Returns the send times.
    fn simulate(
        pacer: &mut Pacer,
        cfg: &PacerConfig,
        t0: Instant,
        seconds: u64,
        slot: Duration,
    ) -> Vec<Instant> {
        let client_interval = Duration::from_secs_f64(1.0 / 77.0);
        let end = t0 + Duration::from_secs(seconds);
        let mut sends = Vec::new();
        let mut next_send = t0;
        let mut id = 0u8;
        loop {
            let arrival = if slot.is_zero() {
                next_send
            } else {
                t0 + slot.mul_f64((next_send - t0).div_duration_f64(slot).ceil())
            };
            if let Some(due) = pacer.next_deadline().filter(|due| *due <= arrival) {
                for _ in pacer.release(due, cfg) {
                    sends.push(due);
                }
                continue;
            }
            if arrival >= end {
                break;
            }
            if pacer.push(arrival, &[id], Some(cfg)) == Push::Forward {
                sends.push(arrival);
            }
            id = id.wrapping_add(1);
            next_send += client_interval;
        }
        sends
    }

    fn gaps_ms(sends: &[Instant]) -> Vec<f64> {
        sends
            .windows(2)
            .map(|w| (w[1] - w[0]).as_secs_f64() * 1000.0)
            .collect()
    }

    #[test]
    fn interval_is_the_measured_rate_once_known() {
        let cfg = PacerConfig::new(13.0, 50, 200, 10.0);
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        let mut t = t0;
        for id in 0..MIN_RATE_SAMPLES as u8 {
            pacer.push(t, &packet(id), Some(&cfg));
            t += 40 * MS;
        }
        // 31 gaps so far: still the configured interval.
        assert_eq!(pacer.interval(t, &cfg), 13 * MS);
        pacer.push(t, &packet(200), Some(&cfg));
        let measured = pacer.interval(t, &cfg).as_secs_f64() * 1000.0;
        assert!((measured - 40.0 * RATE_MARGIN).abs() < 1e-6, "{measured}");
        // A stall is not a rate signal.
        t += 500 * MS;
        pacer.push(t, &packet(201), Some(&cfg));
        let measured = pacer.interval(t, &cfg).as_secs_f64() * 1000.0;
        assert!((measured - 40.0 * RATE_MARGIN).abs() < 1e-6, "{measured}");

        // Neither is a burst: 32 gaps of nothing leave the configured interval in place.
        let mut pacer = Pacer::default();
        for id in 0..=MIN_RATE_SAMPLES as u8 {
            pacer.push(t0, &packet(id), Some(&cfg));
        }
        assert_eq!(pacer.interval(t0, &cfg), 13 * MS);
    }

    #[test]
    fn clumped_arrivals_leave_at_the_client_rate() {
        let cfg = PacerConfig::new(13.0, 50, 200, 10.0);
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        let sends = simulate(&mut pacer, &cfg, t0, 8, 20 * MS);

        // After the rate is measured and the schedule has locked on, every
        // packet leaves on a slot: the clumping is gone and the queue never runs dry.
        let settled: Vec<Instant> = sends
            .into_iter()
            .filter(|t| *t >= t0 + Duration::from_secs(3))
            .collect();
        let gaps = gaps_ms(&settled);
        let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
        let sd = (gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / gaps.len() as f64).sqrt();
        let max = gaps.iter().cloned().fold(0.0, f64::max);
        assert!((mean - 1000.0 / 77.0).abs() < 0.1, "mean gap {mean} ms");
        assert!(sd < 0.2, "gap sd {sd} ms");
        assert!(max < 13.5, "queue ran dry: max gap {max} ms");
        assert_eq!(pacer.dropped, 0);
        // The queue holds about one uplink slot, not more.
        let end = t0 + Duration::from_secs(8);
        let wait = pacer.stats.wait.summary(end);
        assert!(
            wait.mean < 20.0,
            "wait avg {:.1} max {:.1} ms, {} queued",
            wait.mean,
            wait.max,
            pacer.queued()
        );
    }

    #[test]
    fn backlog_drains_at_the_measured_rate() {
        let cfg = PacerConfig::new(13.0, 500, 1000, 10.0);
        let mut pacer = Pacer::default();
        let t0 = Instant::now();

        // A stall delivers four packets at once, then the client is steady.
        assert!(pacer.push(t0, &packet(0), Some(&cfg)) == Push::Forward);
        for id in 1..4 {
            assert_eq!(pacer.push(t0, &packet(id), Some(&cfg)), Push::Queued);
        }
        let client_interval = Duration::from_secs_f64(1.0 / 77.0);
        let mut t = t0;
        let mut empty_at = None;
        for id in 4u8..=255 {
            t += client_interval;
            while let Some(due) = pacer.next_deadline().filter(|due| *due <= t) {
                pacer.release(due, &cfg);
            }
            pacer.push(t, &[id], Some(&cfg));
            if pacer.queued() == 0 && empty_at.is_none() {
                empty_at = Some(t - t0);
            }
        }
        // 39 ms of slack goes at up to 10% of a slot per packet once the first
        // half-second period is over.
        let empty_at = empty_at.expect("queue never drained");
        assert!(
            empty_at <= Duration::from_millis(1500),
            "took {empty_at:?} to drain a 39 ms backlog"
        );
        assert_eq!(pacer.dropped, 0);
    }
}
