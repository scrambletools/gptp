//! What gptpd tells about an interface, and the commands it takes, on
//! `/run/gptpd/<interface>.sock`.
//!
//! Each snapshot is lines of `key value`, a blank line ending it. gptpd
//! sends one as a client connects, at every change and every 2 s, and
//! answers each command with a reply block in the same stream. Readers
//! ignore keys they do not know, so fields can be added.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
use core::str::FromStr;

use crate::relay::Role;
use crate::{ClockIdentity, PortIdentity};

/// The format's version, the first line of every snapshot.
pub const VERSION: u32 = 1;

/// The PTP profile a port runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    #[default]
    Gptp,
    AvbLite,
}

/// Why a port runs the AVB Lite profile, as AVB Lite's profile 2.2 names
/// the reasons, and atdecc's FallbackReason by the same names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fallback {
    /// It runs gPTP.
    #[default]
    None,
    EndpointTlv,
    PdelayUnanswered,
    MultipleResponders,
    /// The operator or a controller set it.
    Configured,
}

/// The grandmaster and its quality, as its Announce carries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grandmaster {
    pub identity: ClockIdentity,
    pub priority1: u8,
    pub clock_class: u8,
    pub clock_accuracy: u8,
    pub offset_scaled_log_variance: u16,
    pub priority2: u8,
    pub steps_removed: u16,
    /// This system is the grandmaster.
    pub is_us: bool,
}

/// One port.
#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub name: String,
    pub up: bool,
    pub as_capable: bool,
    pub role: Role,
    /// The link's mean delay, in nanoseconds, once measured.
    pub link_delay: Option<i64>,
    pub neighbor: Option<PortIdentity>,
    /// The neighbor's rate over this port's clock.
    pub neighbor_rate_ratio: Option<f64>,
}

/// Counts since gptpd started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    pub grandmaster_changes: u64,
    pub link_ups: u64,
    pub link_downs: u64,
    pub sync_timeouts: u64,
}

/// A snapshot of one gptpd instance.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// This system's clock identity.
    pub clock: ClockIdentity,
    pub domain: u8,
    /// This system's priority1; 255 when it is never grandmaster.
    pub priority1: u8,
    pub profile: Profile,
    pub fallback: Fallback,
    pub grandmaster: Option<Grandmaster>,
    /// The clocks from the grandmaster to this system, both included.
    pub path: Vec<ClockIdentity>,
    /// The time mapping's generation, as in the time file.
    pub generation: u32,
    /// The time mapping's residual at the last Sync, in nanoseconds.
    pub residual: Option<i64>,
    pub ports: Vec<Port>,
    pub counters: Counters,
}

/// What gptpd sends: a snapshot, or the reply to a command.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Status(Status),
    Reply(Result<(), String>),
}

/// A command gptpd takes, one line each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Sets this system's priority1, 255 for never grandmaster.
    Priority1(u8),
}

