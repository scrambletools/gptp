//! The grandmaster's time as gptpd gives it to applications: a mapping
//! from this computer's clocks to it, since gptpd never moves a clock to
//! carry it.
//!
//! gptpd writes the mapping to `/run/gptpd/<interface>.time` at each
//! Sync and each clock comparison, as one fixed-size record checked by a
//! CRC. A reader reads the record whole, and reads it again when it
//! catches it half written, so neither side needs shared memory.

use core::fmt;

use crate::ClockIdentity;

/// Whose time the mapping carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No grandmaster: there is no network time to follow.
    None,
    /// A grandmaster elsewhere, followed through the port.
    Following,
    /// This system is the grandmaster.
    Grandmaster,
}

/// How long after gptpd last wrote it a mapping still counts.
pub const STALE_NANOS: i64 = 1_000_000_000;

/// The grandmaster's time against CLOCK_MONOTONIC and against the port's
/// hardware clock, each as a line: the grandmaster's time at one instant
/// of the clock, and its rate over the clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mapping {
    /// Changes at every discontinuity: a new grandmaster or time base, a
    /// jump of the hardware clock, a change of profile, a resume.
    pub generation: u32,
    pub state: State,
    pub grandmaster: ClockIdentity,
    /// CLOCK_MONOTONIC, in nanoseconds, when gptpd wrote it.
    pub updated: i64,
    /// The grandmaster's time `at_monotonic` at CLOCK_MONOTONIC time
    /// `monotonic`, both in nanoseconds.
    pub monotonic: i64,
    pub at_monotonic: i128,
    /// The grandmaster's nanoseconds in one of CLOCK_MONOTONIC's.
    pub monotonic_rate: f64,
    /// The same against the port's hardware clock.
    pub local: i128,
    pub at_local: i128,
    pub local_rate: f64,
    /// Whether the time includes the link's asymmetry (AVB Lite's mixed
    /// link speeds).
    pub asymmetry_included: bool,
    /// How far the last Sync's time was from the mapping, in nanoseconds:
    /// the mapping's residual, not a servo's offset.
    pub residual: Option<i64>,
}

impl Mapping {
    /// The grandmaster's time at CLOCK_MONOTONIC time `monotonic`.
    pub fn grandmaster_at(&self, monotonic: i64) -> i128 {
        along(
            self.at_monotonic,
            i128::from(monotonic) - i128::from(self.monotonic),
            self.monotonic_rate,
        )
    }

    /// CLOCK_MONOTONIC's time when the grandmaster's is `grandmaster`.
    pub fn monotonic_at(&self, grandmaster: i128) -> i64 {
        let monotonic = along(
            i128::from(self.monotonic),
            grandmaster - self.at_monotonic,
            1.0 / self.monotonic_rate,
        );
        monotonic.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }

    /// The grandmaster's time at the hardware clock's time `local`.
    pub fn grandmaster_at_local(&self, local: i128) -> i128 {
        along(self.at_local, local - self.local, self.local_rate)
    }

    /// The hardware clock's time when the grandmaster's is `grandmaster`.
    pub fn local_at(&self, grandmaster: i128) -> i128 {
        along(
            self.local,
            grandmaster - self.at_local,
            1.0 / self.local_rate,
        )
    }

    /// Whether gptpd last wrote it more than [`STALE_NANOS`] before `now`,
    /// CLOCK_MONOTONIC in nanoseconds.
    pub fn stale(&self, now: i64) -> bool {
        now.saturating_sub(self.updated) > STALE_NANOS
    }

    /// The record gptpd writes, numbered `sequence`.
    pub fn encode(&self, sequence: u32) -> [u8; RECORD_LEN] {
        let mut record = [0; RECORD_LEN];
        record[0..4].copy_from_slice(MAGIC);
        record[4..6].copy_from_slice(&VERSION.to_le_bytes());
        record[6..8].copy_from_slice(&(RECORD_LEN as u16).to_le_bytes());
        record[8..12].copy_from_slice(&sequence.to_le_bytes());
        record[12..16].copy_from_slice(&self.generation.to_le_bytes());
        record[16] = match self.state {
            State::None => 0,
            State::Following => 1,
            State::Grandmaster => 2,
        };
        record[17] = u8::from(self.asymmetry_included) | u8::from(self.residual.is_some()) << 1;
        record[20..28].copy_from_slice(&self.grandmaster.0.to_le_bytes());
        record[28..36].copy_from_slice(&self.updated.to_le_bytes());
        record[36..44].copy_from_slice(&self.monotonic.to_le_bytes());
        record[44..60].copy_from_slice(&self.at_monotonic.to_le_bytes());
        record[60..68].copy_from_slice(&self.monotonic_rate.to_le_bytes());
        record[68..84].copy_from_slice(&self.local.to_le_bytes());
        record[84..100].copy_from_slice(&self.at_local.to_le_bytes());
        record[100..108].copy_from_slice(&self.local_rate.to_le_bytes());
        record[108..116].copy_from_slice(&self.residual.unwrap_or(0).to_le_bytes());
        let crc = crc32(&record[..CRC_AT]);
        record[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        record
    }

    /// The mapping in a record and the record's sequence number.
    pub fn decode(record: &[u8]) -> Result<(Self, u32), RecordError> {
        if record.len() < 8 {
            return Err(RecordError::Short);
        }
        if &record[0..4] != MAGIC {
            return Err(RecordError::Magic);
        }
        let version = u16::from_le_bytes([record[4], record[5]]);
        if version != VERSION {
            return Err(RecordError::Version(version));
        }
        let length = u16::from_le_bytes([record[6], record[7]]);
        if usize::from(length) != RECORD_LEN {
            return Err(RecordError::Length(length));
        }
        if record.len() < RECORD_LEN {
            return Err(RecordError::Short);
        }
        let stored = u32::from_le_bytes(array(&record[CRC_AT..RECORD_LEN]));
        if stored != crc32(&record[..CRC_AT]) {
            return Err(RecordError::Checksum);
        }
        let state = match record[16] {
            0 => State::None,
            1 => State::Following,
            2 => State::Grandmaster,
            other => return Err(RecordError::State(other)),
        };
        let mapping = Self {
            generation: u32::from_le_bytes(array(&record[12..16])),
            state,
            grandmaster: ClockIdentity(u64::from_le_bytes(array(&record[20..28]))),
            updated: i64::from_le_bytes(array(&record[28..36])),
            monotonic: i64::from_le_bytes(array(&record[36..44])),
            at_monotonic: i128::from_le_bytes(array(&record[44..60])),
            monotonic_rate: f64::from_le_bytes(array(&record[60..68])),
            local: i128::from_le_bytes(array(&record[68..84])),
            at_local: i128::from_le_bytes(array(&record[84..100])),
            local_rate: f64::from_le_bytes(array(&record[100..108])),
            asymmetry_included: record[17] & 1 != 0,
            residual: (record[17] & 2 != 0).then(|| i64::from_le_bytes(array(&record[108..116]))),
        };
        Ok((mapping, u32::from_le_bytes(array(&record[8..12]))))
    }
}

/// The octets of a record.
pub const RECORD_LEN: usize = 120;
const CRC_AT: usize = RECORD_LEN - 4;
const MAGIC: &[u8; 4] = b"GPTM";
const VERSION: u16 = 1;

/// A record that is not one this version reads, or not whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordError {
    Short,
    Magic,
    Version(u16),
    Length(u16),
    /// Written while it was read, most likely: read it again.
    Checksum,
    State(u8),
}

