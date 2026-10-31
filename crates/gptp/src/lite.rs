//! The AVB Lite profile's PTP side, for an endpoint: telling from the
//! Pdelay exchange whether an AVB bridge is its neighbor, and falling
//! back to AVB Lite when none is (profile 2.2); the Endpoint Declaration
//! TLV its Pdelay_Req carry (2.1) and the beacon it sends once fallen back
//! (2.3); the Grandmaster Link TLV (5) and the asymmetry mixed link speeds
//! make.
//!
//! The endpoint's own link asks for peer delay every second; the caller
//! tells [`Fallback`] of each request it sent and hands it every PTP
//! message heard. Once fallen back the decision holds until the link
//! comes up again, and the link stops asking: [`Fallback`] then sends the
//! beacon in its place.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use crate::message::{ClockIdentity, PortIdentity};

/// Where the AVB Lite PTP profile sends its messages, end to end through
/// switches that know nothing of AVB.
pub const PTP_DESTINATION: [u8; 6] = [0x01, 0x1b, 0x19, 0x00, 0x00, 0x00];

/// PTP message types.
const PDELAY_REQ: u8 = 0x2;
const PDELAY_RESP: u8 = 0x3;
const PDELAY_RESP_FOLLOW_UP: u8 = 0xa;
const ANNOUNCE: u8 = 0xb;
/// Octets of a Pdelay message before any TLV.
const PDELAY_LEN: usize = 54;
/// Octets of an Announce before any TLV.
const ANNOUNCE_LEN: usize = 64;
/// The Endpoint Declaration TLV (profile 2.1): an organization extension
/// under the AVB Lite MA-S, saying the sender is an endpoint, of the
/// do-not-propagate type IEEE 1588-2019 asks of new extensions.
pub const ENDPOINT_TLV: [u8; 12] = [
    0x80, 0x00, 0x00, 0x08, 0x8c, 0x1f, 0x64, 0x36, 0xc0, 0x01, 0x01, 0x00,
];
/// The organization extension types the TLV comes in: the profile's,
/// and the one its earlier revisions used.
const ORGANIZATION_EXTENSION_DO_NOT_PROPAGATE: u16 = 0x8000;
const ORGANIZATION_EXTENSION: u16 = 0x0003;
/// The Grandmaster Link TLV's identifiers (profile 5): the AVB Lite MA-S
/// and sub-id 0x004.
const GRANDMASTER_LINK: [u8; 6] = [0x8c, 0x1f, 0x64, 0x36, 0xc0, 0x04];
/// The octets of a Sync or Delay_Req frame on the wire, a minimum-size
/// frame, which the link-speed asymmetry counts.
const PTP_FRAME_OCTETS: f64 = 64.0;

/// How often the beacon goes out once fallen back.
const BEACON_INTERVAL: Duration = Duration::from_secs(3);
/// Requests in a row unanswered that make the fallback (profile 2.2,
/// condition 2).
const MOST_UNANSWERED: u32 = 9;
/// How long after starting to weigh another endpoint's beacon, sent every
/// 3 s, has been heard twice at least.
const SETTLING: Duration = Duration::from_secs(10);

/// Why an endpoint fell back to AVB Lite (profile 2.2), named as
/// GET_LITE_STATUS names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    /// Another endpoint said it is one, so no bridge lies between.
    EndpointTlv,
    /// Nothing answered nine requests in a row.
    PdelayUnanswered,
    /// More than one clock answered one request, or more than one peer
    /// asked: a switch floods the bridge-group address.
    MultipleResponders,
    /// An operator asked for AVB Lite.
    Configured,
}

