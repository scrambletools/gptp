# gptpd on endpoints

gptpd runs 802.1AS for an endpoint as well as for a bridge. On an endpoint
it runs as `gptpd@<interface>`, an end station with one port. The
applications on the computer share it: triib's entity service and plaa,
each reading the same time and the same profile decision.

What an endpoint gains over running ptp4l:

- The port's hardware clock is never stepped or slewed. That clock stamps
  the Pdelay exchanges the neighbor measures the link with, so those
  measurements stay clean through first lock, grandmaster changes,
  restarts and link bounces. A servo that finds the clock 0.5 s off
  slews an I210 at its limit, 6.25%, for about 8 s, far beyond the
  1000 ppm a bridge accepts.
- Streams follow a new grandmaster within a Sync or two, where a servo
  holds the old time through its slew and then jumps.
- PTP 2.0 on every distribution.
- Grandmaster capability, which Milan asks of every talker (4.3.1.1).
- One gPTP per port, and one AVB Lite fallback decision every
  application sees.
- Status straight from the daemon, with the neighbor's rate ratio, sent
  as it changes.

## The time interface

gptpd gives applications the grandmaster's time as a mapping from this
computer's clocks, since it never moves a clock to carry it.

- The mapping lives in `/run/gptpd/<interface>.time`, one fixed-size
  record on tmpfs with a magic number, version, length, sequence and
  CRC-32. gptpd writes it with pwrite at each Sync and each clock
  comparison. Readers pread it whole and read again when the CRC or
  sequence says they caught it half written, so neither side needs
  shared memory or unsafe code.
- gptpd rewrites the file in place across restarts and never recreates
  it, as a reader holding a recreated path's old file would never see the
  new one.
- The record carries:
  - the grandmaster's time as a line against CLOCK_MONOTONIC and another
    against the port's hardware clock, each a point and a rate;
  - the state: following a grandmaster, being one, or none;
  - the grandmaster's identity;
  - the CLOCK_MONOTONIC time of the write; a reader takes a record over
    a second old as no grandmaster, so a gptpd that died leaves nothing
    stale behind;
  - a generation, which changes at every discontinuity: a new grandmaster
    or time base, a jump of the hardware clock, a change of profile, a
    resume from suspend. A takeover that carries the time on keeps the
    generation;
  - whether AVB Lite's link asymmetry is included;
  - the residual: how far the last Sync stood from the line. It is not a
    servo's offset, as there is no servo.
- `gptp::time` holds the record's type, no_std, and a reader behind the
  `std` feature. Applications depend only on that.
- avb-stream's MediaClock reads the record against CLOCK_MONOTONIC, and
  computes wake times from the inverse. A new line that does not carry
  the last one on, by more than 10 µs from where the last predicted, is
  a discontinuity once three records agree, and talkers start their
  timeline again across it. With no gptpd, streams run on the monotonic
  clock, as on systems without a PTP hardware clock.

## Status and commands

`/run/gptpd/<interface>.sock` sends snapshots of `key value` lines, each
ended by a blank line: one as a client connects, then at every change and
every 2 s. Readers ignore keys they don't know. Anyone on the computer can
read the time file and the socket. A command that changes state, such as
setting priority1, is taken only from root or the `gptpd` group, checked
with SO_PEERCRED. gptpd uses no D-Bus.

## End stations

- The port's hardware clock is the LocalClock.
- The clock identity is the EUI-64 of the port's MAC address, as ptp4l
  makes it, so switching a machine from ptp4l doesn't reset the
  neighbor's rate measurement.
- `/etc/gptpd/<interface>.conf` holds the settings for each interface:
  priority1 (248 by default), the profile (auto, gptp or avb-lite) and
  whether the port declares itself gPTP-capable.
- gptpd compares the hardware clock with CLOCK_MONOTONIC_RAW through
  PTP_SYS_OFFSET_EXTENDED, and CLOCK_MONOTONIC with RAW, since
  applications sleep on MONOTONIC. NTP's steps and frequency changes,
  which move CLOCK_REALTIME, never read as jumps of the hardware clock.
- A jump of the hardware clock is published as a new generation. igb
  resets an I210's clock on a link bounce, and igc moves an i226's by a
  few µs on link changes. After a jump the port stays down until the
  clock is compared afresh.
- Links are watched by netlink, and ENETDOWN counts as down. A resume
  from suspend restarts every port's measurements with a new generation.
- gptpd locks itself in memory and runs at FIFO 10, below streams at RR
  15. It reads every frame waiting before acting on a timeout, so a
  stalled process doesn't take a late read for a Sync gone.

## Grandmaster capability

- A grandmaster capable system carries 802.1AS-2020's defaults: clockClass
  248, clockAccuracy 0xFE, offsetScaledLogVariance 0x436A, priority2 248.
  One that isn't carries clockClass 255. Endpoints default to priority1
  248, as Milan 4.2.6.2.1 asks; 255 means never, and 246 preferred.
