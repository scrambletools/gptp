//! One LocalClock over the ports' hardware clocks. Each i226 port has a
//! clock of its own, with no cross time stamping between them, so one
//! clock is the LocalClock and every port's is compared with it: a time
//! stamp stays on its port's clock for what the neighbor sees, and is put
//! on the LocalClock as the two last compared, rate and all, for the
//! relay's residence times. No clock is stepped or slewed for it: each
//! runs free, with any frequency adjustment linuxptp left taken off as
//! the daemon starts.
//!
//! An end station's clock may follow CLOCK_TAI instead (steer.rs): then
//! its times go through the steering to the free clock gptpd's model runs
//! on, and the record's line back to the hardware clock.

use std::io;
use std::time::Duration;

use gptp::relay::Stamp;
use gptp::time::Mapping;
use raw_ether::clock::{Comparison, HardwareClock, Reading, SystemClock};

use crate::station::{SLOW_TIGHT, Spans};
use crate::steer::{self, Change, Steering};

/// How long a reading against CLOCK_TAI may take, in ns, and still steer.
const SLOW_STEERING: u64 = 20_000;

/// How far a comparison may stand from what the last ones foretold before
/// the port's clock is taken to have been set, as its driver does when it
/// resets the card: well beyond the few tens of ns comparisons stray.
const JUMP: i128 = 1_000;

/// How far a coarse clock's reading may stand from the line before it is
/// left out. An 802.11 radio's clock reads in whole microseconds, through
/// a read some 12 us long, so its readings stray microseconds.
const COARSE_OFF_LINE: i128 = 4_000;

/// How many readings in a row off the line restart it: a second of them.
/// A reading gone astray stands alone; a run of them means the line went
/// astray, its clock set or its slope taken from too few readings.
const COARSE_SET_AFTER: u32 = 10;

/// How far back a coarse clock's comparisons count: over 2 s its line
/// moves up to 1.6 us as each reading comes, over 16 s under 0.3 us.
const COARSE_WINDOW: Duration = Duration::from_secs(16);

/// What a radio's clock steps by when its driver counts a wrap of the
/// card's 32-bit microsecond counter that never came, as iwlwifi does for
/// a time stamp handed over more than 5 ms after a later reading.
const COUNTER_WRAP: i128 = (1 << 32) * 1_000;

/// How near whole wraps a step must come to be taken as them: beyond a
/// slow reading's error, and nothing else steps a clock by within this of
/// 71 minutes.
const WRAP_SLACK: f64 = 20_000.0;

/// The whole counter wraps, in ns, by which a reading `offset` at `at`
/// stands off `comparison`'s line, if it stands off by any.
fn wrapped(comparison: &Comparison, at: i128, offset: i128) -> Option<i128> {
    let moved = crate::station::moved(comparison, at, offset)?;
    let rate = comparison.rate()?;
    // A step moves the time read and the offset alike, and the line
    // foretells its rate times the step more at the later time.
    let by = moved as f64 / (1.0 - rate);
    let wraps = (by / COUNTER_WRAP as f64).round();
    let whole = (wraps as i128).checked_mul(COUNTER_WRAP)?;
    (whole != 0 && (by - whole as f64).abs() <= WRAP_SLACK).then_some(whole)
}

/// How far a radio's time stamp may stand from its clock's last reading
/// kept and still be on the clock as read: a stamp is a moment old, and
/// whole counter wraps put one 71 minutes on.
const STAMP_REACH: i128 = 10_000_000_000;

/// Whether any of `times` stands beyond [`STAMP_REACH`] from the clock's
/// last reading in `comparison`.
fn beyond_reach(comparison: &Comparison, times: &[i128]) -> bool {
    comparison
        .last_at()
        .is_some_and(|last| times.iter().any(|time| (time - last).abs() > STAMP_REACH))
}

/// What a coarse clock's reading says against the line.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Take,
    LeaveOut,
    /// The clock was set: compare afresh from this reading.
    Set,
}

/// A coarse clock's readings off the line, to tell a reading gone astray
/// from a line gone astray.
#[derive(Debug, Default)]
struct Outlying {
    /// Readings off the line in a row.
    in_a_row: u32,
    /// Readings left out so far.
    left_out: u64,
}

