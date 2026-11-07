# gptp

IEEE 802.1AS gPTP for Linux: a protocol crate, and a daemon that runs
it with hardware time stamps and never steps or slews a clock.

| Crate | What it does |
|---|---|
| `gptp` | gPTP's messages and TLVs; the link (Pdelay) and the relay, which is also a one-port end station and may be grandmaster; IEEE 1588's end-to-end client for the AVB Lite PTP profile, and AVB Lite's fallback; the time mapping applications read, and the status format. No I/O; without the `std` feature, neither std nor more than `alloc` |
| `raw-ether` | Raw Ethernet on Linux, macOS and Windows: MAC addresses, interfaces, frames of one ethertype, frames with hardware time stamps, PTP hardware clocks and captures. It moves frames and reads clocks, and parses no payload |
| `gptpd` | The daemon: an end station on one interface, or a time-aware relay across a Linux bridge's ports |

`gptp` depends on nothing, and gptpd only on it and `raw-ether`. AVB
crates may depend on these, never the other way round.

## gptpd

gPTP on raw sockets with hardware time stamps, never stepping or
slewing a clock, so neither a new grandmaster nor the daemon starting
shows in what the neighbors measure. `docs/gptp-relay.md` says why ptp4l
and phc2sys could not do that.

### An end station

```
gptpd enp2s0
```

One interface and no `--bridge` make an end station. Its settings are in
`/etc/gptpd/INTERFACE.conf` (`--config`), `key value` lines, each
optional:

- `priority1 N`: 1 to 255, 248 by default as Milan asks of an endpoint;
  255 is never grandmaster.
- `profile auto|gptp|avb-lite`: `auto` runs gPTP and falls back to the
  AVB Lite PTP profile where the Pdelay exchange says no AVB bridge is
  the neighbor (AVB Lite profile 2.2), and back to gPTP when the link
  comes up again; `avb-lite` runs AVB Lite only.
- `gptp-capable yes|no`: whether the port says each second, in
  Signaling, that it runs gPTP (802.1AS-2020 10.6.4.4).

The hardware clock runs free, so applications get the grandmaster's time
as a mapping, in `/run/gptpd/INTERFACE.time`: one record, rewritten at
each Sync and ten times a second, of the grandmaster's time as a line on
CLOCK_MONOTONIC and on the hardware clock, its rate, whose it is, and a
generation that changes at every discontinuity: a jump of the hardware
clock, a resume, a change of profile, a restart, or a new grandmaster
whose time does not carry on the last's. `gptp::time::Reader` reads it.
A record is checked by its CRC, so no shared memory or unsafe code is
needed on either side.

Its state is on `/run/gptpd/INTERFACE.sock` (`gptp::status::Client`): a
snapshot of `key value` lines as a client connects, at each change and
every 2 s. Anyone may read it; root and members of the group `gptpd`
may set priority1 (`priority1 N`), which is kept in the settings file.
`/run/gptpd/INTERFACE.status` says the same for people.

As it starts, an end station refuses an interface a ptp4l runs on,
exiting 3 and saying why in `/run/gptpd/INTERFACE.refused`: two daemons
answering one Pdelay_Req make a strict neighbor fall back to plain PTP.
`gptpd --check INTERFACE` checks the same without starting.

As grandmaster, its time is TAI from the system's clock as it starts,
then the hardware clock's, never steered; taking over from a grandmaster
gone, it carries that one's time on. The kernel's TAI offset is used
where something such as chrony set it, and otherwise UTC + 37 s.

A Sync whose time stands more than 5 µs from the line, on the hardware
clock, is held back: the last line stays until a later Sync says the same,
a step and a new generation, or for 1 s. One bad Sync costs applications
nothing. CLOCK_MONOTONIC's rate steps as NTP steers it. Two readings in a
row off the comparison's line restart it, so the rate is right within
0.2 s, and no record's line on CLOCK_MONOTONIC moves more than 4 µs from
the last, so what it lagged is made up over a few records rather than in
one step.

