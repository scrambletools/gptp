# An 802.1AS time-aware relay for the bridge

The bridge runs gPTP with linuxptp today: ptp4l with
`boundary_clock_jbod` across the ports, and phc2sys keeping the ports'
hardware clocks together. That works, but it gives every port's clock two
jobs at once, and the second disturbs the first:

- Each port's PTP hardware clock (PHC) time stamps that port's Pdelay
  exchanges, from which each neighbor measures the link's delay and its
  rate against the bridge.
- The same clock is kept on the grandmaster's time: ptp4l steps and slews
  the clock of the port facing the grandmaster, and phc2sys steps and
  slews the others after it.

802.1AS assumes the opposite: a free-running LocalClock time stamps
everything, and the grandmaster's time is carried in software as an
offset and a rate. With linuxptp, every grandmaster change, and every
boot, moves port clocks in ways the neighbors see in their own
measurements:

- After a grandmaster change, phc2sys by default slewed port clocks at
  the hardware's limit for days, so neighbors measured the bridge 6.25%
  slow and strict endpoints refused it (fixed with step thresholds).
- At boot, ptp4l steps the clock facing the grandmaster from system time
  onto the grandmaster's. The PreSonus switch takes the Pdelay interval
  spanning that step as a bad rate measurement and holds back Sync for
  1.1 s, and ptp4l, by 802.1AS's sync receipt timeout, follows another
  grandmaster until Sync resumes (reproduced 2026-10-29 with ptp4l's
  debug log and captures).

Neither is a ptp4l bug: it is how a boundary clock built on one clock per
port behaves. The fix is a different design, which this note describes:
a time-aware relay as 802.1AS defines one, whose port clocks are never
moved after it starts.

## Goals

- Replace ptp4l and phc2sys on the bridge with one daemon, `avb-gptpd`,
  built on a protocol crate, `avb-gptp`, in the avb workspace.
- A time-aware relay for domain 0 over full-duplex Ethernet with the
  instance-specific peer-to-peer delay mechanism, as 802.1AS-2011 and
  802.1AS-2020 both define it, so it works with what is on the bench:
  the PreSonus AVB switch, the MOTU 8D, macOS, the ESP boards and triib.
- After the daemon starts, no port clock is ever stepped, and none is
  slewed because of anything the network does. A grandmaster change or
  a boot is invisible in the neighbors' Pdelay measurements.
- The bridge is not grandmaster capable (priority1 255), as now.
- `avb-bridged` takes each port's asCapable from `avb-gptpd` instead of
  asking ptp4l.