impl Outlying {
    /// The verdict on a reading `moved` off the line, none while the line
    /// has no rate yet.
    fn judge(&mut self, moved: Option<i128>) -> Verdict {
        if moved.is_none_or(|moved| moved.abs() <= COARSE_OFF_LINE) {
            self.in_a_row = 0;
            return Verdict::Take;
        }
        self.in_a_row += 1;
        if self.in_a_row >= COARSE_SET_AFTER {
            self.in_a_row = 0;
            return Verdict::Set;
        }
        self.left_out += 1;
        Verdict::LeaveOut
    }
}

/// A port's clock, as put on the LocalClock.
enum Translation {
    /// The port's clock is the LocalClock.
    Same,
    Compared {
        clock: HardwareClock,
        comparison: Comparison,
        spans: Spans,
        /// Readings left out as slow, so far.
        slow: u64,
        /// Comparisons that failed in a row, so as to say so once.
        failing: u32,
        /// For a coarse clock, an 802.11 radio's, its readings off the
        /// line.
        outlying: Option<Outlying>,
        /// Time stamps left out as far from the clock's readings.
        stale: u64,
    },
}

struct PortClock {
    name: String,
    translation: Translation,
}

pub struct Clocks {
    /// The interface whose clock is the LocalClock.
    local_name: String,
    local: HardwareClock,
    ports: Vec<PortClock>,
    /// Where an end station's clock follows CLOCK_TAI.
    steering: Option<Steering>,
    following: bool,
    /// How far the clock was set on as gptpd started, which an earlier
    /// gptpd's record does not know.
    set_at_start: i128,
}

/// What following CLOCK_TAI came to, worth saying.
#[derive(Debug)]
pub enum Followed {
    /// The clock was set this many ns on.
    Set(i128),
    /// It came within reach of CLOCK_TAI, or left it, standing this many ns
    /// from it.
    Began(i128),
    Lost(i128),
    Failed(String),
}

/// An interface's hardware clock index.
fn index_of(interface: &str) -> Result<u32, String> {
    raw_ether::interfaces()
        .into_iter()
        .find(|each| each.name == interface)
        .and_then(|each| each.hardware_clock)
        .ok_or_else(|| format!("{interface} has no PTP hardware clock"))
}

/// Opens `interface`'s clock to adjust, and lets it run free.
fn open_free(interface: &str, index: u32) -> Result<HardwareClock, String> {
    let fail = |error: io::Error| format!("{interface}'s clock: {error}");
    let clock = HardwareClock::open_adjustable(index).map_err(fail)?;
    let adjusted = clock.frequency_adjustment().map_err(fail)?;
    if adjusted != 0.0 {
        clock.set_frequency_adjustment(0.0).map_err(fail)?;
        eprintln!(
            "{interface}: its clock was adjusted {:+.3} ppm, now runs free",
            adjusted / 1000.0
        );
    }
    Ok(clock)
}

/// Opens `interface`'s clock to steer near CLOCK_TAI, keeping the
/// adjustment it has, from an earlier gptpd most likely, and setting it to
/// CLOCK_TAI where it stands too far off to slew.
/// Also how far it was set on, if at all.
fn open_steered(interface: &str, index: u32) -> Result<(HardwareClock, Steering, i128), String> {
    let fail = |error: io::Error| format!("{interface}'s clock: {error}");
    let clock = HardwareClock::open_adjustable(index).map_err(fail)?;
    let adjust = clock.frequency_adjustment().map_err(fail)?;
    let offset = tai_offset(&clock).map_err(fail)?;
    let mut set = 0;
    if offset.abs() > steer::SET_BEYOND {
        clock.step(-offset).map_err(fail)?;
        set = -offset;
        eprintln!("{interface}: its clock set {set} ns on, to CLOCK_TAI");
    }
    let now = clock.now().map_err(fail)?.as_nanos() as i128;
    Ok((clock, Steering::new(now, adjust), set))
}

/// How far `clock` stands ahead of CLOCK_TAI, and how long the reading
/// took.
fn tai_reading(clock: &HardwareClock) -> io::Result<Reading> {
    clock
        .reading_from_system_clock(SystemClock::Tai)
        .or_else(|_| clock.reading_against(SystemClock::Tai))
}

fn tai_offset(clock: &HardwareClock) -> io::Result<i128> {
    tai_reading(clock).map(|reading| reading.offset)
}

