//! IEEE 1588's end-to-end delay request-response mechanism, for an
//! ordinary clock that follows, as the AVB Lite PTP profile runs it
//! (profile 5): the grandmaster's Announce, Sync and Follow_Up come to
//! 01:1B:19:00:00:00 through switches that know nothing of PTP, and
//! Delay_Req and Delay_Resp measure the path end to end.
//!
//! Like the relay, it does no I/O and moves no clock: the caller hands it
//! frames with the time they arrived on the port's clock, and takes back
//! frames to send and the grandmaster's time as a line on that clock.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use crate::link::{correction_nanos, interval};
use crate::message::{Body, Flags, Header, Message, MessageType, PortIdentity, Timestamp};
use crate::relay::SystemIdentity;

/// Where the profile sends every message.
pub const DESTINATION: [u8; 6] = crate::lite::PTP_DESTINATION;

const DELAY_REQ_CONTROL: u8 = 1;
/// logMessageInterval of a message with no interval to give.
const NO_INTERVAL: i8 = 0x7f;

/// How many Syncs the grandmaster's rate is fitted over: 2 s at 8 a second.
const RATE_SPAN: usize = 16;
/// How many path delays the delay is the median of.
const DELAY_SPAN: usize = 9;

/// How the client runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    pub identity: PortIdentity,
    pub domain: u8,
    /// majorSdoId, transportSpecific in 1588-2008: 0 for the profile.
    pub major_sdo_id: u8,
    pub minor_version: u8,
    pub log_delay_req_interval: i8,
    /// Announce intervals without one before a sender's ages.
    pub announce_receipt_timeout: u8,
    /// delayAsymmetry in nanoseconds (IEEE 1588-2019 7.4.2): how much
    /// longer the path from the grandmaster takes than the path back,
    /// halved. AVB Lite's comes of the two links' speeds
    /// ([`crate::lite::link_asymmetry`]).
    pub asymmetry: i64,
}

impl Config {
    /// The AVB Lite PTP profile's settings: domain 0, Delay_Req each
    /// second.
    pub fn avb_lite(identity: PortIdentity) -> Self {
        Self {
            identity,
            domain: 0,
            major_sdo_id: 0,
            minor_version: 0,
            log_delay_req_interval: 0,
            announce_receipt_timeout: 3,
            asymmetry: 0,
        }
    }
}

/// A frame to send to [`DESTINATION`], and whether to report when it left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmit {
    pub frame: Vec<u8>,
    pub stamp: Option<(MessageType, u16)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The grandmaster followed now, or none.
    Grandmaster(Option<SystemIdentity>),
}

/// A sender's last Announce, while it lasts.
#[derive(Debug, Clone, Copy)]
struct Heard {
    source: PortIdentity,
    system: SystemIdentity,
    steps_removed: u16,
    /// The grandmaster's link speed, from its Grandmaster Link TLV.
    link_speed: Option<u32>,
    expires: Duration,
}

impl Heard {
    /// Ranked as the BTCA ranks it, lower first.
    fn rank(&self) -> (SystemIdentity, u16, PortIdentity) {
        (self.system, self.steps_removed, self.source)
    }
}

/// A two-step Sync waiting for its Follow_Up.
#[derive(Debug, Clone, Copy)]
struct Waiting {
    sequence: u16,
    received: i128,
    correction: i64,
}

pub struct Client {
    config: Config,
    up: bool,
    heard: Vec<Heard>,
    transmitter: Option<Heard>,
    waiting: Option<Waiting>,
    /// The grandmaster's origin of each recent Sync, against when it
    /// arrived here.
    syncs: VecDeque<(i128, i128)>,
    rate: Option<f64>,
    delays: VecDeque<f64>,
    /// The Delay_Req awaiting its response: its sequence, and when it left.
    request: Option<(u16, Option<i128>)>,
    request_sequence: u16,
    next_request: Option<Duration>,
    outgoing: VecDeque<Transmit>,
    events: VecDeque<Event>,
}

impl Client {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            up: false,
            heard: Vec::new(),
            transmitter: None,
            waiting: None,
            syncs: VecDeque::new(),
            rate: None,
            delays: VecDeque::new(),
            request: None,
            request_sequence: 0,
            next_request: None,
            outgoing: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    /// The link came up or went down: what was heard over it goes.
    pub fn set_link(&mut self, now: Duration, up: bool) {
        if up == self.up {
            return;
        }
        self.up = up;
        self.heard.clear();
        self.select(now);
    }

    /// The delayAsymmetry to apply, as when the grandmaster's link speed
    /// becomes known.
    pub fn set_asymmetry(&mut self, asymmetry: i64) {
        self.config.asymmetry = asymmetry;
    }

