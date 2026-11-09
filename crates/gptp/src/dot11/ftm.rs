//! Fine Timing Measurement for gPTP (802.1AS-2020 12.1.2.2, master state
//! machine B of 12.5.1, and 12.5.2's FTM states): the FTM Request and FTM
//! frames (802.11-2020 9.6.7.32 and 9.6.7.33) with the FTM Parameters
//! element (9.4.2.167); the responder, which sends a station a burst of
//! frames, each carrying the times and Follow_Up of the one before; and
//! the initiator, which asks for a burst each sync interval and measures
//! from the frames of each.
//!
//! Unlike Timing Measurement, the station paces the exchange: it asks for
//! a burst of three frames as soon as possible, or two if three are
//! refused, and uses the frame and the Ack that crossed the air fastest.
//! TOD and TOA are in picoseconds modulo 2^48.

use alloc::vec::Vec;
use core::time::Duration;

use super::{
    Chain, Dot11Error, FOLLOW_UP_LENGTH, Measurement, RATE_LIMIT, elements, follow_up_in,
    push_follow_up,
};
use crate::link::interval;
use crate::message::{Message, MessageType};
use crate::relay::Stamp;

/// The Public category of Action frames (802.11-2020 9.4.1.11).
pub const CATEGORY_PUBLIC: u8 = 4;
/// The Public Actions of FTM (802.11-2020 Table 9-364).
pub const ACTION_FTM_REQUEST: u8 = 32;
pub const ACTION_FTM: u8 = 33;
/// The Fine Timing Measurement Parameters element (802.11-2020 Table 9-92).
pub const ELEMENT_FTM_PARAMETERS: u8 = 206;
/// Status Indication values (802.11-2020 Table 9-278).
pub const STATUS_SUCCESSFUL: u8 = 1;
pub const STATUS_INCAPABLE: u8 = 2;
pub const STATUS_FAILED: u8 = 3;
/// Format And Bandwidth: non-HT at 20 MHz (802.11-2020 Table 9-280).
pub const FORMAT_NON_HT_20: u8 = 8;
/// FTMs Per Burst asked for first, and on a retry (802.1AS-2020 Table
/// 12-2).
const FRAMES_FIRST: u8 = 3;
const FRAMES_RETRY: u8 = 2;
/// How long the initiator waits for the initial FTM frame before asking
/// again (802.1AS-2020 Figure 12-7).
const INITIAL_WAIT: Duration = Duration::from_millis(10);
/// The longest burst duration (802.11-2020 Table 9-279), for a burst that
/// names none.
const LONGEST_BURST: u8 = 11;
/// 20 ms, the Min Delta FTM of the slowest sync (802.1AS-2020 Table 12-3).
const SLOWEST_MIN_DELTA: u8 = 200;
const PICOS_PER_NANO: i128 = 1_000;
const TIME_BITS: u32 = 48;
const PARAMETERS_LENGTH: usize = 9;
/// The FTM frame's Action field up to its elements.
const FIXED_LENGTH: usize = 20;

/// The Fine Timing Measurement Parameters field (802.11-2020 9.4.2.167):
/// the burst a station asks for, or the one granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FtmParameters {
    pub status: u8,
    /// With status 3, the seconds before asking again.
    pub value: u8,
    pub bursts_exponent: u8,
    /// 250 µs times 2^(value - 2), for 2 to 11; 15 for no preference.
    pub burst_duration: u8,
    /// In units of 100 µs.
    pub min_delta: u8,
    pub partial_tsf: u16,
    pub partial_tsf_no_preference: bool,
    pub asap_capable: bool,
    pub asap: bool,
    pub ftms_per_burst: u8,
    pub format_and_bandwidth: u8,
    /// In units of 100 ms.
    pub burst_period: u16,
}

impl FtmParameters {
    /// What a timereceiver asks for at `log_sync_interval` (802.1AS-2020
    /// Tables 12-2 and 12-3): one burst of `frames` as soon as possible.
    pub fn for_sync(log_sync_interval: i8, frames: u8) -> Self {
        let (burst_duration, min_delta) = match log_sync_interval {
            ..=-6 => (6, 6),
            -5 => (8, 25),
            -4 => (9, 50),
            -3 => (10, 100),
            _ => (11, 200),
        };
        Self {
            burst_duration,
            min_delta,
            partial_tsf: 1,
            asap: true,
            ftms_per_burst: frames,
            ..Self::default()
        }
    }