/// `clock`'s time now, or where it cannot be read, CLOCK_TAI's and
/// `offset`, how far the clock stood from it at the last reading: off by
/// a few us at most, which the steering's few ppm make well under a ns.
/// Once the clock was changed its bookkeeping must go on, read or not.
fn hardware_now(clock: &HardwareClock, offset: i128) -> i128 {
    clock.now().map_or_else(
        |_| raw_ether::clock::system_now(SystemClock::Tai).as_nanos() as i128 + offset,
        |now| now.as_nanos() as i128,
    )
}

impl Clocks {
    /// The clocks of `ports`, with `local`'s the LocalClock, which follows
    /// CLOCK_TAI where `follow_tai` asks, on an end station. The clocks of
    /// `coarse` ports, 802.11 radios', read in whole microseconds.
    pub fn open(
        ports: &[String],
        local: &str,
        follow_tai: bool,
        coarse: &[String],
    ) -> Result<Self, String> {
        let local_index = index_of(local)?;
        let mut freed = vec![local_index];
        let (local_clock, steering, set_at_start) = if follow_tai && ports.len() == 1 {
            let (clock, steering, set) = open_steered(local, local_index)?;
            (clock, Some(steering), set)
        } else {
            (open_free(local, local_index)?, None, 0)
        };
        let mut clocks = Vec::new();
        for name in ports {
            let index = index_of(name)?;
            let translation = if index == local_index {
                Translation::Same
            } else {
                let clock = if freed.contains(&index) {
                    HardwareClock::open(index)
                        .map_err(|error| format!("{name}'s clock: {error}"))?
                } else {
                    freed.push(index);
                    open_free(name, index)?
                };
                let coarse = coarse.contains(name);
                Translation::Compared {
                    clock,
                    comparison: if coarse {
                        Comparison::keeping(COARSE_WINDOW)
                    } else {
                        Comparison::default()
                    },
                    spans: Spans::default(),
                    slow: 0,
                    failing: 0,
                    outlying: coarse.then(Outlying::default),
                    stale: 0,
                }
            };
            clocks.push(PortClock {
                name: name.clone(),
                translation,
            });
        }
        Ok(Self {
            local_name: local.to_owned(),
            local: local_clock,
            ports: clocks,
            steering,
            following: false,
            set_at_start,
        })
    }

    pub fn local_name(&self) -> &str {
        &self.local_name
    }

    /// The LocalClock itself.
    pub fn local_clock(&self) -> &HardwareClock {
        &self.local
    }

    /// The LocalClock's time, in nanoseconds, on the free clock where it
    /// is steered.
    pub fn now(&self) -> io::Result<i128> {
        Ok(self.free(self.local.now()?.as_nanos() as i128))
    }

    /// The free clock's time at the LocalClock's `hardware`.
    pub fn free(&self, hardware: i128) -> i128 {
        self.steering
            .as_ref()
            .map_or(hardware, |steering| steering.free(hardware))
    }

    /// The LocalClock's steering, where it follows CLOCK_TAI.
    pub fn steering(&self) -> Option<&Steering> {
        self.steering.as_ref()
    }

    /// Steers the LocalClock after a comparison, the free clock running
    /// `drift` faster than CLOCK_MONOTONIC, where known; left alone while
    /// this system is grandmaster.
    pub fn follow_tai(&mut self, drift: Option<f64>, grandmaster: bool) -> Option<Followed> {
        let steering = self.steering.as_mut()?;
        let reading = match tai_reading(&self.local) {
            Ok(reading) => reading,
            Err(error) => {
                return Some(Followed::Failed(format!(
                    "reading against CLOCK_TAI: {error}"
                )));
            }
        };
        if reading.span > SLOW_STEERING {
            return None;
        }
        let said = match steering.follow(reading.offset, drift, grandmaster) {
            Change::None => None,
            Change::Adjust(adjust) => {
                if let Err(error) = self.local.set_frequency_adjustment(adjust) {
                    return Some(Followed::Failed(format!(
                        "adjusting its frequency: {error}"
                    )));
                }
                // The change took from about now: what it moved before the
                // reading below is a few us at the change's few ppb, well
                // under a ns.
                steering.changed(hardware_now(&self.local, reading.offset), adjust);
                None
            }
            Change::Set(by) => {
                if let Err(error) = self.local.step(by) {
                    return Some(Followed::Failed(format!("setting it: {error}")));
                }
                let at = hardware_now(&self.local, reading.offset + by);
                steering.restart(at, steering.adjust());
                Some(Followed::Set(by))
            }
        };
        let following = steering.following();
        if following.is_some() != self.following {
            self.following = following.is_some();
            return Some(match following {
                Some(offset) => Followed::Began(offset),
                None => Followed::Lost(reading.offset),
            });
        }
        said
    }