A port that is down never takes the role. With the link down, the record
holds the last line for 5 s, then says none. The hardware clock jumping,
as a card reset sets it, takes the port down until the clock is compared
afresh; the grandmaster's time is then drawn on it again from its line on
CLOCK_MONOTONIC, which no jump of the hardware clock moves.

`config/systemd/gptpd@.service` runs it, ordered against and conflicting
with triib's and plaa's ptp4l units.

### A bridge

```
gptpd --bridge br0 --clock enp1s0 enp2s0 enp3s0 enp4s0 enp5s0 enp6s0
```

- The ports are numbered from 1 in the order given, as ptp4l numbers
  them.
- With `--bridge`, a port runs only while it is in the bridge, and the
  bridge's address names the system's clock.
- A port's own clock stamps what its neighbor sees, and runs free: any
  frequency adjustment linuxptp left on it is taken off as the daemon
  starts, and nothing moves it after. Taking over from linuxptp, a
  port's clock goes on from the grandmaster's time where phc2sys left
  it, and its neighbor sees no step, only a rate a few ppm off.
- For residence times each stamp is also put on one LocalClock,
  `--clock`'s, which every port's clock is compared with ten times a
  second, rate and all. A card reset sets its clock again; the
  comparison finds that and starts afresh.
- It writes how gPTP stands to `/run/gptpd/status`: the grandmaster,
  with its time and rate against the LocalClock, and each port's
  asCapable, role, link delay and neighbor.
- On a loop, against ptp4l's port at the far end and with the LocalClock
  on another card's clock, the grandmaster's time it worked out from
  each Sync stood -5 ns from the time ptp4l and phc2sys keep on the far
  port's clock, with a standard deviation of 50 ns, over 5 minutes.

`config/systemd/gptpd.service` runs it on a bridge. It conflicts with
linuxptp's units and is ordered against them, so one command switches,
and needs `CAP_NET_RAW`, `CAP_NET_ADMIN` and the ports' `/dev/ptpN`
read-write. It starts as the bridge appears, not once the network is
online, so its ports answer Pdelay as their links come up. The bridge
must have its own MAC by then.

`docs/endpoints-plan.md` is the plan for end stations, grandmaster
capability and the AVB Lite profile.

### Either way

- Links are read at once when netlink says one changed, and every 250 ms
  besides. A port whose sending finds the network down goes down at once.
- Every frame waiting is read before a timeout is acted on, so a stall is
  never taken for a Sync or a Pdelay response gone.
- It locks itself in RAM as it starts, and both units run it real-time,
  FIFO priority 10, below the streams' round robin 15.
- A transmit time is paired with its frame by the number the kernel gives
  it, so one that came late is never taken for the next frame's.
- The link delay is the median of the last 8 measurements. One more
  than 400 ns from it comes of a bad time stamp on either side, and that
  exchange is left out, its rate too, unless fifteen in a row say the
  link changed, and then the rate starts afresh from there. Each left out
  is logged with its size.
- On a card that stamps every frame, a frame that comes without a time
  leaves the one before it distrusted, as it may carry that frame's time:
  its Pdelay exchange or Sync is left out, and logged.
- A port no longer asCapable says why in the log, and its delay when that
  is the reason.

## Packages

`scripts/dist-install.sh` lays out a release build as the packages do:
`/usr/bin/gptpd`, `gptpd@.service`, a polkit rule letting the person at
the computer start, stop and restart `gptpd@*`, the group `gptpd` (by
sysusers.d) and `/etc/gptpd`. `packaging/nfpm.yaml` makes Debian and RPM
packages from that tree, and `packaging/arch/gptpd/PKGBUILD` an Arch
one. Installing starts and enables nothing.

## Building

```sh
cargo test
cargo build --release
```

The workspace builds on its own: `gptpd`'s platform code is `raw-ether`,
the raw-Ethernet layer split from avb-net, which now builds on it.

## License

MIT OR Apache-2.0, at your option.
