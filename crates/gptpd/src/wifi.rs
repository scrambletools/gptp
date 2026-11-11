//! An 802.11 port (802.1AS-2020 clause 12): its radio's Action frames,
//! through nl80211, carry gPTP's timing, and its one station, the one
//! the radio stamps frames for, is the relay's neighbor on it. On an
//! access point that is the first station associated; on a station, its
//! access point. Which ways to measure the station takes part in come
//! from its Extended Capabilities, as hostapd or wpa_supplicant holds
//! them, unless told.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::Duration;

use gptp::dot11::ftm::{ACTION_FTM, ACTION_FTM_REQUEST, CATEGORY_PUBLIC};
use gptp::dot11::{ACTION_TIMING_MEASUREMENT, CATEGORY_UNPROTECTED_WNM, Support};
use gptp::relay::Destination;
use raw_ether::MacAddress;
use raw_ether::wifi::{Event, Radio};

/// Frames sent whose status is still to come: a few Sync intervals'.
const PENDING: usize = 16;
/// The Extended Capabilities element (802.11-2020 Table 9-92).
const ELEMENT_EXTENDED_CAPABILITIES: u8 = 127;
/// Its bits for Timing Measurement, and FTM as responder and initiator
/// (802.11-2020 Table 9-153).
const BIT_TIMING_MEASUREMENT: usize = 23;
const BIT_FTM_RESPONDER: usize = 70;
const BIT_FTM_INITIATOR: usize = 71;
/// How long hostapd or wpa_supplicant may take to answer.
const CONTROL_WAIT: Duration = Duration::from_millis(500);

/// What the relay is to hear of the radio, its times on the radio's clock.
#[derive(Debug, PartialEq)]
pub enum Heard {
    /// The station to measure, and the ways both take part in, or none.
    Peer(Option<(MacAddress, Support)>),
    FtmRequest(Vec<u8>),
    Measurement {
        action: Vec<u8>,
        arrival: i128,
        ack_departure: i128,
    },
    Confirm {
        dialog_token: u8,
        departure: i128,
        ack_arrival: i128,
    },
}

/// What a station's Extended Capabilities say, as far as known yet.
#[derive(Debug, PartialEq)]
enum Advertised {
    Known(Support),
    /// hostapd has the station, but not yet its Association Request, which
    /// carries them.
    NotYet,
    Unknown,
}

pub struct WifiPort {
    name: String,
    radio: Radio,
    access_point: bool,
    /// The ways to measure that this end takes part in: Timing
    /// Measurement always, FTM where the frames it needs come to it.
    own: Support,
    /// What to take the station to take part in, where told.
    assumed: Option<Support>,
    stations: Vec<MacAddress>,
    peer: Option<MacAddress>,
    /// Whether the peer was chosen before it associated, the ways to
    /// measure it still to read.
    unsettled: bool,
    /// Measurement frames sent: the cookie of each, and its dialog token.
    pending: VecDeque<(u64, u8)>,
}

