//! gPTP over IEEE 802.11 links (802.1AS-2020 clause 12): the Timing
//! Measurement frame (802.11-2020 9.6.14.3), the Vendor Specific element
//! carrying a whole Follow_Up in it (12.7), and each end's side of the
//! exchange. Fine Timing Measurement, the other way to the same times,
//! is [`ftm`]'s.
//!
//! The timetransmitter sends a Timing Measurement frame for each Sync it
//! relays, and the next frame carries when that one left (t1) and when
//! its Ack came back (t4), with the Follow_Up for it. The timereceiver
//! stamps the frame's arrival (t2) and its Ack's departure (t3), and from
//! two measurements in turn takes the neighbor's rate and the link delay,
//! as a Pdelay exchange gives them on a full-duplex link.
//!
//! Times on the wire are on the sender's port clock, the radio's, in
//! 10 ns units modulo 2^32, so only differences between them are used. As
//! the rest of the crate, nothing here does I/O: the caller sends and
//! receives the frames through its driver and hands in the times the
//! radio stamped.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt;

use crate::message::{HEADER_LENGTH, Message, MessageType};
use crate::relay::Stamp;

pub mod ftm;

/// The Unprotected WNM category of Action frames (802.11-2020 9.4.1.11).
pub const CATEGORY_UNPROTECTED_WNM: u8 = 11;
/// The Unprotected WNM Action of a Timing Measurement frame (Table 9-434).
pub const ACTION_TIMING_MEASUREMENT: u8 = 1;
/// The Vendor Specific element (802.11-2020 Table 9-92).
pub const ELEMENT_VENDOR_SPECIFIC: u8 = 221;
/// The OUI and type of the element carrying a Follow_Up (802.1AS-2020
/// 12.7): IEEE 802.1's, type 0.
pub const FOLLOW_UP_OUI: [u8; 3] = [0x00, 0x80, 0xc2];
pub const FOLLOW_UP_TYPE: u8 = 0;
/// A Follow_Up with its Follow_Up information TLV and nothing else.
pub const FOLLOW_UP_LENGTH: usize = HEADER_LENGTH + 10 + 32;
/// The Action field up to its elements.
const FIXED_LENGTH: usize = 14;
/// Nanoseconds in a unit of TOD and TOA.
const TICK: i128 = 10;
/// How far a neighbor's rate may stand from this port clock's before a
/// measurement is taken for a fault, as the Pdelay exchange's check.
const RATE_LIMIT: f64 = 1e-3;
/// Frames kept for a Follow_Up still to come, or one heard waiting for its
/// times: a few Sync intervals.
const KEPT: usize = 4;

/// Why octets are not the Action frame wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot11Error {
    TooShort(usize),
    /// Another category or action.
    OtherAction {
        category: u8,
        action: u8,
    },
    /// An element running past the frame's end.
    Truncated,
}

impl fmt::Display for Dot11Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort(length) => write!(formatter, "{length} octets, too short"),
            Self::OtherAction { category, action } => {
                write!(formatter, "category {category}, action {action}")
            }
            Self::Truncated => write!(formatter, "an element runs past the frame's end"),
        }
    }
}

impl core::error::Error for Dot11Error {}

/// The ways to measure a station that it and this port both take part in,
/// as its Extended Capabilities say (802.1AS-2020 12.3, tmFtmSupport).
/// With both, Fine Timing Measurement is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Support {
    pub timing_measurement: bool,
    pub fine_timing_measurement: bool,
}

impl Support {
    pub const NONE: Self = Self {
        timing_measurement: false,
        fine_timing_measurement: false,
    };
    pub const TIMING_MEASUREMENT: Self = Self {
        timing_measurement: true,
        fine_timing_measurement: false,
    };
    pub const FINE_TIMING_MEASUREMENT: Self = Self {
        timing_measurement: false,
        fine_timing_measurement: true,
    };
    pub const BOTH: Self = Self {
        timing_measurement: true,
        fine_timing_measurement: true,
    };
}

