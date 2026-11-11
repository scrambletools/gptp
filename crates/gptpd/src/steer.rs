//! Keeps an end station's hardware clock near CLOCK_TAI, for cards that
//! send a frame when their clock reaches its launch time (the ETF queue
//! discipline with offload), which takes its times on CLOCK_TAI.
//!
//! The rest of gptpd goes on as if nothing moved the clock: it keeps its
//! model, its Pdelay exchanges, its lines and its jump checks, on a
//! virtual clock that runs free, the hardware clock less what the
//! steering has added. gptpd knows each change of frequency it made and
//! when, so it converts each of the hardware clock's times to the free
//! clock's, and the record's line back to the hardware clock's, which is
//! the one the card launches by.
//!
//! Each change is small, and comes right after a comparison, so a reader
//! carrying a record a comparison old strays by a few ns at most. The
//! clock closes on CLOCK_TAI slowly, by its rate, and is set only when it
//! stands too far off to slew, as at a start, after its driver reset it,
//! or when CLOCK_TAI itself stepped. Never while this system is
//! grandmaster: then its clock is the network's time.

use std::collections::VecDeque;

/// How long, in seconds, an offset from CLOCK_TAI takes to close once the
/// rate matches: an offset of `e` ns adds `e / TAU` ppb.
const TAU: f64 = 20.0;
/// The most the offset adds to the rate, in parts per billion.
const MOST_PHASE: f64 = 1_000.0;
/// The most the rate changes at a comparison, in ppb: little while the
/// clock follows CLOCK_TAI, so a reader carrying a record a comparison old
/// (100 ms) strays under 10 ns, and more while it closes on it, when no
/// reader takes the clock for CLOCK_TAI.
const CHANGE_FOLLOWING: f64 = 100.0;
const CHANGE_CLOSING: f64 = 1_000.0;
/// The most the oscillator's rate is adjusted, in ppb.
const MOST_ADJUST: f64 = 500_000.0;
/// Within how far of CLOCK_TAI, in ns, the clock counts as following it,
/// and beyond how far it is set rather than slewed.
pub const FOLLOWING: i128 = 10_000;
pub const SET_BEYOND: i128 = 1_000_000;
/// Comparisons within [`FOLLOWING`] in a row before the clock counts as
/// following CLOCK_TAI.
const STEADY: u32 = 10;
/// Changes remembered, one a comparison at most: time stamps some seconds
/// old still convert.
const KEPT: usize = 64;

/// From hardware time `hardware`, free time `free`, the hardware clock
/// runs `1 + adjust` ppb times as fast as the free one.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Segment {
    hardware: i128,
    free: i128,
    adjust: f64,
}

/// What following CLOCK_TAI asks of the clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Change {
    None,
    /// Its frequency adjustment, in ppb from its oscillator's.
    Adjust(f64),
    /// Set it this many ns on.
    Set(i128),
}

#[derive(Debug)]
pub struct Steering {
    segments: VecDeque<Segment>,
    /// The hardware clock less CLOCK_TAI at the last comparison, in ns.
    offset: Option<i128>,
    /// Comparisons within [`FOLLOWING`] in a row.
    steady: u32,
}

impl Steering {
    /// A clock adjusted `adjust` ppb from its oscillator's, its free time
    /// the same as its own at hardware time `hardware`.
    pub fn new(hardware: i128, adjust: f64) -> Self {
        let mut steering = Self {
            segments: VecDeque::new(),
            offset: None,
            steady: 0,
        };
        steering.restart(hardware, adjust);
        steering
    }

    /// Starts afresh, as after the clock was set.
    pub fn restart(&mut self, hardware: i128, adjust: f64) {
        self.segments.clear();
        self.segments.push_back(Segment {
            hardware,
            free: hardware,
            adjust,
        });
        self.offset = None;
        self.steady = 0;
    }

    /// The frequency adjustment now, in ppb.
    pub fn adjust(&self) -> f64 {
        self.segments.back().map_or(0.0, |segment| segment.adjust)
    }

    /// The free clock's time at hardware time `hardware`.
    pub fn free(&self, hardware: i128) -> i128 {
        let segment = self.at(|segment| segment.hardware <= hardware);
        let span = hardware - segment.hardware;
        // `span` hardware ns at `1 + adjust` ppb are `span / (1 + adjust)`
        // free ns: `span` less a small correction.
        let fraction = segment.adjust * 1e-9;
        segment.free + span - (span as f64 * fraction / (1.0 + fraction)).round() as i128
    }

