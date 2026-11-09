# Changelog

## [Unreleased]

- gptp: the FTM responder follows up a frame with its times alone where
  the relay has no Follow_Up for it yet, so a station that only ranges
  still measures; Timing Measurement still waits for the Follow_Up. A
  station asking no Min Delta FTM gets its frames spaced to fit the
  burst it asked for, not 20 ms apart, which held 2 of the 8 an ESP32
  asks for in 32 ms.
- gptp: `dot11`, gPTP over 802.11 links (802.1AS-2020 clause 12):
  Timing Measurement and Fine Timing Measurement frames carrying the
  Follow_Up element of 12.7, both ends of each exchange, and
  `Relay::set_wireless`, which makes a port an 802.11 port towards one
  station.
- raw-ether: `wifi`, 802.11 Action frames through nl80211 on Linux with
  the radio's times; a hardware clock whose driver has no
  PTP_SYS_OFFSET_EXTENDED is read between two readings of the system
  clock. iwlwifi's radio clock has none, and its
  PTP_SYS_OFFSET_PRECISE is a firmware command of about 1 ms that
  logs a line each time.
- gptpd: Wi-Fi ports, their station taken from hostapd or
  wpa_supplicant, and `--wifi-support`. A Wi-Fi port's radio is opened
  each time the port comes up: hostapd stopping takes the frames gptpd
  registered for, and a radio opened before hostapd starts is a
  station's. Run as an access point with one station, which follows it
  leaving and rejoining; nothing measured yet, as the station does not
  range.
- gptpd's status names a Wi-Fi port's station, so a beacon carrier
  sending its own Announce leaves that station to gptpd and the station
  hears one Announce for the port.
- gptpd reads each port's operstate, not its carrier. Reading carrier
  waits for RTNL, and while a Wi-Fi driver restarting its firmware held
  that for 3 s, no port sent a Sync or answered a Pdelay.
- gptp: `Relay::handle_unstamped` and `Link::distrust_response`. A frame
  that came without a hardware time stamp, from a card that stamps every
  frame, leaves the frame before it distrusted: an I210 can give one
  frame the next one's time. A distrusted Pdelay exchange is left out of
  the delay and the rate, up to `ALLOWED_DISTRUSTED_EXCHANGES` in a row,
  and a distrusted Sync's time is not taken.

## [0.1.0] - 2026-10-31

The first release: gptp and raw-ether on crates.io, and gptpd's packages.

- gptp: IEEE 802.1AS without I/O: messages, the link's Pdelay, a
  time-aware relay and an end station, grandmaster capability with
  802.1AS-2020's defaults, Milan's gPTP rules, the AVB Lite PTP profile
  and its fallback, and the time record and status gptpd gives
  applications, with their readers behind the `std` feature. It builds
  without std.
- raw-ether: raw Ethernet on Linux, macOS and Windows, with hardware time
  stamps, PTP hardware clocks and captures, split out of avb-net.
- gptpd: runs gptp with hardware time stamps on a Linux bridge's ports or
  as an end station (`gptpd@<interface>`), never stepping or slewing a
  hardware clock. Applications read the grandmaster's time from
  `/run/gptpd/<interface>.time` and its state from
  `/run/gptpd/<interface>.sock`. Packages for Debian, Ubuntu and Fedora
  on x86_64, ARM64 and RISC-V, and a PKGBUILD.
- gptpd leaves out, and logs, a Pdelay exchange or Sync whose frame may
  carry the next frame's time, as an I210 can give it. It does so with
  gptp's `Relay::handle_unstamped`, which reaches crates.io in gptp's
  next release.