    /// After the LocalClock was set by something else, as its driver does
    /// when it resets the card: puts the adjustment back, which the reset
    /// may have dropped, and starts the free clock afresh from it.
    pub fn steering_reset(&mut self) {
        let Some(steering) = &mut self.steering else {
            return;
        };
        let _ = self.local.set_frequency_adjustment(steering.adjust());
        let offset = tai_offset(&self.local).unwrap_or(0);
        steering.restart(hardware_now(&self.local, offset), steering.adjust());
        self.following = false;
    }

    /// `mapping` as the record carries it: its line on the hardware clock
    /// the card launches frames by, not the free one, and how far that
    /// stands from CLOCK_TAI where it follows it.
    pub fn recorded(&self, mapping: Mapping) -> Mapping {
        let Some(steering) = &self.steering else {
            return mapping;
        };
        let (local, at_local, local_rate) =
            steering.hardware_line(mapping.local, mapping.at_local, mapping.local_rate);
        Mapping {
            local,
            at_local,
            local_rate,
            tai: steering
                .following()
                .map(|offset| offset.clamp(i16::MIN.into(), i16::MAX.into()) as i16),
            ..mapping
        }
    }

    /// A line of the grandmaster's time on the hardware clock, as an earlier
    /// gptpd recorded it, on the free clock: its point and rate.
    pub fn free_line(&self, local: i128, rate: f64) -> (i128, f64) {
        // A clock set on as gptpd started reads that much more at the same
        // instant.
        let local = local + self.set_at_start;
        match &self.steering {
            Some(steering) => {
                let free = steering.free(local);
                (free, rate * steering.rate_at(free))
            }
            None => (local, rate),
        }
    }

    /// Compares each port's clock with the LocalClock once more.
    pub fn compare(&mut self) {
        for index in 0..self.ports.len() {
            self.compare_port(index);
        }
    }

    /// Whether `times`, from port `index`'s radio, stand on its clock as
    /// last read. Where one stands far from the last reading, as after
    /// the driver put whole counter wraps on the clock, the port is
    /// compared at once, so the line steps with the clock before the
    /// times are put on the LocalClock; times still far, as from both
    /// sides of a step, are not to be used.
    pub fn fresh(&mut self, index: usize, times: &[i128]) -> bool {
        let far = |clocks: &Self| match clocks.ports.get(index).map(|port| &port.translation) {
            Some(Translation::Compared {
                comparison,
                outlying: Some(_),
                ..
            }) => beyond_reach(comparison, times),
            _ => false,
        };
        if !far(self) {
            return true;
        }
        self.compare_port(index);
        if !far(self) {
            return true;
        }
        if let Some(PortClock {
            name,
            translation: Translation::Compared { stale, .. },
        }) = self.ports.get_mut(index)
        {
            *stale += 1;
            if *stale <= 10 || crate::telling(*stale) {
                eprintln!(
                    "{name}: a time stamp far from its clock's readings left out ({stale} so far)"
                );
            }
        }
        false
    }