impl fmt::Display for RecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Short => formatter.write_str("the record is short"),
            Self::Magic => formatter.write_str("not a gptpd time record"),
            Self::Version(version) => write!(formatter, "record version {version} is unknown"),
            Self::Length(length) => write!(formatter, "a record of {length} octets is unknown"),
            Self::Checksum => formatter.write_str("the record's CRC does not match"),
            Self::State(state) => write!(formatter, "state {state} is unknown"),
        }
    }
}

/// `at` plus `elapsed` scaled by `rate`, with the rate's part apart so a
/// long span keeps its precision.
fn along(at: i128, elapsed: i128, rate: f64) -> i128 {
    let part = elapsed as f64 * (rate - 1.0);
    // Rounded to the nearest; `as` truncates, and core has no round.
    at + elapsed + if part < 0.0 { part - 0.5 } else { part + 0.5 } as i128
}

fn array<const N: usize>(octets: &[u8]) -> [u8; N] {
    let mut array = [0; N];
    array.copy_from_slice(octets);
    array
}

/// CRC-32 as Ethernet's frame check sequence computes it.
fn crc32(octets: &[u8]) -> u32 {
    let mut crc = !0u32;
    for octet in octets {
        crc ^= u32::from(*octet);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                crc >> 1 ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(all(feature = "std", unix))]
pub use reader::{Reader, path};

#[cfg(all(feature = "std", unix))]
mod reader {
    use std::format;
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::FileExt;
    use std::path::PathBuf;
    use std::string::ToString;

    use super::{Mapping, RECORD_LEN, RecordError};

    /// How often a read is tried again on a record caught half written.
    const TRIES: usize = 4;

    /// Where gptpd writes the mapping for `interface`.
    pub fn path(interface: &str) -> io::Result<PathBuf> {
        if interface.is_empty()
            || interface.len() > 15
            || interface.contains(['/', '\0'])
            || interface.chars().any(char::is_whitespace)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{interface:?} is not an interface name"),
            ));
        }
        Ok(PathBuf::from(format!("/run/gptpd/{interface}.time")))
    }

    /// Reads gptpd's mapping for one interface, opening its file afresh
    /// whenever it is missing or stale, as after gptpd restarts.
    pub struct Reader {
        path: PathBuf,
        file: Option<File>,
    }

    impl Reader {
        pub fn open(interface: &str) -> io::Result<Self> {
            Ok(Self::at(path(interface)?))
        }

        /// A reader of the record at `path`.
        pub fn at(path: PathBuf) -> Self {
            Self { path, file: None }
        }

        /// The mapping, or none where gptpd writes none or last wrote it
        /// more than a second before `now`, CLOCK_MONOTONIC in nanoseconds.
        pub fn read(&mut self, now: i64) -> io::Result<Option<Mapping>> {
            for _ in 0..TRIES {
                let file = match &self.file {
                    Some(file) => file,
                    None => match File::open(&self.path) {
                        Ok(file) => self.file.insert(file),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                        Err(error) => return Err(error),
                    },
                };
                let mut record = [0; RECORD_LEN];
                let read = file.read_at(&mut record, 0)?;
                match Mapping::decode(&record[..read]) {
                    Ok((mapping, _)) if mapping.stale(now) => {
                        self.file = None;
                        return Ok(None);
                    }
                    Ok((mapping, _)) => return Ok(Some(mapping)),
                    Err(RecordError::Checksum | RecordError::Short) => {}
                    Err(error) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            error.to_string(),
                        ));
                    }
                }
            }
            // Torn every time, or not yet written whole: none for now.
            self.file = None;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests;
