//! avb-gptpd: an 802.1AS time-aware relay on a Linux bridge's ports. It
//! runs avb-gptp's relay with each port's gPTP frames on a raw socket,
//! hardware time stamped, and the ports' clocks made into one LocalClock
//! in software, so that no clock is ever stepped or slewed and nothing
//! the network does shows in what the neighbors measure.

mod clocks;
mod status;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use avb_gptp::relay::{Config, Event, Relay, Stamp};
use avb_gptp::{ClockIdentity, DESTINATION, ETHERTYPE_GPTP, MessageType};
use avb_net::MacAddress;
use avb_net::stamped::{self, StampedSocket};

use crate::clocks::Clocks;

/// How often the ports' clocks are compared with the LocalClock.
const COMPARE_EVERY: Duration = Duration::from_millis(100);

/// How often each port's link, and its bridge membership, is read.
const SCAN_EVERY: Duration = Duration::from_secs(1);

/// The ports start once the clocks have been compared a few times, so
/// their rates are known from the first frame.
const SETTLE: Duration = Duration::from_millis(350);

/// How often the status file is written at most, but for changes.
const STATUS_EVERY: Duration = Duration::from_secs(1);

/// How long a port may take to say when a frame went out: igc can take a
/// few ms.
const STAMP_WAIT: Duration = Duration::from_millis(20);

const USAGE: &str = "\
usage: avb-gptpd [options] INTERFACE...
  INTERFACE           a port to run gPTP on, numbered from 1 in the order
                      given
  --bridge NAME       run each port only while it is a member of this
                      Linux bridge, whose address names the system (by
                      default the first port's names it)
  --clock INTERFACE   the interface whose hardware clock is the LocalClock,
                      a port or not (the first port)
  --status PATH       where to write how gPTP stands
                      (/run/avb-gptpd/status)";

struct Options {
    ports: Vec<String>,
    bridge: Option<String>,
    clock: Option<String>,
    status: PathBuf,
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        ports: Vec::new(),
        bridge: None,
        clock: None,
        status: PathBuf::from("/run/avb-gptpd/status"),
    };
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--help" || argument == "-h" {
            return Err(String::new());
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
            "--status" => options.status = PathBuf::from(value),
            _ => return Err(format!("unknown option {argument}")),
        }
    }
    if options.ports.is_empty() {
        return Err("no ports given".to_owned());
    }
    if options.ports.len() > usize::from(u16::MAX) {
        return Err("too many ports".to_owned());
    }
    Ok(options)
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
        for (index, port) in self.ports.iter_mut().enumerate() {
            let carrier = read(&port.name, "carrier").as_deref() == Some("1");
            let member = self.bridge.as_ref().is_none_or(|bridge| {
                Path::new("/sys/class/net")
                    .join(bridge)
                    .join("brif")
                    .join(&port.name)
                    .exists()
            });
            let up = carrier && member;
            if up == port.up {
                continue;
            }
            port.up = up;
            self.changed = true;
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
            self.relay.set_port(now, number(index), up);
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
                        }
                    }
                    Err(error) => eprintln!("{}: sending gPTP: {error}", port.name),
                }
            }
            if waiting.is_empty() {
                break;
            }
            for (index, message_type, sequence) in waiting {
                self.collect(index, message_type, sequence);
            }
        }
        while let Some(event) = self.relay.poll_event() {
            self.changed = true;
            eprintln!("{}", self.describe(event));
        }
    }

    /// Gives the relay the time the event message last sent on port
    /// `index` left.
    fn collect(&mut self, index: usize, message_type: MessageType, sequence: u16) {
        let port = &mut self.ports[index];
        match port.socket.transmit_time(STAMP_WAIT) {
            Ok(Some(time)) => {
                let time = self.clocks.stamp(index, time);
                self.relay
                    .handle_transmit_time(number(index), message_type, sequence, time);
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
            Event::AsCapable { port, .. } => format!("{}: not asCapable", name(port)),
            Event::Role { port, role } => format!("{}: {}", name(port), status::role(role)),
            Event::Grandmaster(Some(grandmaster)) => format!(
                "grandmaster {}, priority1 {}, clock class {}",
                grandmaster.clock, grandmaster.priority1, grandmaster.clock_class
            ),
            Event::Grandmaster(None) => "no grandmaster".to_owned(),
        }
    }
}

/// One of an interface's sysfs attributes, trimmed.
fn read(interface: &str, attribute: &str) -> Option<String> {
    let path = Path::new("/sys/class/net").join(interface).join(attribute);
    Some(std::fs::read_to_string(path).ok()?.trim().to_owned())
}

fn open(name: &str) -> Result<StampedSocket, String> {
    let fail = |error: std::io::Error| format!("{name}: {error}");
    stamped::enable(name).map_err(fail)?;
    let socket = StampedSocket::open(name, ETHERTYPE_GPTP).map_err(fail)?;
    socket
        .join_multicast(MacAddress(DESTINATION))
        .map_err(fail)?;
    Ok(socket)
}

fn run(options: Options) -> Result<(), String> {
    let named_by = options.bridge.as_ref().unwrap_or(&options.ports[0]);
    let mac = avb_net::interfaces::mac_of(named_by)
        .ok_or_else(|| format!("no interface named {named_by}"))?;
    let identity = identity_of(mac);
    let local = options.clock.as_ref().unwrap_or(&options.ports[0]);
    let mut clocks = Clocks::open(&options.ports, local)?;
    clocks.compare();
    let mut ports = Vec::new();
    for name in &options.ports {
        ports.push(Port {
            name: name.clone(),
            socket: open(name)?,
            up: false,
            missed: 0,
        });
    }
    eprintln!(
        "a time-aware relay on {}, as clock {identity}, with {}'s hardware clock the LocalClock",
        options.ports.join(", "),
        clocks.local_name()
    );
    let relay = Relay::new(Config::new(identity), ports.len() as u16);
    let mut daemon = Daemon {
        identity,
        relay,
        ports,
        clocks,
        bridge: options.bridge,
        started: Instant::now(),
        changed: true,
    };
    let mut next_compare = COMPARE_EVERY;
    let mut next_scan = SETTLE;
    let mut next_status = Duration::ZERO;
    loop {
        let now = daemon.now();
        if now >= next_compare {
            daemon.clocks.compare();
            next_compare = now + COMPARE_EVERY;
        }
        if now >= next_scan {
            daemon.scan();
            next_scan = now + SCAN_EVERY;
        }
        if daemon.relay.poll_timeout().is_some_and(|due| due <= now) {
            daemon.relay.handle_timeout(now);
        }
        daemon.flush();
        let now = daemon.now();
        if daemon.changed || now >= next_status {
            if let Err(error) = status::write(&options.status, &status::text(&daemon)) {
                eprintln!("cannot write {}: {error}", options.status.display());
            }
            daemon.changed = false;
            next_status = now + STATUS_EVERY;
        }
        let due = [
            Some(next_compare),
            Some(next_scan),
            daemon.relay.poll_timeout(),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(next_compare);
        let sockets: Vec<&StampedSocket> = daemon.ports.iter().map(|port| &port.socket).collect();
        let ready = stamped::wait_any(&sockets, Some(due.saturating_sub(daemon.now())))
            .map_err(|error| format!("waiting for gPTP frames: {error}"))?;
        for index in ready {
            daemon.receive(index);
            daemon.flush();
        }
    }
}

fn main() -> ExitCode {
    let options = match options() {
        Ok(options) => options,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("{message}");
            }
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
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