    /// The burst's length, if it names one.
    pub fn burst_length(&self) -> Option<Duration> {
        (2..=LONGEST_BURST)
            .contains(&self.burst_duration)
            .then(|| Duration::from_micros(250 << (self.burst_duration - 2)))
    }

    /// The least time between frames of the burst.
    pub fn min_delta_time(&self) -> Duration {
        Duration::from_micros(100 * u64::from(self.min_delta))
    }

    pub fn encode(&self) -> [u8; PARAMETERS_LENGTH] {
        let field = u128::from(self.status & 0x3)
            | u128::from(self.value & 0x1f) << 2
            | u128::from(self.bursts_exponent & 0xf) << 8
            | u128::from(self.burst_duration & 0xf) << 12
            | u128::from(self.min_delta) << 16
            | u128::from(self.partial_tsf) << 24
            | u128::from(self.partial_tsf_no_preference) << 40
            | u128::from(self.asap_capable) << 41
            | u128::from(self.asap) << 42
            | u128::from(self.ftms_per_burst & 0x1f) << 43
            | u128::from(self.format_and_bandwidth & 0x3f) << 50
            | u128::from(self.burst_period) << 56;
        let mut octets = [0; PARAMETERS_LENGTH];
        octets.copy_from_slice(&field.to_le_bytes()[..PARAMETERS_LENGTH]);
        octets
    }

    pub fn decode(octets: &[u8]) -> Option<Self> {
        let octets: [u8; PARAMETERS_LENGTH] = octets.try_into().ok()?;
        let mut wide = [0; 16];
        wide[..PARAMETERS_LENGTH].copy_from_slice(&octets);
        let field = u128::from_le_bytes(wide);
        let bits = |at: u32, width: u32| (field >> at) & ((1 << width) - 1);
        Some(Self {
            status: bits(0, 2) as u8,
            value: bits(2, 5) as u8,
            bursts_exponent: bits(8, 4) as u8,
            burst_duration: bits(12, 4) as u8,
            min_delta: bits(16, 8) as u8,
            partial_tsf: bits(24, 16) as u16,
            partial_tsf_no_preference: bits(40, 1) == 1,
            asap_capable: bits(41, 1) == 1,
            asap: bits(42, 1) == 1,
            ftms_per_burst: bits(43, 5) as u8,
            format_and_bandwidth: bits(50, 6) as u8,
            burst_period: bits(56, 16) as u16,
        })
    }

    fn push(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[ELEMENT_FTM_PARAMETERS, PARAMETERS_LENGTH as u8]);
        out.extend_from_slice(&self.encode());
    }
}

/// The FTM Parameters element among the elements in `octets`, if any.
fn parameters_in(octets: &[u8]) -> Result<Option<FtmParameters>, Dot11Error> {
    Ok(elements(octets)?
        .into_iter()
        .find_map(|(id, body)| (id == ELEMENT_FTM_PARAMETERS).then(|| FtmParameters::decode(body)))
        .flatten())
}

fn check_action(octets: &[u8], action: u8, length: usize) -> Result<(), Dot11Error> {
    if octets.len() < 2 {
        return Err(Dot11Error::TooShort(octets.len()));
    }
    if octets[0] != CATEGORY_PUBLIC || octets[1] != action {
        return Err(Dot11Error::OtherAction {
            category: octets[0],
            action: octets[1],
        });
    }
    if octets.len() < length {
        return Err(Dot11Error::TooShort(octets.len()));
    }
    Ok(())
}

/// A Fine Timing Measurement Request frame's Action field: the initial one
/// carries the parameters asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FtmRequest {
    /// 1 to start or go on, 0 to stop.
    pub trigger: u8,
    pub parameters: Option<FtmParameters>,
}

impl FtmRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(3 + 2 + PARAMETERS_LENGTH);
        out.extend_from_slice(&[CATEGORY_PUBLIC, ACTION_FTM_REQUEST, self.trigger]);
        if let Some(parameters) = &self.parameters {
            parameters.push(&mut out);
        }
        out
    }

    /// The request in `octets`, which start at the Category; the LCI and
    /// civic location requests are skipped.
    pub fn decode(octets: &[u8]) -> Result<Self, Dot11Error> {
        check_action(octets, ACTION_FTM_REQUEST, 3)?;
        Ok(Self {
            trigger: octets[2],
            parameters: parameters_in(&octets[3..])?,
        })
    }
}