/// A Timing Measurement frame's Action field and, after it, the Follow_Up
/// in a Vendor Specific element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingMeasurement {
    /// Nonzero, naming this frame for the one that follows it up.
    pub dialog_token: u8,
    /// The frame whose times and Follow_Up this one carries, or 0.
    pub follow_up_dialog_token: u8,
    /// When that frame left and when its Ack came back, on the sender's
    /// clock, in 10 ns units modulo 2^32.
    pub tod: u32,
    pub toa: u32,
    /// Error bounds in 10 ns units, 0 for unknown.
    pub max_tod_error: u8,
    pub max_toa_error: u8,
    /// The Follow_Up for that frame, as encoded.
    pub follow_up: Option<[u8; FOLLOW_UP_LENGTH]>,
}

impl TimingMeasurement {
    /// The Action field, from the Category on, as the frame body carries it.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FIXED_LENGTH + 2 + 4 + FOLLOW_UP_LENGTH);
        out.extend_from_slice(&[
            CATEGORY_UNPROTECTED_WNM,
            ACTION_TIMING_MEASUREMENT,
            self.dialog_token,
            self.follow_up_dialog_token,
        ]);
        out.extend_from_slice(&self.tod.to_le_bytes());
        out.extend_from_slice(&self.toa.to_le_bytes());
        out.extend_from_slice(&[self.max_tod_error, self.max_toa_error]);
        if let Some(follow_up) = &self.follow_up {
            push_follow_up(&mut out, follow_up);
        }
        out
    }

    /// The frame in `octets`, which start at the Category. Elements other
    /// than the Follow_Up's are skipped.
    pub fn decode(octets: &[u8]) -> Result<Self, Dot11Error> {
        if octets.len() < 2 {
            return Err(Dot11Error::TooShort(octets.len()));
        }
        if octets[0] != CATEGORY_UNPROTECTED_WNM || octets[1] != ACTION_TIMING_MEASUREMENT {
            return Err(Dot11Error::OtherAction {
                category: octets[0],
                action: octets[1],
            });
        }
        if octets.len() < FIXED_LENGTH {
            return Err(Dot11Error::TooShort(octets.len()));
        }
        let word = |at: usize| {
            u32::from_le_bytes([octets[at], octets[at + 1], octets[at + 2], octets[at + 3]])
        };
        Ok(Self {
            dialog_token: octets[2],
            follow_up_dialog_token: octets[3],
            tod: word(4),
            toa: word(8),
            max_tod_error: octets[12],
            max_toa_error: octets[13],
            follow_up: follow_up_in(&octets[FIXED_LENGTH..])?,
        })
    }

    /// The Follow_Up it carries, decoded, if it is one.
    pub fn follow_up_message(&self) -> Option<Message<'_>> {
        let message = Message::decode(self.follow_up.as_ref()?).ok()?;
        (message.message_type() == MessageType::FollowUp).then_some(message)
    }
}

/// The elements in `octets`, ID and body, in order.
fn elements(mut octets: &[u8]) -> Result<Vec<(u8, &[u8])>, Dot11Error> {
    let mut found = Vec::new();
    while !octets.is_empty() {
        if octets.len() < 2 || octets.len() < 2 + usize::from(octets[1]) {
            return Err(Dot11Error::Truncated);
        }
        let length = usize::from(octets[1]);
        found.push((octets[0], &octets[2..2 + length]));
        octets = &octets[2 + length..];
    }
    Ok(found)
}

/// The Follow_Up the elements in `octets` carry (802.1AS-2020 12.7), if
/// any; other elements are skipped.
fn follow_up_in(octets: &[u8]) -> Result<Option<[u8; FOLLOW_UP_LENGTH]>, Dot11Error> {
    Ok(elements(octets)?.into_iter().find_map(|(id, body)| {
        (id == ELEMENT_VENDOR_SPECIFIC
            && body.len() == 4 + FOLLOW_UP_LENGTH
            && body[..3] == FOLLOW_UP_OUI
            && body[3] == FOLLOW_UP_TYPE)
            .then(|| {
                let mut follow_up = [0; FOLLOW_UP_LENGTH];
                follow_up.copy_from_slice(&body[4..]);
                follow_up
            })
    }))
}

/// Appends the Vendor Specific element carrying `follow_up`.
fn push_follow_up(out: &mut Vec<u8>, follow_up: &[u8; FOLLOW_UP_LENGTH]) {
    out.extend_from_slice(&[ELEMENT_VENDOR_SPECIFIC, (4 + FOLLOW_UP_LENGTH) as u8]);
    out.extend_from_slice(&FOLLOW_UP_OUI);
    out.push(FOLLOW_UP_TYPE);
    out.extend_from_slice(follow_up);
}

