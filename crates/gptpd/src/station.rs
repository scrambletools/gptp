//! An end station's time for applications: the grandmaster's time against
//! the port's hardware clock and against CLOCK_MONOTONIC, written to
//! `/run/gptpd/<interface>.time` (gptp::time) at each comparison and each
//! Sync. Nothing moves the hardware clock, so the mapping carries what a
//! servo would have: offset and rate, and a new generation at each
//! discontinuity.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::Duration;

use gptp::ClockIdentity;
use gptp::time::{Mapping, State};
use raw_ether::clock::{Comparison, HardwareClock, SystemClock};

/// How far a comparison against the raw clock may stand from what the
/// last ones foretold before the hardware clock is taken to have jumped,
/// with the driver's readings and with reads between the system clock's.
const JUMP_TIGHT: i128 = 1_000;
const JUMP_READ: i128 = 10_000;

/// How far, in nanoseconds, two readings of CLOCK_MONOTONIC in a row may
/// stand from the comparison's line, on the same side, before its rate is
/// taken to have changed, as NTP steers it: then the line starts afresh
/// from them. Read by the driver, a reading is good to tens of ns.
const KINK_TIGHT: i128 = 500;
const KINK_READ: i128 = 20_000;

/// How old an earlier gptpd's record may be for a restart to go on from
/// its time.
const FRESH_NANOS: i64 = 5_000_000_000;

/// How much suspended time counts as a resume.
const RESUMED: Duration = Duration::from_millis(100);

pub struct Station {
    path: PathBuf,
    file: File,
    sequence: u32,
    generation: u32,
    /// The hardware clock against CLOCK_MONOTONIC, for the mapping.
    monotonic: Comparison,
    /// Against CLOCK_MONOTONIC_RAW, which nothing adjusts, to tell when
    /// the hardware clock jumps.
    raw: Comparison,
    /// Whether the driver compares with the system clocks, the kernel
    /// taking a clock other than the wall clock for it.
    tight: bool,
    suspended: Duration,
    last_sync: Option<i128>,
    /// What an earlier gptpd last wrote, as this one started.
    earlier: Option<Mapping>,
    /// The last line with a time before the hardware clock jumped, to draw
    /// the grandmaster's time afresh on it: its line against
    /// CLOCK_MONOTONIC holds, as no jump of the hardware clock moves that.
    before_jump: Option<Mapping>,
    written: Option<Mapping>,
    /// The last mapping with a grandmaster's time, against which a new
    /// grandmaster's is weighed.
    last_timed: Option<Mapping>,
    /// Since when, CLOCK_MONOTONIC in nanoseconds, no grandmaster is
    /// heard.
    lost_at: Option<i64>,
    residual: Option<i64>,
    /// How far the last reading of CLOCK_MONOTONIC stood from the line,
    /// when that was too far.
    kink: Option<i128>,
    /// A line that moved, held back since a CLOCK_MONOTONIC time.
    doubted: Option<(Mapping, i64)>,
    steadying: Option<Steadying>,
}

/// The grandmaster's time as the profile running has it: whose, whether
/// this system's own, and as a line on the hardware clock (its time at a
/// reading of the clock, and its rate over it), drawn from a recent
/// reading so the comparison is not stretched far.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub grandmaster: Option<ClockIdentity>,
    pub ours: bool,
    pub line: Option<(i128, i128, f64)>,
    pub asymmetry_included: bool,
}

/// What a comparison found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    Steady,
    /// The hardware clock was set, as a driver does when it resets the
    /// card.
    Jumped(i128),
    /// The computer slept: the link and everything measured on it start
    /// over.
    Resumed,
}

impl Station {
    /// The station for `interface`, its generation going on from what an
    /// earlier gptpd last wrote.
    pub fn open(interface: &str, clock: &HardwareClock) -> Result<Self, String> {
        let path = gptp::time::path(interface).map_err(|error| error.to_string())?;
        let fail = |error: io::Error| format!("{}: {error}", path.display());
        if let Some(folder) = path.parent() {
            fs::create_dir_all(folder).map_err(fail)?;
        }
        let earlier = fs::read(&path)
            .ok()
            .and_then(|record| Mapping::decode(&record).ok())
            .map(|(mapping, _)| mapping);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .open(&path)
            .map_err(fail)?;
        let tight = clock
            .offset_from_system_clock(SystemClock::MonotonicRaw)
            .and_then(|_| clock.offset_from_system_clock(SystemClock::Monotonic))
            .is_ok();
        Ok(Self {
            path,
            file,
            sequence: 0,
            generation: earlier.map_or(0, |earlier| earlier.generation.wrapping_add(1)),
            earlier,
            before_jump: None,
            monotonic: Comparison::default(),
            raw: Comparison::default(),
            tight,
            suspended: raw_ether::clock::suspended(),
            last_sync: None,
            written: None,
            last_timed: None,
            lost_at: None,
            residual: None,
            kink: None,
            doubted: None,
            steadying: None,
        })
    }

