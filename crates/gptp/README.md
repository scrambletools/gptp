# gptp

IEEE 802.1AS gPTP without I/O: gPTP's messages and TLVs; the link
(Pdelay, or the Common Mean Link Delay Service) and the time-aware relay,
which is also a one-port end station and may be grandmaster, by the BTCA
or on a working clock's domain by configured port roles; gPTP over 802.11 links, by Timing Measurement or
FTM; IEEE 1588's end-to-end client for the AVB Lite PTP profile, and AVB
Lite's fallback; the time mapping applications read, and the status
format.

It does no I/O, keeps no clock and starts no threads: the caller hands
it received messages with their hardware time stamps and the time, and
takes back messages to send and the next deadline. It depends on
nothing. Without the default `std` feature it needs neither std nor more
than `alloc`; `std` adds the reader of gptpd's time file and the status
client.

[gptpd](https://github.com/scrambletools/gptp#gptpd), in the same
repository, runs it on Linux with hardware time stamps, on
[raw-ether](https://crates.io/crates/raw-ether).
[gptp-relay.md](https://github.com/scrambletools/gptp/blob/main/docs/gptp-relay.md)
is the relay's design, and why it moves no clock.

## License

MIT OR Apache-2.0, at your option.