- As grandmaster, gptpd sends two-step Sync and Follow_Up on every
  timetransmitter port, and Announce with timeSource 0xA0.
- The time base is one line, grandmaster time = offset + rate ×
  LocalClock:
  - at a cold start, the rate is 1 and the offset comes from the system's
    TAI time, then the line runs free and NTP never reaches the network;
  - a restart goes on from the last record's line;
  - taking over from a grandmaster that left keeps its last line, so
    followers see no step in phase or frequency.
- A starting port listens for three Announce intervals before it leads,
  and the last grandmaster holds for 5 s through a gap, so a link coming
  up doesn't hand the role to the endpoint for a moment.
- No port that isn't asCapable makes this system grandmaster.
- The entity model sets GPTP_GRANDMASTER_SUPPORTED in AVB_INTERFACE and
  reports the clock's identity and quality there.

## Link delay and Sync

Real neighbors answer late and stamp wrongly at times, and an endpoint
must ride that out without losing asCapable:

- The link delay is the median of recent exchanges. An exchange far from
  it is left out, with its rate, and logged. Fifteen in a row away from
  the median count as the link having changed, and the rate starts afresh
  across the change. A neighbor that pairs two of its own frames back to
  back on a port can put one frame time, about 10 µs, into a stamp; a
  shorter run of such outliers once became the median and cost asCapable
  for 20 s.
- A Sync that moves the line by more than 5 µs is held back until a later
  Sync says the same, and dropped otherwise.
- A Sync is judged on the hardware clock, and the comparison with
  CLOCK_MONOTONIC restarts on two readings in a row off its line. NTP
  slewing the system clock steps MONOTONIC's rate by tens of ppm; the
  mapping follows within 0.2 s, and no record's line moves by more than
  4 µs at once.

## Milan's gPTP rules

- asCapable after enough Pdelay responses (4.2.6.2.4).
- Pdelay_Req stops for 5 minutes, or until the link toggles, after three
  requests in a row each answered by more than one clock (4.2.6.2.5).
- Negative link delays are accepted (4.2.6.2.7).
- The gPTP-capable TLV of 802.1AS-2020 goes out in Signaling, and the
  neighbor's is tracked.

## AVB Lite

gptpd runs the AVB Lite PTP profile as well as gPTP: end-to-end Sync,
Follow_Up, Delay_Req and Delay_Resp, the Grandmaster Link TLV in Announce,
and the Endpoint Declaration TLV in its Pdelay_Req. `gptp::lite` decides
the fallback by the profile's rule (2.2): a neighbor's Endpoint
Declaration TLV, nine unanswered Pdelay_Req, or more than one responder.
The decision holds until the link comes up again, when the port tries
gPTP first. gptpd applies the link-speed asymmetry from the Grandmaster
Link TLV, and the record says so, so applications apply none themselves.
avb-lite keeps CVU SRP and admission.

## The unit and the package

- `gptpd@.service` restarts always, a second after a stop, with no start
  limit, since three unanswered Pdelay_Req cost a neighbor asCapable.
- It conflicts with, and orders after, the ptp4l units triib and plaa
  shipped before, so systemd finishes one stop before the other start.
- It refuses to start while a ptp4l someone runs by hand holds the
  interface, since two Pdelay responders on one link make strict
  neighbors fall back. It exits with a status that keeps systemd from
  restarting it, and leaves `/run/gptpd/<interface>.refused` for the
  applications to show.
- Neither application binds it, so gptpd@ outlives both, and stopping one
  doesn't take the port's time from the other.
- It holds CAP_NET_RAW and CAP_NET_ADMIN, and the hardware clock's device
  for writing. It needs no CAP_SYS_TIME.
- A polkit rule lets the person at the computer start, stop and restart
  `gptpd@*`. The package creates the `gptpd` group.
- gptpd ships as its own package, one version from one build, attached to
  triib's and plaa's releases, as no distribution carries it. triib's and
  plaa's debs and rpms recommend it. triib's PKGBUILD depends on it;
  plaa's lists it as optional, as plaa works without AVB. Each
  application's post-install stops ptp4l instances its older version
  left running, since those were started, not enabled.
- A bridge runs `gptpd.service` with `--bridge` instead.

## Switching a machine over

- Before switching, find everything that starts the old daemon: units
  that want or restart a ptp4l unit, and scripts that do.
- Never let two daemons answer one Pdelay_Req, even once. Some endpoints
  latch AVB Lite fallback on it until their link flaps.
- Capture stream frames as well as PTP while switching.

## Limits

- gptpd has been run on the I210 (igb) and the i226 (igc). On other
  cards it should refuse with a clear message where a feature it needs,
  such as PTP_SYS_OFFSET_EXTENDED with a clockid, is missing.
- The time record is an interface between gptpd and applications, hence
  its version field: a gptpd and an application of different versions
  refuse each other's record rather than misread it.
- Applications see discontinuities as generations and must handle them.
  A listener that mishandles one glitches where it would otherwise drift.