/// A block or a command that does not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Status {
    /// The snapshot as gptpd sends it, its ending blank line included.
    pub fn encode(&self, out: &mut impl fmt::Write) -> fmt::Result {
        writeln!(out, "version {VERSION}")?;
        writeln!(out, "clock {}", self.clock)?;
        writeln!(out, "domain {}", self.domain)?;
        writeln!(out, "priority1 {}", self.priority1)?;
        writeln!(out, "profile {}", self.profile)?;
        writeln!(out, "fallback {}", self.fallback)?;
        if let Some(grandmaster) = &self.grandmaster {
            writeln!(out, "grandmaster {}", grandmaster.identity)?;
            writeln!(out, "grandmaster.priority1 {}", grandmaster.priority1)?;
            writeln!(out, "grandmaster.clock_class {}", grandmaster.clock_class)?;
            writeln!(
                out,
                "grandmaster.clock_accuracy {}",
                grandmaster.clock_accuracy
            )?;
            writeln!(
                out,
                "grandmaster.offset_scaled_log_variance {}",
                grandmaster.offset_scaled_log_variance
            )?;
            writeln!(out, "grandmaster.priority2 {}", grandmaster.priority2)?;
            writeln!(
                out,
                "grandmaster.steps_removed {}",
                grandmaster.steps_removed
            )?;
            writeln!(out, "grandmaster.is_us {}", grandmaster.is_us)?;
        }
        if !self.path.is_empty() {
            out.write_str("path")?;
            for clock in &self.path {
                write!(out, " {clock}")?;
            }
            out.write_str("\n")?;
        }
        writeln!(out, "generation {}", self.generation)?;
        if let Some(residual) = self.residual {
            writeln!(out, "residual {residual}")?;
        }
        for port in &self.ports {
            let name = &port.name;
            writeln!(out, "port {name}")?;
            writeln!(out, "port.up {name} {}", port.up)?;
            writeln!(out, "port.as_capable {name} {}", port.as_capable)?;
            writeln!(out, "port.role {name} {}", RoleName(port.role))?;
            if let Some(delay) = port.link_delay {
                writeln!(out, "port.link_delay {name} {delay}")?;
            }
            if let Some(neighbor) = port.neighbor {
                writeln!(out, "port.neighbor {name} {neighbor}")?;
            }
            if let Some(ratio) = port.neighbor_rate_ratio {
                writeln!(out, "port.neighbor_rate_ratio {name} {ratio}")?;
            }
        }
        let counters = &self.counters;
        writeln!(
            out,
            "counters.grandmaster_changes {}",
            counters.grandmaster_changes
        )?;
        writeln!(out, "counters.link_ups {}", counters.link_ups)?;
        writeln!(out, "counters.link_downs {}", counters.link_downs)?;
        writeln!(out, "counters.sync_timeouts {}", counters.sync_timeouts)?;
        out.write_str("\n")
    }

    fn parse(lines: &str) -> Result<Self, ParseError> {
        let mut clock = None;
        let mut status = Status {
            clock: ClockIdentity::default(),
            domain: 0,
            priority1: 255,
            profile: Profile::default(),
            fallback: Fallback::default(),
            grandmaster: None,
            path: Vec::new(),
            generation: 0,
            residual: None,
            ports: Vec::new(),
            counters: Counters::default(),
        };
        let mut grandmaster: Option<Grandmaster> = None;
        for line in lines.lines().filter(|line| !line.is_empty()) {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "version" => {}
                "clock" => clock = Some(parsed(key, value)?),
                "domain" => status.domain = parsed(key, value)?,
                "priority1" => status.priority1 = parsed(key, value)?,
                "profile" => status.profile = parsed(key, value)?,
                "fallback" => status.fallback = parsed(key, value)?,
                "grandmaster" => {
                    grandmaster = Some(Grandmaster {
                        identity: parsed(key, value)?,
                        priority1: 255,
                        clock_class: 255,
                        clock_accuracy: 0xfe,
                        offset_scaled_log_variance: 0xffff,
                        priority2: 255,
                        steps_removed: 0,
                        is_us: false,
                    });
                }
                "path" => {
                    status.path = value
                        .split_whitespace()
                        .map(|clock| parsed(key, clock))
                        .collect::<Result<_, _>>()?;
                }
                "generation" => status.generation = parsed(key, value)?,
                "residual" => status.residual = Some(parsed(key, value)?),
                "port" => status.ports.push(Port {
                    name: value.to_string(),
                    up: false,
                    as_capable: false,
                    role: Role::Disabled,
                    link_delay: None,
                    neighbor: None,
                    neighbor_rate_ratio: None,
                }),
                _ => {
                    if let Some(field) = key.strip_prefix("grandmaster.") {
                        if let Some(grandmaster) = &mut grandmaster {
                            set_grandmaster(grandmaster, key, field, value)?;
                        }
                    } else if let Some(field) = key.strip_prefix("port.") {
                        let (name, value) = value.split_once(' ').unwrap_or((value, ""));
                        if let Some(port) = status.ports.iter_mut().find(|port| port.name == name) {
                            set_port(port, key, field, value)?;
                        }
                    } else if let Some(field) = key.strip_prefix("counters.") {
                        let counters = &mut status.counters;
                        match field {
                            "grandmaster_changes" => {
                                counters.grandmaster_changes = parsed(key, value)?;
                            }
                            "link_ups" => counters.link_ups = parsed(key, value)?,
                            "link_downs" => counters.link_downs = parsed(key, value)?,
                            "sync_timeouts" => counters.sync_timeouts = parsed(key, value)?,
                            _ => {}
                        }
                    }
                }
            }
        }
        status.clock = clock.ok_or_else(|| ParseError("a snapshot without clock".to_string()))?;
        status.grandmaster = grandmaster;
        Ok(status)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.encode(formatter)
    }
}