/// A Fine Timing Measurement frame's Action field, with the Follow_Up of
/// the frame it follows up (802.1AS-2020 12.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FineTimingMeasurement {
    /// Nonzero, naming this frame for the one that follows it up; 0 for
    /// the last of the session.
    pub dialog_token: u8,
    pub follow_up_dialog_token: u8,
    /// When the frame followed up left and when its Ack came back, on the
    /// sender's clock, in picoseconds modulo 2^48.
    pub tod: u64,
    pub toa: u64,
    pub tod_error: u16,
    pub toa_error: u16,
    /// The burst granted, in the initial frame.
    pub parameters: Option<FtmParameters>,
    pub follow_up: Option<[u8; FOLLOW_UP_LENGTH]>,
}

impl FineTimingMeasurement {
    pub fn encode(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(FIXED_LENGTH + 2 + PARAMETERS_LENGTH + 6 + FOLLOW_UP_LENGTH);
        out.extend_from_slice(&[
            CATEGORY_PUBLIC,
            ACTION_FTM,
            self.dialog_token,
            self.follow_up_dialog_token,
        ]);
        out.extend_from_slice(&self.tod.to_le_bytes()[..6]);
        out.extend_from_slice(&self.toa.to_le_bytes()[..6]);
        out.extend_from_slice(&self.tod_error.to_le_bytes());
        out.extend_from_slice(&self.toa_error.to_le_bytes());
        if let Some(parameters) = &self.parameters {
            parameters.push(&mut out);
        }
        if let Some(follow_up) = &self.follow_up {
            push_follow_up(&mut out, follow_up);
        }
        out
    }

    /// The frame in `octets`, which start at the Category; the location
    /// reports and other elements are skipped.
    pub fn decode(octets: &[u8]) -> Result<Self, Dot11Error> {
        check_action(octets, ACTION_FTM, FIXED_LENGTH)?;
        let time = |at: usize| {
            let mut wide = [0; 8];
            wide[..6].copy_from_slice(&octets[at..at + 6]);
            u64::from_le_bytes(wide)
        };
        let half = |at: usize| u16::from_le_bytes([octets[at], octets[at + 1]]);
        let rest = &octets[FIXED_LENGTH..];
        Ok(Self {
            dialog_token: octets[2],
            follow_up_dialog_token: octets[3],
            tod: time(4),
            toa: time(10),
            tod_error: half(16),
            toa_error: half(18),
            parameters: parameters_in(rest)?,
            follow_up: follow_up_in(rest)?,
        })
    }

    /// The Follow_Up it carries, decoded, if it is one.
    pub fn follow_up_message(&self) -> Option<Message<'_>> {
        let message = Message::decode(self.follow_up.as_ref()?).ok()?;
        (message.message_type() == MessageType::FollowUp).then_some(message)
    }
}

/// A time in nanoseconds as TOD and TOA carry it.
fn picoseconds(nanos: i128) -> u64 {
    ((nanos * PICOS_PER_NANO) as u64) & ((1 << TIME_BITS) - 1)
}

/// The picoseconds from `earlier` to `later`, both modulo 2^48: good while
/// they stand within about 140 s.
fn picoseconds_between(earlier: u64, later: u64) -> i128 {
    let shift = 64 - TIME_BITS;
    i128::from(((later.wrapping_sub(earlier)) << shift) as i64 >> shift)
}

/// What the responder grants a request: the burst asked for as soon as
/// possible, with a burst length and spacing where the station has no
/// preference, and every frame stamped from the first (ASAP capable).
fn grant(asked: &FtmParameters) -> FtmParameters {
    let burst_duration = if asked.burst_length().is_some() {
        asked.burst_duration
    } else {
        LONGEST_BURST
    };
    let ftms_per_burst = if asked.ftms_per_burst == 0 {
        FRAMES_FIRST
    } else {
        asked.ftms_per_burst
    };
    FtmParameters {
        status: STATUS_SUCCESSFUL,
        value: 0,
        bursts_exponent: 0,
        burst_duration,
        min_delta: if asked.min_delta == 0 {
            fitting_min_delta(burst_duration, ftms_per_burst)
        } else {
            asked.min_delta
        },
        // The radio's TSF is not known here; with ASAP the burst starts
        // with the initial frame.
        partial_tsf: 0,
        partial_tsf_no_preference: false,
        asap_capable: true,
        asap: true,
        ftms_per_burst,
        format_and_bandwidth: if asked.format_and_bandwidth == 0 {
            FORMAT_NON_HT_20
        } else {
            asked.format_and_bandwidth
        },
        burst_period: 0,
    }
}

