//! A time-aware relay (802.1AS-2020 clause 10, on clause 11's links): the
//! BTCA choosing the grandmaster and each port's role from the Announce
//! messages heard, Announce sent on the timetransmitter ports, and each
//! Sync from the grandmaster carried on to them with its Follow_Up
//! corrected for the upstream link and the time it spent here.
//!
//! The relay never needs the grandmaster's time on a clock of its own:
//! a Sync's residence is its departure on one port's time stamp less its
//! arrival on another's, both put on the caller's free-running
//! LocalClock, scaled to the grandmaster's rate with the rate ratio the
//! Follow_Up brings and the upstream link measured. Each port's own
//! clock, which may be another, stamps what its neighbor sees, so nothing
//! the network does moves a clock that a neighbor measures.
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
    Announce, Body, ClockIdentity, Flags, FollowUpInformation, GptpCapable, Header, Message,
    MessageType, ORGANIZATION_EXTENSION_DO_NOT_PROPAGATE, PortIdentity, Timestamp, Tlv,
};

/// A system's identity as the BTCA ranks it (802.1AS-2020 10.3.2): field
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
    /// A system that is not grandmaster capable (802.1AS-2020 8.6.2):
    /// priority1 and clockClass 255.
    pub fn not_grandmaster_capable(clock: ClockIdentity) -> Self {
        Self {
            priority1: 255,
            clock_class: 255,
            clock_accuracy: 0xfe,
            offset_scaled_log_variance: 0x436a,
            priority2: 248,
            clock,
        }
    }

    /// A grandmaster capable system with 802.1AS-2020's defaults (8.6.2):
    /// clockClass 248 and its accuracy and variance unknown. priority2 is
    /// 248 for an end station and 247 for a relay (8.6.2.5); priority1 255
    /// makes the system not grandmaster capable.
    pub fn grandmaster_capable(clock: ClockIdentity, priority1: u8, priority2: u8) -> Self {
        if priority1 == 255 {
            return Self::not_grandmaster_capable(clock);
        }
        Self {
            priority1,
            clock_class: 248,
            clock_accuracy: 0xfe,
            offset_scaled_log_variance: 0x436a,
            priority2,
            clock,
        }
    }

    /// Whether the system may be grandmaster.
    pub fn may_be_grandmaster(&self) -> bool {
        self.priority1 < 255
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
    TimeReceiver,
    /// Away from the grandmaster: Announce and Sync go out here.
    TimeTransmitter,
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
    /// Sync intervals without one before the timereceiver port's
    /// information ages.
    pub sync_receipt_timeout: u8,
    /// What this system announces as grandmaster: TAI less UTC, in
    /// seconds, and whether that is known rather than assumed.
    pub current_utc_offset: i16,
    pub utc_offset_valid: bool,
    /// log2 of the seconds between gPTP-capable Signaling messages on each
    /// port (802.1AS-2020 10.6.4.4), and the intervals without one from
    /// the neighbor before its word lapses.
    pub log_gptp_capable_interval: i8,
    pub gptp_capable_receipt_timeout: u8,
    /// Whether ports send the gPTP-capable Signaling at all.
    pub send_gptp_capable: bool,
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
            current_utc_offset: 37,
            utc_offset_valid: false,
            log_gptp_capable_interval: 0,
            gptp_capable_receipt_timeout: 9,
            send_gptp_capable: true,
        }
    }
}

/// This system's time as grandmaster: `time` when the LocalClock read
/// `local`, running at `rate` times the LocalClock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeBase {
    pub local: i128,
    pub time: i128,
    pub rate: f64,
}

impl TimeBase {
    /// The time when the LocalClock reads `local`.
    pub fn at(&self, local: i128) -> i128 {
        let elapsed = local - self.local;
        self.time + elapsed + (elapsed as f64 * (self.rate - 1.0)) as i128
    }
}

impl Default for TimeBase {
    /// The LocalClock's own time.
    fn default() -> Self {
        Self {
            local: 0,
            time: 0,
            rate: 1.0,
        }
    }
}