impl Block {
    /// A block's lines, the ending blank line left off or not.
    pub fn parse(lines: &str) -> Result<Self, ParseError> {
        let first = lines.lines().find(|line| !line.is_empty()).unwrap_or("");
        match first.split_once(' ').unwrap_or((first, "")) {
            ("reply", "ok") => Ok(Self::Reply(Ok(()))),
            ("reply", answer) => match answer.strip_prefix("error ") {
                Some(error) => Ok(Self::Reply(Err(error.to_string()))),
                None => Err(ParseError(alloc::format!("an unknown reply: {answer}"))),
            },
            _ => Status::parse(lines).map(Self::Status),
        }
    }

    /// A reply as gptpd sends it, its ending blank line included.
    pub fn encode_reply(out: &mut impl fmt::Write, result: Result<(), &str>) -> fmt::Result {
        match result {
            Ok(()) => out.write_str("reply ok\n\n"),
            Err(error) => {
                let error: String = error
                    .chars()
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .collect();
                write!(out, "reply error {error}\n\n")
            }
        }
    }
}

impl Command {
    /// One line of a client's, its line end left off or not.
    pub fn parse(line: &str) -> Result<Self, ParseError> {
        let line = line.trim_end();
        match line.split_once(' ') {
            Some(("priority1", value)) => match value.parse::<u8>() {
                Ok(priority1) if priority1 > 0 => Ok(Self::Priority1(priority1)),
                _ => Err(ParseError(alloc::format!(
                    "priority1 takes 1 to 255, not {value}"
                ))),
            },
            _ => Err(ParseError(alloc::format!("an unknown command: {line}"))),
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Priority1(priority1) => write!(formatter, "priority1 {priority1}"),
        }
    }
}

fn set_grandmaster(
    grandmaster: &mut Grandmaster,
    key: &str,
    field: &str,
    value: &str,
) -> Result<(), ParseError> {
    match field {
        "priority1" => grandmaster.priority1 = parsed(key, value)?,
        "clock_class" => grandmaster.clock_class = parsed(key, value)?,
        "clock_accuracy" => grandmaster.clock_accuracy = parsed(key, value)?,
        "offset_scaled_log_variance" => {
            grandmaster.offset_scaled_log_variance = parsed(key, value)?;
        }
        "priority2" => grandmaster.priority2 = parsed(key, value)?,
        "steps_removed" => grandmaster.steps_removed = parsed(key, value)?,
        "is_us" => grandmaster.is_us = parsed(key, value)?,
        _ => {}
    }
    Ok(())
}

fn set_port(port: &mut Port, key: &str, field: &str, value: &str) -> Result<(), ParseError> {
    match field {
        "up" => port.up = parsed(key, value)?,
        "as_capable" => port.as_capable = parsed(key, value)?,
        "role" => port.role = parsed::<RoleName>(key, value)?.0,
        "link_delay" => port.link_delay = Some(parsed(key, value)?),
        "neighbor" => port.neighbor = Some(parsed(key, value)?),
        "neighbor_rate_ratio" => port.neighbor_rate_ratio = Some(parsed(key, value)?),
        _ => {}
    }
    Ok(())
}

fn parsed<T: FromStr>(key: &str, value: &str) -> Result<T, ParseError> {
    value
        .parse()
        .map_err(|_| ParseError(alloc::format!("{key}: {value:?} does not parse")))
}

impl fmt::Display for Profile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Gptp => "gptp",
            Self::AvbLite => "avb-lite",
        })
    }
}

impl FromStr for Profile {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, ParseError> {
        match text {
            "gptp" => Ok(Self::Gptp),
            "avb-lite" => Ok(Self::AvbLite),
            _ => Err(ParseError(alloc::format!("an unknown profile: {text}"))),
        }
    }
}