    /// Compares port `index`'s clock with the LocalClock once more.
    fn compare_port(&mut self, index: usize) {
        let local = &self.local;
        let Some(port) = self.ports.get_mut(index) else {
            return;
        };
        let Translation::Compared {
            clock,
            comparison,
            spans,
            slow,
            failing,
            outlying,
            ..
        } = &mut port.translation
        else {
            return;
        };
        // A slow reading counts towards neither the line nor a jump.
        let mut counted = |read: Option<(i128, Reading)>| {
            let (at, reading) = read?;
            if spans.counts(reading, SLOW_TIGHT) {
                return Some((at, reading.offset));
            }
            *slow += 1;
            if *slow <= 10 || crate::telling(*slow) {
                eprintln!(
                    "{}: a slow reading of its clock left out, {} ns ({} so far)",
                    port.name, reading.span, slow
                );
            }
            None
        };
        let taken = read(clock, local, &port.name, failing);
        // Whole counter wraps step the line at once, a slow reading
        // being near enough to tell them.
        if outlying.is_some()
            && let Some((at, reading)) = taken
            && let Some(by) = wrapped(comparison, at, reading.offset)
        {
            comparison.step(by);
            eprintln!(
                "{}: its clock stepped {by} ns, whole wraps of its counter; the line steps with it",
                port.name
            );
        }
        let Some((at, offset)) = counted(taken) else {
            return;
        };
        let moved = crate::station::moved(comparison, at, offset);
        if let Some(outlying) = outlying {
            match outlying.judge(moved) {
                Verdict::Take => {}
                Verdict::LeaveOut => {
                    if outlying.left_out <= 10 || crate::telling(outlying.left_out) {
                        eprintln!(
                            "{}: a reading of its clock {} ns off the line left out ({} so far)",
                            port.name,
                            moved.unwrap_or_default(),
                            outlying.left_out
                        );
                    }
                    return;
                }
                Verdict::Set => {
                    eprintln!(
                        "{}: its clock stood off the line for 1 s, {} ns now; comparing afresh",
                        port.name,
                        moved.unwrap_or_default()
                    );
                    comparison.clear();
                }
            }
        } else if let Some(moved) = moved
            && moved.abs() > JUMP
        {
            // Read again, to tell a clock that was set from a
            // clock misread.
            let Some((_, again)) = counted(read(clock, local, &port.name, failing)) else {
                return;
            };
            if (again - offset).abs() > JUMP / 2 {
                return;
            }
            eprintln!(
                "{}: its clock moved {moved} ns against the LocalClock; comparing afresh",
                port.name
            );
            comparison.clear();
        }
        comparison.update(at, offset);
    }

    /// A time on port `index`'s clock, and on the LocalClock.
    pub fn stamp(&self, index: usize, time: Duration) -> Stamp {
        let time = time.as_nanos() as i128;
        match self.ports.get(index).map(|port| &port.translation) {
            Some(Translation::Compared { comparison, .. }) => Stamp {
                port: time,
                local: time - comparison.offset_at(time),
                rate: rate_over_local(comparison),
            },
            _ => Stamp::local(self.free(time)),
        }
    }

    /// How much faster port `index`'s clock runs than the LocalClock, as
    /// a fraction, where it is not the LocalClock and has been compared
    /// enough to tell.
    pub fn rate(&self, index: usize) -> Option<f64> {
        match &self.ports.get(index)?.translation {
            Translation::Same => None,
            Translation::Compared { comparison, .. } => {
                comparison.rate().map(|_| rate_over_local(comparison) - 1.0)
            }
        }
    }
}

/// The port clock's rate over the LocalClock's. The comparison follows
/// the port clock less the LocalClock against the port clock.
fn rate_over_local(comparison: &Comparison) -> f64 {
    1.0 / (1.0 - comparison.rate().unwrap_or(0.0))
}

