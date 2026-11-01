//! One port's link to its neighbor (802.1AS-2020 clause 11): the
//! MDPdelayReq requester measuring the link's delay and the neighbor's
//! rate against this system, the MDPdelayResp responder answering the
//! neighbor's own requests, and asCapable.
//!
//! Every time stamp is on the LocalClock, which the caller never moves:
//! a free-running clock is what lets a neighbor measure this system's
//! rate across any interval. Timers run on the caller's `now`, a
//! monotonic time.
//!
//! A rate ratio outside a plausible range, as a clock step on either side
//! gives for the one interval spanning it, is dropped, and the last good
//! ratio kept for up to [`Config::allowed_rate_outliers`] intervals in a
//! row, so a step costs one measurement rather than asCapable. A neighbor
//! whose rate is implausible from the first measurement stays not
//! asCapable.
//!
//! A card that stamps every frame can give one frame the time of the
//! next and that one none, as the I210 does now and then with two frames
//! some 10 us apart: a response so stamped gives a delay half that gap
//! too long. The caller says so with
//! [`distrust_response`](Link::distrust_response), and the exchange is
//! left out, of the delay and the rate.

use core::time::Duration;

use crate::message::{Body, Flags, Header, Message, MessageType, PortIdentity, Timestamp};

/// Why a port is not asCapable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incapable {
    Down,
    Unanswered,
    MultipleResponders,
    ImplausibleRate,
    NoRate,
    NoDelay,
    LongDelay,
}

impl Incapable {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Down => "its link is down",
            Self::Unanswered => "its requests went unanswered",
            Self::MultipleResponders => "more than one port answered a request",
            Self::ImplausibleRate => "the neighbor's rate read implausible",
            Self::NoRate => "the neighbor's rate is not known yet",
            Self::NoDelay => "the link delay is not known yet",
            Self::LongDelay => "the link delay is over the threshold",
        }
    }
}

/// How a port measures its link.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// log2 of the seconds between Pdelay requests.
    pub log_interval: i8,
    /// The longest mean link delay, in nanoseconds, for asCapable.
    pub neighbor_prop_delay_thresh: f64,
    /// Requests in a row left unanswered before the port is not asCapable.
    pub allowed_lost_responses: u8,
    /// How far, as a fraction, the neighbor's rate may stand from this
    /// system's before a measurement of it is taken for a clock step.
    pub plausible_rate: f64,
    /// Implausible rate measurements in a row that keep the last good
    /// ratio before it is dropped too.
    pub allowed_rate_outliers: u8,
    /// Delay measurements whose median is the mean link delay.
    pub delay_samples: usize,
    /// How far, in nanoseconds, a delay measurement may stand from the
    /// median of those kept before it is taken for a bad time stamp on
    /// either side and left out.
    pub delay_outlier: f64,
    /// Delay measurements in a row left out, after which the link is taken
    /// to have changed: those kept make way for the new. A neighbor with
    /// bad time stamps can give several in a row, while a link that really
    /// changed went down and up, which starts the measurements afresh.
    pub allowed_delay_outliers: u8,
    /// minorVersionPTP of messages sent: 0, as some switches ignore 1.
    pub minor_version: u8,
    /// Whether the Pdelay messages sent carry the Endpoint Declaration
    /// TLV, as an end station supporting AVB Lite says it is one (AVB
    /// Lite profile 2.1).
    pub endpoint_declaration: bool,
    /// Requests in a row each answered by more than one clock, after
    /// which the port stops asking for `cease_for`, or until the link goes
    /// down and up (Milan 4.2.6.2.5): a switch is flooding the requests.
    pub cease_after_multiple: u8,
    pub cease_for: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_interval: 0,
            neighbor_prop_delay_thresh: 800.0,
            allowed_lost_responses: 3,
            plausible_rate: 1000e-6,
            allowed_rate_outliers: 3,
            delay_samples: 8,
            delay_outlier: 400.0,
            allowed_delay_outliers: 15,
            minor_version: 0,
            endpoint_declaration: false,
            cease_after_multiple: 3,
            cease_for: Duration::from_secs(300),
        }
    }
}

/// A message to send, and whether the caller is to report the time it
/// left with [`Link::handle_transmit_time`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transmit {
    pub message: Message<'static>,
    pub stamp: bool,
}

