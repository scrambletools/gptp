//! A time-aware relay (802.1AS-2020 clause 10, on clause 11's links): the
//! BMCA choosing the grandmaster and each port's role from the Announce
//! messages heard, Announce sent on the master ports, and each Sync from
//! the grandmaster carried on to them with its Follow_Up corrected for
//! the upstream link and the time it spent here.
//!
//! The relay never needs the grandmaster's time on a clock of its own:
//! a Sync's residence is its departure on one port's time stamp less its
//! arrival on another's, both on the caller's free-running LocalClock,
//! scaled to the grandmaster's rate with the rate ratio the Follow_Up
//! brings and the upstream link measured. So nothing the network does
//! moves a clock that a neighbor measures.
//!
//! It is not grandmaster capable: when no Announce names a grandmaster
//! better than this system's own priority, the relay announces itself, as
//! a root with no grandmaster present, and sends no Sync.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use crate::link::{self, Link, correction_nanos, interval};
use crate::message::{
    Announce, Body, ClockIdentity, Flags, FollowUpInformation, Header, Message, MessageType,
    PortIdentity, Timestamp, Tlv,
};

/// A system's identity as the BMCA ranks it (802.1AS-2020 10.3.2): field
/// by field, lower first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SystemIdentity {
    pub priority1: u8,
    pub clock_class: u8,
    pub clock_accuracy: u8,
    pub offset_scaled_log_variance: u16,
    pub priority2: u8,
    pub clock: ClockIdentity,
}

impl SystemIdentity {
    /// A bridge that is not grandmaster capable (802.1AS-2020 8.6.2).
    pub fn not_grandmaster_capable(clock: ClockIdentity) -> Self {
        Self {
            priority1: 255,
            clock_class: 248,
            clock_accuracy: 0xfe,
            offset_scaled_log_variance: 0x4e5d,
            priority2: 248,
            clock,
        }
    }

    fn of(announce: &Announce) -> Self {
        Self {
            priority1: announce.priority1,
            clock_class: announce.clock_class,
            clock_accuracy: announce.clock_accuracy,
            offset_scaled_log_variance: announce.offset_scaled_log_variance,
            priority2: announce.priority2,
            clock: announce.grandmaster,
        }
    }
}

/// A priority vector (802.1AS-2020 10.3.4): the grandmaster, how many
/// systems away, the port it was heard from and the port that heard it,
/// ranked field by field, lower first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PriorityVector {
    pub root: SystemIdentity,
    pub steps_removed: u16,
    pub source: PortIdentity,
    pub port: u16,
}

/// A port's role (802.1AS-2020 10.3.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Down, or its neighbor not asCapable.
    Disabled,
    /// Towards the grandmaster: its Sync is relayed from here.
    Slave,
    /// Away from the grandmaster: Announce and Sync go out here.
    Master,
    /// A better path to the grandmaster than this system's lies beyond.
    Passive,
}

/// How the relay runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    pub identity: SystemIdentity,
    pub link: link::Config,
    pub log_announce_interval: i8,
    pub log_sync_interval: i8,
    /// Announce intervals without one before a port's information ages.
    pub announce_receipt_timeout: u8,
    /// Sync intervals without one before the slave port's information ages.
    pub sync_receipt_timeout: u8,
}

impl Config {
    pub fn new(clock: ClockIdentity) -> Self {
        Self {
            identity: SystemIdentity::not_grandmaster_capable(clock),
            link: link::Config::default(),
            log_announce_interval: 0,
            log_sync_interval: -3,
            announce_receipt_timeout: 3,
            sync_receipt_timeout: 3,
        }
    }
}

/// A frame to send on a port, the octets after its Ethernet header, and
/// the message whose departure the caller is to report with
/// [`Relay::handle_transmit_time`], if it is an event message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmit {
    pub port: u16,
    pub frame: Vec<u8>,
    pub stamp: Option<(MessageType, u16)>,
}

