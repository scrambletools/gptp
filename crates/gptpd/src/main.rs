//! gptpd: an 802.1AS daemon, a time-aware relay on a Linux bridge's
//! ports or an end station on one interface. It runs gptp's relay with
//! each port's gPTP frames on a raw socket, hardware time stamped, and the
//! ports' clocks made into one LocalClock in software, so that no clock is
//! ever stepped or slewed and nothing the network does shows in what the
//! neighbors measure. An end station gives applications the
//! grandmaster's time as a mapping instead (gptp::time), and its state on
//! a socket (gptp::status).

mod clocks;
mod config;
mod lite;
mod server;
mod station;
mod status;
mod steer;
mod wifi;

use std::collections::VecDeque;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use gptp::dot11::Support;
use gptp::link::Incapable;
use gptp::relay::{Config, Destination, Event, Relay, Stamp, SystemIdentity, TimeBase};
use gptp::{ClockIdentity, DESTINATION, ETHERTYPE_GPTP, MessageType};
use raw_ether::MacAddress;
use raw_ether::interfaces::LinkChanges;
use raw_ether::stamped::{self, StampedSocket};

use crate::clocks::{Clocks, Followed};
use crate::lite::Lite;
use crate::server::Server;
use crate::station::{Found, Station, Steadying, View};
use crate::wifi::{Heard, WifiPort};

/// How often the ports' clocks are compared with the LocalClock.
const COMPARE_EVERY: Duration = Duration::from_millis(100);

/// How often each port's link, and its bridge membership, is read, besides
/// at once when netlink says a link changed: soon enough, should netlink
/// say nothing, that a link gone is seen before a Sync timeout makes much
/// of it.
const SCAN_EVERY: Duration = Duration::from_millis(250);

/// The ports start once the clocks have been compared a few times, so
/// their rates are known from the first frame.
const SETTLE: Duration = Duration::from_millis(350);

/// How often the status file is written at most, but for changes.
const STATUS_EVERY: Duration = Duration::from_secs(1);

/// How long a port may take to say when a frame went out: igc can take a
/// few ms.
const STAMP_WAIT: Duration = Duration::from_millis(20);

/// How many frames a wired port remembers, both ways together, to show
/// what came before a Sync timeout: two seconds' worth or more.
const REMEMBERED: usize = 64;

/// How far back a Sync timeout shows the frames its port handled.
const SHOWN_BEFORE_TIMEOUT: Duration = Duration::from_secs(1);

/// How far from the interval its header gives a neighbor's Sync or
/// Pdelay_Req may come, in ns, before it is told.
const RHYTHM_SLACK: i128 = 1_000_000;

/// The most lines about the rhythm told a minute on a port; the rest are
/// counted.
const RHYTHM_TOLD_A_MINUTE: u32 = 12;

/// The exit status of `--check` when another PTP daemon holds the
/// interface, for the unit's RestartPreventExitStatus=.
const REFUSED: u8 = 3;

/// An end station's priority2 (802.1AS-2020 8.6.2.5).
const END_STATION_PRIORITY2: u8 = 248;

/// TAI less UTC where the kernel has not been told it: 37 s since 2017.
const ASSUMED_UTC_OFFSET: i16 = 37;

/// TAI less UTC, in seconds, and whether the kernel knew it.
fn utc_offset() -> (i16, bool) {
    use raw_ether::clock::{SystemClock, system_now};
    let tai = system_now(SystemClock::Tai).as_nanos() as i128;
    let utc = system_now(SystemClock::Realtime).as_nanos() as i128;
    let seconds = ((tai - utc) as f64 / 1e9).round() as i16;
    if seconds == 0 {
        (ASSUMED_UTC_OFFSET, false)
    } else {
        (seconds, true)
    }
}

/// This system's time as grandmaster from a cold start: TAI, as the wall
/// clock and the UTC offset have it, at a reading of the LocalClock.
fn cold_time_base(clocks: &Clocks, utc_offset: i16) -> Result<TimeBase, String> {
    use raw_ether::clock::SystemClock;
    let fail = |error: std::io::Error| format!("{}'s clock: {error}", clocks.local_name());
    let clock = clocks.local_clock();
    let ahead = clock
        .offset_from_system_clock(SystemClock::Realtime)
        .or_else(|_| clock.offset_read_against(SystemClock::Realtime))
        .map_err(fail)?;
    // The wall clock's time from the hardware clock's own reading; the time
    // base then stands at that reading on the free clock, where the
    // hardware clock is steered.
    let hardware = clock.now().map_err(fail)?.as_nanos() as i128;
    Ok(TimeBase {
        local: clocks.free(hardware),
        time: hardware - ahead + i128::from(utc_offset) * 1_000_000_000,
        rate: 1.0,
    })
}

const USAGE: &str = "\
usage: gptpd [options] INTERFACE...
       gptpd --check INTERFACE
  INTERFACE           a port to run gPTP on, numbered from 1 in the order
                      given; one interface without --bridge is an end
                      station, with its time in /run/gptpd/INTERFACE.time
                      and its state on /run/gptpd/INTERFACE.sock
  --bridge NAME       run each port only while it is a member of this
                      Linux bridge, whose address names the system (by
                      default the first port's names it)
  --clock INTERFACE   the interface whose hardware clock is the LocalClock,
                      a port or not (the first port)
  --config PATH       an end station's settings
                      (/etc/gptpd/INTERFACE.conf)
  --status PATH       where to write how gPTP stands (/run/gptpd/status,
                      or /run/gptpd/INTERFACE.status for an end station)
  --check INTERFACE   exit 0 if no other PTP daemon runs on INTERFACE, or
                      else 3, saying why in /run/gptpd/INTERFACE.refused;
                      an end station checks so as it starts
  --wifi-support WAYS take a Wi-Fi port's station to measure with WAYS,
                      tm, ftm or both, whatever its capabilities say, as
                      hostapd's own beacon says neither
  --version           print gptpd's version
  --help              print this";

struct Options {
    ports: Vec<String>,
    bridge: Option<String>,
    clock: Option<String>,
    config: Option<PathBuf>,
    status: Option<PathBuf>,
    check: Option<String>,
    wifi_support: Option<Support>,
}

impl Options {
    /// The interface of an end station: one, and no bridge.
    fn station(&self) -> Option<&str> {
        (self.bridge.is_none() && self.ports.len() == 1).then(|| self.ports[0].as_str())
    }
}

/// The options given, or None once --help or --version has printed.
fn options() -> Result<Option<Options>, String> {
    let mut options = Options {
        ports: Vec::new(),
        bridge: None,
        clock: None,
        config: None,
        status: None,
        check: None,
        wifi_support: None,
    };
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--help" || argument == "-h" {
            println!("{USAGE}");
            return Ok(None);
        }
        if argument == "--version" || argument == "-V" {
            println!("gptpd {}", env!("CARGO_PKG_VERSION"));
            return Ok(None);
        }
        if !argument.starts_with("--") {
            options.ports.push(argument);
            continue;
        }
        let value = arguments
            .next()
            .ok_or_else(|| format!("{argument} needs a value"))?;
        match argument.as_str() {
            "--bridge" => options.bridge = Some(value),
            "--clock" => options.clock = Some(value),
            "--config" => options.config = Some(PathBuf::from(value)),
            "--status" => options.status = Some(PathBuf::from(value)),
            "--check" => options.check = Some(value),
            "--wifi-support" => {
                options.wifi_support = Some(
                    wifi::parse_support(&value)
                        .ok_or_else(|| format!("--wifi-support {value}: tm, ftm or both"))?,
                );
            }
            _ => return Err(format!("unknown option {argument}")),
        }
    }
    if options.check.is_some() {
        return Ok(Some(options));
    }
    if options.ports.is_empty() {
        return Err("no ports given".to_owned());
    }
    if options.ports.len() > usize::from(u16::MAX) {
        return Err("too many ports".to_owned());
    }
    Ok(Some(options))
}

/// An EUI-64 from a MAC address, as IEEE 1588 makes a clock identity.
fn identity_of(mac: MacAddress) -> ClockIdentity {
    let [a, b, c, d, e, f] = mac.0;
    ClockIdentity(u64::from_be_bytes([a, b, c, 0xff, 0xfe, d, e, f]))
}