/// What the link knows of the neighbor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// The mean link delay in nanoseconds, in this system's time base.
    pub mean_link_delay: f64,
    /// The neighbor's rate over this system's.
    pub neighbor_rate_ratio: f64,
    pub neighbor: PortIdentity,
}

/// Distrusted exchanges in a row left out, after which they are taken
/// as any other, so a port whose every response is distrusted is still
/// measured: some 25 come at a time where a neighbor's Follow_Up follows
/// its response closely.
pub const ALLOWED_DISTRUSTED_EXCHANGES: u8 = 60;

const RESPONSE_CONTROL: u8 = 5;
const NO_INTERVAL: i8 = 0x7f;

/// The request in flight and what has come back for it.
#[derive(Debug, Clone, Copy)]
struct Request {
    sequence: u16,
    /// t1: when the request left, once the caller says.
    sent: Option<i128>,
    /// The response's responder, t2, and its arrival here, t4.
    response: Option<(PortIdentity, i128, i128)>,
    /// A second port answered the same request.
    multiple: bool,
    /// The response's time stamp may be another frame's.
    distrusted: bool,
}

/// One port's peer delay requester and responder.
#[derive(Debug, Clone)]
pub struct Link {
    config: Config,
    own: PortIdentity,
    enabled: bool,
    sequence: u16,
    next_request: Option<Duration>,
    request: Option<Request>,
    lost: u8,
    /// Responses to answer: the requester, its sequence, and t2.
    answering: heapless_queue::Queue,
    /// The last exchange's t3 (corrected) and t4, the rate's anchor.
    anchor: Option<(i128, i128)>,
    rate: Option<f64>,
    rate_outliers: u8,
    delays: DelayFilter,
    /// The delay measurements left out in a row, in case the link changed.
    changed: DelayFilter,
    delay_outliers: u8,
    /// The last delay measurement left out, with the median it stood from,
    /// until taken.
    outlier: Option<(f64, f64)>,
    /// Distrusted exchanges in a row.
    distrusted: u8,
    neighbor: Option<PortIdentity>,
    multiple_responders: u8,
}