    /// The grandmaster's time as a line on the hardware clock, from what an
    /// earlier gptpd wrote less than [`FRESH_NANOS`] before CLOCK_MONOTONIC
    /// time `now`: a restart's time base goes on from it, so it never shows
    /// a time of its own in place of the network's. The hardware clock is
    /// never stepped, so the line holds across the restart.
    pub fn earlier_line(&self, now: i64) -> Option<(i128, i128, f64)> {
        self.earlier
            .filter(|earlier| earlier.state != State::None)
            .filter(|earlier| now.saturating_sub(earlier.updated) < FRESH_NANOS)
            .map(|earlier| (earlier.local, earlier.at_local, earlier.local_rate))
    }

    /// Whether the grandmaster's time is to be drawn afresh on the hardware
    /// clock, which jumped.
    pub fn awaiting_rebase(&self) -> bool {
        self.before_jump.is_some()
    }

    /// Once the jumped hardware clock is compared afresh: the grandmaster's
    /// time as a line on it at its reading `local`, from the line against
    /// CLOCK_MONOTONIC before the jump.
    pub fn rebased(&mut self, local: i128) -> Option<(i128, i128, f64)> {
        let before = self.before_jump?;
        let slope = self.monotonic.rate()?;
        self.before_jump = None;
        let monotonic = local - self.monotonic.offset_at(local);
        // The grandmaster's nanoseconds in one of CLOCK_MONOTONIC's, times
        // CLOCK_MONOTONIC's in one of the hardware clock's.
        let rate = before.monotonic_rate * (1.0 - slope);
        Some((local, before.grandmaster_at(monotonic as i64), rate))
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn residual(&self) -> Option<i64> {
        self.residual
    }

    /// Whether the driver compares the clock with the system's.
    pub fn tight(&self) -> bool {
        self.tight
    }

    fn offset(&self, clock: &HardwareClock, system: SystemClock) -> io::Result<i128> {
        if self.tight {
            clock.offset_from_system_clock(system)
        } else {
            clock.offset_read_against(system)
        }
    }

    /// Compares the hardware clock with the system's once more.
    pub fn compare(&mut self, clock: &HardwareClock) -> io::Result<Found> {
        let suspended = raw_ether::clock::suspended();
        if suspended.saturating_sub(self.suspended) > RESUMED {
            self.suspended = suspended;
            self.restart();
            return Ok(Found::Resumed);
        }
        let at = clock.now()?.as_nanos() as i128;
        let raw = self.offset(clock, SystemClock::MonotonicRaw)?;
        let jump = if self.tight { JUMP_TIGHT } else { JUMP_READ };
        let mut found = Found::Steady;
        if let Some(moved) = moved(&self.raw, at, raw)
            && moved.abs() > jump
        {
            // Read again, to tell a clock that was set from one misread.
            let again = self.offset(clock, SystemClock::MonotonicRaw)?;
            if (again - raw).abs() > jump / 2 {
                return Ok(Found::Steady);
            }
            self.before_jump = self.last_timed;
            self.restart();
            found = Found::Jumped(moved);
        }
        self.raw.update(at, raw);
        let monotonic = self.offset(clock, SystemClock::Monotonic)?;
        self.follow_monotonic(at, monotonic);
        Ok(found)
    }

    /// Takes a comparison with CLOCK_MONOTONIC, whose rate changes in a
    /// step as NTP steers it. Two readings in a row standing too far from
    /// the line, on the same side, start the line afresh from them, so the
    /// mapping follows within two readings rather than a whole span.
    fn follow_monotonic(&mut self, at: i128, offset: i128) {
        let bound = if self.tight { KINK_TIGHT } else { KINK_READ };
        let off = self
            .monotonic
            .rate()
            .map(|_| offset - self.monotonic.offset_at(at));
        match off {
            Some(off) if off.abs() > bound => {
                if self.kink.is_some_and(|last| last.signum() == off.signum()) {
                    self.monotonic.keep_last(1);
                    self.kink = None;
                } else {
                    self.kink = Some(off);
                }
            }
            _ => self.kink = None,
        }
        self.monotonic.update(at, offset);
    }

    /// Forgets the comparisons and starts a new generation.
    fn restart(&mut self) {
        self.kink = None;
        self.last_timed = None;
        self.raw.clear();
        self.monotonic.clear();
        self.generation = self.generation.wrapping_add(1);
        self.written = None;
        self.residual = None;
    }

    /// A new generation for a discontinuity found elsewhere, such as a
    /// change of profile.
    pub fn discontinue(&mut self) {
        self.last_timed = None;
        self.generation = self.generation.wrapping_add(1);
        self.written = None;
        self.residual = None;
    }

    /// The mapping from `view`, at CLOCK_MONOTONIC time `now` in
    /// nanoseconds.
    pub fn mapping(&mut self, view: View, now: i64) -> Mapping {
        let mapping = self.weighed(view, now);
        if mapping.state != State::None {
            self.lost_at = None;
            return self.steadied(mapping, now);
        }
        self.doubted = None;
        let lost_at = *self.lost_at.get_or_insert(now);
        held(self.last_timed, lost_at, now, mapping)
    }

    /// `mapping`, unless its Sync's time stands more than [`STEADY_NANOS`]
    /// from the last line written, on the hardware clock, which nothing
    /// steers. Then the last line is kept until a later Sync says the same
    /// as `mapping`'s, or for [`DOUBT_NANOS`], and the step starts a new
    /// generation: one Sync with a bad time costs nothing. CLOCK_MONOTONIC
    /// changing rate, as NTP steers it, moves no line on the hardware
    /// clock, so it is never taken for a step; the line kept goes on along
    /// CLOCK_MONOTONIC through the comparison as it stands.
    fn steadied(&mut self, mapping: Mapping, now: i64) -> Mapping {
        let moved =
            |line: &Mapping, from: &Mapping| line.at_local - from.grandmaster_at_local(line.local);
        let Some(written) = self.written.filter(|written| {
            written.state == mapping.state
                && written.generation == mapping.generation
                && written.grandmaster == mapping.grandmaster
        }) else {
            self.doubted = None;
            return mapping;
        };
        let off = moved(&mapping, &written);
        if off.abs() <= STEADY_NANOS {
            if self.doubted.take().is_some() {
                self.steadying = Some(Steadying::Dropped);
            }
            return slewed(mapping, &written);
        }
        match self.doubted {
            Some((doubted, since))
                if (doubted.local != mapping.local
                    && moved(&mapping, &doubted).abs() <= STEADY_NANOS)
                    || now - since >= DOUBT_NANOS =>
            {
                self.doubted = None;
                self.discontinue();
                let stepped = Mapping {
                    generation: self.generation,
                    residual: None,
                    ..mapping
                };
                self.last_timed = Some(stepped);
                self.steadying = Some(Steadying::Stepped(off, self.generation));
                stepped
            }
            Some(_) => {
                let held = self.hold(written, mapping, now);
                slewed(held, &written)
            }
            None => {
                self.doubted = Some((mapping, now));
                self.steadying = Some(Steadying::HeldBack(off));
                let held = self.hold(written, mapping, now);
                slewed(held, &written)
            }
        }
    }

    /// The last line written on the hardware clock, put on CLOCK_MONOTONIC
    /// afresh, in place of `mapping`'s.
    fn hold(&mut self, written: Mapping, mapping: Mapping, now: i64) -> Mapping {
        let mut held = Mapping {
            updated: now,
            residual: mapping.residual,
            ..written
        };
        if let Some(slope) = self.monotonic.rate() {
            let (monotonic, monotonic_rate) = on_monotonic(
                held.local,
                held.local_rate,
                self.monotonic.offset_at(held.local),
                slope,
            );
            held.monotonic = monotonic;
            held.at_monotonic = held.at_local;
            held.monotonic_rate = monotonic_rate;
        }
        self.last_timed = Some(held);
        held
    }

    /// What became of a line that moved, once.
    pub fn take_steadying(&mut self) -> Option<Steadying> {
        self.steadying.take()
    }

    /// The mapping from `view`, in a new generation where a new
    /// grandmaster's time does not carry on the last one's. It is weighed
    /// at the new grandmaster's first line, whenever that comes, not when
    /// its identity first shows, before it has a time to weigh.
    fn weighed(&mut self, view: View, now: i64) -> Mapping {
        let mapping = self.drawn(view, now);
        if mapping.state == State::None {
            return mapping;
        }
        if let Some(last) = self.last_timed
            && last.grandmaster != mapping.grandmaster
            && !carries_on(&last, &mapping)
        {
            self.discontinue();
            let mapping = self.drawn(view, now);
            self.last_timed = Some(mapping);
            return mapping;
        }
        self.last_timed = Some(mapping);
        mapping
    }

    /// The mapping from `view` in this generation.
    fn drawn(&mut self, view: View, now: i64) -> Mapping {
        let line = view.line.zip(self.monotonic.rate());
        let mut mapping = Mapping {
            generation: self.generation,
            state: State::None,
            grandmaster: view.grandmaster.unwrap_or_default(),
            updated: now,
            monotonic: 0,
            at_monotonic: 0,
            monotonic_rate: 1.0,
            local: 0,
            at_local: 0,
            local_rate: 1.0,
            asymmetry_included: view.asymmetry_included,
            residual: self.residual,
        };
        if let (Some(_), Some(((local, at_local, local_rate), slope))) = (view.grandmaster, line) {
            if view.ours {
                self.residual = None;
            } else if self.last_sync != Some(local) {
                // The Sync's time against what the mapping foretold for it.
                self.residual = self
                    .written
                    .filter(|written| written.state == State::Following)
                    .filter(|written| written.generation == self.generation)
                    .map(|written| (at_local - written.grandmaster_at_local(local)) as i64);
                self.last_sync = Some(local);
            }
            let (monotonic, monotonic_rate) =
                on_monotonic(local, local_rate, self.monotonic.offset_at(local), slope);
            mapping = Mapping {
                state: if view.ours {
                    State::Grandmaster
                } else {
                    State::Following
                },
                monotonic,
                at_monotonic: at_local,
                monotonic_rate,
                local,
                at_local,
                local_rate,
                residual: self.residual,
                ..mapping
            };
        }
        mapping
    }

    /// Writes `mapping` over the record, whole.
    pub fn write(&mut self, mapping: Mapping) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        self.file.write_all_at(&mapping.encode(self.sequence), 0)?;
        self.written = Some(mapping);
        Ok(())
    }
}