impl Reason {
    /// As gptpd's status names it.
    pub fn name(self) -> &'static str {
        match self {
            Reason::EndpointTlv => "endpoint-tlv",
            Reason::PdelayUnanswered => "pdelay-unanswered",
            Reason::MultipleResponders => "multiple-responders",
            Reason::Configured => "configured",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [
            Reason::EndpointTlv,
            Reason::PdelayUnanswered,
            Reason::MultipleResponders,
            Reason::Configured,
        ]
        .into_iter()
        .find(|reason| reason.name() == name)
    }
}

/// A Pdelay_Req from port 1 of `identity` with the Endpoint Declaration
/// TLV, as the beacon goes, and as esp_ptp sends its own.
pub fn pdelay_request(identity: ClockIdentity, sequence: u16) -> Vec<u8> {
    let mut message = vec![0; PDELAY_LEN];
    // gPTP (majorSdoId 1), Pdelay_Req; PTP version 2.
    message[0] = 0x10 | PDELAY_REQ;
    message[1] = 0x02;
    message[2..4].copy_from_slice(&((PDELAY_LEN + ENDPOINT_TLV.len()) as u16).to_be_bytes());
    // The PTP timescale flag.
    message[7] = 0x08;
    message[20..28].copy_from_slice(&identity.0.to_be_bytes());
    message[28..30].copy_from_slice(&1u16.to_be_bytes());
    message[30..32].copy_from_slice(&sequence.to_be_bytes());
    message[32] = 5;
    message.extend_from_slice(&ENDPOINT_TLV);
    message
}

/// The Grandmaster Link TLV for a grandmaster linked at `speed` Mb/s, for
/// its Announce under the AVB Lite PTP profile (profile 5).
pub fn grandmaster_link_tlv(speed: u32) -> [u8; 14] {
    let mut tlv = [0; 14];
    tlv[..2].copy_from_slice(&ORGANIZATION_EXTENSION_DO_NOT_PROPAGATE.to_be_bytes());
    tlv[2..4].copy_from_slice(&10u16.to_be_bytes());
    tlv[4..10].copy_from_slice(&GRANDMASTER_LINK);
    tlv[10..].copy_from_slice(&speed.to_be_bytes());
    tlv
}

/// The grandmaster an Announce names and the link speed in Mb/s its
/// Grandmaster Link TLV gives (profile 5), when it carries one.
pub fn grandmaster_link(message: &[u8]) -> Option<(ClockIdentity, u32)> {
    if message.first()? & 0x0f != ANNOUNCE || message.len() < ANNOUNCE_LEN {
        return None;
    }
    let grandmaster = u64::from_be_bytes(message[53..61].try_into().ok()?);
    let length = usize::from(u16::from_be_bytes([message[2], message[3]])).min(message.len());
    let mut at = ANNOUNCE_LEN;
    while at + 4 <= length {
        let kind = u16::from_be_bytes([message[at], message[at + 1]]);
        let size = usize::from(u16::from_be_bytes([message[at + 2], message[at + 3]]));
        let body = message.get(at + 4..at + 4 + size)?;
        if kind == ORGANIZATION_EXTENSION_DO_NOT_PROPAGATE
            && body.len() >= 10
            && body[..6] == GRANDMASTER_LINK
        {
            let speed = u32::from_be_bytes(body[6..10].try_into().ok()?);
            return (speed > 0).then_some((ClockIdentity(grandmaster), speed));
        }
        at += 4 + size;
    }
    None
}

/// How much longer, in nanoseconds, a minimum-size frame takes from a
/// grandmaster linked at `grandmaster` Mb/s to an endpoint linked at
/// `local` Mb/s than back, halved: PTP's delayAsymmetry (IEEE 1588-2019,
/// 7.4.2), which store-and-forward switches make of the two links'
/// speeds whatever lies between (profile 5). An endpoint following the
/// grandmaster settles that much behind it.
pub fn link_asymmetry(grandmaster: u32, local: u32) -> i64 {
    let held = |megabits: u32| PTP_FRAME_OCTETS * 8.0 * 1000.0 / f64::from(megabits.max(1));
    let halved = (held(grandmaster) - held(local)) / 2.0;
    // Rounded half away from zero, as f64::round, which core lacks.
    if halved < 0.0 {
        (halved - 0.5) as i64
    } else {
        (halved + 0.5) as i64
    }
}