/// Min Delta FTM for a station with no preference, in 100 µs: the slowest
/// sync's, or less where `frames` would not fit in the burst so spaced, as
/// for a station asking 8 frames in 32 ms to range.
fn fitting_min_delta(burst_duration: u8, frames: u8) -> u8 {
    let burst = (250u32 << (burst_duration - 2)) / 100;
    let spacing = burst / u32::from(frames.max(1));
    spacing.clamp(1, u32::from(SLOWEST_MIN_DELTA)) as u8
}

/// A burst under way at the responder.
#[derive(Debug, Clone, Copy)]
struct Burst {
    granted: FtmParameters,
    /// The frames sent and acked, counted as 802.1AS-2020's nframesSent:
    /// 1 before the first.
    sent: u8,
    /// When the next frame may go, and when the burst ends.
    next: Duration,
    ends: Duration,
    /// A frame sent whose Ack the next one waits for.
    waiting: Option<u8>,
}

/// The timetransmitter's side towards one station (802.1AS-2020 12.5.1,
/// master state machine B): a burst for each request, its frames at least
/// Min Delta FTM apart, each after the one before was acked.
#[derive(Debug, Clone, Default)]
pub struct Responder {
    chain: Chain,
    burst: Option<Burst>,
}

impl Responder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets the burst and the frames sent, as when the station changes.
    pub fn reset(&mut self) {
        self.chain.reset();
        self.burst = None;
    }

    /// The answer to an FTM Request: for an initial one, the burst's first
    /// frame with the parameters granted. A request to stop ends the
    /// burst.
    pub fn request(
        &mut self,
        now: Duration,
        request: &FtmRequest,
    ) -> Option<FineTimingMeasurement> {
        if request.trigger == 0 {
            self.burst = None;
            return None;
        }
        let granted = grant(&request.parameters?);
        self.burst = Some(Burst {
            granted,
            sent: 1,
            next: now,
            ends: now + granted.burst_length().unwrap_or_default(),
            waiting: None,
        });
        let mut frame = self.frame(now)?;
        frame.parameters = Some(granted);
        Some(frame)
    }

    /// When [`poll_frame`](Self::poll_frame) is next due.
    pub fn poll_timeout(&self) -> Option<Duration> {
        let burst = self.burst?;
        Some(match burst.waiting {
            Some(_) => burst.ends,
            None => burst.next.min(burst.ends),
        })
    }

    /// The burst's next frame, once the last was acked and Min Delta FTM
    /// has passed; none from the burst's end.
    pub fn poll_frame(&mut self, now: Duration) -> Option<FineTimingMeasurement> {
        let burst = self.burst?;
        if now >= burst.ends || burst.sent > burst.granted.ftms_per_burst {
            self.burst = None;
            return None;
        }
        if burst.waiting.is_some() || now < burst.next {
            return None;
        }
        self.frame(now)
    }

    /// When frame `token` left and when its Ack came back, on the port
    /// clock.
    pub fn confirmed(&mut self, token: u8, departure: i128, ack_arrival: i128) {
        if token == 0 {
            return;
        }
        self.chain.confirmed(token, departure, ack_arrival);
        if let Some(burst) = self.burst.as_mut()
            && burst.waiting == Some(token)
        {
            burst.waiting = None;
            burst.sent += 1;
        }
    }

    /// The Follow_Up for frame `token`, for the time it left: it goes out
    /// in the next frame.
    pub fn set_follow_up(&mut self, token: u8, follow_up: &[u8]) {
        self.chain.set_follow_up(token, follow_up);
    }

    fn frame(&mut self, now: Duration) -> Option<FineTimingMeasurement> {
        let burst = self.burst.as_mut()?;
        let (token, previous) = self.chain.next(false);
        // The last of the burst is followed up by none (802.1AS-2020
        // 12.5.1.4.5): it only carries the times of the one before.
        let last = burst.sent >= burst.granted.ftms_per_burst;
        burst.next = now + burst.granted.min_delta_time();
        if last {
            self.burst = None;
        } else {
            burst.waiting = Some(token);
        }
        Some(FineTimingMeasurement {
            dialog_token: if last { 0 } else { token },
            follow_up_dialog_token: previous.map_or(0, |previous| previous.token),
            tod: previous.map_or(0, |previous| picoseconds(previous.departure)),
            toa: previous.map_or(0, |previous| picoseconds(previous.ack_arrival)),
            tod_error: 0,
            toa_error: 0,
            parameters: None,
            follow_up: previous.and_then(|previous| previous.follow_up),
        })
    }
}