/// How far `offset`, read at `at`, stands from what `comparison`
/// foretells: only once it has a rate to foretell by, as two clocks free
/// of each other drift tens of ppm apart, microseconds between readings.
pub fn moved(comparison: &Comparison, at: i128, offset: i128) -> Option<i128> {
    comparison.rate()?;
    Some(offset - comparison.offset_at(at))
}

/// How long, in nanoseconds, the last grandmaster's line holds once none
/// is heard: a handback leaves about a second with none before the new
/// grandmaster's Announce comes, and listeners would unlock meanwhile.
const HOLDOVER_NANOS: i64 = 5_000_000_000;

/// How far, in nanoseconds, a Sync's time may stand from the last line
/// written before it is held back as a bad one: half the 10 us in which
/// applications see a step.
const STEADY_NANOS: i128 = 5_000;

/// How far, in nanoseconds, a mapping's line on CLOCK_MONOTONIC moves
/// from the last one written: the comparison catching up with
/// CLOCK_MONOTONIC changing rate puts it right over a few mappings, each
/// well within the 10 us in which applications see a step.
const SLEW_NANOS: i128 = 4_000;

/// How long a line that moved is held back, waiting for a later Sync to
/// say the same, before it is taken as a step anyway.
const DOUBT_NANOS: i64 = 1_000_000_000;