    /// A frame heard, the octets after the Ethernet header, and when it
    /// arrived on the port's clock (for event messages; others need none).
    pub fn handle_frame(&mut self, now: Duration, frame: &[u8], received: i128) {
        if !self.up {
            return;
        }
        let Ok(message) = Message::decode(frame) else {
            return;
        };
        let header = &message.header;
        if header.major_sdo_id != self.config.major_sdo_id
            || header.domain != self.config.domain
            || header.source.clock == self.config.identity.clock
        {
            return;
        }
        match message.body {
            Body::Announce(announce) => {
                let heard = Heard {
                    source: header.source,
                    system: SystemIdentity {
                        priority1: announce.priority1,
                        clock_class: announce.clock_class,
                        clock_accuracy: announce.clock_accuracy,
                        offset_scaled_log_variance: announce.offset_scaled_log_variance,
                        priority2: announce.priority2,
                        clock: announce.grandmaster,
                    },
                    steps_removed: announce.steps_removed,
                    link_speed: crate::lite::grandmaster_link(frame).map(|(_, speed)| speed),
                    expires: now
                        + interval(header.log_interval)
                            * u32::from(self.config.announce_receipt_timeout),
                };
                // Only a grandmaster capable one: priority1 255 never is.
                if heard.system.priority1 == 255 {
                    return;
                }
                self.heard.retain(|each| each.source != heard.source);
                self.heard.push(heard);
                self.select(now);
            }
            Body::Sync { origin } if self.sent_by_transmitter(header) => {
                if header.flags.two_step() {
                    self.waiting = Some(Waiting {
                        sequence: header.sequence_id,
                        received,
                        correction: header.correction,
                    });
                } else {
                    let origin = origin.nanos() + correction_nanos(header.correction);
                    self.synced(received, origin);
                }
            }
            Body::FollowUp { precise_origin } if self.sent_by_transmitter(header) => {
                let Some(waiting) = self
                    .waiting
                    .take()
                    .filter(|waiting| waiting.sequence == header.sequence_id)
                else {
                    return;
                };
                let origin = precise_origin.nanos()
                    + correction_nanos(waiting.correction)
                    + correction_nanos(header.correction);
                self.synced(waiting.received, origin);
            }
            Body::DelayResp {
                receive,
                requesting,
            } if self.sent_by_transmitter(header) && requesting == self.config.identity => {
                let Some((sequence, Some(sent))) = self.request else {
                    return;
                };
                if sequence != header.sequence_id {
                    return;
                }
                self.request = None;
                let arrived = receive.nanos() - correction_nanos(header.correction);
                self.measured(sent, arrived);
            }
            _ => {}
        }
    }

    /// When the Delay_Req last sent left, on the port's clock.
    pub fn handle_transmit_time(&mut self, message_type: MessageType, sequence: u16, time: i128) {
        if message_type != MessageType::DelayReq {
            return;
        }
        if let Some((pending, sent)) = &mut self.request
            && *pending == sequence
        {
            *sent = Some(time);
        }
    }

    pub fn poll_timeout(&self) -> Option<Duration> {
        self.heard
            .iter()
            .map(|heard| heard.expires)
            .chain(self.next_request)
            .min()
    }