impl Link {
    /// The TLVs each Pdelay message sent carries.
    fn tlvs(&self) -> &'static [u8] {
        if self.config.endpoint_declaration {
            &crate::lite::ENDPOINT_TLV
        } else {
            &[]
        }
    }

    pub fn new(own: PortIdentity, config: Config) -> Self {
        Self {
            config,
            own,
            enabled: false,
            sequence: 0,
            next_request: None,
            request: None,
            lost: 0,
            answering: heapless_queue::Queue::default(),
            anchor: None,
            rate: None,
            rate_outliers: 0,
            delays: DelayFilter::new(config.delay_samples),
            changed: DelayFilter::new(config.delay_samples),
            delay_outliers: 0,
            outlier: None,
            distrusted: 0,
            neighbor: None,
            multiple_responders: 0,
        }
    }

    /// The port's link came up (`true`) or went down: measuring starts at
    /// `now`, or stops and forgets the neighbor.
    pub fn set_enabled(&mut self, now: Duration, enabled: bool) {
        if enabled == self.enabled {
            return;
        }
        let (config, own, sequence) = (self.config, self.own, self.sequence);
        *self = Self::new(own, config);
        self.sequence = sequence;
        self.enabled = enabled;
        self.next_request = enabled.then_some(now);
    }

    /// Whether the port may carry gPTP's time (802.1AS-2020 11.2.2): the
    /// neighbor answers, from one port, its rate is known and the link is
    /// short enough.
    pub fn as_capable(&self) -> bool {
        self.enabled
            && self.lost <= self.config.allowed_lost_responses
            && self.multiple_responders == 0
            && self.rate.is_some()
            && self
                .delays
                .median()
                .is_some_and(|delay| delay <= self.config.neighbor_prop_delay_thresh)
    }

    /// Why the port may not carry gPTP's time, while it may not: the first
    /// of [`as_capable`](Self::as_capable)'s conditions to fail.
    pub fn incapable_because(&self) -> Option<Incapable> {
        let thresh = self.config.neighbor_prop_delay_thresh;
        if !self.enabled {
            Some(Incapable::Down)
        } else if self.lost > self.config.allowed_lost_responses {
            Some(Incapable::Unanswered)
        } else if self.multiple_responders > 0 {
            Some(Incapable::MultipleResponders)
        } else if self.rate.is_none() && self.rate_outliers > self.config.allowed_rate_outliers {
            Some(Incapable::ImplausibleRate)
        } else if self.rate.is_none() {
            Some(Incapable::NoRate)
        } else {
            match self.delays.median() {
                None => Some(Incapable::NoDelay),
                Some(delay) if delay > thresh => Some(Incapable::LongDelay),
                Some(_) => None,
            }
        }
    }

    /// What is known of the neighbor, once a rate and a delay are.
    pub fn measurement(&self) -> Option<Measurement> {
        Some(Measurement {
            mean_link_delay: self.delays.median()?,
            neighbor_rate_ratio: self.rate?,
            neighbor: self.neighbor?,
        })
    }

    /// When [`handle_timeout`](Self::handle_timeout) is next due.
    pub fn poll_timeout(&self) -> Option<Duration> {
        self.next_request
    }

    /// Sends the next request when it is due, counting the last one lost
    /// if nothing whole answered it.
    pub fn handle_timeout(&mut self, now: Duration) -> Option<Transmit> {
        let due = self.next_request.filter(|due| *due <= now)?;
        if self.multiple_responders >= self.config.cease_after_multiple {
            self.multiple_responders = 0;
            self.request = None;
            self.next_request = Some(now + self.config.cease_for);
            return None;
        }
        if self.request.is_some() {
            self.lost = self.lost.saturating_add(1);
            if self.lost > self.config.allowed_lost_responses {
                self.forget_neighbor();
            }
        }
        let interval = interval(self.config.log_interval);
        // Keep the cadence, but never queue a burst after a stall.
        let mut next = due + interval;
        if next <= now {
            next = now + interval;
        }
        self.next_request = Some(next);
        self.sequence = self.sequence.wrapping_add(1);
        self.request = Some(Request {
            sequence: self.sequence,
            sent: None,
            response: None,
            multiple: false,
            distrusted: false,
        });
        Some(Transmit {
            message: Message {
                header: self.header(
                    self.sequence,
                    Flags::PTP_TIMESCALE,
                    RESPONSE_CONTROL,
                    self.config.log_interval,
                ),
                body: Body::PdelayReq { reserved: [0; 20] },
                tlvs: self.tlvs(),
            },
            stamp: true,
        })
    }

    /// The time a message the link sent left, on the LocalClock. For a
    /// response it gives back the follow up that carries that time.
    pub fn handle_transmit_time(
        &mut self,
        message_type: MessageType,
        sequence: u16,
        time: i128,
    ) -> Option<Transmit> {
        match message_type {
            MessageType::PdelayReq => {
                if let Some(request) = self
                    .request
                    .as_mut()
                    .filter(|request| request.sequence == sequence)
                {
                    request.sent = Some(time);
                }
                None
            }
            MessageType::PdelayResp => {
                let requesting = self.answering.take(sequence)?;
                Some(Transmit {
                    message: Message {
                        header: self.header(
                            sequence,
                            Flags::PTP_TIMESCALE,
                            RESPONSE_CONTROL,
                            NO_INTERVAL,
                        ),
                        body: Body::PdelayRespFollowUp {
                            response_origin: Timestamp::from_nanos(time)?,
                            requesting,
                        },
                        tlvs: self.tlvs(),
                    },
                    stamp: false,
                })
            }
            _ => None,
        }
    }

    /// Distrusts the time stamp of the response to request `sequence`, the
    /// frame heard after it having come without one: the response may
    /// carry that frame's time. Whether such a response waits for its
    /// follow up.
    pub fn distrust_response(&mut self, sequence: u16) -> bool {
        let Some(request) = self
            .request
            .as_mut()
            .filter(|request| request.sequence == sequence && request.response.is_some())
        else {
            return false;
        };
        request.distrusted = true;
        true
    }

    /// A Pdelay message heard on the port, `received` its arrival on the
    /// LocalClock. A request gives back the response to send.
    pub fn handle_message(&mut self, message: &Message, received: i128) -> Option<Transmit> {
        if !self.enabled {
            return None;
        }
        match message.body {
            Body::PdelayReq { .. } => {
                let request_receipt = Timestamp::from_nanos(received)?;
                let requesting = message.header.source;
                let sequence = message.header.sequence_id;
                self.answering.push(sequence, requesting);
                Some(Transmit {
                    message: Message {
                        header: self.header(
                            sequence,
                            Flags::TWO_STEP | Flags::PTP_TIMESCALE,
                            RESPONSE_CONTROL,
                            NO_INTERVAL,
                        ),
                        body: Body::PdelayResp {
                            request_receipt,
                            requesting,
                        },
                        tlvs: self.tlvs(),
                    },
                    stamp: true,
                })
            }
            Body::PdelayResp {
                request_receipt,
                requesting,
            } if requesting == self.own => {
                let request = self
                    .request
                    .as_mut()
                    .filter(|request| request.sequence == message.header.sequence_id)?;
                let responder = message.header.source;
                match request.response {
                    Some((first, ..)) if first != responder => request.multiple = true,
                    Some(_) => {}
                    None => {
                        let t2 =
                            request_receipt.nanos() + correction_nanos(message.header.correction);
                        request.response = Some((responder, t2, received));
                    }
                }
                None
            }
            Body::PdelayRespFollowUp {
                response_origin,
                requesting,
            } if requesting == self.own => {
                let request = self
                    .request
                    .filter(|request| request.sequence == message.header.sequence_id)?;
                let (responder, t2, t4) = request.response?;
                if responder != message.header.source {
                    return None;
                }
                let t1 = request.sent?;
                self.request = None;
                if request.multiple {
                    self.multiple_responders = self.multiple_responders.saturating_add(1);
                    self.forget_neighbor();
                    return None;
                }
                self.multiple_responders = 0;
                self.lost = 0;
                if self.neighbor.is_some_and(|neighbor| neighbor != responder) {
                    self.forget_neighbor();
                }
                self.neighbor = Some(responder);
                if request.distrusted {
                    self.distrusted = self.distrusted.saturating_add(1);
                    if self.distrusted <= ALLOWED_DISTRUSTED_EXCHANGES {
                        return None;
                    }
                } else {
                    self.distrusted = 0;
                }
                let t3 = response_origin.nanos() + correction_nanos(message.header.correction);
                self.measure(t1, t2, t3, t4);
                None
            }
            _ => None,
        }
    }

    /// Takes one exchange's four times into the rate and the delay.
    fn measure(&mut self, t1: i128, t2: i128, t3: i128, t4: i128) {
        let measured = self.anchor.map(|(anchor_t3, anchor_t4)| {
            let local = (t4 - anchor_t4) as f64;
            let neighbor = (t3 - anchor_t3) as f64;
            if local > 0.0 { neighbor / local } else { 0.0 }
        });
        if measured.is_some_and(|ratio| (ratio - 1.0).abs() > self.config.plausible_rate) {
            self.rate_outliers = self.rate_outliers.saturating_add(1);
            if self.rate_outliers > self.config.allowed_rate_outliers {
                self.rate = None;
            }
            // The exchange may span a step on either side: its delay
            // is no measurement either.
            self.anchor = Some((t3, t4));
            return;
        }
        let Some(rate) = measured.or(self.rate) else {
            self.anchor = Some((t3, t4));
            return;
        };
        // 802.1AS-2020 11.2.19.3.4: the round trip in the neighbor's time
        // less its turnaround, halved, in this system's time base.
        let delay = (((t4 - t1) as f64) * rate - (t3 - t2) as f64) / 2.0 / rate;
        if !delay.is_finite() || delay <= -self.config.neighbor_prop_delay_thresh * 10.0 {
            return;
        }
        // Once a few are kept, one far from their median comes of a bad
        // time stamp on either side, not of the link: the exchange is left
        // out, its rate too, unless enough in a row say the link changed,
        // and then they are kept in place of the old.
        if let Some(median) = self.delays.median().filter(|_| self.delays.len() >= 3)
            && (delay - median).abs() > self.config.delay_outlier
        {
            self.outlier = Some((delay, median));
            self.changed.push(delay);
            self.delay_outliers = self.delay_outliers.saturating_add(1);
            if self.delay_outliers <= self.config.allowed_delay_outliers {
                return;
            }
            // The rate since the anchor spans the old measurements and the
            // new, and is neither's: the next one starts from here.
            self.delays = self.changed.clone();
            self.changed.clear();
            self.delay_outliers = 0;
            self.anchor = Some((t3, t4));
            return;
        }
        self.delays.push(delay);
        self.changed.clear();
        self.delay_outliers = 0;
        if let Some(ratio) = measured {
            self.rate = Some(ratio);
            self.rate_outliers = 0;
        }
        self.anchor = Some((t3, t4));
    }

    /// The last delay measurement left out as an outlier, and the median
    /// it stood from, once.
    pub fn take_outlier(&mut self) -> Option<(f64, f64)> {
        self.outlier.take()
    }

    fn forget_neighbor(&mut self) {
        self.anchor = None;
        self.rate = None;
        self.rate_outliers = 0;
        self.delays.clear();
        self.changed.clear();
        self.delay_outliers = 0;
        self.distrusted = 0;
        self.neighbor = None;
    }

    fn header(&self, sequence: u16, flags: u16, control: u8, log_interval: i8) -> Header {
        Header {
            major_sdo_id: 1,
            minor_version: self.config.minor_version,
            version: 2,
            domain: 0,
            minor_sdo_id: 0,
            flags: Flags(flags),
            correction: 0,
            type_specific: 0,
            source: self.own,
            sequence_id: sequence,
            control,
            log_interval,
        }
    }
}