/// Whether a PTP message's TLVs, after its first `start` octets, hold
/// the Endpoint Declaration TLV.
pub fn has_endpoint_tlv(message: &[u8], start: usize) -> bool {
    let length = message
        .get(2..4)
        .map_or(0, |octets| {
            usize::from(u16::from_be_bytes([octets[0], octets[1]]))
        })
        .min(message.len());
    let mut at = start;
    while at + 4 <= length {
        let kind = u16::from_be_bytes([message[at], message[at + 1]]);
        let size = usize::from(u16::from_be_bytes([message[at + 2], message[at + 3]]));
        let Some(body) = message.get(at + 4..at + 4 + size) else {
            return false;
        };
        // The identifiers and dataField; a pad octet may follow.
        if matches!(
            kind,
            ORGANIZATION_EXTENSION_DO_NOT_PROPAGATE | ORGANIZATION_EXTENSION
        ) && body.len() >= 7
            && body[..7] == ENDPOINT_TLV[4..11]
        {
            return true;
        }
        at += 4 + size;
    }
    false
}

/// A message's sourcePortIdentity.
fn source(message: &[u8]) -> Option<PortIdentity> {
    Some(PortIdentity {
        clock: ClockIdentity(u64::from_be_bytes(message.get(20..28)?.try_into().ok()?)),
        port: u16::from_be_bytes(message.get(28..30)?.try_into().ok()?),
    })
}

/// Decides between AVB and AVB Lite from the Pdelay exchange (profile
/// 2.2): an Endpoint Declaration TLV heard, nine requests unanswered, or
/// two clocks answering one request, or two peers asking, each make the
/// fallback, which then holds until the link comes up again. An AVB
/// bridge's port is the one peer that asks and the one that answers;
/// more than one is a switch flooding the bridge-group address.
pub struct Fallback {
    identity: ClockIdentity,
    /// The latest request this system's link sent, and who answered it.
    asked: Option<(u16, Vec<PortIdentity>)>,
    unanswered: u32,
    reason: Option<Reason>,
    configured: bool,
    /// The link is down, so nothing can answer and nothing is weighed.
    link_down: bool,
    /// Since when it has weighed: its start, or the link coming up.
    armed_at: Duration,
    /// The gPTP peers heard asking for peer delay without the Endpoint
    /// Declaration TLV since then.
    peers: Vec<PortIdentity>,
    beacon_sequence: u16,
    next_beacon: Option<Duration>,
    outgoing: VecDeque<Vec<u8>>,
}

impl Fallback {
    /// For the port of clock `identity`; `configured` falls back at once,
    /// as an operator asked.
    pub fn new(identity: ClockIdentity, configured: bool, now: Duration) -> Self {
        let mut fallback = Fallback {
            identity,
            asked: None,
            unanswered: 0,
            reason: None,
            configured,
            link_down: false,
            armed_at: now,
            peers: Vec::new(),
            beacon_sequence: 0,
            next_beacon: None,
            outgoing: VecDeque::new(),
        };
        if configured {
            fallback.fall_back(now, Reason::Configured);
        }
        fallback
    }

    /// Why it fell back, while it holds.
    pub fn reason(&self) -> Option<Reason> {
        self.reason
    }

    /// Whether the link is up. While it is down nothing is weighed; as it
    /// comes up the weighing starts afresh in gPTP (profile 2.2), but for
    /// a fallback an operator asked for.
    pub fn set_link(&mut self, now: Duration, up: bool) {
        if up && self.link_down {
            self.asked = None;
            self.unanswered = 0;
            self.armed_at = now;
            self.peers.clear();
            self.reason = None;
            self.next_beacon = None;
            if self.configured {
                self.fall_back(now, Reason::Configured);
            }
        }
        self.link_down = !up;
    }