/// A time in nanoseconds as TOD and TOA carry it.
fn ticks(nanos: i128) -> u32 {
    nanos.div_euclid(TICK) as u32
}

/// The nanoseconds from `earlier` to `later`, both in 10 ns units modulo
/// 2^32: good while they stand within about 21 s.
fn ticks_between(earlier: u32, later: u32) -> i128 {
    i128::from(later.wrapping_sub(earlier) as i32) * TICK
}

/// A frame sent, waiting for its times or its Follow_Up.
#[derive(Debug, Clone, Copy)]
struct Sent {
    token: u8,
    /// When it left and when its Ack came back, on the port clock.
    times: Option<(i128, i128)>,
    follow_up: Option<[u8; FOLLOW_UP_LENGTH]>,
}

/// The frame a new one follows up: its dialog token, when it left and
/// when its Ack came back on the port clock, and its Follow_Up.
#[derive(Debug, Clone, Copy)]
struct Previous {
    token: u8,
    departure: i128,
    ack_arrival: i128,
    follow_up: [u8; FOLLOW_UP_LENGTH],
}

/// The frames a timetransmitter sent a station, each new one carrying the
/// times and Follow_Up of the last one both are known for: Timing
/// Measurement's and Fine Timing Measurement's alike.
#[derive(Debug, Clone, Default)]
struct Chain {
    last_token: u8,
    sent: VecDeque<Sent>,
}

impl Chain {
    fn reset(&mut self) {
        self.sent.clear();
    }

    /// A new frame's dialog token, and the frame it follows up, if any.
    fn next(&mut self) -> (u8, Option<Previous>) {
        self.last_token = self.last_token.wrapping_add(1).max(1);
        let mut previous = None;
        let complete = self
            .sent
            .iter()
            .rposition(|sent| sent.times.is_some() && sent.follow_up.is_some());
        if let Some(index) = complete {
            let sent = self.sent[index];
            if let (Some((departure, ack_arrival)), Some(follow_up)) = (sent.times, sent.follow_up)
            {
                previous = Some(Previous {
                    token: sent.token,
                    departure,
                    ack_arrival,
                    follow_up,
                });
            }
            self.sent.drain(..=index);
        }
        self.sent.push_back(Sent {
            token: self.last_token,
            times: None,
            follow_up: None,
        });
        while self.sent.len() > KEPT {
            self.sent.pop_front();
        }
        (self.last_token, previous)
    }

    fn confirmed(&mut self, token: u8, departure: i128, ack_arrival: i128) {
        if let Some(sent) = self.sent.iter_mut().find(|sent| sent.token == token) {
            sent.times = Some((departure, ack_arrival));
        }
    }

    fn set_follow_up(&mut self, token: u8, follow_up: &[u8]) {
        let Ok(follow_up) = <[u8; FOLLOW_UP_LENGTH]>::try_from(follow_up) else {
            return;
        };
        if let Some(sent) = self.sent.iter_mut().find(|sent| sent.token == token) {
            sent.follow_up = Some(follow_up);
        }
    }
}

/// The timetransmitter's side towards one station (802.1AS-2020 12.5.1,
/// master state machine A): a frame for each Sync relayed, each carrying
/// the times and Follow_Up of the last frame both are known for.
#[derive(Debug, Clone, Default)]
pub struct Transmitter {
    chain: Chain,
}

impl Transmitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets every frame sent, as when the station changes.
    pub fn reset(&mut self) {
        self.chain.reset();
    }

    /// The frame to send for the next Sync, with a new dialog token, which
    /// [`confirmed`](Self::confirmed) and [`set_follow_up`](Self::set_follow_up)
    /// take back.
    pub fn next_frame(&mut self) -> TimingMeasurement {
        let (token, previous) = self.chain.next();
        TimingMeasurement {
            dialog_token: token,
            follow_up_dialog_token: previous.map_or(0, |previous| previous.token),
            tod: previous.map_or(0, |previous| ticks(previous.departure)),
            toa: previous.map_or(0, |previous| ticks(previous.ack_arrival)),
            max_tod_error: 0,
            max_toa_error: 0,
            follow_up: previous.map(|previous| previous.follow_up),
        }
    }

    /// When frame `token` left and when its Ack came back, on the port
    /// clock, as the radio reports them on its Ack.
    pub fn confirmed(&mut self, token: u8, departure: i128, ack_arrival: i128) {
        self.chain.confirmed(token, departure, ack_arrival);
    }

    /// The Follow_Up for frame `token`, as for a Sync that left when it did:
    /// it goes out in the next frame. Anything but a Follow_Up of the
    /// usual length is ignored.
    pub fn set_follow_up(&mut self, token: u8, follow_up: &[u8]) {
        self.chain.set_follow_up(token, follow_up);
    }
}