    /// The hardware clock's time at free time `free`.
    pub fn hardware(&self, free: i128) -> i128 {
        let segment = self.at(|segment| segment.free <= free);
        let span = free - segment.free;
        segment.hardware + span + (span as f64 * segment.adjust * 1e-9).round() as i128
    }

    /// A line of the grandmaster's time on the free clock, its time
    /// `time` at free time `free` and `rate` of its ns in one free ns, as
    /// a line on the hardware clock for readers to carry forward: drawn
    /// from the latest change of rate where that came after `free`, at the
    /// rate the clock runs at now, so a reader extrapolating it does not
    /// carry an older rate past the change.
    pub fn hardware_line(&self, free: i128, time: i128, rate: f64) -> (i128, i128, f64) {
        let latest = self.segments.back().map_or(free, |segment| segment.free);
        let anchor = free.max(latest);
        let time = time + ((anchor - free) as f64 * rate).round() as i128;
        (self.hardware(anchor), time, rate / self.rate_at(anchor))
    }

    /// Hardware ns in one free ns, at free time `free`.
    pub fn rate_at(&self, free: i128) -> f64 {
        1.0 + self.at(|segment| segment.free <= free).adjust * 1e-9
    }

    /// The last segment that `starts`, or the first.
    fn at(&self, starts: impl Fn(&Segment) -> bool) -> &Segment {
        self.segments
            .iter()
            .rev()
            .find(|segment| starts(segment))
            .or_else(|| self.segments.front())
            .expect("a segment, from new")
    }

    /// Notes that the clock's adjustment became `adjust` at hardware time
    /// `hardware`.
    pub fn changed(&mut self, hardware: i128, adjust: f64) {
        let free = self.free(hardware);
        self.segments.push_back(Segment {
            hardware,
            free,
            adjust,
        });
        while self.segments.len() > KEPT {
            self.segments.pop_front();
        }
    }

    /// What to do after a comparison found the hardware clock `offset` ns
    /// ahead of CLOCK_TAI, with the free clock running `drift` faster
    /// than CLOCK_MONOTONIC (whose rate CLOCK_TAI shares), as a fraction,
    /// where known.
    pub fn follow(&mut self, offset: i128, drift: Option<f64>, grandmaster: bool) -> Change {
        self.offset = Some(offset);
        if grandmaster {
            self.steady = 0;
            return Change::None;
        }
        if offset.abs() > SET_BEYOND {
            self.steady = 0;
            return Change::Set(-offset);
        }
        self.steady = if offset.abs() <= FOLLOWING {
            self.steady.saturating_add(1)
        } else {
            0
        };
        let Some(drift) = drift else {
            return Change::None;
        };
        // The offset changes at the free clock's drift plus the
        // adjustment: asked to close at 1 / TAU of it a second.
        let phase = (offset as f64 / TAU).clamp(-MOST_PHASE, MOST_PHASE);
        let target = (-drift * 1e9 - phase).clamp(-MOST_ADJUST, MOST_ADJUST);
        let now = self.adjust();
        let most = if self.following().is_some() {
            CHANGE_FOLLOWING
        } else {
            CHANGE_CLOSING
        };
        let next = now + (target - now).clamp(-most, most);
        if (next - now).abs() < 0.5 {
            Change::None
        } else {
            Change::Adjust(next)
        }
    }

