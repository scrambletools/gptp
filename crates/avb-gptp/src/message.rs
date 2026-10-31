//! gPTP's messages (802.1AS-2020 clauses 10 and 11, on IEEE 1588's
//! formats): a common header, a body by message type, and the TLVs that
//! follow it.
//!
//! [`Message::decode`] reads the octets after the Ethernet header, up to
//! the header's messageLength, so Ethernet padding is left behind. The
//! header and body decode into plain values and the TLVs stay a view over
//! the received octets, read with [`Message::tlvs`]. [`Message::encode`]
//! writes a message into the caller's buffer, setting messageLength, and
//! gives back the same octets for anything decoded: reserved fields are
//! kept, and so are TLVs this crate does not interpret, such as a
//! vendor's. Nothing here allocates.

use core::fmt;

/// The length of the common header.
pub const HEADER_LENGTH: usize = 34;

/// tlvType of an organization extension, as 802.1AS's own TLVs are.
pub const ORGANIZATION_EXTENSION: u16 = 0x0003;
/// tlvType of the path trace TLV.
pub const PATH_TRACE: u16 = 0x0008;
/// IEEE 802.1's organization identifier, 00-80-C2.
pub const IEEE_802_1: u32 = 0x0080c2;

/// IEEE 1588's identity of a clock, its EUI-64. It orders as its octets
/// do, as the BTCA compares identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ClockIdentity(pub u64);

/// A clock's port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PortIdentity {
    pub clock: ClockIdentity,
    pub port: u16,
}

/// A PTP timestamp: 48 bits of seconds and the nanoseconds within one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp {
    pub seconds: u64,
    pub nanoseconds: u32,
}

impl Timestamp {
    /// The time in nanoseconds from the epoch.
    pub fn nanos(self) -> i128 {
        i128::from(self.seconds) * 1_000_000_000 + i128::from(self.nanoseconds)
    }

    /// The timestamp `nanos` nanoseconds from the epoch, if that is
    /// neither before it nor past 48 bits of seconds.
    pub fn from_nanos(nanos: i128) -> Option<Self> {
        let seconds = u64::try_from(nanos.div_euclid(1_000_000_000)).ok()?;
        (seconds < 1 << 48).then(|| Self {
            seconds,
            nanoseconds: nanos.rem_euclid(1_000_000_000) as u32,
        })
    }
}

/// The type of a message, the low nibble of its first octet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageType {
    Sync = 0x0,
    PdelayReq = 0x2,
    PdelayResp = 0x3,
    FollowUp = 0x8,
    PdelayRespFollowUp = 0xa,
    Announce = 0xb,
    Signaling = 0xc,
}

impl MessageType {
    fn of(nibble: u8) -> Option<Self> {
        Some(match nibble {
            0x0 => Self::Sync,
            0x2 => Self::PdelayReq,
            0x3 => Self::PdelayResp,
            0x8 => Self::FollowUp,
            0xa => Self::PdelayRespFollowUp,
            0xb => Self::Announce,
            0xc => Self::Signaling,
            _ => return None,
        })
    }

    /// Whether its transmission and reception are time stamped.
    pub fn event(self) -> bool {
        matches!(self, Self::Sync | Self::PdelayReq | Self::PdelayResp)
    }
}

/// The header's flags, as their two octets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Flags(pub u16);

impl Flags {
    /// A Follow_Up carries this message's precise time.
    pub const TWO_STEP: u16 = 0x0200;
    /// The grandmaster's time is on the PTP timescale.
    pub const PTP_TIMESCALE: u16 = 0x0008;

    pub fn two_step(self) -> bool {
        self.0 & Self::TWO_STEP != 0
    }
}

/// The common header, but for the message type, which the body gives,
/// and messageLength, which encoding works out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Header {
    /// 1 for gPTP (transportSpecific in 802.1AS-2011).
    pub major_sdo_id: u8,
    pub minor_version: u8,
    pub version: u8,
    pub domain: u8,
    pub minor_sdo_id: u8,
    pub flags: Flags,
    /// In nanoseconds times 2^16.
    pub correction: i64,
    pub type_specific: u32,
    pub source: PortIdentity,
    pub sequence_id: u16,
    pub control: u8,
    pub log_interval: i8,
}

/// An Announce's fields after the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Announce {
    pub origin: Timestamp,
    pub current_utc_offset: i16,
    pub reserved: u8,
    pub priority1: u8,
    pub clock_class: u8,
    pub clock_accuracy: u8,
    pub offset_scaled_log_variance: u16,
    pub priority2: u8,
    pub grandmaster: ClockIdentity,
    pub steps_removed: u16,
    pub time_source: u8,
}