/// When an event message crossed a port's wire: on the port's own clock,
/// as its neighbor sees it, and the same moment on the LocalClock, with
/// the port clock's rate over the LocalClock's. A port whose clock is the
/// LocalClock has [`Stamp::local`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stamp {
    pub port: i128,
    pub local: i128,
    pub rate: f64,
}

impl Stamp {
    /// A time on a port whose clock is the LocalClock.
    pub fn local(time: i128) -> Self {
        Self {
            port: time,
            local: time,
            rate: 1.0,
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
    /// No Sync came on the timereceiver port for too long (802.1AS-2020
    /// 10.3.11): its information aged.
    SyncTimeout {
        port: u16,
    },
    /// A link delay measured on `port`, in nanoseconds, stood so far from
    /// the median of those kept that it was left out.
    DelayOutlier {
        port: u16,
        delay: i64,
        median: i64,
    },
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

/// A Sync sent on a timetransmitter port, waiting for the time it left
/// and the upstream Follow_Up it relays.
#[derive(Debug, Clone, Copy)]
struct Relaying {
    sequence: u16,
    upstream: u16,
    sent: Option<Stamp>,
    /// A Sync of this system's own, as grandmaster.
    own: bool,
}

/// The timereceiver port's last Sync and, once it comes, its Follow_Up.
#[derive(Debug, Clone, Copy)]
struct Upstream {
    sequence: u16,
    received: Stamp,
    follow_up: Option<FollowedUp>,
}

#[derive(Debug, Clone, Copy)]
struct FollowedUp {
    precise_origin: Timestamp,
    correction: i64,
    information: FollowUpInformation,
    /// The grandmaster's rate over the LocalClock's.
    rate_ratio: f64,
    /// The upstream link's delay on the LocalClock, from the timereceiver
    /// port.
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
    /// When this port next says it runs gPTP, and until when its neighbor's
    /// word that it does stands.
    next_capable: Option<Duration>,
    capable_until: Option<Duration>,
    signaling_sequence: u16,
    /// The last message heard on the port, for one that comes after it
    /// without a time stamp.
    last_heard: Option<(MessageType, u16)>,
}

/// A time-aware relay across its ports.
pub struct Relay {
    config: Config,
    ports: Vec<Port>,
    root: PriorityVector,
    receiver: Option<u16>,
    upstream: Option<Upstream>,
    /// The last Sync from the grandmaster whose Follow_Up came: when it
    /// arrived, and what the Follow_Up said. It stands while the next
    /// Sync waits for its own.
    synced: Option<(i128, FollowedUp)>,
    sync_expires: Option<Duration>,
    /// This system's time as grandmaster.
    time_base: TimeBase,
    /// Changes whenever this system's time base does (802.1AS-2020
    /// 9.2.2.3), as when it takes over as grandmaster.
    time_base_indicator: u16,
    /// The last phase and frequency changes of the time base, as
    /// Follow_Up carries them (9.2.2.4, 9.2.2.5).
    last_phase_change: i128,
    last_frequency_change: i32,
    /// When this system as grandmaster sends its next Sync; none while it
    /// is not grandmaster.
    next_sync: Option<Duration>,
    /// Until when this system only listens, ranking as one that is not
    /// grandmaster capable, so a better grandmaster's Announce reaches it
    /// before it takes the role (as IEEE 1588's LISTENING state): from
    /// the start, and from a port becoming asCapable while it does not
    /// lead. `Duration::MAX` until a port first does.
    listening_until: Duration,
    outgoing: VecDeque<Transmit>,
    events: VecDeque<Event>,
}

const ANNOUNCE_CONTROL: u8 = 5;
const SYNC_CONTROL: u8 = 0;
const FOLLOW_UP_CONTROL: u8 = 2;
const SIGNALING_CONTROL: u8 = 5;
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
                next_capable: None,
                capable_until: None,
                signaling_sequence: 0,
                last_heard: None,
            })
            .collect();
        Self {
            config,
            ports,
            root: own_vector(SystemIdentity::not_grandmaster_capable(
                config.identity.clock,
            )),
            receiver: None,
            upstream: None,
            synced: None,
            sync_expires: None,
            time_base: TimeBase::default(),
            time_base_indicator: 0,
            last_phase_change: 0,
            last_frequency_change: 0,
            next_sync: None,
            listening_until: Duration::MAX,
            outgoing: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    /// When listening ends, while it is to.
    fn listening_due(&self) -> Option<Duration> {
        (self.listening_until != Duration::ZERO && self.listening_until != Duration::MAX)
            .then_some(self.listening_until)
    }

    /// How this system ranks now: as configured, or not grandmaster
    /// capable while it listens.
    fn own_identity(&self, now: Duration) -> SystemIdentity {
        if now < self.listening_until {
            SystemIdentity::not_grandmaster_capable(self.config.identity.clock)
        } else {
            self.config.identity
        }
    }

    /// Sets this system's time as grandmaster, as at a cold start from the
    /// computer's TAI. While it is grandmaster, followers see the change
    /// as a new time base.
    pub fn set_time_base(&mut self, time_base: TimeBase) {
        if self.next_sync.is_some() {
            let local = self.time_base.local;
            let phase = time_base.at(local) - self.time_base.at(local);
            self.last_phase_change = phase << 16;
            self.last_frequency_change =
                ((time_base.rate / self.time_base.rate - 1.0) * RATE_SCALE) as i32;
            self.time_base_indicator = self.time_base_indicator.wrapping_add(1);
        }
        self.time_base = time_base;
    }

    /// Changes how this system ranks, as when its priority1 is set: 255
    /// for never grandmaster. The clock identity stays.
    pub fn set_identity(&mut self, now: Duration, identity: SystemIdentity) {
        self.config.identity = SystemIdentity {
            clock: self.config.identity.clock,
            ..identity
        };
        self.select(now);
    }

    /// Whether this system is the grandmaster.
    pub fn is_grandmaster(&self) -> bool {
        self.next_sync.is_some()
    }

    /// The grandmaster's time as a line on the LocalClock: its time at a
    /// LocalClock time, and its rate over the LocalClock. Followed, from
    /// the last Sync; as grandmaster, this system's time base.
    pub fn time_line(&self) -> Option<(i128, i128, f64)> {
        if self.is_grandmaster() {
            let base = self.time_base;
            return Some((base.local, base.time, base.rate));
        }
        let (received, followed) = self.synced?;
        Some((
            received,
            time_at(&followed, received, received),
            followed.rate_ratio,
        ))
    }

    /// Port `number`'s link came up or went down.
    pub fn set_port(&mut self, now: Duration, number: u16, up: bool) {
        let sends_capable = self.config.send_gptp_capable;
        let Some(port) = self.port_mut(number) else {
            return;
        };
        if port.up == up {
            return;
        }
        port.up = up;
        port.link.set_enabled(now, up);
        port.next_capable = (up && sends_capable).then_some(now);
        if !up {
            port.information = None;
            port.capable_until = None;
            port.last_heard = None;
        }
        self.check_capable(now, number);
        self.select(now);
    }

    /// A frame heard on port `number`, the octets after the Ethernet
    /// header, and when it arrived (for event messages the hardware time
    /// stamp; others need none).
    pub fn handle_frame(&mut self, now: Duration, number: u16, frame: &[u8], received: Stamp) {
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
        self.ports[index].last_heard = Some((message.message_type(), message.header.sequence_id));
        match message.message_type() {
            MessageType::PdelayReq | MessageType::PdelayResp | MessageType::PdelayRespFollowUp => {
                let link = &mut self.ports[index].link;
                let transmit = link.handle_message(&message, received.port);
                if let Some((delay, median)) = link.take_outlier() {
                    self.events.push_back(Event::DelayOutlier {
                        port: number,
                        delay: delay as i64,
                        median: median as i64,
                    });
                }
                if let Some(transmit) = transmit {
                    self.queue(number, &transmit.message, transmit.stamp);
                }
                self.check_capable(now, number);
            }
            MessageType::Announce => self.receive_announce(now, index, &message),
            MessageType::Sync => self.receive_sync(now, index, &message, received),
            MessageType::FollowUp => self.receive_follow_up(index, &message),
            MessageType::Signaling => {
                let capable = message.tlvs().find_map(|tlv| match tlv {
                    Tlv::GptpCapable(capable) => Some(capable),
                    _ => None,
                });
                if let Some(capable) = capable {
                    let lasts = interval(capable.log_interval)
                        * u32::from(self.config.gptp_capable_receipt_timeout);
                    self.ports[index].capable_until = Some(now + lasts);
                }
            }
            // 1588's end-to-end exchange, which gPTP does not use.
            MessageType::DelayReq | MessageType::DelayResp => {}
        }
    }

    /// A frame heard on port `number` came without a hardware time stamp,
    /// from a card that stamps every frame: the frame heard just before it
    /// may carry its time, as an I210 now and then gives one frame the
    /// next one's. That frame's time is left out where it counts, a
    /// Pdelay_Resp's exchange or the timereceiver port's Sync, before its
    /// follow up comes. Says which, if either. The caller tells this
    /// before it hands over the frame, if at all.
    pub fn handle_unstamped(&mut self, number: u16) -> Option<MessageType> {
        let index = self.index(number)?;
        let (message_type, sequence) = self.ports[index].last_heard.take()?;
        match message_type {
            MessageType::PdelayResp => self.ports[index]
                .link
                .distrust_response(sequence)
                .then_some(message_type),
            MessageType::Sync => {
                let waiting = self.upstream.is_some_and(|upstream| {
                    upstream.sequence == sequence && upstream.follow_up.is_none()
                });
                if self.receiver != Some(number) || !waiting {
                    return None;
                }
                // Its Follow_Up then finds no Sync to follow: the line
                // stands as the last Sync left it.
                self.upstream = None;
                Some(message_type)
            }
            _ => None,
        }
    }

    /// The time a frame the relay asked to have stamped left port
    /// `number`.
    pub fn handle_transmit_time(
        &mut self,
        number: u16,
        message_type: MessageType,
        sequence: u16,
        time: Stamp,
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
                        .handle_transmit_time(message_type, sequence, time.port)
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
                    port.next_capable,
                    port.information
                        .as_ref()
                        .map(|information| information.expires),
                ]
            })
            .chain([self.sync_expires, self.next_sync, self.listening_due()])
            .flatten()
            .min()
    }

    pub fn handle_timeout(&mut self, now: Duration) {
        let mut reselect = false;
        if self.listening_due().is_some_and(|due| due <= now) {
            // Listened long enough: from now on it may lead.
            self.listening_until = Duration::ZERO;
            reselect = true;
        }
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
            // long ages the timereceiver port's information as no Announce
            // would.
            self.sync_expires = None;
            if let Some(index) = self.receiver.and_then(|receiver| self.index(receiver)) {
                self.ports[index].information = None;
                self.events.push_back(Event::SyncTimeout {
                    port: self.ports[index].number,
                });
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
        for index in 0..self.ports.len() {
            if self.ports[index].next_capable.is_some_and(|due| due <= now) {
                self.send_capable(now, index);
            }
        }
        if let Some(due) = self.next_sync.filter(|due| *due <= now) {
            let interval = interval(self.config.log_sync_interval);
            // Behind by more than an interval, as after a stall: from now.
            self.next_sync = Some(if now - due < interval {
                due + interval
            } else {
                now + interval
            });
            self.send_own_syncs();
        }
    }

    /// Says on the port that this system runs gPTP (802.1AS-2020
    /// 10.6.4.4).
    fn send_capable(&mut self, now: Duration, index: usize) {
        let port = &mut self.ports[index];
        let every = interval(self.config.log_gptp_capable_interval);
        port.next_capable = port.next_capable.map(|due| {
            if now - due < every {
                due + every
            } else {
                now + every
            }
        });
        port.signaling_sequence = port.signaling_sequence.wrapping_add(1);
        let (number, sequence) = (port.number, port.signaling_sequence);
        let capable = GptpCapable {
            tlv_type: ORGANIZATION_EXTENSION_DO_NOT_PROPAGATE,
            log_interval: self.config.log_gptp_capable_interval,
            flags: 0,
        };
        let mut tlv = [0; 16];
        let Ok(length) = Tlv::GptpCapable(capable).encode(&mut tlv) else {
            return;
        };
        let message = Message {
            header: self.header(number, sequence, 0, SIGNALING_CONTROL, 0x7f),
            body: Body::Signaling {
                target: PortIdentity {
                    clock: ClockIdentity(u64::MAX),
                    port: u16::MAX,
                },
            },
            tlvs: &tlv[..length],
        };
        self.queue(number, &message, false);
    }

    /// Whether port `number`'s neighbor has lately said it runs gPTP.
    pub fn neighbor_gptp_capable(&self, now: Duration, number: u16) -> bool {
        self.index(number)
            .and_then(|index| self.ports[index].capable_until)
            .is_some_and(|until| now < until)
    }

    /// This system's Sync, as grandmaster, on every timetransmitter port.
    fn send_own_syncs(&mut self) {
        for index in 0..self.ports.len() {
            let port = &mut self.ports[index];
            if port.role != Role::TimeTransmitter {
                continue;
            }
            port.sync_sequence = port.sync_sequence.wrapping_add(1);
            let sequence = port.sync_sequence;
            port.relaying = Some(Relaying {
                sequence,
                upstream: 0,
                sent: None,
                own: true,
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

    /// This system as it would announce itself.
    pub fn identity(&self) -> SystemIdentity {
        self.config.identity
    }

    /// The grandmaster, if one is present: this system, while it is.
    pub fn grandmaster(&self) -> Option<SystemIdentity> {
        (self.root.root.priority1 < 255 && (self.receiver.is_some() || self.is_grandmaster()))
            .then_some(self.root.root)
    }

    /// The grandmaster's rate over the LocalClock's, from the last
    /// Follow_Up relayed, or this system's time base's.
    pub fn rate_ratio(&self) -> Option<f64> {
        if self.is_grandmaster() {
            return Some(self.time_base.rate);
        }
        self.synced.map(|(_, followed)| followed.rate_ratio)
    }

    /// When the last Sync whose Follow_Up came arrived, on the LocalClock.
    pub fn last_sync(&self) -> Option<i128> {
        self.synced.map(|(received, _)| received)
    }

    /// The communication paths between the grandmaster and this system:
    /// 0 when it is the root, 1 with the grandmaster its neighbor.
    pub fn steps_removed(&self) -> u16 {
        // The root's vector already counts the step from the neighbor.
        match self.receiver {
            Some(_) => self.root.steps_removed,
            None => 0,
        }
    }

    /// The clocks from the grandmaster to this system, both included, as
    /// the timereceiver port's Announce traced them.
    pub fn path_trace(&self) -> Vec<ClockIdentity> {
        let mut path = self
            .receiver
            .and_then(|number| self.index(number))
            .and_then(|index| self.ports[index].information.as_ref())
            .map(|information| information.path.clone())
            .unwrap_or_default();
        path.push(self.config.identity.clock);
        path
    }

    /// The grandmaster's time, in nanoseconds, when the LocalClock reads
    /// `local`, from the last Sync and Follow_Up heard.
    pub fn grandmaster_time(&self, local: i128) -> Option<i128> {
        if self.is_grandmaster() {
            return Some(self.time_base.at(local));
        }
        let (received, followed) = self.synced?;
        Some(time_at(&followed, received, local))
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

    fn receive_sync(&mut self, now: Duration, index: usize, message: &Message, received: Stamp) {
        let port = &self.ports[index];
        if self.receiver != Some(port.number)
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
            if port.role != Role::TimeTransmitter {
                continue;
            }
            port.sync_sequence = port.sync_sequence.wrapping_add(1);
            let sequence = port.sync_sequence;
            port.relaying = Some(Relaying {
                sequence,
                upstream,
                sent: None,
                own: false,
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
        if self.receiver != Some(port.number)
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
        // The link measured on the port's clock: the neighbor's rate over
        // it, and the delay in its time.
        let port_rate = upstream.received.rate;
        let followed = FollowedUp {
            precise_origin,
            correction: message.header.correction,
            information,
            rate_ratio: upstream_rate * measured.neighbor_rate_ratio * port_rate,
            link_delay: measured.mean_link_delay / port_rate,
        };
        upstream.follow_up = Some(followed);
        self.synced = Some((upstream.received.local, followed));
        for index in 0..self.ports.len() {
            self.send_follow_up(index);
        }
    }

    /// Sends the Follow_Up of the Sync relayed on the port, once both the
    /// time it left and the upstream Follow_Up are known.
    fn send_follow_up(&mut self, index: usize) {
        if self.ports[index]
            .relaying
            .is_some_and(|relaying| relaying.own)
        {
            self.send_own_follow_up(index);
            return;
        }
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
        let elapsed = (sent.local - upstream.received.local) as f64 + followed.link_delay;
        let added = elapsed * followed.rate_ratio * CORRECTION_SCALE;
        let correction = followed.correction.saturating_add(added as i64);
        // The neighbor measures this port's clock, so the rate ratio it
        // takes on is the grandmaster's over that clock.
        let information = FollowUpInformation {
            cumulative_scaled_rate_offset: ((followed.rate_ratio / sent.rate - 1.0) * RATE_SCALE)
                as i32,
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

    /// The Follow_Up of this system's own Sync on the port, once the time
    /// it left is known: this system's time then, and its rate over the
    /// port's clock, which the neighbor measures.
    fn send_own_follow_up(&mut self, index: usize) {
        let port = &mut self.ports[index];
        let Some(relaying) = port.relaying else {
            return;
        };
        let Some(sent) = relaying.sent else {
            return;
        };
        port.relaying = None;
        let number = port.number;
        let Some(origin) = Timestamp::from_nanos(self.time_base.at(sent.local)) else {
            return;
        };
        let information = FollowUpInformation {
            cumulative_scaled_rate_offset: ((self.time_base.rate / sent.rate - 1.0) * RATE_SCALE)
                as i32,
            gm_time_base_indicator: self.time_base_indicator,
            last_gm_phase_change: self.last_phase_change,
            scaled_last_gm_freq_change: self.last_frequency_change,
        };
        let mut tlv = [0; 32];
        let Ok(length) = Tlv::FollowUpInformation(information).encode(&mut tlv) else {
            return;
        };
        let follow_up = Message {
            header: self.header(
                number,
                relaying.sequence,
                Flags::PTP_TIMESCALE,
                FOLLOW_UP_CONTROL,
                self.config.log_sync_interval,
            ),
            body: Body::FollowUp {
                precise_origin: origin,
            },
            tlvs: &tlv[..length],
        };
        self.queue(number, &follow_up, false);
    }

    fn send_announce(&mut self, now: Duration, index: usize) {
        let port = &mut self.ports[index];
        if port.role != Role::TimeTransmitter {
            port.next_announce = None;
            return;
        }
        port.next_announce = Some(now + interval(self.config.log_announce_interval));
        port.announce_sequence = port.announce_sequence.wrapping_add(1);
        let (number, sequence) = (port.number, port.announce_sequence);
        let own = self.config.identity.clock;
        let heard = self
            .receiver
            .and_then(|receiver| self.index(receiver))
            .and_then(|receiver| self.ports[receiver].information.as_ref());
        let root = self.root.root;
        let (announce, flags, mut path) = match heard {
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
                    current_utc_offset: self.config.current_utc_offset,
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
                if self.config.utc_offset_valid {
                    Flags::PTP_TIMESCALE | Flags::UTC_OFFSET_VALID
                } else {
                    Flags::PTP_TIMESCALE
                },
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
        if as_capable && self.next_sync.is_none() {
            // A new neighbor: hear its Announce before leading it.
            self.listening_until = now
                + interval(self.config.log_announce_interval)
                    * u32::from(self.config.announce_receipt_timeout);
        }
        self.select(now);
        true
    }

    /// The BTCA (802.1AS-2020 10.3.12): the best of this system and what
    /// each asCapable port heard decides the grandmaster, the timereceiver
    /// port and every other port's role.
    fn select(&mut self, now: Duration) {
        let followed = self.synced;
        let identity = self.own_identity(now);
        let own = own_vector(identity);
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
        let receiver = (best != own).then_some(best.port);
        let old_grandmaster = self.grandmaster();
        if receiver != self.receiver {
            self.upstream = None;
            self.synced = None;
            self.sync_expires = receiver.map(|_| {
                now + interval(self.config.log_sync_interval)
                    * u32::from(self.config.sync_receipt_timeout)
            });
        }
        self.root = best;
        self.receiver = receiver;
        // With no port asCapable there is no one to lead: a lone port
        // going down is no takeover.
        let grandmaster = receiver.is_none()
            && identity.may_be_grandmaster()
            && self.ports.iter().any(|port| port.as_capable);
        if grandmaster && self.next_sync.is_none() {
            // Taking over from a grandmaster gone, carry on its time, so
            // its followers see no step: a new time base all the same.
            if let Some((received, followed)) = followed {
                self.time_base = TimeBase {
                    local: received,
                    time: time_at(&followed, received, received),
                    rate: followed.rate_ratio,
                };
                self.last_phase_change = 0;
                self.last_frequency_change = 0;
            }
            self.time_base_indicator = self.time_base_indicator.wrapping_add(1);
            self.next_sync = Some(now);
        } else if !grandmaster {
            self.next_sync = None;
        }
        let clock = self.config.identity.clock;
        for port in &mut self.ports {
            let role = if !port.as_capable {
                Role::Disabled
            } else if Some(port.number) == receiver {
                Role::TimeReceiver
            } else {
                let transmitted = PriorityVector {
                    source: PortIdentity {
                        clock,
                        port: port.number,
                    },
                    port: port.number,
                    ..best
                };
                match &port.information {
                    Some(information) if information.vector < transmitted => Role::Passive,
                    _ => Role::TimeTransmitter,
                }
            };
            if role != port.role {
                port.role = role;
                port.relaying = None;
                port.next_announce = (role == Role::TimeTransmitter).then_some(now);
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

    /// Why port `number` is not asCapable, while it is not.
    pub fn incapable_because(&self, number: u16) -> Option<link::Incapable> {
        let port = &self.ports[self.index(number)?];
        if !port.up {
            return Some(link::Incapable::Down);
        }
        port.link.incapable_because()
    }

    fn index(&self, number: u16) -> Option<usize> {
        self.ports.iter().position(|port| port.number == number)
    }

    fn port_mut(&mut self, number: u16) -> Option<&mut Port> {
        self.ports.iter_mut().find(|port| port.number == number)
    }
}

/// The grandmaster's time when the LocalClock reads `local`, from a Sync
/// that arrived at `received` and its Follow_Up.
fn time_at(followed: &FollowedUp, received: i128, local: i128) -> i128 {
    let since = (local - received) as f64 + followed.link_delay;
    followed.precise_origin.nanos()
        + correction_nanos(followed.correction)
        + (since * followed.rate_ratio) as i128
}

fn own_vector(identity: SystemIdentity) -> PriorityVector {
    PriorityVector {
        root: identity,
        steps_removed: 0,
        source: PortIdentity {
            clock: identity.clock,
            port: 0,
        },
        port: 0,
    }
}

#[cfg(test)]
mod tests;