    /// How far the hardware clock stood from CLOCK_TAI at the last
    /// comparison, in ns, while it follows it.
    pub fn following(&self) -> Option<i128> {
        self.offset.filter(|_| self.steady >= STEADY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsteered_the_free_clock_is_the_hardware_clock() {
        let steering = Steering::new(1_000, 0.0);
        assert_eq!(steering.free(5_000), 5_000);
        assert_eq!(steering.hardware(5_000), 5_000);
        assert_eq!(steering.free(0), 0, "before the first segment too");
    }

    #[test]
    fn times_convert_both_ways_across_changes() {
        let mut steering = Steering::new(0, 0.0);
        // +20 ppm from 1 s, -5 ppm from 3 s.
        steering.changed(1_000_000_000, 20_000.0);
        steering.changed(3_000_000_000, -5_000.0);
        // At 3 s on the hardware clock, 2 s at +20 ppm put it 40 us ahead.
        let at = steering.free(3_000_000_000);
        assert!((at - (3_000_000_000 - 39_999)).abs() <= 1, "{at}");
        for hardware in [
            500_000_000i128,
            1_500_000_000,
            2_999_999_999,
            4_000_000_000,
            1_000_000_000_000,
        ] {
            let back = steering.hardware(steering.free(hardware));
            assert!((back - hardware).abs() <= 1, "{hardware}: {back}");
        }
        assert!((steering.rate_at(steering.free(2_000_000_000)) - 1.000_02).abs() < 1e-12);
    }

    #[test]
    fn a_time_stamp_taken_before_a_change_converts_as_it_was_taken() {
        let mut steering = Steering::new(0, 8_000.0);
        // A transmit time stamp from just before the change, collected
        // after it.
        let stamped = 999_990_000;
        let before = steering.free(stamped);
        steering.changed(1_000_000_000, -3_000.0);
        assert_eq!(steering.free(stamped), before);
    }

    #[test]
    fn a_recorded_line_runs_at_the_rate_the_clock_runs_at_now() {
        let mut steering = Steering::new(0, 0.0);
        // The last Sync at free 0.5 s; the rate changed at 1 s.
        let (free, time, rate) = (500_000_000i128, 7_000_000_000_000i128, 1.000_001);
        steering.changed(1_000_000_000, 1_000.0);
        let (local, at_local, local_rate) = steering.hardware_line(free, time, rate);
        assert_eq!(local, 1_000_000_000);
        // Carried forward on the hardware clock, it agrees with the line on
        // the free clock.
        for hardware in [1_000_000_000i128, 1_125_000_000, 3_000_000_000] {
            let recorded = at_local + ((hardware - local) as f64 * local_rate).round() as i128;
            let truth = time + ((steering.free(hardware) - free) as f64 * rate).round() as i128;
            assert!(
                (recorded - truth).abs() <= 1,
                "{hardware}: {}",
                recorded - truth
            );
        }
    }

    #[test]
    fn times_near_the_tai_epoch_keep_their_nanoseconds() {
        let base = 1_762_000_000_000_000_000i128;
        let mut steering = Steering::new(base, 12_345.0);
        steering.changed(base + 100_000_000, -6_789.0);
        let hardware = base + 3_600_000_000_007;
        assert!((steering.hardware(steering.free(hardware)) - hardware).abs() <= 1);
    }

    /// A clock `drift` ppb fast running against CLOCK_TAI, compared each
    /// 100 ms, from `offset` ns ahead: how far it stands after `seconds`,
    /// and whether it then follows.
    fn run(drift: f64, offset: i128, seconds: u32) -> (i128, bool, u32) {
        let mut steering = Steering::new(0, 0.0);
        let mut offset = offset as f64;
        let mut sets = 0;
        for _ in 0..seconds * 10 {
            let rate = (1.0 + drift * 1e-9) * (1.0 + steering.adjust() * 1e-9) - 1.0;
            offset += rate * 1e8;
            match steering.follow(offset.round() as i128, Some(drift * 1e-9), false) {
                Change::Adjust(adjust) => steering.changed(0, adjust),
                Change::Set(by) => {
                    offset += by as f64;
                    sets += 1;
                }
                Change::None => {}
            }
        }
        (offset.round() as i128, steering.following().is_some(), sets)
    }

    #[test]
    fn a_fast_clock_closes_on_tai_and_follows_it() {
        let (offset, following, sets) = run(25_000.0, 0, 240);
        assert!(offset.abs() < 500, "{offset} ns");
        assert!(following);
        assert_eq!(sets, 0);
    }

    #[test]
    fn a_clock_far_off_is_set_once_then_slewed() {
        let (offset, following, sets) = run(-15_000.0, 37_000_000_000, 240);
        assert_eq!(sets, 1);
        assert!(offset.abs() < 500, "{offset} ns");
        assert!(following);
    }

    #[test]
    fn as_grandmaster_it_is_left_alone() {
        let mut steering = Steering::new(0, 3.0);
        assert_eq!(steering.follow(5_000_000, Some(1e-5), true), Change::None);
        assert_eq!(steering.follow(500, Some(1e-5), true), Change::None);
        assert_eq!(steering.following(), None);
    }

    #[test]
    fn it_follows_only_after_staying_close() {
        let mut steering = Steering::new(0, 0.0);
        for _ in 0..STEADY - 1 {
            steering.follow(2_000, Some(0.0), false);
        }
        assert_eq!(steering.following(), None);
        steering.follow(2_000, Some(0.0), false);
        assert_eq!(steering.following(), Some(2_000));
        steering.follow(FOLLOWING + 1, Some(0.0), false);
        assert_eq!(steering.following(), None, "one far comparison ends it");
    }
}
