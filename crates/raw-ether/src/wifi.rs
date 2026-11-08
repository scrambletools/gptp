//! 802.11 Action frames on Linux through nl80211, with the times the
//! radio stamped on them: what gPTP over Wi-Fi (802.1AS-2020 clause 12)
//! needs of a station's or an access point's interface. A Timing
//! Measurement frame goes out with [`Radio::send_action`], and when it
//! left and when its Ack came back arrive in [`Event::TxStatus`]; one
//! received arrives in [`Event::Frame`], with when it came and when its
//! Ack left. The radio stamps the frames to and from the station named
//! to [`Radio::set_hw_timestamp`], one station at a time on Intel's.
//!
//! The times are the driver's, on the radio's clock: Intel's iwlwifi
//! gives them on its "iwlwifi-PTP" hardware clock. On an access point,
//! hostapd takes every Action frame received, so there frames can only
//! be sent; a station's wpa_supplicant leaves those this needs.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use crate::MacAddress;

/// Generic netlink's controller, which names the families.
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const CTRL_ATTR_MCAST_GROUPS: u16 = 7;
const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;

/// nl80211's commands and attributes (linux/nl80211.h).
const CMD_GET_INTERFACE: u8 = 5;
const CMD_GET_STATION: u8 = 17;
const CMD_NEW_STATION: u8 = 19;
const CMD_DEL_STATION: u8 = 20;
const CMD_REGISTER_FRAME: u8 = 58;
const CMD_FRAME: u8 = 59;
const CMD_FRAME_TX_STATUS: u8 = 60;
const CMD_SET_HW_TIMESTAMP: u8 = 153;
const ATTR_IFINDEX: u16 = 3;
const ATTR_IFTYPE: u16 = 5;
const ATTR_MAC: u16 = 6;
const ATTR_FRAME: u16 = 51;
const ATTR_COOKIE: u16 = 88;
const ATTR_FRAME_MATCH: u16 = 91;
const ATTR_ACK: u16 = 92;
const ATTR_FRAME_TYPE: u16 = 101;
const ATTR_TX_HW_TIMESTAMP: u16 = 319;
const ATTR_RX_HW_TIMESTAMP: u16 = 320;
const ATTR_HW_TIMESTAMP_ENABLED: u16 = 324;
const IFTYPE_AP: u32 = 3;

/// The multicast group carrying frame transmit status and stations
/// joining and leaving.
const MLME_GROUP: &str = "mlme";
/// An Action frame's frame control: a management frame of subtype 13.
const ACTION_FRAME_TYPE: u16 = 0x00d0;
/// A management frame's header: frame control, duration, three addresses
/// and the sequence control.
const MANAGEMENT_HEADER: usize = 24;

const NETLINK_HEADER: usize = 16;
const GENERIC_HEADER: usize = 4;
const ATTRIBUTE_HEADER: usize = 4;
/// How long a request waits for the kernel's answer.
const ANSWER_WAIT_MS: i32 = 1_000;

/// Word from the radio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// An Action frame received, from its Category on: when it arrived and
    /// when the radio's Ack for it left, both in nanoseconds on the radio's
    /// clock, where the radio stamped them.
    Frame {
        source: MacAddress,
        action: Vec<u8>,
        received: Option<i128>,
        ack_sent: Option<i128>,
    },
    /// What became of a frame sent: whether its Ack came, when it left and
    /// when the Ack arrived.
    TxStatus {
        cookie: u64,
        acked: bool,
        sent: Option<i128>,
        ack_received: Option<i128>,
    },
    /// A station associated with the access point, or left it.
    Station { peer: MacAddress, joined: bool },
}

/// An 802.11 interface's Action frames, through nl80211.
pub struct Radio {
    fd: OwnedFd,
    family: u16,
    interface: u32,
    address: MacAddress,
    sequence: Cell<u32>,
    /// Events read while waiting for an answer, for [`take`](Self::take).
    held: RefCell<VecDeque<Vec<u8>>>,
}