/// What became of a line that moved, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Steadying {
    /// Held back, the last line kept: how far it moved, in nanoseconds.
    HeldBack(i128),
    /// The line held back was one Sync's, and the next came back.
    Dropped,
    /// A later Sync said the same, or it was held back long enough: a
    /// step of so many nanoseconds, and the new generation.
    Stepped(i128, u32),
}

/// `mapping`, its line on CLOCK_MONOTONIC moved from `written`'s, where it
/// is drawn, by at most [`SLEW_NANOS`].
fn slewed(mapping: Mapping, written: &Mapping) -> Mapping {
    let moved = mapping.at_monotonic - written.grandmaster_at(mapping.monotonic);
    if moved.abs() <= SLEW_NANOS {
        return mapping;
    }
    Mapping {
        at_monotonic: mapping.at_monotonic - moved + moved.signum() * SLEW_NANOS,
        ..mapping
    }
}

/// The mapping while no grandmaster is heard, since `lost_at`: the `last`
/// one with a time, kept fresh, for [`HOLDOVER_NANOS`]; then `none`.
fn held(last: Option<Mapping>, lost_at: i64, now: i64, none: Mapping) -> Mapping {
    match last {
        Some(last) if now - lost_at < HOLDOVER_NANOS && last.generation == none.generation => {
            Mapping {
                updated: now,
                residual: None,
                ..last
            }
        }
        _ => none,
    }
}