    pub fn handle_timeout(&mut self, now: Duration) {
        let before = self.heard.len();
        self.heard.retain(|heard| heard.expires > now);
        if self.heard.len() != before {
            self.select(now);
        }
        if let Some(due) = self.next_request.filter(|due| *due <= now) {
            let every = interval(self.config.log_delay_req_interval);
            self.next_request = Some(if now - due < every {
                due + every
            } else {
                now + every
            });
            self.send_request();
        }
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.outgoing.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// The grandmaster followed.
    pub fn grandmaster(&self) -> Option<SystemIdentity> {
        self.transmitter.map(|transmitter| transmitter.system)
    }

    /// The grandmaster's link speed in Mb/s, from its Grandmaster Link
    /// TLV.
    pub fn grandmaster_link_speed(&self) -> Option<u32> {
        self.transmitter
            .and_then(|transmitter| transmitter.link_speed)
    }

    /// The mean path delay to the grandmaster, in nanoseconds, the median
    /// of the last few measured.
    pub fn mean_path_delay(&self) -> Option<f64> {
        if self.delays.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.delays.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        Some(sorted[sorted.len() / 2])
    }

    /// The grandmaster's rate over the port's clock.
    pub fn rate_ratio(&self) -> Option<f64> {
        self.rate
    }

    /// The grandmaster's time as a line on the port's clock: its time at
    /// the last Sync's arrival, and its rate, once the path's delay is
    /// measured.
    pub fn time_line(&self) -> Option<(i128, i128, f64)> {
        let (received, origin) = *self.syncs.back()?;
        let rate = self.rate?;
        let delay = self.mean_path_delay()?;
        Some((
            received,
            origin + delay as i128 + i128::from(self.config.asymmetry),
            rate,
        ))
    }

    /// The grandmaster's time when the port's clock reads `local`.
    pub fn grandmaster_time(&self, local: i128) -> Option<i128> {
        let (at, time, rate) = self.time_line()?;
        let elapsed = local - at;
        Some(time + elapsed + (elapsed as f64 * (rate - 1.0)) as i128)
    }

    fn sent_by_transmitter(&self, header: &Header) -> bool {
        self.transmitter
            .is_some_and(|transmitter| transmitter.source == header.source)
    }

    fn synced(&mut self, received: i128, origin: i128) {
        self.syncs.push_back((received, origin));
        while self.syncs.len() > RATE_SPAN {
            self.syncs.pop_front();
        }
        self.rate = fitted_rate(&self.syncs);
    }

    /// A Delay_Req that left at `sent` here reached the grandmaster at its
    /// time `arrived`: with the last Sync, twice the path's delay.
    fn measured(&mut self, sent: i128, arrived: i128) {
        let (Some(&(received, origin)), Some(rate)) = (self.syncs.back(), self.rate) else {
            return;
        };
        // From the Sync's origin to the request's arrival, less the time
        // spent here between them, in the grandmaster's time.
        let delay = ((arrived - origin) as f64 - (sent - received) as f64 * rate) / 2.0;
        if !delay.is_finite() || delay < 0.0 {
            return;
        }
        self.delays.push_back(delay);
        while self.delays.len() > DELAY_SPAN {
            self.delays.pop_front();
        }
    }

    fn send_request(&mut self) {
        if self.transmitter.is_none() {
            return;
        }
        self.request_sequence = self.request_sequence.wrapping_add(1);
        let sequence = self.request_sequence;
        self.request = Some((sequence, None));
        let message = Message {
            header: Header {
                major_sdo_id: self.config.major_sdo_id,
                minor_version: self.config.minor_version,
                version: 2,
                domain: self.config.domain,
                minor_sdo_id: 0,
                flags: Flags(0),
                correction: 0,
                type_specific: 0,
                source: self.config.identity,
                sequence_id: sequence,
                control: DELAY_REQ_CONTROL,
                log_interval: NO_INTERVAL,
            },
            body: Body::DelayReq {
                origin: Timestamp::default(),
            },
            tlvs: &[],
        };
        let mut frame = vec![0; message.encoded_len()];
        if message.encode(&mut frame).is_ok() {
            self.outgoing.push_back(Transmit {
                frame,
                stamp: Some((MessageType::DelayReq, sequence)),
            });
        }
    }

    /// The BTCA among the Announces heard: the best becomes the timetransmitter.
    fn select(&mut self, now: Duration) {
        let best = self.heard.iter().copied().min_by_key(Heard::rank);
        let changed = best.map(|best| (best.source, best.system))
            != self
                .transmitter
                .map(|transmitter| (transmitter.source, transmitter.system));
        let grandmaster_before = self.grandmaster();
        if changed {
            self.waiting = None;
            self.syncs.clear();
            self.rate = None;
            self.delays.clear();
            self.request = None;
        }
        self.transmitter = best;
        self.next_request = match (best, self.next_request) {
            (Some(_), Some(due)) => Some(due),
            (Some(_), None) => Some(now),
            (None, _) => None,
        };
        let grandmaster = self.grandmaster();
        if grandmaster != grandmaster_before {
            self.events.push_back(Event::Grandmaster(grandmaster));
        }
    }
}

/// The grandmaster's rate over the port's clock: the slope of a line fitted
/// through the Syncs' origins against their arrivals, once there are four.
fn fitted_rate(syncs: &VecDeque<(i128, i128)>) -> Option<f64> {
    if syncs.len() < 4 {
        return None;
    }
    let (last_received, last_origin) = *syncs.back()?;
    let count = syncs.len() as f64;
    let points: Vec<(f64, f64)> = syncs
        .iter()
        .map(|(received, origin)| {
            let x = (received - last_received) as f64;
            (x, (origin - last_origin) as f64 - x)
        })
        .collect();
    let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / count;
    let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / count;
    let spread: f64 = points
        .iter()
        .map(|(x, _)| (x - mean_x) * (x - mean_x))
        .sum();
    if spread == 0.0 {
        return None;
    }
    let covariance: f64 = points
        .iter()
        .map(|(x, y)| (x - mean_x) * (y - mean_y))
        .sum();
    Some(1.0 + covariance / spread)
}

/// The clock identity a Delay_Resp names, as a check in tests.
#[cfg(test)]
fn requesting(frame: &[u8]) -> Option<crate::message::ClockIdentity> {
    match Message::decode(frame).ok()?.body {
        Body::DelayResp { requesting, .. } => Some(requesting.clock),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