impl Radio {
    /// Opens `interface`'s radio, listening for the transmit status of the
    /// frames it sends and for stations joining and leaving. Registering
    /// for frames and stamping them need CAP_NET_ADMIN.
    pub fn open(interface: &str) -> io::Result<Self> {
        let address = crate::interfaces::mac_of(interface).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("{interface}: no address"))
        })?;
        let index = crate::interfaces::index_of(interface).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{interface}: no such interface"),
            )
        })?;
        let fd = socket::open()?;
        let mut radio = Self {
            fd,
            family: GENL_ID_CTRL,
            interface: index as u32,
            address,
            sequence: Cell::new(1),
            held: RefCell::new(VecDeque::new()),
        };
        let answers = radio.request(
            GENL_ID_CTRL,
            CTRL_CMD_GETFAMILY,
            &[(CTRL_ATTR_FAMILY_NAME, b"nl80211\0")],
        )?;
        let (family, mlme) = answers
            .iter()
            .find_map(|answer| family_of(answer, MLME_GROUP))
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "no nl80211"))?;
        radio.family = family;
        socket::join(&radio.fd, mlme)?;
        Ok(radio)
    }

    /// The interface's own address.
    pub fn address(&self) -> MacAddress {
        self.address
    }

    /// Whether the interface is an access point, not a station.
    pub fn access_point(&self) -> io::Result<bool> {
        let index = self.interface.to_ne_bytes();
        let answers = self.request(self.family, CMD_GET_INTERFACE, &[(ATTR_IFINDEX, &index)])?;
        Ok(answers.iter().any(|answer| is_access_point(answer)))
    }

    /// The stations associated with the access point, or on a station the
    /// access point it is associated with.
    pub fn stations(&self) -> io::Result<Vec<MacAddress>> {
        let index = self.interface.to_ne_bytes();
        let answers = self.dump(CMD_GET_STATION, &[(ATTR_IFINDEX, &index)])?;
        Ok(answers
            .iter()
            .filter_map(|answer| station_in(answer))
            .collect())
    }

    /// Has Action frames starting with `prefix` (their Category, then
    /// action and more as needed) come to this radio as [`Event::Frame`].
    /// Fails if another program already takes frames that overlap.
    pub fn register_action(&self, prefix: &[u8]) -> io::Result<()> {
        let index = self.interface.to_ne_bytes();
        let frame_type = ACTION_FRAME_TYPE.to_ne_bytes();
        self.request(
            self.family,
            CMD_REGISTER_FRAME,
            &[
                (ATTR_IFINDEX, &index),
                (ATTR_FRAME_TYPE, &frame_type),
                (ATTR_FRAME_MATCH, prefix),
            ],
        )
        .map(drop)
    }

    /// Has the radio stamp the Timing Measurement and Fine Timing
    /// Measurement frames to and from `peer`, or stop.
    pub fn set_hw_timestamp(&self, peer: MacAddress, enable: bool) -> io::Result<()> {
        let index = self.interface.to_ne_bytes();
        let mut attributes: Vec<(u16, &[u8])> = vec![(ATTR_IFINDEX, &index), (ATTR_MAC, &peer.0)];
        if enable {
            attributes.push((ATTR_HW_TIMESTAMP_ENABLED, &[]));
        }
        self.request(self.family, CMD_SET_HW_TIMESTAMP, &attributes)
            .map(drop)
    }

    /// Sends an Action frame, `action` from its Category on, to
    /// `destination` in the BSS `bssid` (the access point's address),
    /// returning the cookie its [`Event::TxStatus`] will carry.
    pub fn send_action(
        &self,
        destination: MacAddress,
        bssid: MacAddress,
        action: &[u8],
    ) -> io::Result<u64> {
        let frame = action_frame(destination, self.address, bssid, action);
        let index = self.interface.to_ne_bytes();
        let answers = self.request(
            self.family,
            CMD_FRAME,
            &[(ATTR_IFINDEX, &index), (ATTR_FRAME, &frame)],
        )?;
        answers
            .iter()
            .find_map(|answer| {
                let (_, attributes) = generic(answer)?;
                attribute(attributes, ATTR_COOKIE).and_then(u64_of)
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no cookie for the frame"))
    }

    /// The next word from the radio, without blocking.
    pub fn take(&self) -> io::Result<Option<Event>> {
        loop {
            let held = self.held.borrow_mut().pop_front();
            let message = match held {
                Some(message) => message,
                None => {
                    let mut buffer = vec![0; 16_384];
                    let Some(length) = socket::receive(&self.fd, &mut buffer)? else {
                        return Ok(None);
                    };
                    buffer.truncate(length);
                    let mut events = VecDeque::new();
                    for message in messages(&buffer) {
                        events.push_back(message.to_vec());
                    }
                    self.held.borrow_mut().extend(events);
                    continue;
                }
            };
            if let Some(event) = event_of(&message, self.family, self.interface) {
                return Ok(Some(event));
            }
        }
    }

    /// Sends a request and waits for its answer: the messages of its
    /// sequence before the acknowledgement. Others read meanwhile are held
    /// for [`take`](Self::take).
    fn request(
        &self,
        family: u16,
        command: u8,
        attributes: &[(u16, &[u8])],
    ) -> io::Result<Vec<Vec<u8>>> {
        self.exchange(request(family, command, 0, attributes))
    }

    /// Asks nl80211 for every entry of a kind: the messages up to the
    /// dump's end.
    fn dump(&self, command: u8, attributes: &[(u16, &[u8])]) -> io::Result<Vec<Vec<u8>>> {
        let mut message = request(self.family, command, 0, attributes);
        let flags = (libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16;
        message[6..8].copy_from_slice(&flags.to_ne_bytes());
        self.exchange(message)
    }

    /// Sends `message`, numbered afresh, and gathers its answer.
    fn exchange(&self, mut message: Vec<u8>) -> io::Result<Vec<Vec<u8>>> {
        let sequence = self.sequence.get();
        self.sequence.set(sequence.wrapping_add(1));
        message[8..12].copy_from_slice(&sequence.to_ne_bytes());
        socket::send(&self.fd, &message)?;
        let mut answers = Vec::new();
        let mut buffer = vec![0; 16_384];
        loop {
            if !socket::wait(&self.fd, ANSWER_WAIT_MS)? {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no answer from nl80211",
                ));
            }
            let Some(length) = socket::receive(&self.fd, &mut buffer)? else {
                continue;
            };
            for message in messages(&buffer[..length]) {
                if sequence_of(message) != Some(sequence) {
                    self.held.borrow_mut().push_back(message.to_vec());
                    continue;
                }
                if u16_at(message, 4) == Some(libc::NLMSG_DONE as u16) {
                    return Ok(answers);
                }
                match error_of(message) {
                    Some(0) => return Ok(answers),
                    Some(error) => return Err(io::Error::from_raw_os_error(-error)),
                    None => answers.push(message.to_vec()),
                }
            }
        }
    }
}