/// A message's fields after the header, by its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Body {
    /// Two-step in gPTP, so the origin is zero or a rough time.
    Sync {
        origin: Timestamp,
    },
    FollowUp {
        precise_origin: Timestamp,
    },
    /// Ten octets of origin and ten reserved, kept as sent.
    PdelayReq {
        reserved: [u8; 20],
    },
    PdelayResp {
        request_receipt: Timestamp,
        requesting: PortIdentity,
    },
    PdelayRespFollowUp {
        response_origin: Timestamp,
        requesting: PortIdentity,
    },
    Announce(Announce),
    Signaling {
        target: PortIdentity,
    },
}

impl Body {
    pub fn message_type(&self) -> MessageType {
        match self {
            Self::Sync { .. } => MessageType::Sync,
            Self::FollowUp { .. } => MessageType::FollowUp,
            Self::PdelayReq { .. } => MessageType::PdelayReq,
            Self::PdelayResp { .. } => MessageType::PdelayResp,
            Self::PdelayRespFollowUp { .. } => MessageType::PdelayRespFollowUp,
            Self::Announce(_) => MessageType::Announce,
            Self::Signaling { .. } => MessageType::Signaling,
        }
    }

    /// The octets the body takes after the header.
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Sync { .. } | Self::FollowUp { .. } | Self::Signaling { .. } => 10,
            Self::PdelayReq { .. } | Self::PdelayResp { .. } | Self::PdelayRespFollowUp { .. } => {
                20
            }
            Self::Announce(_) => 30,
        }
    }
}

/// The Follow_Up information TLV (802.1AS-2020 11.4.4.3): how the
/// grandmaster's rate and time base stand, as each relay updates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FollowUpInformation {
    /// The grandmaster's rate over this system's, less one, times 2^41.
    pub cumulative_scaled_rate_offset: i32,
    pub gm_time_base_indicator: u16,
    /// A 96-bit ScaledNs, sign-extended.
    pub last_gm_phase_change: i128,
    pub scaled_last_gm_freq_change: i32,
}

/// The message interval request TLV (802.1AS-2020 10.6.4.3), a
/// neighbor's ask for this port's Pdelay, Sync and Announce intervals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IntervalRequest {
    pub tlv_type: u16,
    pub link_delay: i8,
    pub time_sync: i8,
    pub announce: i8,
    pub flags: u8,
    pub reserved: [u8; 2],
}

/// A path trace TLV's clocks, the grandmaster's first, as a view over
/// the received octets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PathTrace<'a>(&'a [u8]);