/// A frame of the burst heard at the initiator.
#[derive(Debug, Clone, Copy)]
struct Heard {
    token: u8,
    arrival: Stamp,
    ack_departure: i128,
    /// When it left and when its Ack came back on the responder's clock,
    /// and its Follow_Up, once a later frame brings them.
    times: Option<(u64, u64)>,
    follow_up: Option<[u8; FOLLOW_UP_LENGTH]>,
}

/// A burst asked for at the initiator.
#[derive(Debug, Clone)]
struct Session {
    asked: Duration,
    granted: Option<FtmParameters>,
    heard: Vec<Heard>,
}

/// The timereceiver's side (802.1AS-2020 12.5.2 with FTM): a burst asked
/// for each sync interval, three frames or else two, and from each burst
/// the frame and the Ack that crossed fastest (setTimes, 12.5.2.4.3).
#[derive(Debug, Clone)]
pub struct Initiator {
    log_sync_interval: i8,
    frames: u8,
    next_request: Duration,
    session: Option<Session>,
    /// The last measurement's departure on the responder's clock and its
    /// arrival here, for the neighbor's rate.
    last: Option<(u64, Stamp)>,
    /// The neighbor's rate as last measured, to compare a burst's frames.
    rate: f64,
    capable: Option<bool>,
}

impl Initiator {
    pub fn new(log_sync_interval: i8) -> Self {
        Self {
            log_sync_interval,
            frames: FRAMES_FIRST,
            next_request: Duration::ZERO,
            session: None,
            last: None,
            rate: 1.0,
            capable: None,
        }
    }

    /// Forgets the bursts, as when the link changes.
    pub fn reset(&mut self) {
        *self = Self::new(self.log_sync_interval);
    }

    /// Whether the responder granted a burst of three or two frames, once
    /// it answered (802.1AS-2020 12.4).
    pub fn capable(&self) -> Option<bool> {
        self.capable
    }

    /// When [`poll_request`](Self::poll_request) is next due.
    pub fn poll_timeout(&self) -> Option<Duration> {
        Some(match &self.session {
            Some(session) => session.asked + self.waits(session),
            None => self.next_request,
        })
    }

    /// The FTM Request to send now, if one is due: at the sync interval, or
    /// again once the last went unanswered.
    pub fn poll_request(&mut self, now: Duration) -> Option<FtmRequest> {
        match &self.session {
            Some(session) if now < session.asked + self.waits(session) => return None,
            None if now < self.next_request => return None,
            _ => {}
        }
        self.session = Some(Session {
            asked: now,
            granted: None,
            heard: Vec::new(),
        });
        self.next_request = now + interval(self.log_sync_interval);
        Some(FtmRequest {
            trigger: 1,
            parameters: Some(FtmParameters::for_sync(self.log_sync_interval, self.frames)),
        })
    }