Not in the first version: being grandmaster, more than one domain, CMLDS
(the bench's devices speak the instance-specific mechanism only), and
anything other than Ethernet.

## The clock

The relay needs one LocalClock across all its ports, because a Sync's
residence time is its departure from one port minus its arrival on
another. The i226-V gives each port its own PHC, and on this box the PCIe
root port lacks Precision Time Measurement, so the PHCs offer no cross
time stamping (`phc_ctl caps`: "doesn't have cross timestamping
support").

So `avb-gptpd` makes one LocalClock out of them:

- One port's PHC is the reference. It runs free: the daemon never adjusts
  it.
- Every other port's PHC follows the reference, in frequency and phase,
  from paired readings of both against the system's monotonic clock
  (`PTP_SYS_OFFSET_EXTENDED`), filtered. The loop only slews, and only
  by the crystals' difference, a steady few ppm that a neighbor measures
  as a fixed rate, which 802.1AS allows.
- When the daemon starts, it steps every PHC onto the reference once,
  before any port sends or answers a gPTP message. Nothing steps after
  that.
- The grandmaster's time never touches the PHCs. The relay carries it as
  802.1AS does: the rate ratio of the grandmaster to the LocalClock,
  built from each upstream link's neighbor rate ratio and the
  grandmaster's cumulative rate ratio in Follow_Up, and the correction
  each Sync carries.

The alignment loop's error adds to every residence time. The target is
under 100 ns, measured on the bench before the relay depends on it.
Anything that later needs the grandmaster's time on a PHC, such as launch
times for a credit-based shaper, converts from the LocalClock with the
relay's offset and rate, rather than moving a clock.

## The protocol

`avb-gptp` follows the state machines of 802.1AS-2020, clauses 10 and
11, and their names, for domain 0:

- **Link (clause 11):** per port, the MDPdelayReq requester and
  MDPdelayResp responder; the link delay, the neighbor rate ratio, and
  asCapable, with neighborPropDelayThresh 800 ns and allowedLostResponses
  3. A rate ratio measured across an interval is checked against a
  plausible range, and an implausible one is dropped while the last good
  ratio is kept for up to three intervals, so a neighbor's own clock step
  costs one measurement rather than asCapable.
- **Announce and the BMCA (10.3):** per-port announce receipt and
  information, the system's priority vector from the best Announce, port
  roles (SlavePort, MasterPort, PassivePort, DisabledPort), stepsRemoved,
  and the path trace TLV. Announce is sent per MasterPort from the
  system's own priority vector, not forwarded.
- **Sync relay (10.2 and 11.2):** a Sync and its Follow_Up received on
  the SlavePort go out on every MasterPort at once, two-step. The
  Follow_Up keeps preciseOriginTimestamp, adds the upstream link delay
  and the residence time to the correction (both scaled to the
  grandmaster's rate), and updates cumulativeScaledRateOffset with the
  upstream link's neighbor rate ratio. Sync receipt timeout is 3 Sync
  intervals.
- **Signaling (10.6.4):** honoring a neighbor's message interval request
  TLV, as the ESP boards send.

What the bench taught, carried into the design:

- Pdelay with `minorVersionPTP` 0: the PreSonus ignores 2.1.
- Neighbors check the rate ratio they measure within about ±1000 ppm,
  and the PreSonus holds back Sync after one bad interval. Hence port
  clocks that never step.
- The grandmaster's identity and time survive a bridge's absence: the
  amp keeps PreSonus time while scrambox is down, and each grandmaster-
  capable neighbor announces itself meanwhile.

The crate does no I/O, keeps no clock and starts no threads, like
`avb-mrp`. The caller hands it received messages with their hardware
receive times, the hardware transmit times of what it sent, and the
LocalClock's time, and takes back messages to send, port state changes
and the next deadline. That keeps it testable with recorded messages and
simulated clocks, and leaves it usable without `std`.

## The daemon

`avb-gptpd`, in Linux-AVB-Bridge:

- A raw socket per port for ethertype 0x88F7, with hardware time
  stamping on (transmit on, receive filter all, as ptp4l sets today);
  transmit times come back on the error queue.
- The PHC alignment loop above.
- One thread running `avb-gptp`, its sockets and its timers.
- Its state for people and for `avb-bridged` in
  `/run/avb-gptpd/status`: per port asCapable, role, link delay, rate
  ratio; the grandmaster, the offset and rate to it.
- A systemd unit that conflicts with `gptp-bridge.service` and
  `gptp-phc2sys.service`, so one command switches between linuxptp and
  the relay, and back.

Hardware time stamping and PHC adjustment go into `avb-net`, next to its
`HardwareClock`, on Linux.

## Testing

- **Unit tests** in `avb-gptp`: messages from today's captures (the
  PreSonus, the 8D, the Mac and the ESP boards) parse and rebuild byte
  for byte; link, BMCA and relay state machines against scripted
  neighbors with simulated clocks, including rate offsets, a neighbor's
  step, a grandmaster's departure and return, and a boot.
- **Two daemons back to back** on a veth pair or two spare ports, with
  software time stamps, for the plumbing.
- **Bench windows** agreed with the ESP session: `avb-gptpd` replaces
  ptp4l on scrambox, then the boards' own offsets and asCapable, the
  PreSonus's Sync, and captures of our Follow_Up corrections against the
  boards' measured time. The acceptance test is a reboot, and a
  grandmaster change, with no neighbor losing Sync or asCapable.

## Steps

1. `avb-gptp`: the messages and TLVs, encoded and decoded, tested on
   recorded frames.
2. The link: Pdelay requester and responder, rate ratio, asCapable.
3. Announce, the BMCA and port roles.
4. The Sync relay and the time arithmetic.
5. `avb-net`: hardware time stamps on raw sockets, PHC frequency and
   offset reads.
6. `avb-gptpd`: the alignment loop, the event loop, the status file, the
   unit and the switch.
7. `avb-bridged` on `avb-gptpd`'s asCapable.
8. Bench windows, then a release.