struct Port {
    name: String,
    socket: StampedSocket,
    up: bool,
    /// Transmit time stamps that did not come.
    missed: u64,
    /// Link delay measurements left out as outliers.
    outliers: u64,
    /// Whether the card stamps every frame, general messages too, as an
    /// I210 or i226 does: then one without a time is a fault.
    stamps_every_frame: bool,
    /// Times left out, the frame after them having come without one.
    distrusted: u64,
    /// Event messages left out for having come without a time stamp.
    unstamped: u64,
    /// Syncs that came without a time stamp since the last that came
    /// with one, and the sequence ID of the latest.
    unstamped_syncs: u64,
    last_unstamped_sync: u16,
    /// The last frames a wired port handled, oldest first.
    seen: VecDeque<Seen>,
    /// The card's time and sequence ID of the last Sync and Pdelay_Req
    /// received, none after one that came without a time stamp.
    last_sync: Option<(Duration, u16)>,
    last_pdelay_req: Option<(Duration, u16)>,
    /// Syncs and Pdelay_Reqs received off the rhythm their headers give.
    off_rhythm_syncs: u64,
    off_rhythm_pdelay_reqs: u64,
    /// A Pdelay_Req off its rhythm, its sequence ID, the card's time and
    /// whether it was told, until the Pdelay_Resp to it has left.
    off_rhythm_answer: Option<(u16, Duration, bool)>,
    /// A Sync stamped off its rhythm, its sequence ID, its gap in ns and
    /// whether it was told, until its Follow_Up comes.
    off_rhythm_sync: Option<(u16, i128, bool)>,
    /// The Sync interval the neighbor's headers give, in ns.
    sync_interval: Option<i128>,
    /// The origin and sequence ID of the last Follow_Up received.
    last_follow_up: Option<(i128, u16)>,
    /// Follow_Ups whose origins strayed while their Syncs' stamps did not.
    off_rhythm_origins: u64,
    /// The minute lines about the rhythm were last told in, how many, and
    /// how many were held back.
    rhythm_minute: u64,
    rhythm_told: u32,
    rhythm_untold: u64,
    /// Whether the port is an 802.11 port.
    wireless: bool,
    /// An 802.11 port's radio, whose Action frames carry the timing,
    /// opened while the port is up.
    wifi: Option<WifiPort>,
}

/// A gPTP frame a wired port received or sent, as remembered.
#[derive(Clone, Copy)]
struct Seen {
    /// When the daemon handled it.
    at: Duration,
    sent: bool,
    /// The low nibble of its first octet.
    message_type: Option<u8>,
    sequence: Option<u16>,
    stamping: Stamping,
}

/// What became of a remembered frame's time stamp.
#[derive(Clone, Copy)]
enum Stamping {
    /// Received, stamped this time by the card.
    Came(Duration),
    Missing,
    NotAsked,
    Waiting,
    /// Sent, its time stamp back `wait` after, stamped `wire` by the card.
    Back {
        wait: Duration,
        wire: Duration,
    },
    NeverCame,
}

/// What a remembered frame shows beside itself, from the frames around it.
#[derive(Clone, Copy, Default)]
struct Beside {
    /// For one received, how long after the last of its type the card
    /// stamped it, in nanoseconds.
    wire_gap: Option<i128>,
    /// For a Pdelay_Resp sent, t3 less t2 on the card's stamps, in
    /// nanoseconds.
    turnaround: Option<i128>,
}

impl Seen {
    /// The frame, `now` being the daemon's time.
    fn describe(&self, now: Duration, beside: Beside) -> String {
        let ago = now.saturating_sub(self.at).as_secs_f64() * 1e3;
        let sequence = self
            .sequence
            .map_or_else(|| "?".to_owned(), |sequence| sequence.to_string());
        let stamp = match self.stamping {
            Stamping::Came(_) => match beside.wire_gap {
                Some(gap) => format!(
                    ", stamped {:.3} ms after the {} before",
                    gap as f64 / 1e6,
                    type_name(self.message_type)
                ),
                None => ", stamped".to_owned(),
            },
            Stamping::Missing => ", without a time stamp".to_owned(),
            Stamping::NotAsked => String::new(),
            Stamping::Waiting => ", its time stamp not back yet".to_owned(),
            Stamping::Back { wait, .. } => {
                let mut told = format!(
                    ", its time stamp back after {:.3} ms",
                    wait.as_secs_f64() * 1e3
                );
                if let Some(turnaround) = beside.turnaround {
                    told.push_str(&format!(", turnaround {:.3} us", turnaround as f64 / 1e3));
                }
                told
            }
            Stamping::NeverCame => ", its time stamp never came".to_owned(),
        };
        format!(
            "{ago:.1} ms before, {} {} {sequence}{stamp}",
            if self.sent { "sent" } else { "received" },
            type_name(self.message_type)
        )
    }
}

/// A gPTP frame's message type and sequence ID, read from the header
/// octets so that a frame that would not decode still shows.
fn header(frame: &[u8]) -> (Option<u8>, Option<u16>) {
    let message_type = frame.first().map(|octet| octet & 0x0f);
    let sequence = frame
        .get(30..32)
        .map(|octets| u16::from_be_bytes([octets[0], octets[1]]));
    (message_type, sequence)
}

/// A message type's name, as the standard spells it.
fn type_name(message_type: Option<u8>) -> &'static str {
    const SYNC: u8 = MessageType::Sync as u8;
    const DELAY_REQ: u8 = MessageType::DelayReq as u8;
    const PDELAY_REQ: u8 = MessageType::PdelayReq as u8;
    const PDELAY_RESP: u8 = MessageType::PdelayResp as u8;
    const FOLLOW_UP: u8 = MessageType::FollowUp as u8;
    const DELAY_RESP: u8 = MessageType::DelayResp as u8;
    const PDELAY_RESP_FOLLOW_UP: u8 = MessageType::PdelayRespFollowUp as u8;
    const ANNOUNCE: u8 = MessageType::Announce as u8;
    const SIGNALING: u8 = MessageType::Signaling as u8;
    match message_type {
        Some(SYNC) => "Sync",
        Some(DELAY_REQ) => "Delay_Req",
        Some(PDELAY_REQ) => "Pdelay_Req",
        Some(PDELAY_RESP) => "Pdelay_Resp",
        Some(FOLLOW_UP) => "Follow_Up",
        Some(DELAY_RESP) => "Delay_Resp",
        Some(PDELAY_RESP_FOLLOW_UP) => "Pdelay_Resp_Follow_Up",
        Some(ANNOUNCE) => "Announce",
        Some(SIGNALING) => "Signaling",
        _ => "message",
    }
}

impl Port {
    /// Remembers a frame a wired port handled, forgetting the oldest.
    fn remember(&mut self, at: Duration, frame: &[u8], sent: bool, stamping: Stamping) {
        if self.wireless {
            return;
        }
        if self.seen.len() >= REMEMBERED {
            self.seen.pop_front();
        }
        let (message_type, sequence) = header(frame);
        self.seen.push_back(Seen {
            at,
            sent,
            message_type,
            sequence,
            stamping,
        });
    }

    /// Notes whether the time stamp of a frame sent came back, when, and
    /// the time the card gave.
    fn stamp_back(
        &mut self,
        message_type: MessageType,
        sequence: u16,
        now: Duration,
        wire: Option<Duration>,
    ) {
        let wanted = Some(message_type as u8);
        if let Some(seen) = self.seen.iter_mut().rev().find(|seen| {
            seen.sent && seen.message_type == wanted && seen.sequence == Some(sequence)
        }) {
            seen.stamping = match wire {
                Some(wire) => Stamping::Back {
                    wait: now.saturating_sub(seen.at),
                    wire,
                },
                None => Stamping::NeverCame,
            };
        }
        if message_type == MessageType::PdelayResp
            && let Some((asked, t2, told)) = self.off_rhythm_answer
            && asked == sequence
        {
            self.off_rhythm_answer = None;
            if told {
                match wire {
                    Some(t3) => eprintln!(
                        "{}: Pdelay_Resp {sequence} answered it, turnaround {:.3} us",
                        self.name,
                        (t3.as_nanos() as i128 - t2.as_nanos() as i128) as f64 / 1e3
                    ),
                    None => eprintln!(
                        "{}: Pdelay_Resp {sequence} answered it, its time stamp never came",
                        self.name
                    ),
                }
            }
        }
    }