/// How far a new grandmaster's time may stand from the last's, and its
/// rate, and still carry it on.
const CARRIES_ON_NANOS: i128 = 1_000;
const CARRIES_ON_RATE: f64 = 5e-6;

/// Whether `next` carries on `last`'s time: within [`CARRIES_ON_NANOS`] of
/// where `last` would have it, at `next`'s reading of the hardware clock,
/// and within [`CARRIES_ON_RATE`] of its rate.
fn carries_on(last: &Mapping, next: &Mapping) -> bool {
    let foretold = last.grandmaster_at_local(next.local);
    (next.at_local - foretold).abs() <= CARRIES_ON_NANOS
        && (next.local_rate - last.local_rate).abs() <= CARRIES_ON_RATE
}

/// The hardware clock's time `local` on CLOCK_MONOTONIC, and the
/// grandmaster's rate over CLOCK_MONOTONIC from its rate over the
/// hardware clock, `local_rate`. The hardware clock stood `offset` ahead
/// of CLOCK_MONOTONIC at `local`, the gap growing by `slope` for each of
/// the hardware clock's nanoseconds.
fn on_monotonic(local: i128, local_rate: f64, offset: i128, slope: f64) -> (i64, f64) {
    ((local - offset) as i64, local_rate / (1.0 - slope))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn followed(at_local: i128, local_rate: f64) -> Mapping {
        Mapping {
            generation: 4,
            state: State::Following,
            grandmaster: ClockIdentity(1),
            updated: 0,
            monotonic: 0,
            at_monotonic: at_local,
            monotonic_rate: local_rate,
            local: 1_000_000_000_000,
            at_local,
            local_rate,
            asymmetry_included: false,
            residual: None,
        }
    }

    /// A station writing to a scratch file, its hardware clock 3 ppm fast
    /// of CLOCK_MONOTONIC.
    fn station() -> Station {
        let path = std::env::temp_dir().join(format!("gptpd-station-{}.time", std::process::id()));
        let file = File::create(&path).expect("scratch file");
        let mut station = Station {
            path,
            file,
            sequence: 0,
            generation: 10,
            monotonic: Comparison::default(),
            raw: Comparison::default(),
            tight: false,
            suspended: Duration::ZERO,
            last_sync: None,
            earlier: None,
            before_jump: None,
            written: None,
            last_timed: None,
            lost_at: None,
            residual: None,
            kink: None,
            doubted: None,
            steadying: None,
        };
        for step in 0..5_i128 {
            let at = 1_000_000_000_000 + step * 100_000_000;
            station.monotonic.update(at, 7_000 + at * 3 / 1_000_000);
        }
        station
    }

    /// The `k`th Sync from one grandmaster, 125 ms apart, its time `off`
    /// nanoseconds from where it should be.
    fn sync(k: i128, off: i128) -> View {
        View {
            grandmaster: Some(ClockIdentity(9)),
            ours: false,
            line: Some((
                1_000_400_000_000 + k * 125_000_000,
                5_000_000_000_000 + k * 125_000_000 + off,
                1.0,
            )),
            asymmetry_included: false,
        }
    }

    fn published(station: &mut Station, view: View, now: i64) -> Mapping {
        let mapping = station.mapping(view, now);
        station.write(mapping).expect("written");
        mapping
    }

    /// The hardware clock less CLOCK_MONOTONIC at the hardware clock's
    /// time `at`, as in [`station`], until NTP makes CLOCK_MONOTONIC run
    /// `kink` faster, a fraction, from `kinked` on.
    fn steered(at: i128, kinked: i128, kink: f64) -> i128 {
        let base = 7_000 + at * 3 / 1_000_000;
        if at <= kinked {
            base
        } else {
            base - ((at - kinked) as f64 * kink) as i128
        }
    }

    /// Rides a kink of CLOCK_MONOTONIC's rate, read every 100 ms a few ns
    /// off, with a Sync every 125 ms: no line is held back, the generation
    /// stays, and no mapping's line moves more than [`SLEW_NANOS`] and a
    /// little from the last. The largest error, in ns, of the
    /// grandmaster's time the record gives for CLOCK_MONOTONIC now, before
    /// the kink and from `settle` ns after it.
    fn ride(kink: f64, settle: i128) -> (i128, i128) {
        let mut station = station();
        station.tight = true;
        let kinked = 1_002_037_000_000;
        let wobble = [0, 15, -10, 20, -20, 5];
        let (mut before, mut after) = (0, 0);
        let mut reading = 0;
        let mut last: Option<Mapping> = None;
        for tick in 0..160_i128 {
            let at = 1_000_500_000_000 + tick * 25_000_000;
            if (at - 1_000_500_000_000) % 100_000_000 == 0 {
                station.follow_monotonic(at, steered(at, kinked, kink) + wobble[reading % 6]);
                reading += 1;
            }
            if (at - 1_000_400_000_000) % 125_000_000 != 0 {
                continue;
            }
            let k = (at - 1_000_400_000_000) / 125_000_000;
            let now = (at - steered(at, kinked, kink)) as i64;
            let mapping = published(&mut station, sync(k, 0), now);
            assert_eq!(station.take_steadying(), None, "at {at}");
            assert_eq!(mapping.generation, 10);
            if let Some(last) = last {
                let moved = mapping.at_monotonic - last.grandmaster_at(mapping.monotonic);
                assert!(moved.abs() <= SLEW_NANOS + 100, "moved {moved} ns at {at}");
            }
            last = Some(mapping);
            let error = (mapping.grandmaster_at(now) - (5_000_000_000_000 + k * 125_000_000)).abs();
            if at <= kinked {
                before = before.max(error);
            } else if at > kinked + settle {
                after = after.max(error);
            }
        }
        (before, after)
    }

    #[test]
    fn the_record_follows_ntp_stepping_clock_monotonic_22_ppm_within_half_a_second() {
        // As at 11:09:36.9 on the desktop: from -0.03 to -22.2 ppm. The
        // rate is right from the second reading after; what the line lagged
        // until then is made up within 4 us a mapping.
        let (before, after) = ride(22.2e-6, 500_000_000);
        assert!(before < 100, "{before} ns before");
        assert!(after < 300, "{after} ns after");
    }

    #[test]
    fn the_record_follows_ntp_stepping_clock_monotonic_150_ppm_within_two_seconds() {
        let (before, after) = ride(150e-6, 2_000_000_000);
        assert!(before < 100, "{before} ns before");
        assert!(after < 300, "{after} ns after");
    }

    #[test]
    fn the_record_follows_ntp_stepping_clock_monotonic_65_ppm_within_a_second() {
        let (before, after) = ride(65e-6, 1_000_000_000);
        assert!(before < 100, "{before} ns before");
        assert!(after < 300, "{after} ns after");
    }

    #[test]
    fn one_sync_with_a_bad_time_is_held_back_and_costs_nothing() {
        let mut station = station();
        published(&mut station, sync(0, 0), 1);
        let before = published(&mut station, sync(1, 0), 2);
        assert_eq!(station.take_steadying(), None);
        let held = published(&mut station, sync(2, 50_000), 3);
        assert!(matches!(
            station.take_steadying(),
            Some(Steadying::HeldBack(moved)) if (moved - 50_000).abs() < 10
        ));
        assert_eq!(
            (held.monotonic, held.at_monotonic, held.generation),
            (before.monotonic, before.at_monotonic, 10)
        );
        // Written again before the next Sync, as ten times a second.
        let again = published(&mut station, sync(2, 50_000), 4);
        assert_eq!(again.at_monotonic, before.at_monotonic);
        assert_eq!(station.take_steadying(), None);
        let after = published(&mut station, sync(3, 0), 5);
        assert_eq!(station.take_steadying(), Some(Steadying::Dropped));
        assert_eq!(after.generation, 10);
        assert_eq!(after.at_local, 5_000_000_000_000 + 3 * 125_000_000);
        assert!((after.at_monotonic - before.grandmaster_at(after.monotonic)).abs() < 10);
    }

    #[test]
    fn a_step_a_later_sync_says_again_is_a_new_generation() {
        let mut station = station();
        published(&mut station, sync(0, 0), 1);
        published(&mut station, sync(1, 0), 2);
        published(&mut station, sync(2, 50_000), 3);
        let stepped = published(&mut station, sync(3, 50_000), 4);
        assert!(matches!(
            station.take_steadying(),
            Some(Steadying::Stepped(moved, 11)) if (moved - 50_000).abs() < 10
        ));
        assert_eq!(stepped.generation, 11);
        assert_eq!(
            stepped.at_local,
            5_000_000_000_000 + 3 * 125_000_000 + 50_000
        );
        let next = published(&mut station, sync(4, 50_000), 5);
        assert_eq!((next.generation, station.take_steadying()), (11, None));
    }

    #[test]
    fn a_line_held_back_long_enough_is_taken_as_a_step() {
        let mut station = station();
        published(&mut station, sync(0, 0), 1);
        published(&mut station, sync(1, 0), 2);
        let start = 1_000_000;
        let held = published(&mut station, sync(2, 50_000), start);
        assert_eq!(held.generation, 10);
        station.take_steadying();
        let still = published(&mut station, sync(2, 50_000), start + DOUBT_NANOS - 1);
        assert_eq!((still.generation, station.take_steadying()), (10, None));
        let taken = published(&mut station, sync(2, 50_000), start + DOUBT_NANOS);
        assert_eq!(taken.generation, 11);
        assert!(matches!(
            station.take_steadying(),
            Some(Steadying::Stepped(_, 11))
        ));
    }

    #[test]
    fn a_line_moving_a_little_is_no_step() {
        let mut station = station();
        published(&mut station, sync(0, 0), 1);
        published(&mut station, sync(1, 0), 2);
        let moved = published(&mut station, sync(2, 3_000), 3);
        assert_eq!(station.take_steadying(), None);
        assert_eq!(moved.at_local, 5_000_000_000_000 + 2 * 125_000_000 + 3_000);
        assert_eq!(moved.generation, 10);
    }

    #[test]
    fn a_foreign_time_at_the_first_line_of_a_new_grandmaster_is_a_new_generation() {
        // A restart as seen on hardware: our own time for a moment, no line
        // while the grandmaster's first Sync is awaited, then its time,
        // 1.96 s away.
        let ours = ClockIdentity(0xa010_a3ff_fea5_1726);
        let theirs = ClockIdentity(0x0001_f2ff_feff_3b14);
        let mut station = station();
        let local = 1_000_000_400_000;
        let own = station.mapping(
            View {
                grandmaster: Some(ours),
                ours: true,
                line: Some((local, 5_000_000_000_000, 1.0)),
                asymmetry_included: false,
            },
            1,
        );
        assert_eq!((own.state, own.generation), (State::Grandmaster, 10));
        let waiting = station.mapping(
            View {
                grandmaster: Some(theirs),
                ours: false,
                line: None,
                asymmetry_included: false,
            },
            2,
        );
        assert_eq!(waiting.generation, 10);
        let followed = station.mapping(
            View {
                grandmaster: Some(theirs),
                ours: false,
                line: Some((
                    local + 125_000_000,
                    5_000_000_000_000 + 125_000_000 + 1_960_000_000,
                    1.0,
                )),
                asymmetry_included: false,
            },
            3,
        );
        assert_eq!(
            (followed.state, followed.generation),
            (State::Following, 11)
        );
        // And one carrying the time on keeps it.
        let carried = station.mapping(
            View {
                grandmaster: Some(ours),
                ours: true,
                line: Some((
                    local + 250_000_000,
                    followed.grandmaster_at_local(local + 250_000_000),
                    1.0,
                )),
                asymmetry_included: false,
            },
            4,
        );
        assert_eq!(carried.generation, 11);
    }

    #[test]
    fn after_a_jump_the_grandmasters_time_is_drawn_afresh_from_monotonic() {
        let mut station = station();
        // Before: the grandmaster at 5000 s when CLOCK_MONOTONIC read
        // 0.993 s, 2 ppm fast of it.
        let before = Mapping {
            monotonic: 993_000_000,
            at_monotonic: 5_000_000_000_000,
            monotonic_rate: 1.0 + 2e-6,
            ..followed(5_000_000_000_000, 1.0)
        };
        station.before_jump = Some(before);
        let local = 1_000_000_400_000;
        let (at, time, rate) = station.rebased(local).expect("compared");
        assert_eq!(at, local);
        let monotonic = local - station.monotonic.offset_at(local);
        assert_eq!(time, before.grandmaster_at(monotonic as i64));
        // 2 ppm over CLOCK_MONOTONIC, and the hardware clock 3 ppm fast of
        // it: about 1 ppm under the hardware clock.
        assert!((rate - (1.0 - 1e-6)).abs() < 1e-8, "{rate}");
        assert!(!station.awaiting_rebase());
    }

    #[test]
    fn the_last_line_holds_a_while_once_no_grandmaster_is_heard() {
        let last = followed(5_000_000_000_000, 1.0 + 6.5e-6);
        let none = Mapping {
            state: State::None,
            grandmaster: ClockIdentity::default(),
            ..last
        };
        let lost = 10_000_000_000;
        let holding = held(Some(last), lost, lost + 1_000_000_000, none);
        assert_eq!(holding.state, State::Following);
        assert_eq!(holding.grandmaster, last.grandmaster);
        assert_eq!(holding.at_local, last.at_local);
        assert_eq!(holding.updated, lost + 1_000_000_000);
        assert_eq!(held(Some(last), lost, lost + 6_000_000_000, none), none);
        assert_eq!(held(None, lost, lost + 1, none), none);
        // Not across a generation: a jump or a resume ends the holdover.
        let later = Mapping {
            generation: 5,
            ..none
        };
        assert_eq!(held(Some(last), lost, lost + 1, later), later);
    }

    #[test]
    fn a_takeover_with_the_old_time_carries_on_and_a_step_does_not() {
        let last = followed(5_000_000_000_000, 1.0 + 6.5e-6);
        // A second later, the new grandmaster has the time the last
        // foretold, give or take its own error.
        let later = |offset: i128, rate: f64| Mapping {
            local: last.local + 1_000_000_000,
            at_local: last.grandmaster_at_local(last.local + 1_000_000_000) + offset,
            local_rate: rate,
            ..last
        };
        assert!(carries_on(&last, &later(0, last.local_rate)));
        assert!(carries_on(&last, &later(-800, last.local_rate + 2e-6)));
        assert!(!carries_on(&last, &later(1_500, last.local_rate)));
        assert!(!carries_on(&last, &later(0, last.local_rate + 12e-6)));
        assert!(!carries_on(&last, &later(-37_000_000_000, last.local_rate)));
    }

    #[test]
    fn a_clock_drifting_away_is_no_jump_and_a_jump_is() {
        // The I210's free clock against CLOCK_MONOTONIC_RAW, as at the first
        // field run: 22.6 ppm apart, read every 100 ms.
        let offset = |at: i128| 7_000 + at * 226 / 10_000_000;
        let mut comparison = Comparison::default();
        for step in 0..30 {
            let at = 1_000_000_000_000 + step * 100_000_000;
            let moved = moved(&comparison, at, offset(at));
            match step {
                0 | 1 => assert_eq!(moved, None, "no rate yet"),
                _ => assert!(
                    moved.is_some_and(|moved| moved.abs() < 50),
                    "{step}: {moved:?}"
                ),
            }
            comparison.update(at, offset(at));
        }
        let at = 1_000_000_000_000 + 30 * 100_000_000;
        let jumped = moved(&comparison, at, offset(at) - 3_400).expect("rate");
        assert!((jumped + 3_400).abs() < 50, "{jumped}");
    }

    #[test]
    fn the_mapping_on_monotonic_agrees_with_the_one_on_the_hardware_clock() {
        // The hardware clock runs 3 ppm fast of CLOCK_MONOTONIC, from 7 s
        // ahead; the grandmaster 10 ppm fast of the hardware clock.
        let hardware = |monotonic: i128| 7_000_000_000 + monotonic + monotonic * 3 / 1_000_000;
        let slope = 3e-6 / (1.0 + 3e-6);
        let local_rate = 1.0 + 10e-6;
        let at_monotonic0 = 5_000_000_000_i128;
        let local = hardware(at_monotonic0);
        let offset = local - at_monotonic0;
        let (monotonic, monotonic_rate) = on_monotonic(local, local_rate, offset, slope);
        assert_eq!(i128::from(monotonic), at_monotonic0);
        let mapping = Mapping {
            generation: 0,
            state: State::Following,
            grandmaster: ClockIdentity::default(),
            updated: 0,
            monotonic,
            at_monotonic: 1_000_000_000_000,
            monotonic_rate,
            local,
            at_local: 1_000_000_000_000,
            local_rate,
            asymmetry_included: false,
            residual: None,
        };
        for later in [1_000_000_000_i128, 60_000_000_000] {
            let monotonic = at_monotonic0 + later;
            let by_monotonic = mapping.grandmaster_at(monotonic as i64);
            let by_hardware = mapping.grandmaster_at_local(hardware(monotonic));
            assert!(
                (by_monotonic - by_hardware).abs() <= 2,
                "{later}: {by_monotonic} {by_hardware}"
            );
        }
    }
}