impl<'a> PathTrace<'a> {
    /// Its clocks, in order.
    pub fn iter(&self) -> impl Iterator<Item = ClockIdentity> + 'a {
        let (clocks, _) = self.0.as_chunks::<8>();
        clocks
            .iter()
            .map(|octets| ClockIdentity(u64::from_be_bytes(*octets)))
    }

    pub fn len(&self) -> usize {
        self.0.len() / 8
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A TLV after a message's body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tlv<'a> {
    FollowUpInformation(FollowUpInformation),
    PathTrace(PathTrace<'a>),
    IntervalRequest(IntervalRequest),
    /// Any other TLV, as received.
    Other {
        tlv_type: u16,
        value: &'a [u8],
    },
}

impl<'a> Tlv<'a> {
    /// A path trace TLV of `clocks`, each an identity's 8 octets.
    pub fn path_trace(clocks: &'a [u8]) -> Option<Self> {
        clocks
            .len()
            .is_multiple_of(8)
            .then_some(Self::PathTrace(PathTrace(clocks)))
    }

    /// The octets [`encode`](Self::encode) writes, type and length
    /// included.
    pub fn encoded_len(&self) -> usize {
        4 + match self {
            Self::FollowUpInformation(_) => 28,
            Self::PathTrace(path) => path.0.len(),
            Self::IntervalRequest(_) => 12,
            Self::Other { value, .. } => value.len(),
        }
    }

    /// Encodes into the start of `out`, returning the octets written.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        let length = self.encoded_len();
        let mut writer = Writer::new(out, length)?;
        match self {
            Self::FollowUpInformation(information) => {
                writer.organization(ORGANIZATION_EXTENSION, 28, 1);
                writer.put(&information.cumulative_scaled_rate_offset.to_be_bytes());
                writer.put(&information.gm_time_base_indicator.to_be_bytes());
                writer.put(&information.last_gm_phase_change.to_be_bytes()[4..]);
                writer.put(&information.scaled_last_gm_freq_change.to_be_bytes());
            }
            Self::PathTrace(path) => {
                writer.put(&PATH_TRACE.to_be_bytes());
                writer.put(&(path.0.len() as u16).to_be_bytes());
                writer.put(path.0);
            }
            Self::IntervalRequest(request) => {
                writer.organization(request.tlv_type, 12, 2);
                writer.put(&[
                    request.link_delay as u8,
                    request.time_sync as u8,
                    request.announce as u8,
                    request.flags,
                ]);
                writer.put(&request.reserved);
            }
            Self::Other { tlv_type, value } => {
                writer.put(&tlv_type.to_be_bytes());
                writer.put(&(value.len() as u16).to_be_bytes());
                writer.put(value);
            }
        }
        Ok(length)
    }

    /// The TLV of `tlv_type` with `value`, read as one this crate knows
    /// where it is one and has that TLV's length.
    fn read(tlv_type: u16, value: &'a [u8]) -> Self {
        Self::known(tlv_type, value).unwrap_or(Self::Other { tlv_type, value })
    }

    fn known(tlv_type: u16, value: &'a [u8]) -> Option<Self> {
        if tlv_type == PATH_TRACE {
            return Self::path_trace(value);
        }
        if value.len() < 6 || read_u24(value, 0) != IEEE_802_1 {
            return None;
        }
        match (tlv_type, read_u24(value, 3), value.len()) {
            (ORGANIZATION_EXTENSION, 1, 28) => {
                let phase: [u8; 12] = value[12..24].try_into().ok()?;
                let mut wide = [if phase[0] & 0x80 != 0 { 0xff } else { 0 }; 16];
                wide[4..].copy_from_slice(&phase);
                Some(Self::FollowUpInformation(FollowUpInformation {
                    cumulative_scaled_rate_offset: read_u32(value, 6) as i32,
                    gm_time_base_indicator: read_u16(value, 10),
                    last_gm_phase_change: i128::from_be_bytes(wide),
                    scaled_last_gm_freq_change: read_u32(value, 24) as i32,
                }))
            }
            (_, 2, 12) => Some(Self::IntervalRequest(IntervalRequest {
                tlv_type,
                link_delay: value[6] as i8,
                time_sync: value[7] as i8,
                announce: value[8] as i8,
                flags: value[9],
                reserved: [value[10], value[11]],
            })),
            _ => None,
        }
    }
}

/// A message's TLVs, in order, from octets [`Message::decode`] has
/// checked: each TLV's length within the message.
#[derive(Debug, Clone)]
pub struct Tlvs<'a>(&'a [u8]);

impl<'a> Iterator for Tlvs<'a> {
    type Item = Tlv<'a>;

    fn next(&mut self) -> Option<Tlv<'a>> {
        let (tlv_type, value, rest) = split_tlv(self.0)?;
        self.0 = rest;
        Some(Tlv::read(tlv_type, value))
    }
}

/// The first TLV in `octets` and the octets after it, if a whole one is
/// there.
fn split_tlv(octets: &[u8]) -> Option<(u16, &[u8], &[u8])> {
    let length = usize::from(read_u16(octets.get(..4)?, 2));
    let value = octets.get(4..4 + length)?;
    Some((read_u16(octets, 0), value, &octets[4 + length..]))
}

/// Why octets did not decode as a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The octets end before the message's length, or its body.
    Truncated { needed: usize, available: usize },
    /// Not PTP version 2.
    UnsupportedVersion(u8),
    /// A message type gPTP does not use.
    WrongMessageType(u8),
    /// A TLV running past the message's end.
    BadTlv,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, available } => {
                write!(
                    formatter,
                    "message too short: {available} octets, {needed} needed"
                )
            }
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported PTP version {version}")
            }
            Self::WrongMessageType(message_type) => {
                write!(formatter, "message type {message_type:#x} is not gPTP's")
            }
            Self::BadTlv => formatter.write_str("a TLV runs past the message's end"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// Why a message could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// The buffer is smaller than the encoded message.
    BufferTooSmall { needed: usize, available: usize },
    /// The message is longer than messageLength can say.
    TooLong(usize),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferTooSmall { needed, available } => {
                write!(
                    formatter,
                    "buffer too small: {available} octets, {needed} needed"
                )
            }
            Self::TooLong(length) => write!(formatter, "a message of {length} octets is too long"),
        }
    }
}

impl core::error::Error for EncodeError {}

