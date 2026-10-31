# Changelog

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