    /// Takes an FTM frame that arrived at `arrival` and whose Ack left at
    /// `ack_departure`, on this port's clock. Returns the burst's
    /// measurement once its last frame came and one was measured before,
    /// for the neighbor's rate.
    pub fn frame(
        &mut self,
        now: Duration,
        frame: &FineTimingMeasurement,
        arrival: Stamp,
        ack_departure: i128,
    ) -> Option<Measurement> {
        let session = self.session.as_mut()?;
        if session.granted.is_none() {
            let granted = frame.parameters?;
            if granted.status != STATUS_SUCCESSFUL || !(2..=3).contains(&granted.ftms_per_burst) {
                // Refused: ask for two frames at once, and after two are
                // refused too, again at the next interval.
                self.session = None;
                if self.frames == FRAMES_FIRST {
                    self.frames = FRAMES_RETRY;
                    self.next_request = now;
                } else {
                    self.frames = FRAMES_FIRST;
                    self.capable = Some(false);
                }
                return None;
            }
            session.granted = Some(granted);
            self.capable = Some(true);
        }
        if frame.follow_up_dialog_token != 0
            && let Some(heard) = session
                .heard
                .iter_mut()
                .find(|heard| heard.token == frame.follow_up_dialog_token)
        {
            heard.times = Some((frame.tod, frame.toa));
            heard.follow_up = frame.follow_up;
        }
        session.heard.push(Heard {
            token: frame.dialog_token,
            arrival,
            ack_departure,
            times: None,
            follow_up: None,
        });
        let frames = usize::from(session.granted?.ftms_per_burst);
        if frame.dialog_token != 0 && session.heard.len() < frames {
            return None;
        }
        let heard = core::mem::take(&mut session.heard);
        self.session = None;
        self.measure(&heard)
    }

    /// How long a session waits: for the initial frame, then for the burst.
    fn waits(&self, session: &Session) -> Duration {
        session.granted.map_or(INITIAL_WAIT, |granted| {
            granted.burst_length().unwrap_or_default() + INITIAL_WAIT
        })
    }

    fn measure(&mut self, heard: &[Heard]) -> Option<Measurement> {
        let timed: Vec<(Heard, u64, u64)> = heard
            .iter()
            .filter_map(|heard| {
                let (tod, toa) = heard.times?;
                heard.follow_up?;
                Some((*heard, tod, toa))
            })
            .collect();
        let (first, first_tod, first_toa) = *timed.first()?;
        // Each frame's delay and each Ack's against the first's, on this
        // port's clock: the clocks' offsets cancel, so the least is the
        // fastest (802.1AS-2020 12.5.2.4.3). The neighbor's intervals are
        // scaled by its rate, as over Min Delta FTM a few ppm stand for
        // more than the delays differ.
        let rate = self.rate;
        let forward = |(heard, tod, _): &(Heard, u64, u64)| {
            (heard.arrival.port - first.arrival.port) as f64
                - picoseconds_between(first_tod, *tod) as f64 / 1_000.0 / rate
        };
        let reverse = |(heard, _, toa): &(Heard, u64, u64)| {
            picoseconds_between(first_toa, *toa) as f64 / 1_000.0 / rate
                - (heard.ack_departure - first.ack_departure) as f64
        };
        let least = |key: &dyn Fn(&(Heard, u64, u64)) -> f64| {
            timed
                .iter()
                .min_by(|one, other| key(one).total_cmp(&key(other)))
                .copied()
        };
        let (sent, departure, _) = least(&forward)?;
        let (acked, _, ack_arrival) = least(&reverse)?;
        let last = self.last.replace((departure, sent.arrival));
        let (last_departure, last_arrival) = last?;
        let sent_apart = picoseconds_between(last_departure, departure) as f64 / 1_000.0;
        let arrived_apart = (sent.arrival.port - last_arrival.port) as f64;
        if sent_apart <= 0.0 || arrived_apart <= 0.0 {
            return None;
        }
        let neighbor_rate_ratio = sent_apart / arrived_apart;
        if neighbor_rate_ratio < 1.0 - RATE_LIMIT || neighbor_rate_ratio > 1.0 + RATE_LIMIT {
            return None;
        }
        self.rate = neighbor_rate_ratio;
        // 802.1AS-2020 12.5.2.4.4 on this port's clock, the frame and the
        // Ack possibly of two frames of the burst.
        let round_trip =
            picoseconds_between(departure, ack_arrival) as f64 / 1_000.0 / neighbor_rate_ratio;
        let turnaround = (acked.ack_departure - sent.arrival.port) as f64;
        Some(Measurement {
            arrival: sent.arrival,
            follow_up: sent.follow_up?,
            neighbor_rate_ratio,
            mean_link_delay: (round_trip - turnaround) / 2.0,
        })
    }
}

#[cfg(test)]
mod tests;
