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
    /// CLOCK_TAI where `follow_tai` asks, on an end station.
    pub fn open(ports: &[String], local: &str, follow_tai: bool) -> Result<Self, String> {
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
                Translation::Compared {
                    clock,
                    comparison: Comparison::default(),
                    spans: Spans::default(),
                    slow: 0,
                    failing: 0,
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
        for port in &mut self.ports {
            let Translation::Compared {
                clock,
                comparison,
                spans,
                slow,
                failing,
            } = &mut port.translation
            else {
                continue;
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
            let Some((at, offset)) = counted(read(clock, &self.local, &port.name, failing)) else {
                continue;
            };
            if let Some(moved) = crate::station::moved(comparison, at, offset)
                && moved.abs() > JUMP
            {
                // Read again, to tell a clock that was set from a
                // clock misread.
                let Some((_, again)) = counted(read(clock, &self.local, &port.name, failing))
                else {
                    continue;
                };
                if (again - offset).abs() > JUMP / 2 {
                    continue;
                }
                eprintln!(
                    "{}: its clock moved {moved} ns against the LocalClock; comparing afresh",
                    port.name
                );
                comparison.clear();
            }
            comparison.update(at, offset);
        }
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