impl fmt::Display for Fallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::None => "none",
            Self::EndpointTlv => "endpoint-tlv",
            Self::PdelayUnanswered => "pdelay-unanswered",
            Self::MultipleResponders => "multiple-responders",
            Self::Configured => "configured",
        })
    }
}

impl FromStr for Fallback {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, ParseError> {
        match text {
            "none" => Ok(Self::None),
            "endpoint-tlv" => Ok(Self::EndpointTlv),
            "pdelay-unanswered" => Ok(Self::PdelayUnanswered),
            "multiple-responders" => Ok(Self::MultipleResponders),
            "configured" => Ok(Self::Configured),
            _ => Err(ParseError(alloc::format!(
                "an unknown fallback reason: {text}"
            ))),
        }
    }
}

/// A port role by its name in the status, as 802.1AS-2020 names it.
struct RoleName(Role);

impl fmt::Display for RoleName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.0 {
            Role::Disabled => "disabled",
            Role::TimeReceiver => "timereceiver",
            Role::TimeTransmitter => "timetransmitter",
            Role::Passive => "passive",
        })
    }
}

impl FromStr for RoleName {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, ParseError> {
        match text {
            "disabled" => Ok(Self(Role::Disabled)),
            "timereceiver" => Ok(Self(Role::TimeReceiver)),
            "timetransmitter" => Ok(Self(Role::TimeTransmitter)),
            "passive" => Ok(Self(Role::Passive)),
            _ => Err(ParseError(alloc::format!("an unknown role: {text}"))),
        }
    }
}

#[cfg(all(feature = "std", unix))]
pub use client::{Client, path};

#[cfg(all(feature = "std", unix))]
mod client {
    use std::format;
    use std::io::{self, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::string::String;
    use std::time::{Duration, Instant};
    use std::vec::Vec;

    use super::{Block, Command};

    /// Where gptpd listens for `interface`.
    pub fn path(interface: &str) -> io::Result<PathBuf> {
        let time = crate::time::path(interface)?;
        Ok(time.with_extension("sock"))
    }

    /// A connection to gptpd's socket for one interface.
    pub struct Client {
        stream: UnixStream,
        received: Vec<u8>,
    }

    impl Client {
        pub fn connect(interface: &str) -> io::Result<Self> {
            Self::connect_at(&path(interface)?)
        }

        pub fn connect_at(path: &Path) -> io::Result<Self> {
            Ok(Self {
                stream: UnixStream::connect(path)?,
                received: Vec::new(),
            })
        }

        /// The next snapshot or reply, waiting at most `timeout`, or for
        /// ever without one. None when the time runs out first; an error
        /// when gptpd closes the connection or sends what does not parse.
        pub fn next(&mut self, timeout: Option<Duration>) -> io::Result<Option<Block>> {
            let deadline = timeout.map(|timeout| Instant::now() + timeout);
            loop {
                if let Some(end) = self.received.windows(2).position(|pair| pair == b"\n\n") {
                    let block: Vec<u8> = self.received.drain(..end + 2).collect();
                    let text = String::from_utf8(block)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    return Block::parse(&text)
                        .map(Some)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.0));
                }
                let wait = match deadline {
                    Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                        Some(wait) if !wait.is_zero() => Some(wait),
                        _ => return Ok(None),
                    },
                    None => None,
                };
                self.stream.set_read_timeout(wait)?;
                let mut chunk = [0; 4096];
                match self.stream.read(&mut chunk) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "gptpd closed the connection",
                        ));
                    }
                    Ok(read) => self.received.extend_from_slice(&chunk[..read]),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                    {
                        return Ok(None);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        }

        /// Sends `command`; its reply comes from [`Client::next`], among
        /// the snapshots.
        pub fn send(&mut self, command: Command) -> io::Result<()> {
            self.stream.write_all(format!("{command}\n").as_bytes())
        }

        /// Sets this system's priority1, 255 for never grandmaster.
        pub fn set_priority1(&mut self, priority1: u8) -> io::Result<()> {
            self.send(Command::Priority1(priority1))
        }
    }
}

#[cfg(test)]
mod tests;