/// A change the caller may want to know of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    AsCapable {
        port: u16,
        as_capable: bool,
    },
    Role {
        port: u16,
        role: Role,
    },
    /// The grandmaster now, or none present.
    Grandmaster(Option<SystemIdentity>),
}

/// What a port last heard in an Announce, while it lasts.
#[derive(Debug, Clone, PartialEq)]
struct Information {
    vector: PriorityVector,
    announce: Announce,
    flags: Flags,
    path: Vec<ClockIdentity>,
    expires: Duration,
}

/// A Sync sent on a master port, waiting for the time it left and the
/// upstream Follow_Up it relays.
#[derive(Debug, Clone, Copy)]
struct Relaying {
    sequence: u16,
    upstream: u16,
    sent: Option<i128>,
}

/// The slave port's last Sync and, once it comes, its Follow_Up.
#[derive(Debug, Clone, Copy)]
struct Upstream {
    sequence: u16,
    received: i128,
    follow_up: Option<FollowedUp>,
}

#[derive(Debug, Clone, Copy)]
struct FollowedUp {
    precise_origin: Timestamp,
    correction: i64,
    information: FollowUpInformation,
    /// The grandmaster's rate over the LocalClock's.
    rate_ratio: f64,
    /// The upstream link's delay on the LocalClock, from the slave port.
    link_delay: f64,
}

struct Port {
    number: u16,
    link: Link,
    up: bool,
    as_capable: bool,
    role: Role,
    information: Option<Information>,
    next_announce: Option<Duration>,
    announce_sequence: u16,
    sync_sequence: u16,
    relaying: Option<Relaying>,
}

/// A time-aware relay across its ports.
pub struct Relay {
    config: Config,
    ports: Vec<Port>,
    root: PriorityVector,
    slave: Option<u16>,
    upstream: Option<Upstream>,
    sync_expires: Option<Duration>,
    outgoing: VecDeque<Transmit>,
    events: VecDeque<Event>,
}

const ANNOUNCE_CONTROL: u8 = 5;
const SYNC_CONTROL: u8 = 0;
const FOLLOW_UP_CONTROL: u8 = 2;
const RATE_SCALE: f64 = (1u64 << 41) as f64;
const CORRECTION_SCALE: f64 = 65_536.0;
/// timeSource of a system announcing itself: its own oscillator.
const INTERNAL_OSCILLATOR: u8 = 0xa0;