impl WifiPort {
    /// Opens interface `name`'s radio, with the frames gPTP needs coming
    /// to it as far as no other program takes them.
    pub fn open(name: &str, assumed: Option<Support>) -> Result<Self, String> {
        let fail = |error: io::Error| format!("{name}: its radio: {error}");
        let radio = Radio::open(name).map_err(fail)?;
        let access_point = radio.access_point().map_err(fail)?;
        let take = |prefix: &[u8], what: &str| match radio.register_action(prefix) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("{name}: {what} do not come to gptpd: {error}");
                false
            }
        };
        let requests = take(&[CATEGORY_PUBLIC, ACTION_FTM_REQUEST], "FTM Requests");
        let frames = take(&[CATEGORY_PUBLIC, ACTION_FTM], "FTM frames");
        take(
            &[CATEGORY_UNPROTECTED_WNM, ACTION_TIMING_MEASUREMENT],
            "Timing Measurement frames",
        );
        let own = Support {
            timing_measurement: true,
            fine_timing_measurement: requests && frames,
        };
        let stations = radio.stations().map_err(fail)?;
        Ok(Self {
            name: name.to_owned(),
            radio,
            access_point,
            own,
            assumed,
            stations,
            peer: None,
            unsettled: false,
            pending: VecDeque::new(),
        })
    }

    pub fn access_point(&self) -> bool {
        self.access_point
    }

    /// The station the port is towards, once one is chosen.
    pub fn peer(&self) -> Option<MacAddress> {
        self.peer
    }

    /// The station to measure, newly chosen, if it changed: the one kept
    /// while it stays, else the first associated.
    pub fn choose(&mut self) -> Option<Heard> {
        let chosen = self
            .peer
            .filter(|peer| self.stations.contains(peer))
            .or_else(|| self.stations.first().copied());
        if chosen == self.peer {
            return None;
        }
        if let Some(old) = self.peer.take()
            && let Err(error) = self.radio.set_hw_timestamp(old, false)
        {
            eprintln!("{}: stamping {old}'s frames: {error}", self.name);
        }
        self.pending.clear();
        self.unsettled = false;
        let Some(peer) = chosen else {
            return Some(Heard::Peer(None));
        };
        if let Err(error) = self.radio.set_hw_timestamp(peer, true) {
            eprintln!("{}: stamping {peer}'s frames: {error}", self.name);
        }
        self.peer = Some(peer);
        let support = self.measured_with(peer).unwrap_or_else(|| {
            eprintln!("{}: measuring {peer} once it has associated", self.name);
            Support::NONE
        });
        Some(Heard::Peer(Some((peer, support))))
    }

    /// The ways to measure a peer chosen before it associated, once it has:
    /// a station joins the radio on authenticating, before its Association
    /// Request says what it takes part in.
    pub fn settle(&mut self) -> Option<Heard> {
        if !self.unsettled {
            return None;
        }
        let peer = self.peer?;
        let support = self.measured_with(peer)?;
        Some(Heard::Peer(Some((peer, support))))
    }

    /// The ways both ends take part in to measure `peer`, told to the log,
    /// or none while it has not yet associated.
    fn measured_with(&mut self, peer: MacAddress) -> Option<Support> {
        let advertised = match self.assumed {
            Some(assumed) => Advertised::Known(assumed),
            None => self.advertised(peer),
        };
        let theirs = match advertised {
            Advertised::NotYet => {
                self.unsettled = true;
                return None;
            }
            Advertised::Known(theirs) => Some(theirs),
            Advertised::Unknown => None,
        };
        self.unsettled = false;
        let support = Support {
            timing_measurement: self.own.timing_measurement
                && theirs.is_none_or(|theirs| theirs.timing_measurement),
            fine_timing_measurement: self.own.fine_timing_measurement
                && theirs.is_none_or(|theirs| theirs.fine_timing_measurement),
        };
        eprintln!(
            "{}: measuring {peer} with {}{}",
            self.name,
            describe(support),
            if theirs.is_none() {
                ", its capabilities unknown"
            } else {
                ""
            }
        );
        Some(support)
    }

    /// What the radio has said since last asked.
    pub fn take(&mut self) -> Vec<Heard> {
        let mut heard = Vec::new();
        loop {
            let event = match self.radio.take() {
                Ok(Some(event)) => event,
                Ok(None) => return heard,
                Err(error) => {
                    eprintln!("{}: reading its radio: {error}", self.name);
                    return heard;
                }
            };
            match event {
                Event::Station { peer, joined } => {
                    self.stations.retain(|station| *station != peer);
                    if joined {
                        self.stations.push(peer);
                    }
                    heard.extend(self.choose());
                }
                Event::Frame {
                    source,
                    action,
                    received,
                    ack_sent,
                } => {
                    if Some(source) != self.peer {
                        continue;
                    }
                    if action.starts_with(&[CATEGORY_PUBLIC, ACTION_FTM_REQUEST]) {
                        heard.push(Heard::FtmRequest(action));
                    } else if let (Some(arrival), Some(ack_departure)) = (received, ack_sent) {
                        heard.push(Heard::Measurement {
                            action,
                            arrival,
                            ack_departure,
                        });
                    }
                }
                Event::TxStatus {
                    cookie,
                    acked,
                    sent,
                    ack_received,
                } => {
                    let Some(position) = self.pending.iter().position(|(each, _)| *each == cookie)
                    else {
                        continue;
                    };
                    let (_, dialog_token) = self.pending.remove(position).unwrap_or_default();
                    if let (true, Some(departure), Some(ack_arrival)) = (acked, sent, ack_received)
                    {
                        heard.push(Heard::Confirm {
                            dialog_token,
                            departure,
                            ack_arrival,
                        });
                    }
                }
            }
        }
    }

    /// Sends an Action frame the relay has for the station.
    pub fn send(&mut self, destination: Destination, action: &[u8]) -> io::Result<()> {
        let (station, dialog_token) = match destination {
            Destination::Measurement {
                station,
                dialog_token,
            } => (MacAddress(station), Some(dialog_token)),
            Destination::Action { station } => (MacAddress(station), None),
            Destination::Group | Destination::Station(_) => return Ok(()),
        };
        let bssid = if self.access_point {
            self.radio.address()
        } else {
            station
        };
        let cookie = self.radio.send_action(station, bssid, action)?;
        if let Some(dialog_token) = dialog_token {
            self.pending.push_back((cookie, dialog_token));
            while self.pending.len() > PENDING {
                self.pending.pop_front();
            }
        }
        Ok(())
    }

    /// The ways to measure `peer` takes part in, as its Extended
    /// Capabilities say: hostapd's record of a station, or the access
    /// point's beacon as wpa_supplicant keeps it.
    fn advertised(&self, peer: MacAddress) -> Advertised {
        let (folder, command) = if self.access_point {
            ("/run/hostapd", "STA")
        } else {
            ("/run/wpa_supplicant", "BSS")
        };
        match ask(
            &format!("{folder}/{}", self.name),
            &format!("{command} {peer}"),
        ) {
            Ok(answer) => advertised_in(&answer, self.access_point),
            Err(error) => {
                eprintln!("{}: asking {folder} of {peer}: {error}", self.name);
                Advertised::Unknown
            }
        }
    }
}