/// A gPTP message: its header and body, and its TLVs as encoded octets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Message<'a> {
    pub header: Header,
    pub body: Body,
    /// The TLVs after the body, as on the wire; read them with
    /// [`tlvs`](Self::tlvs), and build them with [`Tlv::encode`].
    pub tlvs: &'a [u8],
}

impl<'a> Message<'a> {
    pub fn message_type(&self) -> MessageType {
        self.body.message_type()
    }

    /// The message in `octets`, which start after the Ethernet header.
    pub fn decode(octets: &'a [u8]) -> Result<Self, DecodeError> {
        if octets.len() < 4 {
            return Err(DecodeError::Truncated {
                needed: HEADER_LENGTH,
                available: octets.len(),
            });
        }
        let version = octets[1] & 0x0f;
        if version != 2 {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let nibble = octets[0] & 0x0f;
        let message_type = MessageType::of(nibble).ok_or(DecodeError::WrongMessageType(nibble))?;
        let length = usize::from(read_u16(octets, 2));
        let body_end = HEADER_LENGTH + body_length(message_type);
        if length < body_end {
            // messageLength itself leaves no room for the body.
            return Err(DecodeError::Truncated {
                needed: body_end,
                available: length,
            });
        }
        if octets.len() < length {
            return Err(DecodeError::Truncated {
                needed: length,
                available: octets.len(),
            });
        }
        let octets = &octets[..length];
        let header = Header {
            major_sdo_id: octets[0] >> 4,
            minor_version: octets[1] >> 4,
            version,
            domain: octets[4],
            minor_sdo_id: octets[5],
            flags: Flags(read_u16(octets, 6)),
            correction: read_u64(octets, 8) as i64,
            type_specific: read_u32(octets, 16),
            source: read_port_identity(octets, 20),
            sequence_id: read_u16(octets, 30),
            control: octets[32],
            log_interval: octets[33] as i8,
        };
        let at = HEADER_LENGTH;
        let body = match message_type {
            MessageType::Sync => Body::Sync {
                origin: read_timestamp(octets, at),
            },
            MessageType::FollowUp => Body::FollowUp {
                precise_origin: read_timestamp(octets, at),
            },
            MessageType::PdelayReq => Body::PdelayReq {
                reserved: octets[at..at + 20].try_into().expect("20 octets"),
            },
            MessageType::PdelayResp => Body::PdelayResp {
                request_receipt: read_timestamp(octets, at),
                requesting: read_port_identity(octets, at + 10),
            },
            MessageType::PdelayRespFollowUp => Body::PdelayRespFollowUp {
                response_origin: read_timestamp(octets, at),
                requesting: read_port_identity(octets, at + 10),
            },
            MessageType::Announce => Body::Announce(Announce {
                origin: read_timestamp(octets, at),
                current_utc_offset: read_u16(octets, at + 10) as i16,
                reserved: octets[at + 12],
                priority1: octets[at + 13],
                clock_class: octets[at + 14],
                clock_accuracy: octets[at + 15],
                offset_scaled_log_variance: read_u16(octets, at + 16),
                priority2: octets[at + 18],
                grandmaster: ClockIdentity(read_u64(octets, at + 19)),
                steps_removed: read_u16(octets, at + 27),
                time_source: octets[at + 29],
            }),
            MessageType::Signaling => Body::Signaling {
                target: read_port_identity(octets, at),
            },
        };
        let tlvs = &octets[body_end..];
        let mut rest = tlvs;
        while !rest.is_empty() {
            let (_, _, after) = split_tlv(rest).ok_or(DecodeError::BadTlv)?;
            rest = after;
        }
        Ok(Self { header, body, tlvs })
    }

    /// The octets [`encode`](Self::encode) writes.
    pub fn encoded_len(&self) -> usize {
        HEADER_LENGTH + self.body.encoded_len() + self.tlvs.len()
    }

    /// Encodes into the start of `out`, returning the octets written.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        let length = self.encoded_len();
        let message_length = u16::try_from(length).map_err(|_| EncodeError::TooLong(length))?;
        let header = &self.header;
        let mut writer = Writer::new(out, length)?;
        writer.put(&[
            header.major_sdo_id << 4 | self.message_type() as u8,
            header.minor_version << 4 | (header.version & 0x0f),
        ]);
        writer.put(&message_length.to_be_bytes());
        writer.put(&[header.domain, header.minor_sdo_id]);
        writer.put(&header.flags.0.to_be_bytes());
        writer.put(&header.correction.to_be_bytes());
        writer.put(&header.type_specific.to_be_bytes());
        writer.port_identity(header.source);
        writer.put(&header.sequence_id.to_be_bytes());
        writer.put(&[header.control, header.log_interval as u8]);
        match &self.body {
            Body::Sync { origin } => writer.timestamp(*origin),
            Body::FollowUp { precise_origin } => writer.timestamp(*precise_origin),
            Body::PdelayReq { reserved } => writer.put(reserved),
            Body::PdelayResp {
                request_receipt: time,
                requesting,
            }
            | Body::PdelayRespFollowUp {
                response_origin: time,
                requesting,
            } => {
                writer.timestamp(*time);
                writer.port_identity(*requesting);
            }
            Body::Announce(announce) => {
                writer.timestamp(announce.origin);
                writer.put(&announce.current_utc_offset.to_be_bytes());
                writer.put(&[
                    announce.reserved,
                    announce.priority1,
                    announce.clock_class,
                    announce.clock_accuracy,
                ]);
                writer.put(&announce.offset_scaled_log_variance.to_be_bytes());
                writer.put(&[announce.priority2]);
                writer.put(&announce.grandmaster.0.to_be_bytes());
                writer.put(&announce.steps_removed.to_be_bytes());
                writer.put(&[announce.time_source]);
            }
            Body::Signaling { target } => writer.port_identity(*target),
        }
        writer.put(self.tlvs);
        Ok(length)
    }