impl Relay {
    /// A relay with ports numbered `1..=ports`, all down.
    pub fn new(config: Config, ports: u16) -> Self {
        let clock = config.identity.clock;
        let ports = (1..=ports)
            .map(|number| Port {
                number,
                link: Link::new(
                    PortIdentity {
                        clock,
                        port: number,
                    },
                    config.link,
                ),
                up: false,
                as_capable: false,
                role: Role::Disabled,
                information: None,
                next_announce: None,
                announce_sequence: 0,
                sync_sequence: 0,
                relaying: None,
            })
            .collect();
        Self {
            config,
            ports,
            root: own_vector(&config),
            slave: None,
            upstream: None,
            sync_expires: None,
            outgoing: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    /// Port `number`'s link came up or went down.
    pub fn set_port(&mut self, now: Duration, number: u16, up: bool) {
        let Some(port) = self.port_mut(number) else {
            return;
        };
        if port.up == up {
            return;
        }
        port.up = up;
        port.link.set_enabled(now, up);
        if !up {
            port.information = None;
        }
        self.check_capable(now, number);
        self.select(now);
    }

    /// A frame heard on port `number`, the octets after the Ethernet
    /// header, and when it arrived on the LocalClock (for event messages
    /// the hardware time stamp; others need none).
    pub fn handle_frame(&mut self, now: Duration, number: u16, frame: &[u8], received: i128) {
        let Ok(message) = Message::decode(frame) else {
            return;
        };
        let own = self.config.identity.clock;
        if message.header.major_sdo_id != 1
            || message.header.domain != 0
            || message.header.source.clock == own
        {
            return;
        }
        let Some(index) = self.index(number) else {
            return;
        };
        if !self.ports[index].up {
            return;
        }
        match message.message_type() {
            MessageType::PdelayReq | MessageType::PdelayResp | MessageType::PdelayRespFollowUp => {
                if let Some(transmit) = self.ports[index].link.handle_message(&message, received) {
                    self.queue(number, &transmit.message, transmit.stamp);
                }
                self.check_capable(now, number);
            }
            MessageType::Announce => self.receive_announce(now, index, &message),
            MessageType::Sync => self.receive_sync(now, index, &message, received),
            MessageType::FollowUp => self.receive_follow_up(index, &message),
            MessageType::Signaling => {}
        }
    }

    /// The time a frame the relay asked to have stamped left port
    /// `number`, on the LocalClock.
    pub fn handle_transmit_time(
        &mut self,
        number: u16,
        message_type: MessageType,
        sequence: u16,
        time: i128,
    ) {
        let Some(index) = self.index(number) else {
            return;
        };
        match message_type {
            MessageType::Sync => {
                if let Some(relaying) = self.ports[index]
                    .relaying
                    .as_mut()
                    .filter(|relaying| relaying.sequence == sequence)
                {
                    relaying.sent = Some(time);
                }
                self.send_follow_up(index);
            }
            _ => {
                if let Some(transmit) =
                    self.ports[index]
                        .link
                        .handle_transmit_time(message_type, sequence, time)
                {
                    self.queue(number, &transmit.message, transmit.stamp);
                }
            }
        }
    }

    /// When [`handle_timeout`](Self::handle_timeout) is next due.
    pub fn poll_timeout(&self) -> Option<Duration> {
        self.ports
            .iter()
            .flat_map(|port| {
                [
                    port.link.poll_timeout(),
                    port.next_announce,
                    port.information
                        .as_ref()
                        .map(|information| information.expires),
                ]
            })
            .chain([self.sync_expires])
            .flatten()
            .min()
    }

    pub fn handle_timeout(&mut self, now: Duration) {
        let mut reselect = false;
        for index in 0..self.ports.len() {
            let number = self.ports[index].number;
            if let Some(transmit) = self.ports[index].link.handle_timeout(now) {
                self.queue(number, &transmit.message, transmit.stamp);
            }
            reselect |= self.check_capable(now, number);
            let port = &mut self.ports[index];
            if port
                .information
                .as_ref()
                .is_some_and(|information| information.expires <= now)
            {
                port.information = None;
                reselect = true;
            }
        }
        if self.sync_expires.is_some_and(|expires| expires <= now) {
            // 802.1AS-2020 10.3.11: no Sync from the grandmaster for too
            // long ages the slave port's information as no Announce would.
            self.sync_expires = None;
            if let Some(index) = self.slave.and_then(|slave| self.index(slave)) {
                self.ports[index].information = None;
                reselect = true;
            }
        }
        if reselect {
            self.select(now);
        }
        for index in 0..self.ports.len() {
            if self.ports[index]
                .next_announce
                .is_some_and(|due| due <= now)
            {
                self.send_announce(now, index);
            }
        }
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.outgoing.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub fn as_capable(&self, number: u16) -> bool {
        self.index(number)
            .is_some_and(|index| self.ports[index].as_capable)
    }

    pub fn role(&self, number: u16) -> Option<Role> {
        self.index(number).map(|index| self.ports[index].role)
    }

    /// The port's link to its neighbor, as measured.
    pub fn link(&self, number: u16) -> Option<link::Measurement> {
        self.index(number)
            .and_then(|index| self.ports[index].link.measurement())
    }

    /// The grandmaster, if one is present.
    pub fn grandmaster(&self) -> Option<SystemIdentity> {
        (self.slave.is_some() && self.root.root.priority1 < 255).then_some(self.root.root)
    }

    /// The grandmaster's rate over the LocalClock's, from the last
    /// Follow_Up relayed.
    pub fn rate_ratio(&self) -> Option<f64> {
        self.upstream?.follow_up.map(|followed| followed.rate_ratio)
    }

    /// The grandmaster's time, in nanoseconds, when the LocalClock reads
    /// `local`, from the last Sync and Follow_Up heard.
    pub fn grandmaster_time(&self, local: i128) -> Option<i128> {
        let upstream = self.upstream?;
        let followed = upstream.follow_up?;
        let since = (local - upstream.received) as f64 + followed.link_delay;
        Some(
            followed.precise_origin.nanos()
                + correction_nanos(followed.correction)
                + (since * followed.rate_ratio) as i128,
        )
    }

    fn receive_announce(&mut self, now: Duration, index: usize, message: &Message) {
        let Body::Announce(announce) = message.body else {
            return;
        };
        let own = self.config.identity.clock;
        let port = &mut self.ports[index];
        // 802.1AS-2020 10.3.11.2.2: Announce from a port not asCapable is
        // not qualified, nor one that has passed through this system.
        if !port.as_capable || announce.steps_removed >= 255 {
            return;
        }
        let path: Vec<ClockIdentity> = message
            .path_trace()
            .map(|path| path.iter().collect())
            .unwrap_or_default();
        if path.contains(&own) {
            return;
        }
        let vector = PriorityVector {
            root: SystemIdentity::of(&announce),
            steps_removed: announce.steps_removed,
            source: message.header.source,
            port: port.number,
        };
        let expires = now
            + interval(message.header.log_interval)
                * u32::from(self.config.announce_receipt_timeout);
        let changed = port
            .information
            .as_ref()
            .is_none_or(|information| information.vector != vector);
        port.information = Some(Information {
            vector,
            announce,
            flags: message.header.flags,
            path,
            expires,
        });
        if changed {
            self.select(now);
        }
    }

    fn receive_sync(&mut self, now: Duration, index: usize, message: &Message, received: i128) {
        let port = &self.ports[index];
        if self.slave != Some(port.number)
            || !port.as_capable
            || !message.header.flags.two_step()
            || port
                .information
                .as_ref()
                .is_none_or(|information| information.vector.source != message.header.source)
        {
            return;
        }
        self.sync_expires = Some(
            now + interval(message.header.log_interval)
                * u32::from(self.config.sync_receipt_timeout),
        );
        let upstream = message.header.sequence_id;
        self.upstream = Some(Upstream {
            sequence: upstream,
            received,
            follow_up: None,
        });
        for index in 0..self.ports.len() {
            let port = &mut self.ports[index];
            if port.role != Role::Master {
                continue;
            }
            port.sync_sequence = port.sync_sequence.wrapping_add(1);
            let sequence = port.sync_sequence;
            port.relaying = Some(Relaying {
                sequence,
                upstream,
                sent: None,
            });
            let number = port.number;
            let sync = Message {
                header: self.header(
                    number,
                    sequence,
                    Flags::TWO_STEP | Flags::PTP_TIMESCALE,
                    SYNC_CONTROL,
                    self.config.log_sync_interval,
                ),
                body: Body::Sync {
                    origin: Timestamp::default(),
                },
                tlvs: &[],
            };
            self.queue(number, &sync, true);
        }
    }

    fn receive_follow_up(&mut self, index: usize, message: &Message) {
        let Body::FollowUp { precise_origin } = message.body else {
            return;
        };
        let port = &self.ports[index];
        let Some(upstream) = self.upstream.as_mut() else {
            return;
        };
        if self.slave != Some(port.number)
            || upstream.sequence != message.header.sequence_id
            || port
                .information
                .as_ref()
                .is_none_or(|information| information.vector.source != message.header.source)
        {
            return;
        }
        let Some(measured) = port.link.measurement() else {
            return;
        };
        let information = message.follow_up_information().unwrap_or_default();
        let upstream_rate = 1.0 + f64::from(information.cumulative_scaled_rate_offset) / RATE_SCALE;
        upstream.follow_up = Some(FollowedUp {
            precise_origin,
            correction: message.header.correction,
            information,
            rate_ratio: upstream_rate * measured.neighbor_rate_ratio,
            link_delay: measured.mean_link_delay,
        });
        for index in 0..self.ports.len() {
            self.send_follow_up(index);
        }
    }

    /// Sends the Follow_Up of the Sync relayed on the port, once both the
    /// time it left and the upstream Follow_Up are known.
    fn send_follow_up(&mut self, index: usize) {
        let Some(upstream) = self.upstream else {
            return;
        };
        let Some(followed) = upstream.follow_up else {
            return;
        };
        let port = &mut self.ports[index];
        let Some(relaying) = port.relaying else {
            return;
        };
        let Some(sent) = relaying.sent else {
            return;
        };
        if relaying.upstream != upstream.sequence {
            return;
        }
        port.relaying = None;
        let number = port.number;
        // 802.1AS-2020 11.2.15.2.1: the correction grows by the time since
        // the upstream system sent the Sync, the link delay and the
        // residence here, on the grandmaster's time base.
        let elapsed = (sent - upstream.received) as f64 + followed.link_delay;
        let added = elapsed * followed.rate_ratio * CORRECTION_SCALE;
        let correction = followed.correction.saturating_add(added as i64);
        let information = FollowUpInformation {
            cumulative_scaled_rate_offset: ((followed.rate_ratio - 1.0) * RATE_SCALE) as i32,
            ..followed.information
        };
        let mut tlv = [0; 32];
        let Ok(length) = Tlv::FollowUpInformation(information).encode(&mut tlv) else {
            return;
        };
        let mut header = self.header(
            number,
            relaying.sequence,
            Flags::PTP_TIMESCALE,
            FOLLOW_UP_CONTROL,
            self.config.log_sync_interval,
        );
        header.correction = correction;
        let follow_up = Message {
            header,
            body: Body::FollowUp {
                precise_origin: followed.precise_origin,
            },
            tlvs: &tlv[..length],
        };
        self.queue(number, &follow_up, false);
    }

    fn send_announce(&mut self, now: Duration, index: usize) {
        let port = &mut self.ports[index];
        if port.role != Role::Master {
            port.next_announce = None;
            return;
        }
        port.next_announce = Some(now + interval(self.config.log_announce_interval));
        port.announce_sequence = port.announce_sequence.wrapping_add(1);
        let (number, sequence) = (port.number, port.announce_sequence);
        let own = self.config.identity.clock;
        let slave = self
            .slave
            .and_then(|slave| self.index(slave))
            .and_then(|slave| self.ports[slave].information.as_ref());
        let root = self.root.root;
        let (announce, flags, mut path) = match slave {
            Some(information) => (
                Announce {
                    steps_removed: self.root.steps_removed,
                    ..information.announce
                },
                information.flags.0,
                information.path.clone(),
            ),
            None => (
                Announce {
                    origin: Timestamp::default(),
                    current_utc_offset: 0,
                    reserved: 0,
                    priority1: root.priority1,
                    clock_class: root.clock_class,
                    clock_accuracy: root.clock_accuracy,
                    offset_scaled_log_variance: root.offset_scaled_log_variance,
                    priority2: root.priority2,
                    grandmaster: root.clock,
                    steps_removed: 0,
                    time_source: INTERNAL_OSCILLATOR,
                },
                Flags::PTP_TIMESCALE,
                Vec::new(),
            ),
        };
        path.push(own);
        let path_octets: Vec<u8> = path
            .iter()
            .flat_map(|clock| clock.0.to_be_bytes())
            .collect();
        let Some(tlv) = Tlv::path_trace(&path_octets) else {
            return;
        };
        let mut tlvs = vec![0; tlv.encoded_len()];
        if tlv.encode(&mut tlvs).is_err() {
            return;
        }
        let message = Message {
            header: self.header(
                number,
                sequence,
                flags,
                ANNOUNCE_CONTROL,
                self.config.log_announce_interval,
            ),
            body: Body::Announce(Announce {
                origin: Timestamp::default(),
                ..announce
            }),
            tlvs: &tlvs,
        };
        self.queue(number, &message, false);
    }

    /// Notes a port's asCapable, reporting a change; whether it changed.
    fn check_capable(&mut self, now: Duration, number: u16) -> bool {
        let Some(index) = self.index(number) else {
            return false;
        };
        let port = &mut self.ports[index];
        let as_capable = port.up && port.link.as_capable();
        if as_capable == port.as_capable {
            return false;
        }
        port.as_capable = as_capable;
        if !as_capable {
            port.information = None;
        }
        self.events.push_back(Event::AsCapable {
            port: number,
            as_capable,
        });
        self.select(now);
        true
    }

    /// The BMCA (802.1AS-2020 10.3.12): the best of this system and what
    /// each asCapable port heard decides the grandmaster, the slave port
    /// and every other port's role.
    fn select(&mut self, now: Duration) {
        let own = own_vector(&self.config);
        let best = self
            .ports
            .iter()
            .filter(|port| port.as_capable)
            .filter_map(|port| port.information.as_ref())
            .map(|information| PriorityVector {
                steps_removed: information.vector.steps_removed.saturating_add(1),
                ..information.vector
            })
            .chain([own])
            .min()
            .expect("own vector");
        let slave = (best != own).then_some(best.port);
        let old_grandmaster = self.grandmaster();
        if slave != self.slave {
            self.upstream = None;
            self.sync_expires = slave.map(|_| {
                now + interval(self.config.log_sync_interval)
                    * u32::from(self.config.sync_receipt_timeout)
            });
        }
        self.root = best;
        self.slave = slave;
        let clock = self.config.identity.clock;
        for port in &mut self.ports {
            let role = if !port.as_capable {
                Role::Disabled
            } else if Some(port.number) == slave {
                Role::Slave
            } else {
                let master = PriorityVector {
                    source: PortIdentity {
                        clock,
                        port: port.number,
                    },
                    port: port.number,
                    ..best
                };
                match &port.information {
                    Some(information) if information.vector < master => Role::Passive,
                    _ => Role::Master,
                }
            };
            if role != port.role {
                port.role = role;
                port.relaying = None;
                port.next_announce = (role == Role::Master).then_some(now);
                self.events.push_back(Event::Role {
                    port: port.number,
                    role,
                });
            }
        }
        let grandmaster = self.grandmaster();
        if grandmaster != old_grandmaster {
            self.events.push_back(Event::Grandmaster(grandmaster));
        }
    }

    fn header(
        &self,
        number: u16,
        sequence: u16,
        flags: u16,
        control: u8,
        log_interval: i8,
    ) -> Header {
        Header {
            major_sdo_id: 1,
            minor_version: self.config.link.minor_version,
            version: 2,
            domain: 0,
            minor_sdo_id: 0,
            flags: Flags(flags),
            correction: 0,
            type_specific: 0,
            source: PortIdentity {
                clock: self.config.identity.clock,
                port: number,
            },
            sequence_id: sequence,
            control,
            log_interval,
        }
    }

    fn queue(&mut self, number: u16, message: &Message, stamp: bool) {
        let mut frame = vec![0; message.encoded_len()];
        if message.encode(&mut frame).is_err() {
            return;
        }
        self.outgoing.push_back(Transmit {
            port: number,
            frame,
            stamp: stamp.then_some((message.message_type(), message.header.sequence_id)),
        });
    }

    fn index(&self, number: u16) -> Option<usize> {
        self.ports.iter().position(|port| port.number == number)
    }

    fn port_mut(&mut self, number: u16) -> Option<&mut Port> {
        self.ports.iter_mut().find(|port| port.number == number)
    }
}

fn own_vector(config: &Config) -> PriorityVector {
    PriorityVector {
        root: config.identity,
        steps_removed: 0,
        source: PortIdentity {
            clock: config.identity.clock,
            port: 0,
        },
        port: 0,
    }
}

#[cfg(test)]
mod tests;
