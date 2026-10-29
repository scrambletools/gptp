//! IEEE 802.1AS generalized precision time protocol (gPTP) for a
//! time-aware relay: the messages, and the state machines of a bridge
//! that carries a grandmaster's time across its ports without moving any
//! of its own clocks.
//!
//! - [`message`]: gPTP's messages and TLVs, decoded from and encoded to
//!   the octets after the Ethernet header, byte for byte, keeping TLVs
//!   this crate does not interpret. The codec needs neither `std` nor
//!   `alloc`.
//!
//! Like `avb-mrp`, it does no I/O, keeps no clock and starts no threads:
//! the caller hands it received messages with their hardware time stamps
//! and the time, and takes back messages to send and the next deadline.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod link;
pub mod message;
pub mod relay;

pub use message::{
    ClockIdentity, DecodeError, EncodeError, Message, MessageType, PortIdentity, Timestamp, Tlv,
};

/// Ethertype of gPTP.
pub const ETHERTYPE_GPTP: u16 = 0x88f7;

/// Where gPTP sends every message: the nearest bridge group address, so
/// that no bridge passes one on.
pub const DESTINATION: [u8; 6] = [0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e];