    /// Its TLVs, in order.
    pub fn tlvs(&self) -> Tlvs<'a> {
        Tlvs(self.tlvs)
    }

    /// The Follow_Up information TLV, if the message carries one.
    pub fn follow_up_information(&self) -> Option<FollowUpInformation> {
        self.tlvs().find_map(|tlv| match tlv {
            Tlv::FollowUpInformation(information) => Some(information),
            _ => None,
        })
    }

    /// The path trace, if the message carries one.
    pub fn path_trace(&self) -> Option<PathTrace<'a>> {
        self.tlvs().find_map(|tlv| match tlv {
            Tlv::PathTrace(path) => Some(path),
            _ => None,
        })
    }
}

fn body_length(message_type: MessageType) -> usize {
    match message_type {
        MessageType::Sync | MessageType::FollowUp | MessageType::Signaling => 10,
        MessageType::PdelayReq | MessageType::PdelayResp | MessageType::PdelayRespFollowUp => 20,
        MessageType::Announce => 30,
    }
}

fn read_u16(octets: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([octets[at], octets[at + 1]])
}

fn read_u24(octets: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([0, octets[at], octets[at + 1], octets[at + 2]])
}

fn read_u32(octets: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(octets[at..at + 4].try_into().expect("4 octets"))
}

fn read_u64(octets: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(octets[at..at + 8].try_into().expect("8 octets"))
}

fn read_timestamp(octets: &[u8], at: usize) -> Timestamp {
    Timestamp {
        seconds: u64::from(read_u16(octets, at)) << 32 | u64::from(read_u32(octets, at + 2)),
        nanoseconds: read_u32(octets, at + 6),
    }
}

fn read_port_identity(octets: &[u8], at: usize) -> PortIdentity {
    PortIdentity {
        clock: ClockIdentity(read_u64(octets, at)),
        port: read_u16(octets, at + 8),
    }
}

/// Writes fields one after another into a buffer checked to be long
/// enough for all of them.
struct Writer<'b> {
    out: &'b mut [u8],
    at: usize,
}

impl<'b> Writer<'b> {
    fn new(out: &'b mut [u8], length: usize) -> Result<Self, EncodeError> {
        if out.len() < length {
            return Err(EncodeError::BufferTooSmall {
                needed: length,
                available: out.len(),
            });
        }
        Ok(Self { out, at: 0 })
    }

    fn put(&mut self, octets: &[u8]) {
        self.out[self.at..self.at + octets.len()].copy_from_slice(octets);
        self.at += octets.len();
    }

    fn timestamp(&mut self, time: Timestamp) {
        self.put(&((time.seconds >> 32) as u16).to_be_bytes());
        self.put(&(time.seconds as u32).to_be_bytes());
        self.put(&time.nanoseconds.to_be_bytes());
    }

    fn port_identity(&mut self, identity: PortIdentity) {
        self.put(&identity.clock.0.to_be_bytes());
        self.put(&identity.port.to_be_bytes());
    }

    fn organization(&mut self, tlv_type: u16, length: u16, sub_type: u32) {
        self.put(&tlv_type.to_be_bytes());
        self.put(&length.to_be_bytes());
        self.put(&IEEE_802_1.to_be_bytes()[1..]);
        self.put(&sub_type.to_be_bytes()[1..]);
    }
}

#[cfg(test)]
mod tests;