    /// Notes a Sync or Pdelay_Req a wired port received: one stamped more
    /// than [`RHYTHM_SLACK`] from its interval after the one before it is
    /// counted, and told, a few a minute. A neighbor's schedule slipping so
    /// came before each of its Sync timeouts on the bench.
    fn note_rhythm(&mut self, at: Duration, frame: &[u8], wire: Option<Duration>) {
        const SYNC: u8 = MessageType::Sync as u8;
        const PDELAY_REQ: u8 = MessageType::PdelayReq as u8;
        const FOLLOW_UP: u8 = MessageType::FollowUp as u8;
        if self.wireless {
            return;
        }
        let (message_type, sequence) = header(frame);
        if message_type == Some(FOLLOW_UP) {
            self.note_origin(at, frame, sequence);
            return;
        }
        if message_type == Some(SYNC)
            && let Some(interval) = frame.get(33).and_then(|&octet| interval_nanos(octet as i8))
        {
            self.sync_interval = Some(interval);
        }
        let last = match message_type {
            Some(SYNC) => &mut self.last_sync,
            Some(PDELAY_REQ) => &mut self.last_pdelay_req,
            _ => return,
        };
        let before = last.take();
        let (Some(wire), Some(sequence)) = (wire, sequence) else {
            return;
        };
        *last = Some((wire, sequence));
        let Some((before_wire, before_sequence)) = before else {
            return;
        };
        let interval = frame
            .get(33)
            .and_then(|&octet| interval_nanos(octet as i8))
            .filter(|_| sequence == before_sequence.wrapping_add(1));
        let Some(interval) = interval else {
            return;
        };
        let gap = wire.as_nanos() as i128 - before_wire.as_nanos() as i128;
        if !off_rhythm(gap, interval) {
            return;
        }
        let told = self.may_tell_rhythm(at);
        let count = if message_type == Some(SYNC) {
            self.off_rhythm_sync = Some((sequence, gap, told));
            self.off_rhythm_syncs += 1;
            self.off_rhythm_syncs
        } else {
            self.off_rhythm_answer = Some((sequence, wire, told));
            self.off_rhythm_pdelay_reqs += 1;
            self.off_rhythm_pdelay_reqs
        };
        if told {
            eprintln!(
                "{}: {} {sequence} stamped {:.3} ms after the one before, {:+.3} ms off its \
                 interval ({count} so far)",
                self.name,
                type_name(message_type),
                gap as f64 / 1e6,
                (gap - interval) as f64 / 1e6
            );
        }
    }

    /// Notes when a neighbor's Follow_Up says its Sync left, its precise
    /// origin plus its correction. Told beside a Sync stamped off its
    /// rhythm, so that a Sync sent late tells from one stamped late, and
    /// on its own when the origins stray while the stamps did not.
    fn note_origin(&mut self, at: Duration, frame: &[u8], sequence: Option<u16>) {
        let before = self.last_follow_up.take();
        let (Some(sequence), Some(origin)) = (sequence, follow_up_origin(frame)) else {
            return;
        };
        self.last_follow_up = Some((origin, sequence));
        let flagged = self
            .off_rhythm_sync
            .take_if(|(flagged, _, _)| *flagged == sequence);
        let (Some((before_origin, before_sequence)), Some(interval)) = (before, self.sync_interval)
        else {
            return;
        };
        if sequence != before_sequence.wrapping_add(1) {
            return;
        }
        let gap = origin - before_origin;
        let off = (gap - interval) as f64 / 1e6;
        match flagged {
            Some((_, stamped, true)) => eprintln!(
                "{}: Follow_Up {sequence} puts its Sync's origin {:.3} ms after the one before, \
                 {off:+.3} ms off its interval, stamped {:+.3} ms off",
                self.name,
                gap as f64 / 1e6,
                (stamped - interval) as f64 / 1e6
            ),
            Some(_) => {}
            None if off_rhythm(gap, interval) => {
                self.off_rhythm_origins += 1;
                let count = self.off_rhythm_origins;
                if self.may_tell_rhythm(at) {
                    eprintln!(
                        "{}: Follow_Up {sequence} puts its Sync's origin {:.3} ms after the one \
                         before, {off:+.3} ms off its interval, its stamp within it ({count} so far)",
                        self.name,
                        gap as f64 / 1e6
                    );
                }
            }
            None => {}
        }
    }

    /// Whether another line about the rhythm may be told this minute; how
    /// many were held back is told with the next that is.
    fn may_tell_rhythm(&mut self, at: Duration) -> bool {
        let minute = at.as_secs() / 60;
        if minute != self.rhythm_minute {
            self.rhythm_minute = minute;
            self.rhythm_told = 0;
        }
        if self.rhythm_told >= RHYTHM_TOLD_A_MINUTE {
            self.rhythm_untold += 1;
            return false;
        }
        self.rhythm_told += 1;
        if self.rhythm_untold > 0 {
            eprintln!(
                "{}: {} more off their rhythm before this, not told",
                self.name, self.rhythm_untold
            );
            self.rhythm_untold = 0;
        }
        true
    }
}

/// A Follow_Up's precise origin timestamp plus its correction, in ns: when
/// its Sync left, on the grandmaster's time.
fn follow_up_origin(frame: &[u8]) -> Option<i128> {
    let correction = i64::from_be_bytes(frame.get(8..16)?.try_into().ok()?);
    let seconds = frame
        .get(34..40)?
        .iter()
        .fold(0i128, |value, &octet| (value << 8) | i128::from(octet));
    let nanoseconds = u32::from_be_bytes(frame.get(40..44)?.try_into().ok()?);
    Some(seconds * 1_000_000_000 + i128::from(nanoseconds) + i128::from(correction >> 16))
}

/// Whether a gap between two of a neighbor's messages strays more than
/// [`RHYTHM_SLACK`] from their interval, both in ns.
fn off_rhythm(gap: i128, interval: i128) -> bool {
    (gap - interval).abs() > RHYTHM_SLACK
}

/// A logMessageInterval's interval in nanoseconds, where it gives one.
fn interval_nanos(log: i8) -> Option<i128> {
    match log {
        0..=8 => Some(1_000_000_000 << log),
        -9..=-1 => Some(1_000_000_000 >> -log),
        _ => None,
    }
}

/// What an end station has beyond a relay of one port.
struct EndStation {
    station: Station,
    server: Server,
    settings: config::Settings,
    config: PathBuf,
    lite: Lite,
}

/// Counts since the daemon started, for the status.
#[derive(Default)]
struct Counts {
    grandmaster_changes: u64,
    link_ups: u64,
    link_downs: u64,
    sync_timeouts: u64,
}

struct Daemon {
    identity: ClockIdentity,
    relay: Relay,
    ports: Vec<Port>,
    clocks: Clocks,
    bridge: Option<String>,
    started: Instant,
    /// Something changed that the status file is to show at once.
    changed: bool,
    counts: Counts,
    end_station: Option<EndStation>,
    /// TAI less UTC, for a time base drawn from the system's clock.
    utc_offset: i16,
    /// Ports whose sending found the network down, to take down.
    gone: Vec<usize>,
    /// What to take an 802.11 port's station to measure with, where told.
    wifi_support: Option<Support>,
}

/// A port's number in the relay, from its place among the ports.
fn number(index: usize) -> u16 {
    (index + 1) as u16
}

/// Whether a count is one to tell: the first, the tenth, the hundredth
/// and so on.
fn telling(count: u64) -> bool {
    let mut count = count;
    while count >= 10 && count.is_multiple_of(10) {
        count /= 10;
    }
    count == 1
}

impl Daemon {
    fn now(&self) -> Duration {
        self.started.elapsed()
    }

    /// Reads each port's link and membership, and tells the relay of the
    /// ports that came up or went down.
    fn scan(&mut self) {
        let now = self.now();
        // A clock that was set stamps nothing until it is compared afresh,
        // so no time is led or followed on a mapping it no longer keeps.
        let rebasing = self
            .end_station
            .as_ref()
            .is_some_and(|end| end.station.awaiting_rebase());
        let mut reopen = Vec::new();
        for (index, port) in self.ports.iter_mut().enumerate() {
            // operstate, not carrier: reading carrier waits for RTNL, so
            // while another program held it, a Wi-Fi driver restarting its
            // firmware once for 3 s, no port sent a Sync or a Pdelay answer.
            let carrier = read(&port.name, "operstate").as_deref() == Some("up");
            let member = self.bridge.as_ref().is_none_or(|bridge| {
                Path::new("/sys/class/net")
                    .join(bridge)
                    .join("brif")
                    .join(&port.name)
                    .exists()
            });
            let up = carrier && member && !rebasing;
            if up == port.up {
                continue;
            }
            port.up = up;
            self.changed = true;
            if up {
                self.counts.link_ups += 1;
            } else if !carrier {
                self.counts.link_downs += 1;
            }
            eprintln!(
                "{}: {}",
                port.name,
                if up {
                    "link up"
                } else if !carrier {
                    "link down"
                } else {
                    "not in the bridge"
                }
            );
            // hostapd stopping takes the frames registered for, and a radio
            // opened before hostapd made it an access point is a station's,
            // so it is opened afresh each time the port comes up.
            if port.wireless {
                port.wifi = None;
                if up {
                    reopen.push(index);
                }
            }
            let lite = self.end_station.as_mut().map(|end| &mut end.lite);
            match lite {
                Some(lite) => {
                    let speed = read(&port.name, "speed").and_then(|speed| speed.parse().ok());
                    lite.set_link(now, up, speed);
                    if !lite.active() {
                        self.relay.set_port(now, number(index), up);
                    }
                }
                None => self.relay.set_port(now, number(index), up),
            }
        }
        for index in reopen {
            self.open_radio(index);
        }
        // A station chosen before it associated, measured once it has.
        let settled: Vec<(usize, Heard)> = self
            .ports
            .iter_mut()
            .enumerate()
            .filter_map(|(index, port)| Some((index, port.wifi.as_mut()?.settle()?)))
            .collect();
        for (index, heard) in settled {
            self.hear(index, heard);
        }
    }

