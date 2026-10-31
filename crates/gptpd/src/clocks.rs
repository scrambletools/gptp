//! One LocalClock over the ports' hardware clocks. Each i226 port has a
//! clock of its own, with no cross time stamping between them, so one
//! clock is the LocalClock and every port's is compared with it: a time
//! stamp stays on its port's clock for what the neighbor sees, and is put
//! on the LocalClock as the two last compared, rate and all, for the
//! relay's residence times. No clock is stepped or slewed for it: each
//! runs free, with any frequency adjustment linuxptp left taken off as
//! the daemon starts.

use std::io;
use std::time::Duration;

use gptp::relay::Stamp;
use raw_ether::clock::{Comparison, HardwareClock};

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

impl Clocks {
    /// The clocks of `ports`, with `local`'s the LocalClock.
    pub fn open(ports: &[String], local: &str) -> Result<Self, String> {
        let local_index = index_of(local)?;
        let mut freed = vec![local_index];
        let local_clock = open_free(local, local_index)?;
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
        })
    }

    pub fn local_name(&self) -> &str {
        &self.local_name
    }

    /// The LocalClock itself.
    pub fn local_clock(&self) -> &HardwareClock {
        &self.local
    }

    /// The LocalClock's time, in nanoseconds.
    pub fn now(&self) -> io::Result<i128> {
        Ok(self.local.now()?.as_nanos() as i128)
    }

    /// Compares each port's clock with the LocalClock once more.
    pub fn compare(&mut self) {
        for port in &mut self.ports {
            let Translation::Compared {
                clock,
                comparison,
                failing,
            } = &mut port.translation
            else {
                continue;
            };
            let Some((at, offset)) = read(clock, &self.local, &port.name, failing) else {
                continue;
            };
            if let Some(moved) = crate::station::moved(comparison, at, offset)
                && moved.abs() > JUMP
            {
                // Read again, to tell a clock that was set from a
                // clock misread.
                let Some((_, again)) = read(clock, &self.local, &port.name, failing) else {
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
            _ => Stamp::local(time),
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
) -> Option<(i128, i128)> {
    match clock
        .offset_from(local)
        .and_then(|offset| Ok((clock.now()?.as_nanos() as i128, offset)))
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