/// 2^log seconds.
pub fn interval(log: i8) -> Duration {
    let log = i32::from(log.clamp(-20, 20));
    if log >= 0 {
        Duration::from_secs(1 << log)
    } else {
        Duration::from_nanos(1_000_000_000 >> -log)
    }
}

/// A correctionField's nanoseconds, its fraction dropped.
pub fn correction_nanos(correction: i64) -> i128 {
    i128::from(correction >> 16)
}

/// The median of the last few delay measurements: a few wild ones, as
/// from a bad time stamp, do not move it.
#[derive(Debug, Clone)]
struct DelayFilter {
    samples: [f64; 16],
    count: usize,
    next: usize,
    capacity: usize,
}

impl DelayFilter {
    fn new(capacity: usize) -> Self {
        Self {
            samples: [0.0; 16],
            count: 0,
            next: 0,
            capacity: capacity.clamp(1, 16),
        }
    }

    fn push(&mut self, sample: f64) {
        self.samples[self.next] = sample;
        self.next = (self.next + 1) % self.capacity;
        self.count = (self.count + 1).min(self.capacity);
    }

    fn clear(&mut self) {
        self.count = 0;
        self.next = 0;
    }

    fn len(&self) -> usize {
        self.count
    }

    fn median(&self) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let mut sorted = self.samples;
        let sorted = &mut sorted[..self.count];
        sorted.sort_unstable_by(f64::total_cmp);
        let middle = self.count / 2;
        Some(if self.count.is_multiple_of(2) {
            (sorted[middle - 1] + sorted[middle]) / 2.0
        } else {
            sorted[middle]
        })
    }
}

/// The few responses a port owes its neighbors at once, without `alloc`.
mod heapless_queue {
    use crate::message::PortIdentity;

    #[derive(Debug, Clone, Default)]
    pub struct Queue {
        slots: [Option<(u16, PortIdentity)>; 4],
    }

    impl Queue {
        /// Keeps the requester of `sequence` until its response's time is
        /// known, dropping the oldest when full.
        pub fn push(&mut self, sequence: u16, requesting: PortIdentity) {
            let slot = self
                .slots
                .iter()
                .position(Option::is_none)
                .unwrap_or_else(|| {
                    self.slots.rotate_left(1);
                    self.slots.len() - 1
                });
            self.slots[slot] = Some((sequence, requesting));
        }

        pub fn take(&mut self, sequence: u16) -> Option<PortIdentity> {
            let slot = self
                .slots
                .iter()
                .position(|slot| slot.is_some_and(|(each, _)| each == sequence))?;
            self.slots[slot].take().map(|(_, requesting)| requesting)
        }
    }
}

#[cfg(test)]
mod tests;