/// What a frame's follow-up told the timereceiver: the Follow_Up for the
/// frame it names, when that frame arrived here, and the link as the two
/// measurements in turn give it, on this port's clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    pub arrival: Stamp,
    pub follow_up: [u8; FOLLOW_UP_LENGTH],
    /// The neighbor's clock rate over this port's.
    pub neighbor_rate_ratio: f64,
    /// The link delay in this port clock's nanoseconds.
    pub mean_link_delay: f64,
}

impl Measurement {
    /// The Follow_Up, decoded.
    pub fn follow_up_message(&self) -> Option<Message<'_>> {
        let message = Message::decode(&self.follow_up).ok()?;
        (message.message_type() == MessageType::FollowUp).then_some(message)
    }
}

/// A frame heard, until its follow-up comes.
#[derive(Debug, Clone, Copy)]
struct Heard {
    token: u8,
    arrival: Stamp,
    /// When its Ack left, on the port clock.
    ack_departure: i128,
}

/// The timereceiver's side (802.1AS-2020 12.5.2, the slave state machine
/// for Timing Measurement): each frame's arrival and its Ack's departure
/// kept until the next frame brings the sender's times for it.
#[derive(Debug, Clone, Default)]
pub struct Receiver {
    heard: VecDeque<Heard>,
    /// The last measured frame's departure, on the sender's clock, and its
    /// arrival here.
    last: Option<(u32, Stamp)>,
}

impl Receiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets every frame heard, as when the link changes.
    pub fn reset(&mut self) {
        self.heard.clear();
        self.last = None;
    }

    /// Takes a frame that arrived at `arrival` and whose Ack left at
    /// `ack_departure`, on this port's clock. Returns a measurement once
    /// it follows up a frame heard, and one was measured before it, for
    /// the neighbor's rate.
    pub fn indication(
        &mut self,
        frame: &TimingMeasurement,
        arrival: Stamp,
        ack_departure: i128,
    ) -> Option<Measurement> {
        let measured = self.follow_up(frame);
        if frame.dialog_token != 0 {
            self.heard.retain(|heard| heard.token != frame.dialog_token);
            self.heard.push_back(Heard {
                token: frame.dialog_token,
                arrival,
                ack_departure,
            });
            while self.heard.len() > KEPT {
                self.heard.pop_front();
            }
        }
        measured
    }

    fn follow_up(&mut self, frame: &TimingMeasurement) -> Option<Measurement> {
        let follow_up = frame.follow_up?;
        if frame.follow_up_dialog_token == 0 {
            return None;
        }
        let heard = *self
            .heard
            .iter()
            .find(|heard| heard.token == frame.follow_up_dialog_token)?;
        let last = self.last.replace((frame.tod, heard.arrival));
        let (last_departure, last_arrival) = last?;
        let sent_apart = ticks_between(last_departure, frame.tod);
        let arrived_apart = heard.arrival.port - last_arrival.port;
        if sent_apart <= 0 || arrived_apart <= 0 {
            return None;
        }
        let neighbor_rate_ratio = sent_apart as f64 / arrived_apart as f64;
        if neighbor_rate_ratio < 1.0 - RATE_LIMIT || neighbor_rate_ratio > 1.0 + RATE_LIMIT {
            return None;
        }
        // 802.1AS-2020 12.5.2.4.4, on this port's clock: the round trip at
        // the neighbor less the Ack's turnaround here, halved.
        let round_trip = ticks_between(frame.tod, frame.toa) as f64 / neighbor_rate_ratio;
        let turnaround = (heard.ack_departure - heard.arrival.port) as f64;
        Some(Measurement {
            arrival: heard.arrival,
            follow_up,
            neighbor_rate_ratio,
            mean_link_delay: (round_trip - turnaround) / 2.0,
        })
    }
}

#[cfg(test)]
mod tests;
