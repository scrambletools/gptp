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

use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use gptp::link::Incapable;
use gptp::relay::{Config, Event, Relay, Stamp, SystemIdentity, TimeBase};
use gptp::{ClockIdentity, DESTINATION, ETHERTYPE_GPTP, MessageType};
use raw_ether::MacAddress;
use raw_ether::interfaces::LinkChanges;
use raw_ether::stamped::{self, StampedSocket};

use crate::clocks::Clocks;
use crate::lite::Lite;
use crate::server::Server;
use crate::station::{Found, Station, Steadying, View};

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
    let local = clocks.now().map_err(fail)?;
    Ok(TimeBase {
        local,
        time: local - ahead + i128::from(utc_offset) * 1_000_000_000,
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
  --version           print gptpd's version
  --help              print this";

struct Options {
    ports: Vec<String>,
    bridge: Option<String>,
    clock: Option<String>,
    config: Option<PathBuf>,
    status: Option<PathBuf>,
    check: Option<String>,
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
        for (index, port) in self.ports.iter_mut().enumerate() {
            let carrier = read(&port.name, "carrier").as_deref() == Some("1");
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
        Ok(())
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
            // Sync, Delay_Req, Pdelay_Req and Pdelay_Resp: of no use
            // without the time they came.
            let event = frame.first().is_some_and(|octet| octet & 0x0f < 4);
            let time = match received.time {
                Some(time) => self.clocks.stamp(index, time),
                None if event => continue,
                None => Stamp::local(0),
            };
            let now = self.now();
            self.relay.handle_frame(now, number(index), frame, time);
            if let Some(end) = &mut self.end_station {
                end.lite.handle_frame(now, frame, time.port);
            }
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
                let port = &self.ports[index];
                let sent = if transmit.stamp.is_some() {
                    port.socket
                        .send_stamped(MacAddress(DESTINATION), &transmit.frame)
                } else {
                    port.socket.send(MacAddress(DESTINATION), &transmit.frame)
                };
                match sent {
                    Ok(()) => {
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
                Event::SyncTimeout { .. } => self.counts.sync_timeouts += 1,
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
        let port = &mut self.ports[index];
        match port.socket.transmit_time(STAMP_WAIT) {
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
            Event::SyncTimeout { port } => format!("{}: no Sync for too long", name(port)),
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
        match end.station.compare(self.clocks.local_clock()) {
            Ok(Found::Steady) => {}
            Ok(Found::Jumped(moved)) => {
                self.changed = true;
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
        if let Err(error) = end.station.write(mapping) {
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
/// destination too.
fn open(name: &str, end_station: bool) -> Result<StampedSocket, String> {
    let fail = |error: std::io::Error| format!("{name}: {error}");
    stamped::enable(name).map_err(fail)?;
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
    let mut clocks = Clocks::open(&options.ports, local)?;
    clocks.compare();
    let mut ports = Vec::new();
    for name in &options.ports {
        ports.push(Port {
            name: name.clone(),
            socket: open(name, options.station().is_some())?,
            up: false,
            missed: 0,
            outliers: 0,
        });
    }
    let end_station = match options.station() {
        Some(interface) => {
            let config = options
                .config
                .clone()
                .unwrap_or_else(|| config::path(interface));
            let settings = config::load(&config)?;
            let station = Station::open(interface, clocks.local_clock())?;
            let socket = gptp::status::path(interface).map_err(|error| error.to_string())?;
            let server =
                Server::start(&socket).map_err(|error| format!("{}: {error}", socket.display()))?;
            eprintln!(
                "an end station on {interface}, as clock {identity}, generation {}, \
                 compared with the system's clocks {}",
                station.generation(),
                if station.tight() {
                    "by its driver"
                } else {
                    "by reading both"
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
        None => {
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
    }
    let mut relay = Relay::new(config, ports.len() as u16);
    if let Some(end) = &end_station {
        let now = raw_ether::clock::monotonic_now().as_nanos() as i64;
        match end.station.earlier_line(now) {
            Some((local, time, rate)) => {
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
    };
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
        let (ready, linked) = match &links {
            Some(links) => stamped::wait_any_or(&sockets, links.as_fd(), timeout),
            None => stamped::wait_any(&sockets, timeout).map(|ready| (ready, false)),
        }
        .map_err(|error| format!("waiting for gPTP frames: {error}"))?;
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