impl AsFd for Radio {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// An Action frame from `source` to `destination` in BSS `bssid`, `action`
/// from its Category on.
fn action_frame(
    destination: MacAddress,
    source: MacAddress,
    bssid: MacAddress,
    action: &[u8],
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(MANAGEMENT_HEADER + action.len());
    frame.extend_from_slice(&ACTION_FRAME_TYPE.to_le_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&destination.0);
    frame.extend_from_slice(&source.0);
    frame.extend_from_slice(&bssid.0);
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(action);
    frame
}

/// A generic netlink request, asking for an acknowledgement.
fn request(family: u16, command: u8, sequence: u32, attributes: &[(u16, &[u8])]) -> Vec<u8> {
    let mut message = vec![0; NETLINK_HEADER];
    message.extend_from_slice(&[command, 1, 0, 0]);
    for (kind, payload) in attributes {
        let length = ATTRIBUTE_HEADER + payload.len();
        message.extend_from_slice(&(length as u16).to_ne_bytes());
        message.extend_from_slice(&kind.to_ne_bytes());
        message.extend_from_slice(payload);
        message.resize(aligned(message.len()), 0);
    }
    let length = message.len() as u32;
    let flags = (libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16;
    message[..4].copy_from_slice(&length.to_ne_bytes());
    message[4..6].copy_from_slice(&family.to_ne_bytes());
    message[6..8].copy_from_slice(&flags.to_ne_bytes());
    message[8..12].copy_from_slice(&sequence.to_ne_bytes());
    message
}

fn aligned(length: usize) -> usize {
    (length + 3) & !3
}

fn u16_at(octets: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(octets.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(octets: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(octets.get(at..at + 4)?.try_into().ok()?))
}

fn u64_of(octets: &[u8]) -> Option<u64> {
    Some(u64::from_ne_bytes(octets.try_into().ok()?))
}

/// The netlink messages in what one read returned.
fn messages(mut buffer: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let length = u32_at(buffer, 0)? as usize;
        if length < NETLINK_HEADER || length > buffer.len() {
            return None;
        }
        let message = &buffer[..length];
        buffer = &buffer[aligned(length).min(buffer.len())..];
        Some(message)
    })
}

fn sequence_of(message: &[u8]) -> Option<u32> {
    u32_at(message, 8)
}

/// An error message's error, 0 for an acknowledgement; none for any
/// other message.
fn error_of(message: &[u8]) -> Option<i32> {
    if u16_at(message, 4)? != libc::NLMSG_ERROR as u16 {
        return None;
    }
    Some(u32_at(message, NETLINK_HEADER)? as i32)
}

/// A generic netlink message's type, command and attributes.
fn generic(message: &[u8]) -> Option<(u8, &[u8])> {
    let command = *message.get(NETLINK_HEADER)?;
    Some((command, message.get(NETLINK_HEADER + GENERIC_HEADER..)?))
}

/// The attributes in `octets`, kind and payload; nested ones are another
/// call's.
fn attributes(mut octets: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        let length = usize::from(u16_at(octets, 0)?);
        let kind = u16_at(octets, 2)? & 0x3fff;
        if length < ATTRIBUTE_HEADER || length > octets.len() {
            return None;
        }
        let payload = &octets[ATTRIBUTE_HEADER..length];
        octets = &octets[aligned(length).min(octets.len())..];
        Some((kind, payload))
    })
}

