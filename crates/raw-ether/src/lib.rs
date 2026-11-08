//! Raw Ethernet on Linux, macOS and Windows: MAC addresses, the
//! computer's interfaces, frames of one ethertype, frames with the times
//! the hardware stamped on them, PTP hardware clocks, and capturing whole
//! frames; on Linux, also 802.11 Action frames with the radio's times.
//! It moves frames and reads clocks, and parses no payload: gPTP, MRP
//! and AVTP are other crates' business.
//!
//! Without the `std` feature only the address types remain, so protocol
//! crates built on them also run on microcontrollers.

#![cfg_attr(not(any(feature = "std", test)), no_std)]

mod mac;

pub use mac::{MacAddress, ParseError};

#[cfg(feature = "std")]
pub mod interfaces;

#[cfg(feature = "std")]
pub mod socket;

#[cfg(all(feature = "std", target_os = "linux"))]
pub mod broker;

#[cfg(feature = "std")]
pub mod clock;

#[cfg(feature = "std")]
pub mod capture;

#[cfg(feature = "std")]
pub mod stamped;

#[cfg(all(feature = "std", target_os = "linux"))]
pub mod wifi;

#[cfg(feature = "std")]
pub use interfaces::{Interface, LinkPower, interfaces};
#[cfg(feature = "std")]
pub use socket::{Blocked, Received, Socket, check_access};