    /// Opens 802.11 port `index`'s radio, its station chosen afresh.
    fn open_radio(&mut self, index: usize) {
        let now = self.now();
        let port = &mut self.ports[index];
        let mut wifi = match WifiPort::open(&port.name, self.wifi_support) {
            Ok(wifi) => wifi,
            Err(error) => {
                eprintln!("{error}");
                return;
            }
        };
        eprintln!(
            "{}: an 802.11 port, as {}",
            port.name,
            if wifi.access_point() {
                "access point"
            } else {
                "station"
            }
        );
        let chosen = wifi.choose();
        port.wifi = Some(wifi);
        self.relay
            .set_wireless(now, number(index), None, Support::NONE);
        if let Some(heard) = chosen {
            self.hear(index, heard);
        }
    }

    /// Hands the relay every frame waiting on any port, without waiting.
    fn drain(&mut self) -> Result<(), String> {
        let ready = {
            let sockets: Vec<&StampedSocket> = self.ports.iter().map(|port| &port.socket).collect();
            stamped::wait_any(&sockets, Some(Duration::ZERO))
                .map_err(|error| format!("waiting for gPTP frames: {error}"))?
        };
        for index in ready {
            self.receive(index);
            self.flush();
            self.decide_profile();
            self.flush();
        }
        for index in 0..self.ports.len() {
            self.hear_radio(index);
        }
        Ok(())
    }

    /// Hands the relay what port `index`'s radio said, if it has one.
    fn hear_radio(&mut self, index: usize) {
        let Some(wifi) = self.ports[index].wifi.as_mut() else {
            return;
        };
        for heard in wifi.take() {
            self.hear(index, heard);
        }
        self.flush();
    }

    /// Hands the relay one thing port `index`'s radio said, its times put
    /// on the LocalClock.
    fn hear(&mut self, index: usize, heard: Heard) {
        let now = self.now();
        let stamp = |time: i128| {
            self.clocks.stamp(
                index,
                Duration::from_nanos(u64::try_from(time).unwrap_or(0)),
            )
        };
        match heard {
            Heard::Peer(peer) => {
                let support = peer.map_or(Support::NONE, |(_, support)| support);
                self.relay.set_wireless(
                    now,
                    number(index),
                    peer.map(|(station, _)| station.0),
                    support,
                );
                self.changed = true;
            }
            Heard::FtmRequest(action) => self.relay.handle_ftm_request(now, number(index), &action),
            Heard::Measurement {
                action,
                arrival,
                ack_departure,
            } => {
                let (arrival, ack_departure) = (stamp(arrival), stamp(ack_departure).port);
                self.relay
                    .handle_measurement(now, number(index), &action, arrival, ack_departure);
            }
            Heard::Confirm {
                dialog_token,
                departure,
                ack_arrival,
            } => {
                let (departure, ack_arrival) = (stamp(departure), stamp(ack_arrival).port);
                self.relay.handle_measurement_confirm(
                    number(index),
                    dialog_token,
                    departure,
                    ack_arrival,
                );
            }
        }
    }

    /// Hands the relay each frame waiting on port `index`.
    fn receive(&mut self, index: usize) {
        let mut buffer = [0u8; 1500];
        loop {
            let received = match self.ports[index]
                .socket
                .receive(&mut buffer, Some(Duration::ZERO))
            {
                Ok(Some(received)) => received,
                Ok(None) => return,
                Err(error) => {
                    eprintln!("{}: receiving gPTP: {error}", self.ports[index].name);
                    return;
                }
            };
            let frame = &buffer[..received.length];
            let stamping = received.time.map_or(Stamping::Missing, Stamping::Came);
            let arrived = self.now();
            self.ports[index].remember(arrived, frame, false, stamping);
            self.ports[index].note_rhythm(arrived, frame, received.time);
            // Sync, Delay_Req, Pdelay_Req and Pdelay_Resp: of no use
            // without the time they came.
            let event = frame.first().is_some_and(|octet| octet & 0x0f < 4);
            if received.time.is_some() && !event {
                self.ports[index].stamps_every_frame = true;
            }
            if received.time.is_none() && self.ports[index].stamps_every_frame {
                self.distrust_last(index);
            }
            let time = match received.time {
                Some(time) => {
                    if header(frame).0 == Some(MessageType::Sync as u8) {
                        self.ports[index].unstamped_syncs = 0;
                    }
                    self.clocks.stamp(index, time)
                }
                None if event => {
                    if header(frame).0 == Some(MessageType::Sync as u8) {
                        let now = self.now();
                        self.relay.handle_unstamped_sync(now, number(index), frame);
                    }
                    self.left_unstamped(index, frame);
                    continue;
                }
                None => Stamp::local(0),
            };
            let now = self.now();
            self.relay.handle_frame(now, number(index), frame, time);
            if let Some(end) = &mut self.end_station {
                end.lite.handle_frame(now, frame, time.port);
            }
        }
    }

    /// An event message on port `index` came without a time stamp, so is
    /// of no use: counted, and told while the count is small. An 802.11
    /// port's frames never carry one.
    fn left_unstamped(&mut self, index: usize, frame: &[u8]) {
        let port = &mut self.ports[index];
        if port.wireless {
            return;
        }
        let (message_type, sequence) = header(frame);
        if message_type == Some(MessageType::Sync as u8) {
            port.unstamped_syncs += 1;
            if let Some(sequence) = sequence {
                port.last_unstamped_sync = sequence;
            }
        }
        let what = type_name(message_type);
        port.unstamped += 1;
        let count = port.unstamped;
        if count <= 10 || telling(count) {
            let sequence = sequence.map_or_else(|| "?".to_owned(), |sequence| sequence.to_string());
            eprintln!(
                "{}: {what} {sequence} left out, it came without a time stamp ({count} so far)",
                port.name
            );
        }
    }

    /// How an end station's wired port stamps, read again after a Sync
    /// timeout: another program can turn its receive stamps off for every
    /// program. Not on a bridge, as the read takes RTNL, which a radio's
    /// driver there has held for seconds.
    fn stamping_after_timeout(&self, index: usize) -> String {
        let Some(port) = self.ports.get(index).filter(|port| !port.wireless) else {
            return String::new();
        };
        if self.end_station.is_none() {
            return String::new();
        }
        match stamped::setting(&port.name) {
            Ok(setting) if setting.is_enabled() => {
                ", its card still stamping every frame".to_owned()
            }
            Ok(setting) => format!(
                ", its card now stamping with tx_type {} and rx_filter {}, not as set",
                setting.tx_type, setting.rx_filter
            ),
            Err(error) => format!(", its card's stamping unread: {error}"),
        }
    }

    /// Tells the frames wired port `index` handled in the last second,
    /// oldest first.
    fn tell_seen(&self, index: usize) {
        const PDELAY_REQ: u8 = MessageType::PdelayReq as u8;
        const PDELAY_RESP: u8 = MessageType::PdelayResp as u8;
        let Some(port) = self.ports.get(index).filter(|port| !port.wireless) else {
            return;
        };
        let nanos = |time: Duration| time.as_nanos() as i128;
        let now = self.now();
        // The card's time each type was last received, by its nibble.
        let mut last_received = [None::<Duration>; 16];
        for (position, seen) in port.seen.iter().enumerate() {
            let mut beside = Beside::default();
            match (seen.sent, seen.message_type, seen.stamping) {
                (false, Some(message_type), Stamping::Came(wire)) => {
                    let last = &mut last_received[usize::from(message_type & 0x0f)];
                    beside.wire_gap = last.map(|last| nanos(wire) - nanos(last));
                    *last = Some(wire);
                }
                (true, Some(PDELAY_RESP), Stamping::Back { wire: t3, .. }) => {
                    beside.turnaround = port.seen.iter().take(position).rev().find_map(|request| {
                        match (request.sent, request.message_type, request.stamping) {
                            (false, Some(PDELAY_REQ), Stamping::Came(t2))
                                if request.sequence == seen.sequence =>
                            {
                                Some(nanos(t3) - nanos(t2))
                            }
                            _ => None,
                        }
                    });
                }
                _ => {}
            }
            if now.saturating_sub(seen.at) <= SHOWN_BEFORE_TIMEOUT {
                eprintln!("{}: {}", port.name, seen.describe(now, beside));
            }
        }
    }