/// What hostapd's record of a station, or wpa_supplicant's of the access
/// point, says of its Extended Capabilities.
fn advertised_in(answer: &str, access_point: bool) -> Advertised {
    if answer.is_empty() || answer.starts_with("FAIL") {
        return Advertised::Unknown;
    }
    // hostapd keeps a station from its Authentication on, flagged [ASSOC]
    // once its Association Request came.
    if access_point
        && !answer
            .lines()
            .any(|line| line.starts_with("flags=") && line.contains("[ASSOC]"))
    {
        return Advertised::NotYet;
    }
    let key = if access_point { "ext_capab=" } else { "ie=" };
    // A station that sent no Extended Capabilities has none of them.
    let Some(octets) = answer.lines().find_map(|line| line.strip_prefix(key)) else {
        return if access_point {
            Advertised::Known(Support::NONE)
        } else {
            Advertised::Unknown
        };
    };
    let Some(octets) = hex(octets) else {
        return Advertised::Unknown;
    };
    let capabilities = if access_point {
        &octets[..]
    } else {
        extended_capabilities(&octets).unwrap_or_default()
    };
    Advertised::Known(support_of(capabilities))
}

impl AsFd for WifiPort {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.radio.as_fd()
    }
}

/// Asks hostapd's or wpa_supplicant's control socket at `path`.
fn ask(path: &str, command: &str) -> io::Result<String> {
    let name = format!("gptpd-{}-{}", std::process::id(), path.replace('/', "-"));
    let socket = UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes())?)?;
    socket.connect(path)?;
    socket.set_read_timeout(Some(CONTROL_WAIT))?;
    socket.send(command.as_bytes())?;
    let mut buffer = vec![0; 8192];
    loop {
        let length = socket.recv(&mut buffer)?;
        let answer = String::from_utf8_lossy(&buffer[..length]).into_owned();
        // Unasked event messages start with their level, as "<3>".
        if !answer.starts_with('<') {
            return Ok(answer);
        }
    }
}