fn attribute(octets: &[u8], wanted: u16) -> Option<&[u8]> {
    attributes(octets).find_map(|(kind, payload)| (kind == wanted).then_some(payload))
}

/// The family's number and the named multicast group's, from the
/// controller's answer.
fn family_of(answer: &[u8], group: &str) -> Option<(u16, u32)> {
    let (_, attributes_octets) = generic(answer)?;
    let family = attribute(attributes_octets, CTRL_ATTR_FAMILY_ID).and_then(|id| u16_at(id, 0))?;
    let groups = attribute(attributes_octets, CTRL_ATTR_MCAST_GROUPS)?;
    let id = attributes(groups).find_map(|(_, entry)| {
        let name = attribute(entry, CTRL_ATTR_MCAST_GRP_NAME)?;
        (name.strip_suffix(&[0]).unwrap_or(name) == group.as_bytes())
            .then(|| attribute(entry, CTRL_ATTR_MCAST_GRP_ID).and_then(|id| u32_at(id, 0)))
            .flatten()
    })?;
    Some((family, id))
}

fn time_of(octets: &[u8]) -> Option<i128> {
    u64_of(octets).map(i128::from)
}

fn mac_at(frame: &[u8], at: usize) -> Option<MacAddress> {
    let octets: [u8; 6] = frame.get(at..at + 6)?.try_into().ok()?;
    Some(MacAddress(octets))
}

/// Whether an answer about an interface says it is an access point.
fn is_access_point(answer: &[u8]) -> bool {
    generic(answer)
        .and_then(|(_, attributes)| attribute(attributes, ATTR_IFTYPE))
        .and_then(|kind| u32_at(kind, 0))
        == Some(IFTYPE_AP)
}