    /// A frame on port `index` came without a time, from a card that
    /// stamps every frame: the one before it may carry its time.
    fn distrust_last(&mut self, index: usize) {
        let Some(message_type) = self.relay.handle_unstamped(number(index)) else {
            return;
        };
        let port = &mut self.ports[index];
        port.distrusted += 1;
        let count = port.distrusted;
        if count <= 10 || telling(count) {
            let what = match message_type {
                MessageType::PdelayResp => "a Pdelay exchange",
                _ => "a Sync",
            };
            eprintln!(
                "{}: {what} left out, the frame after it came without a time stamp ({count} so far)",
                port.name
            );
        }
    }

    /// Sends what the relay has to send, giving it the time each event
    /// message left, until it has nothing more.
    fn flush(&mut self) {
        loop {
            // Event messages sent, their times still to collect: one per
            // port at a time, as a port's socket waits for one time only.
            let mut waiting: Vec<(usize, MessageType, u16)> = Vec::new();
            while let Some(transmit) = self.relay.poll_transmit() {
                let index = usize::from(transmit.port).wrapping_sub(1);
                if index >= self.ports.len() {
                    continue;
                }
                if transmit.stamp.is_some()
                    && let Some(position) = waiting.iter().position(|(each, ..)| *each == index)
                {
                    let (index, message_type, sequence) = waiting.remove(position);
                    self.collect(index, message_type, sequence);
                }
                let destination = match transmit.destination {
                    Destination::Group => MacAddress(DESTINATION),
                    Destination::Station(station) => MacAddress(station),
                    // Action frames go through the port's radio.
                    Destination::Measurement { .. } | Destination::Action { .. } => {
                        let port = &mut self.ports[index];
                        if let Some(wifi) = port.wifi.as_mut()
                            && let Err(error) = wifi.send(transmit.destination, &transmit.frame)
                        {
                            eprintln!("{}: sending an Action frame: {error}", port.name);
                        }
                        continue;
                    }
                };
                let port = &self.ports[index];
                let sent = if transmit.stamp.is_some() {
                    port.socket.send_stamped(destination, &transmit.frame)
                } else {
                    port.socket.send(destination, &transmit.frame)
                };
                match sent {
                    Ok(()) => {
                        let stamping = if transmit.stamp.is_some() {
                            Stamping::Waiting
                        } else {
                            Stamping::NotAsked
                        };
                        let now = self.now();
                        self.ports[index].remember(now, &transmit.frame, true, stamping);
                        if let Some((message_type, sequence)) = transmit.stamp {
                            waiting.push((index, message_type, sequence));
                            if message_type == MessageType::PdelayReq
                                && let Some(end) = &mut self.end_station
                            {
                                let now = self.started.elapsed();
                                end.lite.handle_request(now, sequence);
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("{}: sending gPTP: {error}", port.name);
                        if error.raw_os_error() == Some(libc::ENETDOWN) {
                            self.gone.push(index);
                        }
                    }
                }
            }
            // An end station's AVB Lite frames: the beacon, and the
            // profile's Delay_Req.
            while let Some(outgoing) = self
                .end_station
                .as_mut()
                .and_then(|end| end.lite.poll_transmit())
            {
                if outgoing.stamp.is_some()
                    && let Some(position) = waiting.iter().position(|(each, ..)| *each == 0)
                {
                    let (index, message_type, sequence) = waiting.remove(position);
                    self.collect(index, message_type, sequence);
                }
                let port = &self.ports[0];
                let destination = MacAddress(outgoing.destination);
                let sent = if outgoing.stamp.is_some() {
                    port.socket.send_stamped(destination, &outgoing.frame)
                } else {
                    port.socket.send(destination, &outgoing.frame)
                };
                match sent {
                    Ok(()) => {
                        if let Some((message_type, sequence)) = outgoing.stamp {
                            waiting.push((0, message_type, sequence));
                        }
                    }
                    Err(error) => {
                        eprintln!("{}: sending AVB Lite PTP: {error}", port.name);
                        if error.raw_os_error() == Some(libc::ENETDOWN) {
                            self.gone.push(0);
                        }
                    }
                }
            }
            if waiting.is_empty() {
                break;
            }
            for (index, message_type, sequence) in waiting {
                self.collect(index, message_type, sequence);
            }
        }
        let gone = std::mem::take(&mut self.gone);
        for index in gone {
            self.take_down(index, "the network is down");
        }
        while let Some(event) = self.relay.poll_event() {
            match event {
                Event::Grandmaster(_) => self.counts.grandmaster_changes += 1,
                Event::SyncTimeout { port } => {
                    self.counts.sync_timeouts += 1;
                    self.changed = true;
                    let stamping = self.stamping_after_timeout(usize::from(port).wrapping_sub(1));
                    eprintln!("{}{stamping}", self.describe(event));
                    let count = self.counts.sync_timeouts;
                    if count <= 10 || telling(count) {
                        self.tell_seen(usize::from(port).wrapping_sub(1));
                    }
                    continue;
                }
                Event::DelayOutlier { port, .. } => {
                    let Some(port) = self.ports.get_mut(usize::from(port).wrapping_sub(1)) else {
                        continue;
                    };
                    port.outliers += 1;
                    let count = port.outliers;
                    if count <= 10 || telling(count) {
                        eprintln!("{} ({count} so far)", self.describe(event));
                    }
                    continue;
                }
                _ => {}
            }
            self.changed = true;
            eprintln!("{}", self.describe(event));
        }
        while let Some(event) = self
            .end_station
            .as_mut()
            .and_then(|end| end.lite.client.poll_event())
        {
            self.changed = true;
            self.counts.grandmaster_changes += 1;
            let gptp::e2e::Event::Grandmaster(grandmaster) = event;
            match grandmaster {
                Some(grandmaster) => eprintln!(
                    "AVB Lite grandmaster {}, priority1 {}, clock class {}",
                    grandmaster.clock, grandmaster.priority1, grandmaster.clock_class
                ),
                None => eprintln!("no AVB Lite grandmaster"),
            }
        }
    }

    /// Takes port `index` down at once, as its link went or its clock was
    /// set: the next scan brings it up afresh, its measurements anew.
    fn take_down(&mut self, index: usize, why: &str) {
        let now = self.now();
        let port = &mut self.ports[index];
        if !port.up {
            return;
        }
        port.up = false;
        self.changed = true;
        eprintln!("{}: down, {why}", port.name);
        self.relay.set_port(now, number(index), false);
        if let Some(end) = &mut self.end_station {
            end.lite.set_link(now, false, None);
        }
    }

    /// Gives the relay the time the event message last sent on port
    /// `index` left.
    fn collect(&mut self, index: usize, message_type: MessageType, sequence: u16) {
        let result = self.ports[index].socket.transmit_time(STAMP_WAIT);
        let now = self.now();
        let wire = result.as_ref().ok().copied().flatten();
        self.ports[index].stamp_back(message_type, sequence, now, wire);
        let port = &mut self.ports[index];
        match result {
            Ok(Some(time)) => {
                let time = self.clocks.stamp(index, time);
                match (message_type, &mut self.end_station) {
                    (MessageType::DelayReq, Some(end)) => {
                        end.lite
                            .client
                            .handle_transmit_time(message_type, sequence, time.port);
                    }
                    _ => {
                        self.relay
                            .handle_transmit_time(number(index), message_type, sequence, time)
                    }
                }
            }
            Ok(None) => {
                port.missed += 1;
                if telling(port.missed) {
                    eprintln!(
                        "{}: no transmit time stamp for a {message_type:?} ({} so far)",
                        port.name, port.missed
                    );
                }
            }
            Err(error) => eprintln!("{}: transmit time stamp: {error}", port.name),
        }
    }

    fn describe(&self, event: Event) -> String {
        let name = |number: u16| {
            self.ports
                .get(usize::from(number).wrapping_sub(1))
                .map_or_else(|| format!("port {number}"), |port| port.name.clone())
        };
        match event {
            Event::AsCapable {
                port,
                as_capable: true,
            } => match self.relay.link(port) {
                Some(link) => format!(
                    "{}: asCapable, link delay {:.0} ns, neighbor {} at {:+.3} ppm",
                    name(port),
                    link.mean_link_delay,
                    link.neighbor,
                    (link.neighbor_rate_ratio - 1.0) * 1e6
                ),
                None => format!("{}: asCapable", name(port)),
            },
            Event::AsCapable { port, .. } => match self.relay.incapable_because(port) {
                Some(Incapable::LongDelay) => format!(
                    "{}: not asCapable, {}, {:.0} ns",
                    name(port),
                    Incapable::LongDelay.describe(),
                    self.relay
                        .link(port)
                        .map_or(f64::NAN, |link| link.mean_link_delay)
                ),
                Some(why) => format!("{}: not asCapable, {}", name(port), why.describe()),
                None => format!("{}: not asCapable", name(port)),
            },
            Event::Role { port, role } => format!("{}: {}", name(port), status::role(role)),
            Event::Grandmaster(Some(grandmaster)) => format!(
                "grandmaster {}, priority1 {}, clock class {}{}",
                grandmaster.clock,
                grandmaster.priority1,
                grandmaster.clock_class,
                if grandmaster.clock == self.identity {
                    ": this system, sending its own Sync"
                } else {
                    ""
                }
            ),
            Event::Grandmaster(None) => "no grandmaster".to_owned(),
            Event::SyncTimeout { port } => {
                let unstamped = self
                    .ports
                    .get(usize::from(port).wrapping_sub(1))
                    .filter(|port| !port.wireless)
                    .map(|port| (port.unstamped_syncs, port.last_unstamped_sync));
                match unstamped {
                    Some((0, _)) => format!(
                        "{}: no Sync for too long, none came without a time stamp",
                        name(port)
                    ),
                    Some((count, sequence)) => format!(
                        "{}: no Sync for too long, {count} came without a time stamp, the latest {sequence}",
                        name(port)
                    ),
                    None => format!("{}: no Sync for too long", name(port)),
                }
            }
            Event::DelayOutlier {
                port,
                delay,
                median,
            } => format!(
                "{}: a link delay of {delay} ns left out, {median} ns the median kept",
                name(port)
            ),
        }
    }
}

impl Daemon {
    /// Compares an end station's hardware clock with the system's, and
    /// starts its port over after a resume.
    fn compare_station(&mut self) {
        let now = self.now();
        let Some(end) = &mut self.end_station else {
            return;
        };
        let slow = end.station.slow();
        match end
            .station
            .compare(self.clocks.local_clock(), self.clocks.steering())
        {
            Ok(Found::Steady) => {}
            Ok(Found::Jumped(moved)) => {
                self.changed = true;
                // Set by its driver, or by the steering itself: the free
                // clock starts afresh from it.
                self.clocks.steering_reset();
                eprintln!(
                    "{}: its clock moved {moved} ns against the system's; a new generation, {}",
                    self.ports[0].name,
                    end.station.generation()
                );
                // Without a line to draw it afresh from, from the system's
                // clock: no time base stays on a clock that was set.
                if !end.station.awaiting_rebase() {
                    match cold_time_base(&self.clocks, self.utc_offset) {
                        Ok(base) => self.relay.set_time_base(base),
                        Err(error) => eprintln!("{error}"),
                    }
                }
                self.take_down(0, "its clock was set");
            }
            Ok(Found::Resumed) => {
                self.changed = true;
                eprintln!(
                    "resumed: {} starts over, a new generation, {}",
                    self.ports[0].name,
                    end.station.generation()
                );
                // Down, so the next scan brings the link up afresh: gPTP's
                // and AVB Lite's measurements and decision start over.
                self.ports[0].up = false;
                self.relay.set_port(now, 1, false);
                end.lite.set_link(now, false, None);
            }
            Err(error) => eprintln!("{}'s clock: {error}", self.ports[0].name),
        }
        let Some(end) = &mut self.end_station else {
            return;
        };
        let now_slow = end.station.slow();
        if (slow + 1..=now_slow).any(|count| count <= 10 || telling(count)) {
            eprintln!(
                "{}: a slow reading of its clock left out, {} ns ({now_slow} so far)",
                self.ports[0].name,
                end.station.slow_span()
            );
        }
        if end.station.awaiting_rebase()
            && let Ok(local) = self.clocks.now()
            && let Some((local, time, rate)) = end.station.rebased(local)
        {
            self.relay.set_time_base(TimeBase { local, time, rate });
            eprintln!(
                "{}: as grandmaster its time is drawn afresh on the reset clock, from the last \
                 line against CLOCK_MONOTONIC",
                self.ports[0].name
            );
        }
        self.follow_tai();
    }

    /// Steers an end station's clock towards CLOCK_TAI, where it follows it,
    /// after a comparison.
    fn follow_tai(&mut self) {
        if self.clocks.steering().is_none() {
            return;
        }
        let grandmaster = self.view().ours;
        let drift = self
            .end_station
            .as_ref()
            .and_then(|end| end.station.drift());
        let name = &self.ports[0].name;
        match self.clocks.follow_tai(drift, grandmaster) {
            Some(Followed::Set(by)) => {
                eprintln!("{name}: its clock set {by} ns on, to CLOCK_TAI");
            }
            Some(Followed::Began(offset)) => {
                eprintln!("{name}: its clock follows CLOCK_TAI, {offset} ns from it");
            }
            Some(Followed::Lost(offset)) => {
                eprintln!("{name}: its clock no longer follows CLOCK_TAI, {offset} ns from it");
            }
            Some(Followed::Failed(why)) => eprintln!("{name}: steering its clock: {why}"),
            None => {}
        }
    }

    /// Switches an end station between gPTP and AVB Lite as its fallback
    /// decided.
    fn decide_profile(&mut self) {
        let now = self.now();
        let up = self.ports[0].up;
        let Some(end) = &mut self.end_station else {
            return;
        };
        let Some(fell_back) = end.lite.decide(now, up) else {
            return;
        };
        self.changed = true;
        end.station.discontinue();
        if fell_back {
            self.relay.set_port(now, 1, false);
            eprintln!(
                "{}: falls back to AVB Lite ({}); a new generation, {}",
                self.ports[0].name,
                end.lite.reason().map_or("?", gptp::lite::Reason::name),
                end.station.generation()
            );
        } else {
            self.relay.set_port(now, 1, up);
            eprintln!(
                "{}: back to gPTP; a new generation, {}",
                self.ports[0].name,
                end.station.generation()
            );
        }
    }

    /// The grandmaster's time as the profile running has it.
    fn view(&mut self) -> View {
        if let Some(end) = &mut self.end_station
            && end.lite.active()
        {
            return end.lite.view();
        }
        let grandmaster = self
            .relay
            .grandmaster()
            .map(|grandmaster| grandmaster.clock);
        let ours = self.relay.is_grandmaster();
        // As grandmaster the line is drawn from now, not from wherever
        // the time base was set, so the comparison is not stretched back.
        let line = if ours {
            let local = self.clocks.now().unwrap_or_default();
            self.relay
                .grandmaster_time(local)
                .zip(self.relay.rate_ratio())
                .map(|(time, rate)| (local, time, rate))
        } else {
            self.relay.time_line()
        };
        View {
            grandmaster,
            ours,
            line,
            asymmetry_included: false,
        }
    }

    /// When the profile running last heard a Sync, to publish at each.
    fn last_sync(&self) -> Option<i128> {
        match &self.end_station {
            Some(end) if end.lite.active() => end.lite.client.time_line().map(|line| line.0),
            _ => self.relay.last_sync(),
        }
    }

    /// Writes an end station's mapping, and hands its status to clients.
    fn publish_station(&mut self) {
        let monotonic = raw_ether::clock::monotonic_now().as_nanos() as i64;
        let view = self.view();
        let Some(end) = &mut self.end_station else {
            return;
        };
        let mapping = end.station.mapping(view, monotonic);
        let recorded = self.clocks.recorded(mapping);
        if let Err(error) = end.station.write(mapping, recorded) {
            eprintln!("cannot write {}: {error}", end.station.path().display());
        }
        let name = &self.ports[0].name;
        match end.station.take_steadying() {
            Some(Steadying::HeldBack(moved)) => eprintln!(
                "{name}: a line {moved} ns from the last held back, until a later Sync says the same"
            ),
            Some(Steadying::Dropped) => {
                eprintln!("{name}: the line held back was one Sync's, and is dropped")
            }
            Some(Steadying::Stepped(moved, generation)) => eprintln!(
                "{name}: the grandmaster's time moved {moved} ns; a new generation, {generation}"
            ),
            None => {}
        }
        let snapshot = status::snapshot(self);
        if let Some(end) = &self.end_station {
            let mut text = String::new();
            let _ = snapshot.encode(&mut text);
            end.server.publish(text);
        }
    }

    /// Carries out what clients asked of an end station.
    fn answer_station(&mut self) {
        let now = self.now();
        let Some(end) = &mut self.end_station else {
            return;
        };
        let requests: Vec<server::Request> = end.server.requests().collect();
        for request in requests {
            let answer = match request.command {
                gptp::status::Command::Priority1(priority1) => {
                    match config::save_priority1(&end.config, priority1) {
                        Ok(()) => {
                            end.settings.priority1 = priority1;
                            self.relay.set_identity(
                                now,
                                SystemIdentity::grandmaster_capable(
                                    self.identity,
                                    priority1,
                                    END_STATION_PRIORITY2,
                                ),
                            );
                            self.changed = true;
                            eprintln!(
                                "priority1 is {priority1} from now on, in {}",
                                end.config.display()
                            );
                            Ok(())
                        }
                        Err(error) => Err(format!("{}: {error}", end.config.display())),
                    }
                }
            };
            let _ = request.answer.send(answer);
        }
    }
}

/// Another PTP daemon running on `interface`, said in its refusal file;
/// none, the file removed.
fn refusal(interface: &str) -> Option<String> {
    let refused = config::refused_path(interface);
    match config::holder(interface) {
        Some(reason) => {
            if let Some(folder) = refused.parent() {
                let _ = std::fs::create_dir_all(folder);
            }
            let _ = std::fs::write(&refused, format!("{reason}\n"));
            eprintln!("{reason}: gptpd leaves {interface} to it");
            Some(reason)
        }
        None => {
            let _ = std::fs::remove_file(&refused);
            None
        }
    }
}

/// One of an interface's sysfs attributes, trimmed.
fn read(interface: &str, attribute: &str) -> Option<String> {
    let path = Path::new("/sys/class/net").join(interface).join(attribute);
    Some(std::fs::read_to_string(path).ok()?.trim().to_owned())
}

/// The port's gPTP socket; an end station's hears the AVB Lite profile's
/// destination too. An 802.11 port's carries only general messages, its
/// timing going in Action frames, so it stamps none.
fn open(name: &str, end_station: bool, wireless: bool) -> Result<StampedSocket, String> {
    let fail = |error: std::io::Error| format!("{name}: {error}");
    if !wireless {
        stamped::enable(name).map_err(fail)?;
    }
    let socket = StampedSocket::open(name, ETHERTYPE_GPTP).map_err(fail)?;
    socket
        .join_multicast(MacAddress(DESTINATION))
        .map_err(fail)?;
    if end_station {
        socket
            .join_multicast(MacAddress(gptp::e2e::DESTINATION))
            .map_err(fail)?;
    }
    Ok(socket)
}

/// Keeps the daemon in RAM, so none of it waits on the disk while a frame
/// is due, however hard the computer swaps: what is mapped now at once,
/// what is mapped later once touched, so thread stacks cost what they use.
#[allow(unsafe_code)]
fn lock_memory() {
    // SAFETY: mlockall takes no memory.
    let locked = unsafe {
        libc::mlockall(libc::MCL_CURRENT) == 0
            && libc::mlockall(libc::MCL_FUTURE | libc::MCL_ONFAULT) == 0
    };
    if !locked {
        eprintln!(
            "its memory may be paged out: mlockall: {}",
            std::io::Error::last_os_error()
        );
    }
}

fn run(options: Options) -> Result<(), String> {
    lock_memory();
    let status_path = options
        .status
        .clone()
        .unwrap_or_else(|| match options.station() {
            Some(interface) => PathBuf::from(format!("/run/gptpd/{interface}.status")),
            None => PathBuf::from("/run/gptpd/status"),
        });
    let named_by = options.bridge.as_ref().unwrap_or(&options.ports[0]);
    let mac = raw_ether::interfaces::mac_of(named_by)
        .ok_or_else(|| format!("no interface named {named_by}"))?;
    let identity = identity_of(mac);
    let local = options.clock.as_ref().unwrap_or(&options.ports[0]);
    // An end station's settings come first: they say whether its clock
    // follows CLOCK_TAI.
    let station_settings = match options.station() {
        Some(interface) => {
            let config = options
                .config
                .clone()
                .unwrap_or_else(|| config::path(interface));
            let settings = config::load(&config)?;
            Some((config, settings))
        }
        None => None,
    };
    let follow_tai = station_settings
        .as_ref()
        .is_some_and(|(_, settings)| settings.follow_tai);
    let mut clocks = Clocks::open(&options.ports, local, follow_tai)?;
    clocks.compare();
    let interfaces = raw_ether::interfaces();
    let mut ports = Vec::new();
    for name in &options.ports {
        let wireless = interfaces
            .iter()
            .any(|interface| interface.name == *name && interface.wireless);
        ports.push(Port {
            name: name.clone(),
            socket: open(name, options.station().is_some(), wireless)?,
            up: false,
            missed: 0,
            outliers: 0,
            stamps_every_frame: false,
            distrusted: 0,
            unstamped: 0,
            unstamped_syncs: 0,
            last_unstamped_sync: 0,
            seen: VecDeque::with_capacity(REMEMBERED),
            last_sync: None,
            last_pdelay_req: None,
            off_rhythm_syncs: 0,
            off_rhythm_pdelay_reqs: 0,
            off_rhythm_answer: None,
            off_rhythm_sync: None,
            sync_interval: None,
            last_follow_up: None,
            off_rhythm_origins: 0,
            rhythm_minute: 0,
            rhythm_told: 0,
            rhythm_untold: 0,
            wireless,
            wifi: None,
        });
    }
    let end_station = match (options.station(), station_settings) {
        (Some(interface), Some((config, settings))) => {
            let station = Station::open(interface, clocks.local_clock())?;
            let socket = gptp::status::path(interface).map_err(|error| error.to_string())?;
            let server =
                Server::start(&socket).map_err(|error| format!("{}: {error}", socket.display()))?;
            eprintln!(
                "an end station on {interface}, as clock {identity}, generation {}, \
                 compared with the system's clocks {}{}",
                station.generation(),
                if station.tight() {
                    "by its driver"
                } else {
                    "by reading both"
                },
                if clocks.steering().is_some() {
                    ", its clock following CLOCK_TAI"
                } else {
                    ""
                }
            );
            let lite = Lite::new(identity, settings.profile, Duration::ZERO);
            Some(EndStation {
                station,
                server,
                settings,
                config,
                lite,
            })
        }
        _ => {
            eprintln!(
                "a time-aware relay on {}, as clock {identity}, with {}'s hardware clock the \
                 LocalClock",
                options.ports.join(", "),
                clocks.local_name()
            );
            None
        }
    };
    let mut config = Config::new(identity);
    let (utc, known) = utc_offset();
    if let Some(end) = &end_station {
        config.identity = SystemIdentity::grandmaster_capable(
            identity,
            end.settings.priority1,
            END_STATION_PRIORITY2,
        );
        config.current_utc_offset = utc;
        config.utc_offset_valid = known;
        // An end station that may fall back says it is one (profile 2.1).
        config.link.endpoint_declaration = end.settings.profile != config::Profile::Gptp;
        config.send_gptp_capable = end.settings.gptp_capable;
        config.sync_receipt_timeout = end.settings.sync_receipt_timeout;
    }
    let mut relay = Relay::new(config, ports.len() as u16);
    if let Some(end) = &end_station {
        let now = raw_ether::clock::monotonic_now().as_nanos() as i64;
        match end.station.earlier_line(now) {
            Some((local, time, rate)) => {
                // Recorded on the hardware clock; the time base runs on
                // the free one.
                let (local, rate) = clocks.free_line(local, rate);
                relay.set_time_base(TimeBase { local, time, rate });
                eprintln!(
                    "priority1 {}: as grandmaster its time goes on from the last record's, \
                     {:+.3} ppm from the hardware clock's",
                    config.identity.priority1,
                    (rate - 1.0) * 1e6
                );
            }
            None => {
                relay.set_time_base(cold_time_base(&clocks, utc)?);
                eprintln!(
                    "priority1 {}: as grandmaster its time is TAI, UTC + {utc} s ({})",
                    config.identity.priority1,
                    if known {
                        "the kernel's offset"
                    } else {
                        "assumed, as the kernel has none"
                    }
                );
            }
        }
    }
    let mut daemon = Daemon {
        identity,
        relay,
        ports,
        clocks,
        bridge: options.bridge,
        started: Instant::now(),
        changed: true,
        counts: Counts::default(),
        end_station,
        utc_offset: utc,
        gone: Vec::new(),
        wifi_support: options.wifi_support,
    };
    // An 802.11 port measures no link with Pdelay: its station, once one
    // is there, with Action frames, through its radio opened as it comes up.
    for index in 0..daemon.ports.len() {
        if daemon.ports[index].wireless {
            daemon
                .relay
                .set_wireless(Duration::ZERO, number(index), None, Support::NONE);
        }
    }
    let links = LinkChanges::open()
        .inspect_err(|error| {
            eprintln!("links are read every {SCAN_EVERY:?} only: netlink: {error}")
        })
        .ok();
    let mut next_compare = COMPARE_EVERY;
    let mut next_scan = SETTLE;
    let mut next_status = Duration::ZERO;
    let mut last_sync = None;
    loop {
        let now = daemon.now();
        let mut publish = false;
        if now >= next_compare {
            daemon.clocks.compare();
            daemon.compare_station();
            next_compare = now + COMPARE_EVERY;
            publish = true;
        }
        if now >= next_scan {
            daemon.scan();
            next_scan = now + SCAN_EVERY;
        }
        // Frames that came while the loop was busy, as through a stall, are
        // taken before any timeout: a Sync waiting unread is no Sync gone,
        // nor a response waiting a response lost.
        daemon.drain()?;
        if daemon.relay.poll_timeout().is_some_and(|due| due <= now) {
            daemon.relay.handle_timeout(now);
        }
        if let Some(end) = &mut daemon.end_station
            && end.lite.poll_timeout().is_some_and(|due| due <= now)
        {
            end.lite.handle_timeout(now);
        }
        daemon.flush();
        daemon.decide_profile();
        daemon.flush();
        daemon.answer_station();
        let synced = daemon.last_sync();
        if publish || daemon.changed || synced != last_sync {
            last_sync = synced;
            daemon.publish_station();
        }
        let now = daemon.now();
        if daemon.changed || now >= next_status {
            if let Err(error) = status::write(&status_path, &status::text(&daemon)) {
                eprintln!("cannot write {}: {error}", status_path.display());
            }
            daemon.changed = false;
            next_status = now + STATUS_EVERY;
        }
        let due = [
            Some(next_compare),
            Some(next_scan),
            daemon.relay.poll_timeout(),
            daemon
                .end_station
                .as_ref()
                .and_then(|end| end.lite.poll_timeout()),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(next_compare);
        let sockets: Vec<&StampedSocket> = daemon.ports.iter().map(|port| &port.socket).collect();
        let timeout = Some(due.saturating_sub(daemon.now()));
        // Besides the sockets, the link changes and then each radio.
        let radios: Vec<usize> = (0..daemon.ports.len())
            .filter(|index| daemon.ports[*index].wifi.is_some())
            .collect();
        let others: Vec<BorrowedFd<'_>> = links
            .iter()
            .map(|links| links.as_fd())
            .chain(
                daemon
                    .ports
                    .iter()
                    .filter_map(|port| port.wifi.as_ref().map(|wifi| wifi.as_fd())),
            )
            .collect();
        let (ready, others_ready) = stamped::wait_any_with(&sockets, &others, timeout)
            .map_err(|error| format!("waiting for gPTP frames: {error}"))?;
        let linked = links.is_some() && others_ready.contains(&0);
        let radios_ready: Vec<usize> = others_ready
            .iter()
            .filter_map(|position| radios.get(position.checked_sub(usize::from(links.is_some()))?))
            .copied()
            .collect();
        // A link that went is taken down before its frames, or a Sync
        // timeout, are made anything of; one that came is read as soon,
        // once the start has settled.
        if linked
            && let Some(links) = &links
            && links.take().unwrap_or(true)
            && daemon.now() >= SETTLE
        {
            daemon.scan();
            next_scan = daemon.now() + SCAN_EVERY;
        }
        for index in ready {
            daemon.receive(index);
            daemon.flush();
            daemon.decide_profile();
            daemon.flush();
        }
        for index in radios_ready {
            daemon.hear_radio(index);
        }
    }
}

fn main() -> ExitCode {
    let options = match options() {
        Ok(Some(options)) => options,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    // An end station checks as it starts, so that the unit's
    // RestartPreventExitStatus= sees the main process refuse.
    if let Some(interface) = options.check.as_deref().or(options.station()) {
        if refusal(interface).is_some() {
            return ExitCode::from(REFUSED);
        }
        if options.check.is_some() {
            return ExitCode::SUCCESS;
        }
    }
    match run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_is_read_even_from_a_frame_that_would_not_decode() {
        let mut frame = [0u8; 44];
        frame[0] = 0x10;
        frame[30..32].copy_from_slice(&13297u16.to_be_bytes());
        assert_eq!(header(&frame), (Some(0), Some(13297)));
        assert_eq!(header(&frame[..20]), (Some(0), None));
        assert_eq!(header(&[]), (None, None));
        assert_eq!(type_name(Some(0xa)), "Pdelay_Resp_Follow_Up");
        assert_eq!(type_name(Some(0x5)), "message");
    }

    #[test]
    fn a_log_interval_gives_its_interval_where_it_can() {
        assert_eq!(interval_nanos(-3), Some(125_000_000));
        assert_eq!(interval_nanos(0), Some(1_000_000_000));
        assert_eq!(interval_nanos(1), Some(2_000_000_000));
        assert_eq!(interval_nanos(-9), Some(1_953_125));
        assert_eq!(interval_nanos(127), None);
        assert_eq!(interval_nanos(-128), None);
        // The bench's slip: a Pdelay_Req 2.15 ms early and the Sync after
        // it 2.26 ms late are told, a Sync 6 us off is not.
        assert!(off_rhythm(997_848_000, 1_000_000_000));
        assert!(off_rhythm(127_261_000, 125_000_000));
        assert!(!off_rhythm(125_006_000, 125_000_000));
    }

    #[test]
    fn a_follow_up_gives_its_syncs_origin_with_the_correction() {
        let mut frame = [0u8; 76];
        frame[0] = 0x18;
        // 1.5 ns of correction, scaled by 2^16, comes off as 1 ns.
        frame[8..16].copy_from_slice(&(3i64 << 15).to_be_bytes());
        frame[34..40].copy_from_slice(&[0, 0, 0, 0, 0x01, 0x02]);
        frame[40..44].copy_from_slice(&125_000_000u32.to_be_bytes());
        assert_eq!(
            follow_up_origin(&frame),
            Some(258 * 1_000_000_000 + 125_000_001)
        );
        frame[8..16].copy_from_slice(&(-(1i64 << 16)).to_be_bytes());
        assert_eq!(
            follow_up_origin(&frame),
            Some(258 * 1_000_000_000 + 124_999_999)
        );
        assert_eq!(follow_up_origin(&frame[..40]), None);
    }

    #[test]
    fn a_remembered_frame_tells_how_long_before_and_its_stamp() {
        let seen = Seen {
            at: Duration::from_millis(1000),
            sent: true,
            message_type: Some(MessageType::PdelayResp as u8),
            sequence: Some(512),
            stamping: Stamping::Back {
                wait: Duration::from_micros(210),
                wire: Duration::from_secs(7),
            },
        };
        assert_eq!(
            seen.describe(Duration::from_millis(1375), Beside::default()),
            "375.0 ms before, sent Pdelay_Resp 512, its time stamp back after 0.210 ms"
        );
        let beside = Beside {
            turnaround: Some(52_340),
            ..Beside::default()
        };
        assert_eq!(
            seen.describe(Duration::from_millis(1375), beside),
            "375.0 ms before, sent Pdelay_Resp 512, its time stamp back after 0.210 ms, turnaround 52.340 us"
        );
        let sync = Seen {
            sent: false,
            message_type: Some(MessageType::Sync as u8),
            stamping: Stamping::Came(Duration::from_secs(7)),
            ..seen
        };
        let beside = Beside {
            wire_gap: Some(127_004_000),
            ..Beside::default()
        };
        assert_eq!(
            sync.describe(Duration::from_millis(1250), beside),
            "250.0 ms before, received Sync 512, stamped 127.004 ms after the Sync before"
        );
    }

    #[test]
    fn a_clock_identity_is_the_mac_made_eui_64() {
        let identity = identity_of(MacAddress([0x8c, 0x1f, 0x64, 0x36, 0xc1, 0x00]));
        assert_eq!(identity.to_string(), "8c1f64.fffe.36c100");
    }

    #[test]
    fn ports_number_from_one() {
        assert_eq!(number(0), 1);
        assert_eq!(number(4), 5);
    }

    #[test]
    fn missed_time_stamps_are_told_tenfold_apart() {
        let told: Vec<u64> = (1..=1000).filter(|count| telling(*count)).collect();
        assert_eq!(told, [1, 10, 100, 1000]);
    }
}