/// When `clock` read, and how far ahead of `local` it was, or `None`
/// when it could not be read, said once while it fails.
fn read(
    clock: &HardwareClock,
    local: &HardwareClock,
    name: &str,
    failing: &mut u32,
) -> Option<(i128, Reading)> {
    match clock
        .reading_from(local)
        .and_then(|reading| Ok((clock.now()?.as_nanos() as i128, reading)))
    {
        Ok(read) => {
            *failing = 0;
            Some(read)
        }
        Err(error) => {
            if *failing == 0 {
                eprintln!("{name}: cannot compare its clock with the LocalClock: {error}");
            }
            *failing += 1;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coarse_clock_leaves_out_a_stray_reading_and_takes_the_next() {
        let mut outlying = Outlying::default();
        assert_eq!(outlying.judge(None), Verdict::Take);
        assert_eq!(outlying.judge(Some(1_200)), Verdict::Take);
        assert_eq!(outlying.judge(Some(-3_900)), Verdict::Take);
        assert_eq!(outlying.judge(Some(32_393)), Verdict::LeaveOut);
        assert_eq!(outlying.judge(Some(300)), Verdict::Take);
        assert_eq!(outlying.left_out, 1);
        assert_eq!(outlying.in_a_row, 0);
    }

    #[test]
    fn a_second_off_the_line_restarts_it_however_the_readings_stray() {
        // 17:21:23 UTC 11-22: a line with too steep a slope, each reading
        // further off than the last, never ten alike.
        let mut outlying = Outlying::default();
        let off = [
            -5_228, -6_959, -11_383, -12_420, -16_211, -18_016, -21_435, -26_042, -28_319,
        ];
        for moved in off {
            assert_eq!(outlying.judge(Some(moved)), Verdict::LeaveOut);
        }
        assert_eq!(outlying.judge(Some(-28_988)), Verdict::Set);
        assert_eq!(outlying.in_a_row, 0);
    }

    /// A radio's line over 16 s at 7.7 ppm, and the next reading's time
    /// and offset on it.
    fn radio_line() -> (Comparison, i128, i128) {
        let mut comparison = Comparison::keeping(COARSE_WINDOW);
        for step in 0..160i128 {
            comparison.update(step * 100_000_000, 40_000 + step * 774);
        }
        let at = 160 * 100_000_000;
        (comparison, at, 40_000 + 160 * 774)
    }

    #[test]
    fn a_counter_wrap_steps_the_line_with_it() {
        // 00:07:47 UTC 11-23: a time stamp handed over late, and the
        // radio's clock 2^32 us on from then.
        let (mut comparison, at, offset) = radio_line();
        let rate = comparison.rate();
        let at = at + COUNTER_WRAP;
        let offset = offset + COUNTER_WRAP;
        assert_eq!(wrapped(&comparison, at, offset), Some(COUNTER_WRAP));
        // As slow a reading as the radio's.
        assert_eq!(wrapped(&comparison, at, offset + 6_300), Some(COUNTER_WRAP));
        comparison.step(COUNTER_WRAP);
        assert_eq!(comparison.rate(), rate);
        let moved = crate::station::moved(&comparison, at, offset);
        assert!(moved.is_some_and(|moved| moved.abs() <= 1), "{moved:?}");
        assert_eq!(Outlying::default().judge(moved), Verdict::Take);
    }

    #[test]
    fn a_wrap_back_steps_the_line_back() {
        let (comparison, at, offset) = radio_line();
        assert_eq!(
            wrapped(&comparison, at - COUNTER_WRAP, offset - COUNTER_WRAP),
            Some(-COUNTER_WRAP)
        );
    }

    #[test]
    fn steps_other_than_whole_wraps_are_left_to_the_line() {
        let (comparison, at, offset) = radio_line();
        for by in [
            0,
            5_000,
            1_000_000_000,
            COUNTER_WRAP / 2,
            COUNTER_WRAP + 100_000,
            COUNTER_WRAP - 100_000,
        ] {
            assert_eq!(wrapped(&comparison, at + by, offset + by), None, "{by}");
        }
        let mut fresh = Comparison::keeping(COARSE_WINDOW);
        fresh.update(0, 0);
        assert_eq!(wrapped(&fresh, COUNTER_WRAP, COUNTER_WRAP), None);
    }

    #[test]
    fn a_stamp_from_after_a_wrap_is_beyond_the_line_until_it_steps() {
        let (mut comparison, at, offset) = radio_line();
        let stamp = at + COUNTER_WRAP - 40_000_000;
        assert!(!beyond_reach(&comparison, &[at - 50_000_000, at]));
        assert!(beyond_reach(&comparison, &[at, stamp]));
        // The next reading tells the wrap, and the line steps with it.
        let by = wrapped(&comparison, at + COUNTER_WRAP, offset + COUNTER_WRAP);
        comparison.step(by.expect("a wrap"));
        assert!(!beyond_reach(&comparison, &[stamp, stamp + 20_000]));
        // A stamp from before the step, beside one from after it.
        assert!(beyond_reach(&comparison, &[at - 30_000_000, stamp]));
        assert!(!beyond_reach(&Comparison::default(), &[stamp]));
    }

    #[test]
    fn readings_off_the_line_now_and_then_never_restart_it() {
        let mut outlying = Outlying::default();
        for step in 0..10 * COARSE_SET_AFTER {
            let moved = if step % 3 == 0 { 300 } else { -5_000 };
            assert_ne!(outlying.judge(Some(moved)), Verdict::Set);
        }
    }
}