/// The station an answer about one names.
fn station_in(answer: &[u8]) -> Option<MacAddress> {
    let (_, attributes) = generic(answer)?;
    attribute(attributes, ATTR_MAC).and_then(|mac| mac_at(mac, 0))
}

/// The event a message from nl80211 tells of, for this interface.
fn event_of(message: &[u8], family: u16, interface: u32) -> Option<Event> {
    if u16_at(message, 4)? != family {
        return None;
    }
    let (command, attributes_octets) = generic(message)?;
    let index = attribute(attributes_octets, ATTR_IFINDEX).and_then(|index| u32_at(index, 0));
    if index != Some(interface) {
        return None;
    }
    let time = |kind| attribute(attributes_octets, kind).and_then(time_of);
    match command {
        CMD_FRAME => {
            let frame = attribute(attributes_octets, ATTR_FRAME)?;
            if u16::from_le_bytes([*frame.first()?, *frame.get(1)?]) & 0x00fc != ACTION_FRAME_TYPE {
                return None;
            }
            Some(Event::Frame {
                source: mac_at(frame, 10)?,
                action: frame.get(MANAGEMENT_HEADER..)?.to_vec(),
                received: time(ATTR_RX_HW_TIMESTAMP),
                ack_sent: time(ATTR_TX_HW_TIMESTAMP),
            })
        }
        CMD_FRAME_TX_STATUS => Some(Event::TxStatus {
            cookie: attribute(attributes_octets, ATTR_COOKIE).and_then(u64_of)?,
            acked: attribute(attributes_octets, ATTR_ACK).is_some(),
            sent: time(ATTR_TX_HW_TIMESTAMP),
            ack_received: time(ATTR_RX_HW_TIMESTAMP),
        }),
        CMD_NEW_STATION | CMD_DEL_STATION => Some(Event::Station {
            peer: attribute(attributes_octets, ATTR_MAC).and_then(|mac| mac_at(mac, 0))?,
            joined: command == CMD_NEW_STATION,
        }),
        _ => None,
    }
}

/// The socket's system calls.
#[allow(unsafe_code)]
mod socket {
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    pub fn open() -> io::Result<OwnedFd> {
        // SAFETY: socket(2) takes no memory.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::NETLINK_GENERIC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a new socket nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: sockaddr_nl is plain data, valid when zeroed.
        let mut address: libc::sockaddr_nl = unsafe { zeroed() };
        address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // SAFETY: `address` is a sockaddr_nl of the length given.
        let bound = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                std::ptr::from_ref(&address).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if bound < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }

    pub fn join(fd: &OwnedFd, group: u32) -> io::Result<()> {
        // SAFETY: `group` is a u32 of the length given.
        let joined = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_NETLINK,
                libc::NETLINK_ADD_MEMBERSHIP,
                std::ptr::from_ref(&group).cast(),
                size_of::<u32>() as libc::socklen_t,
            )
        };
        if joined < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn send(fd: &OwnedFd, message: &[u8]) -> io::Result<()> {
        // SAFETY: `message` is readable for its length.
        let sent = unsafe { libc::send(fd.as_raw_fd(), message.as_ptr().cast(), message.len(), 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// What one read returned, or none if nothing waits.
    pub fn receive(fd: &OwnedFd, buffer: &mut [u8]) -> io::Result<Option<usize>> {
        loop {
            // SAFETY: `buffer` is writable for its length.
            let length = unsafe {
                libc::recv(
                    fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if length >= 0 {
                return Ok(Some(length as usize));
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EAGAIN) => return Ok(None),
                Some(libc::EINTR) => {}
                _ => return Err(error),
            }
        }
    }

    /// Whether something came to read within `milliseconds`.
    pub fn wait(fd: &OwnedFd, milliseconds: i32) -> io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: `poll` is one pollfd.
            let ready = unsafe { libc::poll(&mut poll, 1, milliseconds) };
            if ready >= 0 {
                return Ok(ready > 0);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error);
            }
        }
    }
}

#[cfg(test)]
mod tests;
