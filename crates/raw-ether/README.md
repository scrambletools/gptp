# raw-ether

Raw Ethernet on Linux, macOS and Windows: MAC addresses, the computer's
interfaces, frames of one ethertype, frames with the times the hardware
stamped on them, PTP hardware clocks, and capturing whole frames; on
Linux, also 802.11 Action frames with the radio's times. It moves frames
and reads clocks, and parses no payload: gPTP, MRP and AVTP are other
crates' business.

Without the default `std` feature only the address types remain, so
protocol crates built on them also run on microcontrollers.

It is the platform code of
[gptpd](https://github.com/scrambletools/gptp#gptpd), which runs
[gptp](https://crates.io/crates/gptp) on it.

## License

MIT OR Apache-2.0, at your option.