    /// How many gPTP peers it has heard asking for peer delay without the
    /// Endpoint Declaration TLV since it started weighing.
    pub fn gptp_peers(&self) -> usize {
        self.peers.len()
    }

    /// Whether it has weighed long enough to have heard another
    /// endpoint's beacon, two of them at least: without a fallback by
    /// then, the link has no AVB Lite endpoint saying so.
    pub fn settled(&self, now: Duration) -> bool {
        !self.link_down && now >= self.armed_at + SETTLING
    }

    fn fall_back(&mut self, now: Duration, reason: Reason) {
        if self.reason.is_none() {
            self.reason = Some(reason);
            self.next_beacon = Some(now);
        }
    }

    /// This system's link sent a Pdelay_Req numbered `sequence`: the one
    /// before is weighed by whether anyone answered it.
    pub fn handle_request(&mut self, now: Duration, sequence: u16) {
        if self.link_down {
            return;
        }
        match self.asked.take() {
            Some((_, answered)) if answered.is_empty() => {
                self.unanswered += 1;
                if self.unanswered >= MOST_UNANSWERED {
                    self.fall_back(now, Reason::PdelayUnanswered);
                }
            }
            Some(_) => self.unanswered = 0,
            None => {}
        }
        self.asked = Some((sequence, Vec::new()));
    }

    /// Hands it a PTP message heard on the port, gPTP's or the AVB Lite
    /// profile's: the octets after the Ethernet header.
    pub fn handle_message(&mut self, now: Duration, message: &[u8]) {
        if self.link_down || message.len() < PDELAY_LEN {
            return;
        }
        let kind = message[0] & 0x0f;
        if !matches!(kind, PDELAY_REQ | PDELAY_RESP | PDELAY_RESP_FOLLOW_UP) {
            return;
        }
        let Some(source) = source(message) else {
            return;
        };
        if source.clock == self.identity {
            return;
        }
        let endpoint = has_endpoint_tlv(message, PDELAY_LEN);
        if endpoint {
            self.fall_back(now, Reason::EndpointTlv);
        }
        if kind == PDELAY_REQ && !endpoint && !self.peers.contains(&source) {
            self.peers.push(source);
            if self.peers.len() >= 2 {
                self.fall_back(now, Reason::MultipleResponders);
            }
        }
        if kind != PDELAY_RESP {
            return;
        }
        let sequence = u16::from_be_bytes([message[30], message[31]]);
        let requester = u64::from_be_bytes(message[44..52].try_into().unwrap_or_default());
        if requester != self.identity.0 {
            return;
        }
        let Some((asked, answered)) = self.asked.as_mut() else {
            return;
        };
        if *asked != sequence {
            return;
        }
        if !answered.contains(&source) {
            answered.push(source);
        }
        if answered.len() >= 2 {
            self.fall_back(now, Reason::MultipleResponders);
        }
    }

    /// Once fallen back, the beacon every 3 s, for peers still in gPTP
    /// (profile 2.3).
    pub fn handle_timeout(&mut self, now: Duration) {
        let Some(due) = self.next_beacon else {
            return;
        };
        if self.link_down {
            self.next_beacon = Some(now + BEACON_INTERVAL);
            return;
        }
        if now < due {
            return;
        }
        let sequence = self.beacon_sequence;
        self.beacon_sequence = self.beacon_sequence.wrapping_add(1);
        self.outgoing
            .push_back(pdelay_request(self.identity, sequence));
        self.next_beacon = Some(now + BEACON_INTERVAL);
    }

    pub fn poll_timeout(&self) -> Option<Duration> {
        self.next_beacon
    }

    /// A PTP message to send to gPTP's destination, the beacon.
    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.outgoing.pop_front()
    }
}

#[cfg(test)]
mod tests;