fn hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// The Extended Capabilities element's body among `elements`.
fn extended_capabilities(mut elements: &[u8]) -> Option<&[u8]> {
    while elements.len() >= 2 {
        let length = usize::from(elements[1]);
        let body = elements.get(2..2 + length)?;
        if elements[0] == ELEMENT_EXTENDED_CAPABILITIES {
            return Some(body);
        }
        elements = &elements[2 + length..];
    }
    None
}

/// The ways to measure Extended Capabilities say a station takes part in:
/// FTM as either end, as 802.1AS-2020 12.3 asks of a peer.
fn support_of(capabilities: &[u8]) -> Support {
    let bit = |number: usize| {
        capabilities
            .get(number / 8)
            .is_some_and(|octet| octet & (1 << (number % 8)) != 0)
    };
    Support {
        timing_measurement: bit(BIT_TIMING_MEASUREMENT),
        fine_timing_measurement: bit(BIT_FTM_RESPONDER) || bit(BIT_FTM_INITIATOR),
    }
}

/// Support as the options and the log name it.
pub fn describe(support: Support) -> &'static str {
    match (support.timing_measurement, support.fine_timing_measurement) {
        (true, true) => "FTM, and Timing Measurement to fall back on",
        (true, false) => "Timing Measurement",
        (false, true) => "FTM",
        (false, false) => "neither Timing Measurement nor FTM",
    }
}

/// Support from the options' words: tm, ftm or both.
pub fn parse_support(text: &str) -> Option<Support> {
    match text {
        "tm" => Some(Support::TIMING_MEASUREMENT),
        "ftm" => Some(Support::FINE_TIMING_MEASUREMENT),
        "both" => Some(Support::BOTH),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_capabilities_say_which_ways_a_station_takes_part_in() {
        // Timing Measurement in bit 23, FTM initiator in bit 71.
        let mut capabilities = [0u8; 9];
        capabilities[2] = 0x80;
        assert_eq!(support_of(&capabilities), Support::TIMING_MEASUREMENT);
        capabilities[8] = 0x80;
        assert_eq!(support_of(&capabilities), Support::BOTH);
        capabilities[2] = 0;
        capabilities[8] = 0x40;
        assert_eq!(support_of(&capabilities), Support::FINE_TIMING_MEASUREMENT);
        // A short element says nothing of the bits it lacks.
        assert_eq!(support_of(&[0, 0, 0x80]), Support::TIMING_MEASUREMENT);
    }

    #[test]
    fn a_station_not_yet_associated_has_its_capabilities_to_come() {
        let authenticated = "fc:01:2c:fd:e5:94\nflags=[AUTH]\naid=0\n";
        assert_eq!(advertised_in(authenticated, true), Advertised::NotYet);
        let associated =
            "fc:01:2c:fd:e5:94\nflags=[AUTH][ASSOC][AUTHORIZED]\next_capab=000000000000000040\n";
        assert_eq!(
            advertised_in(associated, true),
            Advertised::Known(Support::FINE_TIMING_MEASUREMENT)
        );
        let without = "fc:01:2c:fd:e5:94\nflags=[AUTH][ASSOC][AUTHORIZED]\n";
        assert_eq!(
            advertised_in(without, true),
            Advertised::Known(Support::NONE)
        );
        assert_eq!(advertised_in("FAIL\n", true), Advertised::Unknown);
    }

    #[test]
    fn a_beacons_elements_give_up_their_extended_capabilities() {
        let elements = hex("0008736372616d626f78").unwrap();
        assert_eq!(extended_capabilities(&elements), None);
        let mut with = elements.clone();
        with.extend_from_slice(&[127, 3, 0x04, 0x00, 0x80]);
        assert_eq!(extended_capabilities(&with), Some(&[0x04, 0x00, 0x80][..]));
        assert_eq!(hex("0g"), None);
        assert_eq!(hex("abc"), None);
    }

    #[test]
    fn support_is_named_in_the_options() {
        assert_eq!(parse_support("both"), Some(Support::BOTH));
        assert_eq!(parse_support("tm"), Some(Support::TIMING_MEASUREMENT));
        assert_eq!(parse_support("ftm"), Some(Support::FINE_TIMING_MEASUREMENT));
        assert_eq!(parse_support("all"), None);
    }
}
