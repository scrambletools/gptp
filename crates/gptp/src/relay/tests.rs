use super::*;
use std::vec::Vec;

const SECOND: i128 = 1_000_000_000;
/// The LocalClock's reading at monotonic zero.
const LOCAL_BASE: i128 = 1_000 * SECOND;
/// How long the relay's frames take from being queued to leaving.
const TRANSMIT_LATENCY: i128 = 20_000;
/// An 802.11 link: a frame's time over the air, and a station's
/// turnaround before its Ack (SIFS).
const AIR: i128 = 50;
const SIFS: i128 = 16_000;
const STATION: [u8; 6] = [0x8c, 0x1f, 0x64, 0x36, 0xc1, 0x50];
const ACCESS_POINT: [u8; 6] = [0x28, 0x0c, 0x50, 0x6b, 0x33, 0x70];

fn ours() -> ClockIdentity {
    ClockIdentity(0x8c1f_64ff_fe36_c102)
}

fn local(now: Duration) -> i128 {
    LOCAL_BASE + now.as_nanos() as i128
}

/// A neighbor on one of the relay's ports. Its clock runs at `rate`
/// times the LocalClock's from `offset`; the link takes `delay` each way.
#[derive(Clone)]
struct Neighbor {
    port: u16,
    identity: PortIdentity,
    rate: f64,
    offset: i128,
    delay: i128,
    /// Whether it answers the relay's Pdelay requests.
    answers: bool,
    /// What it announces: its system, steps removed and path trace.
    announces: Option<(SystemIdentity, u16, Vec<ClockIdentity>)>,
    /// Whether it sends Sync, as a grandmaster does.
    syncs: bool,
}

impl Neighbor {
    fn clock(&self, local: i128) -> i128 {
        self.offset + (local as f64 * self.rate) as i128
    }

    fn header(&self, sequence: u16, flags: u16, control: u8, log_interval: i8) -> Header {
        Header {
            major_sdo_id: 1,
            minor_version: 0,
            version: 2,
            domain: 0,
            minor_sdo_id: 0,
            flags: Flags(flags),
            correction: 0,
            type_specific: 0,
            source: self.identity,
            sequence_id: sequence,
            control,
            log_interval,
        }
    }
}

fn grandmaster() -> Neighbor {
    let clock = ClockIdentity(0x000a_92ff_fe02_6e05);
    Neighbor {
        port: 1,
        identity: PortIdentity { clock, port: 4 },
        rate: 1.0 + 25e-6,
        offset: 3 * SECOND,
        delay: 350,
        answers: true,
        announces: Some((
            SystemIdentity {
                priority1: 246,
                clock_class: 248,
                clock_accuracy: 0x21,
                offset_scaled_log_variance: 0x436a,
                priority2: 248,
                clock,
            },
            0,
            std::vec![clock],
        )),
        syncs: true,
    }
}

fn endpoint(port: u16, low: u64) -> Neighbor {
    Neighbor {
        port,
        identity: PortIdentity {
            clock: ClockIdentity(0x8c1f_64ff_fe36_c100 | low),
            port: 1,
        },
        rate: 1.0 - 40e-6,
        offset: 7 * SECOND,
        delay: 500,
        answers: true,
        announces: None,
        syncs: false,
    }
}

/// A frame the relay sent, decoded owning its octets.
struct Sent {
    now: Duration,
    port: u16,
    frame: Vec<u8>,
    left: Option<i128>,
    destination: Destination,
}

impl Sent {
    fn message(&self) -> Message<'_> {
        Message::decode(&self.frame).expect("the relay sends what decodes")
    }

    /// A gPTP message, not an 802.11 Action frame.
    fn is_gptp(&self) -> bool {
        !matches!(
            self.destination,
            Destination::Measurement { .. } | Destination::Action { .. }
        )
    }
}

struct Bench {
    relay: Relay,
    neighbors: Vec<Neighbor>,
    /// Each port's clock: from what offset, and at what rate times the
    /// LocalClock's, it runs.
    clocks: [(i128, f64); 3],
    now: Duration,
    sent: Vec<Sent>,
    sequence: u16,
    /// Whether 802.11 stations ack the Timing Measurement frames.
    acks: bool,
    /// Whether the card stamps the Syncs that come.
    stamps_syncs: bool,
}

impl Bench {
    fn new(neighbors: Vec<Neighbor>) -> Self {
        Self::with_clocks(neighbors, [(0, 1.0); 3])
    }

    fn with_clocks(neighbors: Vec<Neighbor>, clocks: [(i128, f64); 3]) -> Self {
        Self::built(neighbors, clocks, Config::new(ours()))
    }

    /// A bench whose relay may be grandmaster, at priority1 248, from TAI
    /// 5000 s at the LocalClock's start.
    fn grandmaster_capable(neighbors: Vec<Neighbor>) -> Self {
        let config = Config {
            identity: SystemIdentity::grandmaster_capable(ours(), 248, 248),
            ..Config::new(ours())
        };
        let mut bench = Self::built(neighbors, [(0, 1.0); 3], config);
        bench.relay.set_time_base(TimeBase {
            local: LOCAL_BASE,
            time: 5_000 * SECOND,
            rate: 1.0,
        });
        bench
    }

    fn built(neighbors: Vec<Neighbor>, clocks: [(i128, f64); 3], config: Config) -> Self {
        let mut bench = Self {
            relay: Relay::new(config, 3),
            neighbors,
            clocks,
            now: Duration::ZERO,
            sent: Vec::new(),
            sequence: 0,
            acks: true,
            stamps_syncs: true,
        };
        for port in 1..=3 {
            bench.relay.set_port(Duration::ZERO, port, true);
        }
        bench.drain();
        bench
    }

    fn neighbor(&self, port: u16) -> Option<&Neighbor> {
        self.neighbors.iter().find(|neighbor| neighbor.port == port)
    }

    /// Runs to `until`, in steps of a Sync interval, the neighbors
    /// announcing each second and the grandmaster syncing each step.
    fn run(&mut self, until: Duration) {
        let step = Duration::from_millis(125);
        while self.now + step <= until {
            self.now += step;
            let now = self.now;
            self.sequence = self.sequence.wrapping_add(1);
            let neighbors = self.neighbors.clone();
            for neighbor in &neighbors {
                if now.subsec_millis() == 0
                    && let Some((system, steps, path)) = &neighbor.announces
                {
                    self.announce(neighbor, system, *steps, path);
                }
                if neighbor.syncs {
                    self.sync(neighbor);
                }
            }
            while self.relay.poll_timeout().is_some_and(|due| due <= now) {
                self.relay.handle_timeout(now);
                self.drain();
            }
            self.drain();
        }
    }

    /// A moment on the LocalClock as port `port`'s clock stamps it.
    fn stamp(&self, port: u16, local: i128) -> Stamp {
        let (offset, rate) = self.clocks[usize::from(port) - 1];
        Stamp {
            port: offset + (local as f64 * rate) as i128,
            local,
            rate,
        }
    }

    fn deliver(&mut self, port: u16, message: &Message, received: i128) {
        let mut frame = [0; 128];
        let length = message.encode(&mut frame).expect("encodes");
        let stamp = self.stamp(port, received);
        self.relay
            .handle_frame(self.now, port, &frame[..length], stamp);
        self.drain();
    }

    fn announce(
        &mut self,
        neighbor: &Neighbor,
        system: &SystemIdentity,
        steps: u16,
        path: &[ClockIdentity],
    ) {
        let octets: Vec<u8> = path
            .iter()
            .flat_map(|clock| clock.0.to_be_bytes())
            .collect();
        let mut tlvs = [0; 64];
        let length = Tlv::path_trace(&octets)
            .expect("path")
            .encode(&mut tlvs)
            .expect("encodes");
        let message = Message {
            header: neighbor.header(self.sequence, Flags::PTP_TIMESCALE, 5, 0),
            body: Body::Announce(Announce {
                origin: Timestamp::default(),
                current_utc_offset: 37,
                reserved: 0,
                priority1: system.priority1,
                clock_class: system.clock_class,
                clock_accuracy: system.clock_accuracy,
                offset_scaled_log_variance: system.offset_scaled_log_variance,
                priority2: system.priority2,
                grandmaster: system.clock,
                steps_removed: steps,
                time_source: 0xa0,
            }),
            tlvs: &tlvs[..length],
        };
        self.deliver(neighbor.port, &message, local(self.now));
    }

    fn sync(&mut self, neighbor: &Neighbor) {
        let received = local(self.now);
        let origin = neighbor.clock(received - neighbor.delay);
        let sync = Message {
            header: neighbor.header(self.sequence, Flags::TWO_STEP | Flags::PTP_TIMESCALE, 0, -3),
            body: Body::Sync {
                origin: Timestamp::default(),
            },
            tlvs: &[],
        };
        if self.stamps_syncs {
            self.deliver(neighbor.port, &sync, received);
        } else {
            let mut frame = [0; 128];
            let length = sync.encode(&mut frame).expect("encodes");
            self.relay
                .handle_unstamped_sync(self.now, neighbor.port, &frame[..length]);
            self.drain();
        }
        let mut tlvs = [0; 32];
        let length = Tlv::FollowUpInformation(FollowUpInformation::default())
            .encode(&mut tlvs)
            .expect("encodes");
        let follow_up = Message {
            header: neighbor.header(self.sequence, Flags::PTP_TIMESCALE, 2, -3),
            body: Body::FollowUp {
                precise_origin: Timestamp::from_nanos(origin).expect("origin"),
            },
            tlvs: &tlvs[..length],
        };
        self.deliver(neighbor.port, &follow_up, received + 2_000);
    }

    /// Takes what the relay sends: reports when event messages left and
    /// has the neighbors answer Pdelay requests.
    fn drain(&mut self) {
        while let Some(transmit) = self.relay.poll_transmit() {
            if let Destination::Measurement { dialog_token, .. } = transmit.destination {
                // The station acks it after the air both ways and its
                // turnaround; the radio stamps both.
                let left = local(self.now) + TRANSMIT_LATENCY;
                if self.acks {
                    let departure = self.stamp(transmit.port, left);
                    let ack_arrival = self.stamp(transmit.port, left + 2 * AIR + SIFS).port;
                    self.relay.handle_measurement_confirm(
                        transmit.port,
                        dialog_token,
                        departure,
                        ack_arrival,
                    );
                }
                self.sent.push(Sent {
                    now: self.now,
                    port: transmit.port,
                    frame: transmit.frame,
                    left: Some(left),
                    destination: transmit.destination,
                });
                continue;
            }
            if let Destination::Action { .. } = transmit.destination {
                self.sent.push(Sent {
                    now: self.now,
                    port: transmit.port,
                    frame: transmit.frame,
                    left: None,
                    destination: transmit.destination,
                });
                continue;
            }
            let left = transmit.stamp.map(|_| local(self.now) + TRANSMIT_LATENCY);
            if let (Some((message_type, sequence)), Some(left)) = (transmit.stamp, left) {
                let stamp = self.stamp(transmit.port, left);
                self.relay
                    .handle_transmit_time(transmit.port, message_type, sequence, stamp);
            }
            let request = Message::decode(&transmit.frame).expect("decodes");
            if let (MessageType::PdelayReq, Some(neighbor), Some(t1)) = (
                request.message_type(),
                self.neighbor(transmit.port).cloned(),
                left,
            ) && neighbor.answers
            {
                self.answer(&neighbor, &request, t1);
            }
            self.sent.push(Sent {
                now: self.now,
                port: transmit.port,
                frame: transmit.frame,
                left,
                destination: transmit.destination,
            });
        }
    }

    fn answer(&mut self, neighbor: &Neighbor, request: &Message, t1: i128) {
        let arrives = t1 + neighbor.delay;
        let leaves = arrives + 1_000_000;
        let header = neighbor.header(
            request.header.sequence_id,
            Flags::TWO_STEP | Flags::PTP_TIMESCALE,
            5,
            0x7f,
        );
        let response = Message {
            header,
            body: Body::PdelayResp {
                request_receipt: Timestamp::from_nanos(neighbor.clock(arrives)).expect("t2"),
                requesting: request.header.source,
            },
            tlvs: &[],
        };
        let follow_up = Message {
            header: Header {
                flags: Flags(Flags::PTP_TIMESCALE),
                ..header
            },
            body: Body::PdelayRespFollowUp {
                response_origin: Timestamp::from_nanos(neighbor.clock(leaves)).expect("t3"),
                requesting: request.header.source,
            },
            tlvs: &[],
        };
        self.deliver(neighbor.port, &response, leaves + neighbor.delay);
        self.deliver(neighbor.port, &follow_up, leaves + neighbor.delay + 1_000);
    }

    fn sent_on(
        &self,
        port: u16,
        message_type: MessageType,
        after: Duration,
    ) -> impl Iterator<Item = &Sent> {
        self.sent.iter().filter(move |sent| {
            sent.port == port
                && sent.now > after
                && sent.is_gptp()
                && sent.message().message_type() == message_type
        })
    }

    fn events(&mut self) -> Vec<Event> {
        core::iter::from_fn(|| self.relay.poll_event()).collect()
    }

    /// The neighbor's Signaling saying it runs gPTP, once a second.
    fn say_gptp_capable(&mut self, neighbor: &Neighbor) {
        let mut tlv = [0; 16];
        let length = Tlv::GptpCapable(GptpCapable {
            tlv_type: 0x8000,
            log_interval: 0,
            flags: 0,
        })
        .encode(&mut tlv)
        .expect("encodes");
        let signaling = Message {
            header: neighbor.header(1, 0, 5, 0x7f),
            body: Body::Signaling {
                target: PortIdentity {
                    clock: ClockIdentity(u64::MAX),
                    port: u16::MAX,
                },
            },
            tlvs: &tlv[..length],
        };
        self.deliver(neighbor.port, &signaling, local(self.now));
    }

    /// Steps the relay's timeouts up to now.
    fn settle(&mut self) {
        while self.relay.poll_timeout().is_some_and(|due| due <= self.now) {
            self.relay.handle_timeout(self.now);
            self.drain();
        }
    }

    /// The Action frames sent on `port` from `from` on in `sent`, of the
    /// destination `kind` picks.
    fn actions_on(
        &self,
        port: u16,
        from: usize,
        kind: fn(&Destination) -> bool,
    ) -> Vec<(Vec<u8>, Option<i128>)> {
        self.sent[from..]
            .iter()
            .filter(|sent| sent.port == port && kind(&sent.destination))
            .map(|sent| (sent.frame.clone(), sent.left))
            .collect()
    }
}

fn is_measurement(destination: &Destination) -> bool {
    matches!(destination, Destination::Measurement { .. })
}

fn is_action(destination: &Destination) -> bool {
    matches!(destination, Destination::Action { .. })
}

/// A time in nanoseconds as FTM's TOD and TOA carry it.
fn picoseconds(nanos: i128) -> u64 {
    ((nanos * 1_000) as u64) & ((1 << 48) - 1)
}

#[test]
fn a_grandmasters_time_is_relayed_on_every_timetransmitter_port() {
    let mut bench = Bench::new(std::vec![
        grandmaster(),
        endpoint(2, 0x30),
        endpoint(3, 0x40)
    ]);
    bench.run(Duration::from_secs(10));
    assert_eq!(bench.relay.role(1), Some(Role::TimeReceiver));
    assert_eq!(bench.relay.role(2), Some(Role::TimeTransmitter));
    assert_eq!(bench.relay.role(3), Some(Role::TimeTransmitter));
    let gm = grandmaster();
    assert_eq!(
        bench.relay.grandmaster().map(|system| system.clock),
        Some(gm.identity.clock)
    );

    // Each relayed Follow_Up says the grandmaster's time when its Sync left
    // here: precise origin plus correction.
    let after = Duration::from_secs(6);
    for port in [2, 3] {
        let syncs: Vec<_> = bench.sent_on(port, MessageType::Sync, after).collect();
        let follow_ups: Vec<_> = bench.sent_on(port, MessageType::FollowUp, after).collect();
        assert!(syncs.len() >= 20, "port {port}: {} syncs", syncs.len());
        for follow_up in &follow_ups {
            let message = follow_up.message();
            let sync = syncs
                .iter()
                .find(|sync| sync.message().header.sequence_id == message.header.sequence_id)
                .expect("its sync");
            let Body::FollowUp { precise_origin } = message.body else {
                panic!("not a follow up");
            };
            let said = precise_origin.nanos() + correction_nanos(message.header.correction);
            let actual = gm.clock(sync.left.expect("stamped"));
            assert!(
                (said - actual).abs() <= 3,
                "port {port}: off by {} ns",
                said - actual
            );
            let information = message.follow_up_information().expect("information");
            let rate = 1.0 + f64::from(information.cumulative_scaled_rate_offset) / RATE_SCALE;
            assert!((rate - gm.rate).abs() < 1e-9, "rate {rate}");
            assert_eq!(
                message.header.source,
                PortIdentity {
                    clock: ours(),
                    port
                }
            );
        }
    }
    let local_now = local(bench.now);
    let time = bench.relay.grandmaster_time(local_now).expect("time");
    assert!((time - gm.clock(local_now)).abs() <= 3);
}

#[test]
fn each_ports_own_clock_stamps_what_its_neighbor_sees() {
    // The timereceiver port's clock 5 s ahead of the LocalClock and 20 ppm
    // fast, port 2's 7 s behind and 30 ppm slow.
    let slow = 1.0 - 30e-6;
    let mut bench = Bench::with_clocks(
        std::vec![grandmaster(), endpoint(2, 0x30), endpoint(3, 0x40)],
        [(5 * SECOND, 1.0 + 20e-6), (-7 * SECOND, slow), (0, 1.0)],
    );
    bench.run(Duration::from_secs(10));
    let gm = grandmaster();
    let neighbor = bench.neighbor(2).expect("a neighbor").clone();
    let link = bench.relay.link(2).expect("measured");
    assert!((link.neighbor_rate_ratio - neighbor.rate / slow).abs() < 1e-9);

    let after = Duration::from_secs(6);
    let syncs: Vec<_> = bench.sent_on(2, MessageType::Sync, after).collect();
    let follow_ups: Vec<_> = bench.sent_on(2, MessageType::FollowUp, after).collect();
    assert!(follow_ups.len() >= 20, "{} follow ups", follow_ups.len());
    for follow_up in &follow_ups {
        let message = follow_up.message();
        let sync = syncs
            .iter()
            .find(|sync| sync.message().header.sequence_id == message.header.sequence_id)
            .expect("its sync");
        let Body::FollowUp { precise_origin } = message.body else {
            panic!("not a follow up");
        };
        let said = precise_origin.nanos() + correction_nanos(message.header.correction);
        let actual = gm.clock(sync.left.expect("stamped"));
        assert!((said - actual).abs() <= 3, "off by {} ns", said - actual);
        // The rate the neighbor takes on is the grandmaster's over port 2's
        // clock, the one it measures.
        let information = message.follow_up_information().expect("information");
        let rate = 1.0 + f64::from(information.cumulative_scaled_rate_offset) / RATE_SCALE;
        assert!((rate - gm.rate / slow).abs() < 1e-9, "rate {rate}");
    }
    let local_now = local(bench.now);
    let time = bench.relay.grandmaster_time(local_now).expect("time");
    assert!((time - gm.clock(local_now)).abs() <= 3);
    let rate = bench.relay.rate_ratio().expect("a rate");
    assert!((rate - gm.rate).abs() < 1e-9, "rate {rate}");
}

#[test]
fn the_grandmasters_time_stands_while_a_sync_waits_for_its_follow_up() {
    let mut bench = Bench::new(std::vec![grandmaster(), endpoint(2, 0x30)]);
    bench.run(Duration::from_secs(10));
    let gm = grandmaster();
    bench.now += Duration::from_millis(125);
    let sync = Message {
        header: gm.header(
            bench.sequence.wrapping_add(1),
            Flags::TWO_STEP | Flags::PTP_TIMESCALE,
            0,
            -3,
        ),
        body: Body::Sync {
            origin: Timestamp::default(),
        },
        tlvs: &[],
    };
    bench.deliver(gm.port, &sync, local(bench.now));
    let local_now = local(bench.now);
    let time = bench.relay.grandmaster_time(local_now).expect("time");
    assert!((time - gm.clock(local_now)).abs() <= 3);
    assert!(bench.relay.rate_ratio().is_some());
}

#[test]
fn announces_carry_the_grandmaster_one_step_further() {
    let mut bench = Bench::new(std::vec![grandmaster(), endpoint(2, 0x30)]);
    bench.run(Duration::from_secs(8));
    let announce = bench
        .sent_on(2, MessageType::Announce, Duration::from_secs(5))
        .last()
        .expect("announce");
    let message = announce.message();
    let Body::Announce(announce) = message.body else {
        panic!("not an announce");
    };
    let gm = grandmaster();
    assert_eq!(announce.grandmaster, gm.identity.clock);
    assert_eq!(announce.priority1, 246);
    assert_eq!(announce.steps_removed, 1);
    assert_eq!(announce.current_utc_offset, 37);
    let path: Vec<_> = message.path_trace().expect("path").iter().collect();
    assert_eq!(path, [gm.identity.clock, ours()]);
    // Nothing goes back towards the grandmaster.
    assert_eq!(
        bench
            .sent_on(1, MessageType::Announce, Duration::from_secs(5))
            .count(),
        0
    );
    assert_eq!(
        bench
            .sent_on(1, MessageType::Sync, Duration::from_secs(5))
            .count(),
        0
    );
}

#[test]
fn with_no_grandmaster_the_relay_announces_itself_and_sends_no_sync() {
    let mut bench = Bench::new(std::vec![endpoint(1, 0x10), endpoint(2, 0x30)]);
    bench.run(Duration::from_secs(6));
    assert_eq!(bench.relay.grandmaster(), None);
    assert_eq!(bench.relay.role(1), Some(Role::TimeTransmitter));
    let announce = bench
        .sent_on(1, MessageType::Announce, Duration::from_secs(4))
        .last()
        .expect("announce");
    let message = announce.message();
    let Body::Announce(announce) = message.body else {
        panic!("not an announce");
    };
    assert_eq!(announce.grandmaster, ours());
    assert_eq!(announce.priority1, 255);
    assert_eq!(announce.steps_removed, 0);
    assert_eq!(
        message
            .path_trace()
            .expect("path")
            .iter()
            .collect::<Vec<_>>(),
        [ours()]
    );
    assert_eq!(
        bench
            .sent
            .iter()
            .filter(|sent| sent.message().message_type() == MessageType::Sync)
            .count(),
        0
    );
}

#[test]
fn a_better_grandmaster_on_another_port_takes_over() {
    let mut bench = Bench::new(std::vec![
        grandmaster(),
        endpoint(2, 0x30),
        endpoint(3, 0x40)
    ]);
    bench.run(Duration::from_secs(6));
    bench.events();
    let better = SystemIdentity {
        priority1: 200,
        ..SystemIdentity::not_grandmaster_capable(ClockIdentity(0x8c1f_64ff_fe36_c130))
    };
    bench.neighbors[1].announces = Some((better, 0, std::vec![better.clock]));
    bench.neighbors[1].syncs = true;
    bench.run(Duration::from_secs(9));
    assert_eq!(bench.relay.role(2), Some(Role::TimeReceiver));
    assert_eq!(bench.relay.role(1), Some(Role::TimeTransmitter));
    assert_eq!(bench.relay.role(3), Some(Role::TimeTransmitter));
    assert!(bench.events().contains(&Event::Grandmaster(Some(better))));
}

#[test]
fn a_grandmaster_gone_quiet_ages_out() {
    let mut bench = Bench::new(std::vec![grandmaster(), endpoint(2, 0x30)]);
    bench.run(Duration::from_secs(6));
    assert!(bench.relay.grandmaster().is_some());
    bench.events();
    bench.neighbors[0].announces = None;
    bench.neighbors[0].syncs = false;
    bench.run(Duration::from_secs(6) + Duration::from_millis(250));
    assert!(bench.relay.grandmaster().is_some(), "not yet");
    bench.run(Duration::from_secs(10));
    assert_eq!(bench.relay.grandmaster(), None);
    assert!(bench.events().contains(&Event::Grandmaster(None)));
    // The relay announces itself on both ports again.
    assert_eq!(bench.relay.role(1), Some(Role::TimeTransmitter));
}

#[test]
fn a_grandmaster_that_stops_syncing_loses_the_timereceiver_port_until_its_next_announce() {
    let mut bench = Bench::new(std::vec![grandmaster(), endpoint(2, 0x30)]);
    bench.run(Duration::from_millis(6_500));
    bench.events();
    bench.neighbors[0].syncs = false;
    // The last Sync came at 6.5 s: three Sync intervals on, at 6.875 s,
    // the timereceiver port's information ages.
    bench.run(Duration::from_millis(6_750));
    assert!(bench.relay.grandmaster().is_some(), "375 ms not yet past");
    bench.run(Duration::from_millis(6_875));
    assert_eq!(bench.relay.grandmaster(), None);
    assert!(bench.events().contains(&Event::SyncTimeout { port: 1 }));
    bench.run(Duration::from_secs(7));
    assert!(
        bench.relay.grandmaster().is_some(),
        "its next Announce qualifies it again"
    );
}

#[test]
fn syncs_without_a_time_stamp_keep_the_grandmaster_and_leave_its_time_standing() {
    let mut bench = Bench::new(std::vec![grandmaster(), endpoint(2, 0x30)]);
    bench.run(Duration::from_millis(6_500));
    bench.events();
    let synced = bench.relay.last_sync();
    bench.stamps_syncs = false;
    bench.run(Duration::from_millis(7_500));
    assert!(bench.relay.grandmaster().is_some());
    assert!(!bench.events().contains(&Event::SyncTimeout { port: 1 }));
    assert_eq!(bench.relay.last_sync(), synced, "no time from them");
    bench.stamps_syncs = true;
    bench.run(Duration::from_millis(7_625));
    assert_eq!(bench.relay.last_sync(), Some(local(bench.now)));
}

#[test]
fn a_card_that_stops_stamping_syncs_still_times_out() {
    let mut bench = Bench::new(std::vec![grandmaster(), endpoint(2, 0x30)]);
    bench.run(Duration::from_millis(6_500));
    bench.events();
    bench.stamps_syncs = false;
    // 24 Syncs without a time stamp, to 9.5 s, each start the timeout
    // afresh; the 25th on does not, so it ages at 9.875 s.
    bench.run(Duration::from_millis(9_750));
    assert!(bench.relay.grandmaster().is_some(), "not yet");
    bench.run(Duration::from_millis(9_875));
    assert_eq!(bench.relay.grandmaster(), None);
    assert!(bench.events().contains(&Event::SyncTimeout { port: 1 }));
}

#[test]
fn an_announce_that_has_passed_through_this_relay_is_ignored() {
    let mut gm = grandmaster();
    let system = gm.announces.as_ref().expect("announces").0;
    gm.announces = Some((
        system,
        2,
        std::vec![system.clock, ours(), ClockIdentity(42)],
    ));
    let mut bench = Bench::new(std::vec![gm, endpoint(2, 0x30)]);
    bench.run(Duration::from_secs(6));
    assert_eq!(bench.relay.grandmaster(), None);
}

#[test]
fn a_grandmaster_behind_a_port_not_as_capable_is_not_followed() {
    let mut gm = grandmaster();
    gm.answers = false;
    let mut bench = Bench::new(std::vec![gm, endpoint(2, 0x30)]);
    bench.run(Duration::from_secs(6));
    assert!(!bench.relay.as_capable(1));
    assert_eq!(bench.relay.role(1), Some(Role::Disabled));
    assert_eq!(bench.relay.grandmaster(), None);
}

#[test]
fn a_port_going_down_drops_what_it_heard() {
    let mut bench = Bench::new(std::vec![grandmaster(), endpoint(2, 0x30)]);
    bench.run(Duration::from_secs(6));
    bench.events();
    bench.relay.set_port(bench.now, 1, false);
    assert_eq!(bench.relay.role(1), Some(Role::Disabled));
    assert_eq!(bench.relay.grandmaster(), None);
    let events = bench.events();
    assert!(events.contains(&Event::AsCapable {
        port: 1,
        as_capable: false
    }));
    assert!(events.contains(&Event::Grandmaster(None)));
}

#[test]
fn messages_of_another_domain_or_profile_are_ignored() {
    let mut bench = Bench::new(std::vec![endpoint(1, 0x10)]);
    bench.run(Duration::from_secs(4));
    let gm = grandmaster();
    let (system, _, _) = gm.announces.clone().expect("announces");
    let mut header = gm.header(1, Flags::PTP_TIMESCALE, 5, 0);
    header.domain = 1;
    let announce = Message {
        header,
        body: Body::Announce(Announce {
            origin: Timestamp::default(),
            current_utc_offset: 37,
            reserved: 0,
            priority1: system.priority1,
            clock_class: system.clock_class,
            clock_accuracy: system.clock_accuracy,
            offset_scaled_log_variance: system.offset_scaled_log_variance,
            priority2: system.priority2,
            grandmaster: system.clock,
            steps_removed: 0,
            time_source: 0xa0,
        }),
        tlvs: &[],
    };
    bench.deliver(1, &announce, local(bench.now));
    assert_eq!(bench.relay.grandmaster(), None);
}

#[test]
fn the_relay_tells_steps_removed_the_path_and_the_last_sync() {
    let mut bench = Bench::new(std::vec![grandmaster()]);
    assert_eq!(bench.relay.steps_removed(), 0);
    assert_eq!(bench.relay.path_trace(), [ours()]);
    assert_eq!(bench.relay.last_sync(), None);
    bench.run(Duration::from_secs(8));
    let gm = grandmaster();
    assert_eq!(bench.relay.steps_removed(), 1);
    assert_eq!(bench.relay.path_trace(), [gm.identity.clock, ours()]);
    assert_eq!(bench.relay.last_sync(), Some(local(bench.now)));
}

/// Each Follow_Up's precise origin sent on `port` after `after`, with when
/// its Sync left, on the LocalClock.
fn own_follow_ups(bench: &Bench, port: u16, after: Duration) -> Vec<(i128, i128)> {
    let syncs: Vec<(u16, i128)> = bench
        .sent_on(port, MessageType::Sync, after)
        .map(|sent| {
            (
                sent.message().header.sequence_id,
                sent.left.expect("stamped"),
            )
        })
        .collect();
    bench
        .sent_on(port, MessageType::FollowUp, after)
        .filter_map(|sent| {
            let message = sent.message();
            let Body::FollowUp { precise_origin } = message.body else {
                return None;
            };
            let left = syncs
                .iter()
                .find(|(sequence, _)| *sequence == message.header.sequence_id)?
                .1;
            Some((left, precise_origin.nanos()))
        })
        .collect()
}

#[test]
fn a_grandmaster_capable_system_with_nothing_better_sends_its_own_time() {
    let mut bench = Bench::grandmaster_capable(std::vec![endpoint(1, 0x10)]);
    bench.run(Duration::from_secs(6));
    assert!(bench.relay.is_grandmaster());
    assert_eq!(
        bench
            .relay
            .grandmaster()
            .map(|grandmaster| grandmaster.clock),
        Some(ours())
    );
    assert_eq!(bench.relay.steps_removed(), 0);
    assert_eq!(bench.relay.role(1), Some(Role::TimeTransmitter));
    let follow_ups = own_follow_ups(&bench, 1, Duration::from_secs(4));
    assert!(follow_ups.len() >= 14, "{} at 8 a second", follow_ups.len());
    for (left, origin) in follow_ups {
        assert_eq!(origin, 5_000 * SECOND + (left - LOCAL_BASE));
    }
    let announce = bench
        .sent_on(1, MessageType::Announce, Duration::from_secs(4))
        .last()
        .expect("announce");
    let message = announce.message();
    let Body::Announce(announce) = message.body else {
        panic!("not an announce");
    };
    assert_eq!(announce.grandmaster, ours());
    assert_eq!(announce.priority1, 248);
    assert_eq!(announce.clock_class, 248);
    assert_eq!(announce.offset_scaled_log_variance, 0x436a);
    assert_eq!(announce.current_utc_offset, 37);
    assert_eq!(announce.time_source, INTERNAL_OSCILLATOR);
    assert_eq!(message.header.flags.0 & Flags::UTC_OFFSET_VALID, 0);
}

#[test]
fn a_better_grandmaster_takes_over_and_its_time_carries_on_when_it_goes() {
    let mut bench = Bench::grandmaster_capable(std::vec![grandmaster(), endpoint(2, 0x30)]);
    bench.run(Duration::from_secs(8));
    let gm = grandmaster();
    assert!(!bench.relay.is_grandmaster());
    assert_eq!(
        bench
            .relay
            .grandmaster()
            .map(|grandmaster| grandmaster.clock),
        Some(gm.identity.clock)
    );
    assert_eq!(bench.relay.role(1), Some(Role::TimeReceiver));
    let indicator = |bench: &Bench, after| {
        bench
            .sent_on(2, MessageType::FollowUp, after)
            .last()
            .and_then(|sent| sent.message().follow_up_information())
            .map(|information| information.gm_time_base_indicator)
    };
    let before = indicator(&bench, Duration::from_secs(7)).expect("relayed");
    // The grandmaster goes quiet; its Sync ages out first.
    bench.neighbors[0].announces = None;
    bench.neighbors[0].syncs = false;
    bench.run(Duration::from_secs(14));
    assert!(bench.relay.is_grandmaster());
    assert_eq!(
        bench
            .relay
            .grandmaster()
            .map(|grandmaster| grandmaster.clock),
        Some(ours())
    );
    // Its followers see the grandmaster's time go on, with no step: the
    // relay holds the rate it last measured.
    let follow_ups = own_follow_ups(&bench, 2, Duration::from_secs(12));
    assert!(!follow_ups.is_empty());
    for (left, origin) in follow_ups {
        let theirs = gm.clock(left);
        assert!((origin - theirs).abs() < 100, "{origin} against {theirs}");
    }
    assert_ne!(indicator(&bench, Duration::from_secs(12)), Some(before));
}

#[test]
fn priority1_255_stops_a_system_being_grandmaster() {
    let mut bench = Bench::grandmaster_capable(std::vec![endpoint(1, 0x10)]);
    bench.run(Duration::from_secs(4));
    assert!(bench.relay.is_grandmaster());
    let now = bench.now;
    bench
        .relay
        .set_identity(now, SystemIdentity::not_grandmaster_capable(ours()));
    bench.run(Duration::from_secs(8));
    assert!(!bench.relay.is_grandmaster());
    assert_eq!(bench.relay.grandmaster(), None);
    assert_eq!(
        bench
            .sent_on(1, MessageType::Sync, Duration::from_secs(5))
            .count(),
        0
    );
}

#[test]
fn each_port_says_it_runs_gptp_and_hears_its_neighbor_say_so() {
    let mut bench = Bench::new(std::vec![endpoint(1, 0x10)]);
    bench.run(Duration::from_secs(3));
    let said: Vec<_> = bench
        .sent_on(1, MessageType::Signaling, Duration::ZERO)
        .filter_map(|sent| {
            sent.message().tlvs().find_map(|tlv| match tlv {
                Tlv::GptpCapable(capable) => Some(capable),
                _ => None,
            })
        })
        .collect();
    assert!(said.len() >= 3, "once a second, {}", said.len());
    assert_eq!(said[0].tlv_type, 0x8000);
    assert!(!bench.relay.neighbor_gptp_capable(bench.now, 1));
    bench.say_gptp_capable(&endpoint(1, 0x10));
    assert!(bench.relay.neighbor_gptp_capable(bench.now, 1));
    // Nine intervals on with no word, it lapses.
    let later = bench.now + Duration::from_secs(10);
    assert!(!bench.relay.neighbor_gptp_capable(later, 1));
}

#[test]
fn a_grandmaster_capable_system_listens_before_it_leads() {
    // With a better grandmaster on the link, it never leads, not even for
    // the moment before that one's Announce comes.
    let mut bench = Bench::grandmaster_capable(std::vec![grandmaster()]);
    bench.run(Duration::from_secs(8));
    let events = bench.events();
    assert!(
        !events.iter().any(
            |event| matches!(event, Event::Grandmaster(Some(system)) if system.clock == ours())
        ),
        "{events:?}"
    );
    assert_eq!(
        bench.sent_on(1, MessageType::Sync, Duration::ZERO).count(),
        0
    );
    assert!(!bench.relay.is_grandmaster());
}

#[test]
fn with_nothing_better_heard_it_leads_once_it_has_listened() {
    let mut bench = Bench::grandmaster_capable(std::vec![endpoint(1, 0x10)]);
    bench.run(Duration::from_secs(10));
    let capable = bench
        .sent
        .iter()
        .find(|sent| {
            sent.message().message_type() == MessageType::PdelayReq
                && sent.now >= Duration::from_secs(1)
        })
        .map(|sent| sent.now)
        .expect("asking");
    let first_sync = bench
        .sent_on(1, MessageType::Sync, Duration::ZERO)
        .next()
        .map(|sent| sent.now)
        .expect("leads in the end");
    // Three Announce intervals after the port became asCapable, at about
    // a second.
    assert!(
        first_sync >= capable + Duration::from_secs(3),
        "{first_sync:?}"
    );
    assert!(bench.relay.is_grandmaster());
}

#[test]
fn a_lone_port_going_down_is_no_takeover() {
    let mut bench = Bench::grandmaster_capable(std::vec![grandmaster()]);
    bench.run(Duration::from_secs(8));
    assert!(bench.relay.grandmaster().is_some());
    assert!(!bench.relay.is_grandmaster());
    let now = bench.now;
    bench.relay.set_port(now, 1, false);
    assert!(!bench.relay.is_grandmaster());
    assert_eq!(bench.relay.grandmaster(), None);
    // Back up, it listens first, and follows the grandmaster again.
    bench.relay.set_port(now, 1, true);
    bench.run(Duration::from_secs(14));
    assert!(!bench.relay.is_grandmaster());
    assert_eq!(
        bench
            .relay
            .grandmaster()
            .map(|grandmaster| grandmaster.clock),
        Some(grandmaster().identity.clock)
    );
    assert!(
        !bench.events().iter().any(
            |event| matches!(event, Event::Grandmaster(Some(system)) if system.clock == ours())
        )
    );
}

#[test]
fn a_sync_followed_by_a_frame_without_a_time_stamp_is_left_out() {
    let mut bench = Bench::new(std::vec![grandmaster()]);
    bench.run(Duration::from_secs(8));
    let gm = grandmaster();
    let synced = bench.relay.last_sync();
    assert_eq!(
        bench.relay.handle_unstamped(gm.port),
        None,
        "after a Follow_Up"
    );
    bench.now += Duration::from_millis(125);
    bench.sequence = bench.sequence.wrapping_add(1);
    let sync = Message {
        header: gm.header(
            bench.sequence,
            Flags::TWO_STEP | Flags::PTP_TIMESCALE,
            0,
            -3,
        ),
        body: Body::Sync {
            origin: Timestamp::default(),
        },
        tlvs: &[],
    };
    bench.deliver(gm.port, &sync, local(bench.now));
    assert_eq!(bench.relay.handle_unstamped(2), None, "another port");
    assert_eq!(
        bench.relay.handle_unstamped(gm.port),
        Some(MessageType::Sync)
    );
    assert_eq!(bench.relay.handle_unstamped(gm.port), None, "once");
    let follow_up = Message {
        header: gm.header(bench.sequence, Flags::PTP_TIMESCALE, 2, -3),
        body: Body::FollowUp {
            precise_origin: Timestamp::from_nanos(gm.clock(local(bench.now) - gm.delay))
                .expect("origin"),
        },
        tlvs: &[],
    };
    bench.deliver(gm.port, &follow_up, local(bench.now) + 2_000);
    assert_eq!(bench.relay.last_sync(), synced);
    bench.run(bench.now + Duration::from_millis(125));
    assert_eq!(bench.relay.last_sync(), Some(local(bench.now)));
}

/// The Timing Measurement frames sent on `port`, decoded, with when each
/// left on the LocalClock.
fn timing_measurements(bench: &Bench, port: u16) -> Vec<(TimingMeasurement, i128)> {
    bench
        .sent
        .iter()
        .filter(|sent| {
            sent.port == port && matches!(sent.destination, Destination::Measurement { .. })
        })
        .map(|sent| {
            (
                TimingMeasurement::decode(&sent.frame).expect("decodes"),
                sent.left.expect("left"),
            )
        })
        .collect()
}

#[test]
fn an_802_11_port_carries_the_grandmasters_time_in_timing_measurement_frames() {
    // The radio's clock 3 s behind the LocalClock and 12 ppm fast.
    let radio = 1.0 + 12e-6;
    let mut bench = Bench::with_clocks(
        std::vec![grandmaster(), endpoint(3, 0x40)],
        [(0, 1.0), (-3 * SECOND, radio), (0, 1.0)],
    );
    bench
        .relay
        .set_wireless(bench.now, 2, Some(STATION), Support::TIMING_MEASUREMENT);
    bench.drain();
    bench.run(Duration::from_secs(10));
    assert!(bench.relay.as_capable(2));
    assert_eq!(bench.relay.role(2), Some(Role::TimeTransmitter));
    let gm = grandmaster();

    // Each frame carries the one before: when it left, on the radio's
    // clock, and its Follow_Up, saying the grandmaster's time then.
    let frames = timing_measurements(&bench, 2);
    let mut checked = 0;
    for (frame, _) in frames
        .iter()
        .filter(|(frame, _)| frame.follow_up_dialog_token != 0)
    {
        let (_, left) = frames
            .iter()
            .find(|(earlier, _)| earlier.dialog_token == frame.follow_up_dialog_token)
            .expect("the frame it follows up");
        let departure = bench.stamp(2, *left);
        assert_eq!(frame.tod, departure.port.div_euclid(10) as u32);
        let ack = bench.stamp(2, left + 2 * AIR + SIFS);
        assert_eq!(frame.toa, ack.port.div_euclid(10) as u32);
        let message = frame.follow_up_message().expect("a Follow_Up");
        let Body::FollowUp { precise_origin } = message.body else {
            panic!("not a Follow_Up");
        };
        let said = precise_origin.nanos() + correction_nanos(message.header.correction);
        let actual = gm.clock(*left);
        assert!((said - actual).abs() <= 3, "off by {} ns", said - actual);
        // The station measures the radio's clock, so the rate is the
        // grandmaster's over it.
        let information = message.follow_up_information().expect("information");
        let rate = 1.0 + f64::from(information.cumulative_scaled_rate_offset) / RATE_SCALE;
        assert!((rate - gm.rate / radio).abs() < 1e-9, "rate {rate}");
        assert_eq!(
            message.header.source,
            PortIdentity {
                clock: ours(),
                port: 2
            }
        );
        checked += 1;
    }
    assert!(checked >= 20, "{checked} frames followed up");

    // Announce and Signaling go to the station's own address; no Pdelay
    // or Sync on the port, and the wired port relays as before.
    let after = Duration::from_secs(1);
    let announces: Vec<_> = bench.sent_on(2, MessageType::Announce, after).collect();
    assert!(!announces.is_empty());
    assert!(
        announces
            .iter()
            .all(|sent| sent.destination == Destination::Station(STATION))
    );
    assert_eq!(bench.sent_on(2, MessageType::PdelayReq, after).count(), 0);
    assert_eq!(bench.sent_on(2, MessageType::Sync, after).count(), 0);
    assert!(bench.sent_on(3, MessageType::Sync, after).count() >= 20);
    assert!(
        bench
            .sent_on(3, MessageType::Sync, after)
            .all(|sent| sent.destination == Destination::Group)
    );
}

#[test]
fn unacked_frames_are_not_followed_up() {
    let mut bench = Bench::new(std::vec![grandmaster()]);
    bench
        .relay
        .set_wireless(bench.now, 2, Some(STATION), Support::TIMING_MEASUREMENT);
    bench.acks = false;
    bench.run(Duration::from_secs(5));
    let frames = timing_measurements(&bench, 2);
    assert!(frames.len() >= 20);
    assert!(
        frames
            .iter()
            .all(|(frame, _)| frame.follow_up_dialog_token == 0 && frame.follow_up.is_none())
    );
}

#[test]
fn an_802_11_port_needs_a_station_that_does_timing_measurement() {
    let mut bench = Bench::new(std::vec![grandmaster()]);
    bench
        .relay
        .set_wireless(bench.now, 2, None, Support::TIMING_MEASUREMENT);
    bench.drain();
    let start = bench.sent.len();
    bench.run(Duration::from_secs(5));
    assert!(!bench.relay.as_capable(2));
    assert_eq!(
        bench.relay.incapable_because(2),
        Some(link::Incapable::NoStation)
    );
    assert!(bench.sent[start..].iter().all(|sent| sent.port != 2));

    bench
        .relay
        .set_wireless(bench.now, 2, Some(STATION), Support::NONE);
    assert!(!bench.relay.as_capable(2));
    assert_eq!(
        bench.relay.incapable_because(2),
        Some(link::Incapable::NoMeasurement)
    );

    bench
        .relay
        .set_wireless(bench.now, 2, Some(STATION), Support::TIMING_MEASUREMENT);
    assert!(bench.relay.as_capable(2));
    assert_eq!(bench.relay.incapable_because(2), None);
}

#[test]
fn an_802_11_timereceiver_follows_the_grandmaster_its_frames_carry() {
    // An access point that is the grandmaster, its radio's clock its time;
    // the relay's radio 2 s ahead of the LocalClock and 8 ppm slow.
    let mut access_point = grandmaster();
    access_point.syncs = false;
    access_point.answers = false;
    let mut bench = Bench::with_clocks(
        std::vec![access_point.clone(), endpoint(2, 0x30)],
        [(2 * SECOND, 1.0 - 8e-6), (0, 1.0), (0, 1.0)],
    );
    bench.relay.set_wireless(
        bench.now,
        1,
        Some(ACCESS_POINT),
        Support::TIMING_MEASUREMENT,
    );
    bench.drain();
    let (system, steps, path) = access_point.announces.clone().expect("announces");
    let mut transmitter = dot11::Transmitter::new();
    let step = Duration::from_millis(125);
    for index in 0..80u16 {
        bench.now += step;
        let now = bench.now;
        bench.sequence = bench.sequence.wrapping_add(1);
        if now.subsec_millis() == 0 {
            bench.announce(&access_point, &system, steps, &path);
        }
        // A frame from the access point, and its Ack.
        let left = local(now);
        let frame = transmitter.next_frame();
        let arrival = bench.stamp(1, left + AIR);
        let ack_departure = bench.stamp(1, left + AIR + SIFS).port;
        bench
            .relay
            .handle_measurement(now, 1, &frame.encode(), arrival, ack_departure);
        bench.drain();
        // What the access point's radio stamped, and the Follow_Up for the
        // frame: the grandmaster's time when it left.
        transmitter.confirmed(
            frame.dialog_token,
            access_point.clock(left),
            access_point.clock(left + 2 * AIR + SIFS),
        );
        let mut tlvs = [0; 32];
        let length = Tlv::FollowUpInformation(FollowUpInformation::default())
            .encode(&mut tlvs)
            .expect("encodes");
        let follow_up = Message {
            header: access_point.header(index, Flags::PTP_TIMESCALE, 2, -3),
            body: Body::FollowUp {
                precise_origin: Timestamp::from_nanos(access_point.clock(left)).expect("origin"),
            },
            tlvs: &tlvs[..length],
        };
        let mut octets = [0; dot11::FOLLOW_UP_LENGTH];
        follow_up.encode(&mut octets).expect("encodes");
        transmitter.set_follow_up(frame.dialog_token, &octets);
        while bench.relay.poll_timeout().is_some_and(|due| due <= now) {
            bench.relay.handle_timeout(now);
            bench.drain();
        }
    }
    assert_eq!(bench.relay.role(1), Some(Role::TimeReceiver));
    assert_eq!(
        bench.relay.grandmaster().map(|system| system.clock),
        Some(access_point.identity.clock)
    );
    // The radio's 10 ns units leave the link delay and the rate over a
    // frame's interval each good to about that.
    let local_now = local(bench.now);
    let time = bench.relay.grandmaster_time(local_now).expect("time");
    let off = time - access_point.clock(local_now);
    assert!(off.abs() <= 30, "off by {off} ns");

    // And it relays that time on its wired port.
    let after = Duration::from_secs(6);
    let syncs: Vec<_> = bench.sent_on(2, MessageType::Sync, after).collect();
    let follow_ups: Vec<_> = bench.sent_on(2, MessageType::FollowUp, after).collect();
    assert!(follow_ups.len() >= 20, "{} follow ups", follow_ups.len());
    for follow_up in &follow_ups {
        let message = follow_up.message();
        let sync = syncs
            .iter()
            .find(|sync| sync.message().header.sequence_id == message.header.sequence_id)
            .expect("its sync");
        let Body::FollowUp { precise_origin } = message.body else {
            panic!("not a follow up");
        };
        let said = precise_origin.nanos() + correction_nanos(message.header.correction);
        let actual = access_point.clock(sync.left.expect("stamped"));
        assert!((said - actual).abs() <= 30, "off by {} ns", said - actual);
    }
}

/// A Follow_Up from `neighbor` saying `time` for the frame it follows up.
fn follow_up_octets(
    neighbor: &Neighbor,
    sequence: u16,
    time: i128,
) -> [u8; dot11::FOLLOW_UP_LENGTH] {
    let mut tlvs = [0; 32];
    let length = Tlv::FollowUpInformation(FollowUpInformation::default())
        .encode(&mut tlvs)
        .expect("encodes");
    let follow_up = Message {
        header: neighbor.header(sequence, Flags::PTP_TIMESCALE, 2, -3),
        body: Body::FollowUp {
            precise_origin: Timestamp::from_nanos(time).expect("origin"),
        },
        tlvs: &tlvs[..length],
    };
    let mut octets = [0; dot11::FOLLOW_UP_LENGTH];
    follow_up.encode(&mut octets).expect("encodes");
    octets
}

#[test]
fn an_802_11_port_answers_ftm_requests_with_the_grandmasters_time() {
    // The radio's clock 3 s behind the LocalClock and 12 ppm fast; the
    // station's runs as the LocalClock.
    let radio = 1.0 + 12e-6;
    let mut bench = Bench::with_clocks(
        std::vec![grandmaster(), endpoint(3, 0x40)],
        [(0, 1.0), (-3 * SECOND, radio), (0, 1.0)],
    );
    bench.relay.set_wireless(
        bench.now,
        2,
        Some(STATION),
        Support::FINE_TIMING_MEASUREMENT,
    );
    bench.drain();
    // FTM alone needs the station's word that it runs gPTP.
    assert_eq!(
        bench.relay.incapable_because(2),
        Some(link::Incapable::NotGptpCapable)
    );
    let gm = grandmaster();
    let (system, steps, path) = gm.announces.clone().expect("announces");
    let station = endpoint(2, 0x50);
    let mut initiator = ftm::Initiator::new(-3);
    let mut measured = Vec::new();
    let mut handed = 0;
    while bench.now < Duration::from_secs(10) {
        bench.now += Duration::from_millis(1);
        let now = bench.now;
        if now.as_millis().is_multiple_of(125) {
            bench.sequence = bench.sequence.wrapping_add(1);
            if now.subsec_millis() == 0 {
                bench.announce(&gm, &system, steps, &path);
                bench.say_gptp_capable(&station);
            }
            bench.sync(&gm);
        }
        if let Some(request) = initiator.poll_request(now) {
            bench.relay.handle_ftm_request(now, 2, &request.encode());
            bench.drain();
        }
        bench.settle();
        // The station hears each frame over the air and acks it a SIFS
        // later, as the bench's acks have it.
        let frames = bench.actions_on(2, handed, is_measurement);
        handed = bench.sent.len();
        for (frame, left) in frames {
            let frame = FineTimingMeasurement::decode(&frame).expect("an FTM frame");
            let arrived = left.expect("left") + AIR;
            if let Some(measurement) =
                initiator.frame(now, &frame, Stamp::local(arrived), arrived + SIFS)
            {
                measured.push(measurement);
            }
        }
    }
    assert!(bench.relay.as_capable(2));
    assert_eq!(bench.relay.role(2), Some(Role::TimeTransmitter));

    // Each frame but a burst's first carries the one before: when it left
    // and its Ack came back, on the radio's clock in picoseconds, and once
    // the relay is synchronized its Follow_Up, saying the grandmaster's
    // time then.
    let frames: Vec<(FineTimingMeasurement, i128)> = bench
        .actions_on(2, 0, is_measurement)
        .into_iter()
        .map(|(frame, left)| {
            (
                FineTimingMeasurement::decode(&frame).expect("only FTM frames"),
                left.expect("left"),
            )
        })
        .collect();
    let mut checked = 0;
    let mut synchronized = false;
    for (at, (frame, _)) in frames.iter().enumerate() {
        if frame.follow_up_dialog_token == 0 {
            continue;
        }
        let (_, left) = frames[..at]
            .iter()
            .rev()
            .find(|(earlier, _)| earlier.dialog_token == frame.follow_up_dialog_token)
            .expect("the frame it follows up");
        assert_eq!(frame.tod, picoseconds(bench.stamp(2, *left).port));
        assert_eq!(
            frame.toa,
            picoseconds(bench.stamp(2, left + 2 * AIR + SIFS).port)
        );
        let Some(message) = frame.follow_up_message() else {
            assert!(!synchronized, "a Follow_Up missing once synchronized");
            continue;
        };
        synchronized = true;
        let Body::FollowUp { precise_origin } = message.body else {
            panic!("not a Follow_Up");
        };
        let said = precise_origin.nanos() + correction_nanos(message.header.correction);
        let actual = gm.clock(*left);
        assert!((said - actual).abs() <= 3, "off by {} ns", said - actual);
        let information = message.follow_up_information().expect("information");
        let rate = 1.0 + f64::from(information.cumulative_scaled_rate_offset) / RATE_SCALE;
        assert!((rate - gm.rate / radio).abs() < 1e-9, "rate {rate}");
        checked += 1;
    }
    assert!(checked >= 40, "{checked} frames followed up");

    // The station measures the radio's rate and the air between them.
    assert!(measured.len() >= 20, "{} measurements", measured.len());
    for measurement in &measured[2..] {
        assert!(
            (measurement.neighbor_rate_ratio - radio).abs() < 1e-8,
            "rate {}",
            measurement.neighbor_rate_ratio
        );
        assert!(
            (measurement.mean_link_delay - AIR as f64).abs() < 2.0,
            "delay {}",
            measurement.mean_link_delay
        );
    }
    let after = Duration::from_secs(2);
    assert_eq!(bench.sent_on(2, MessageType::Sync, after).count(), 0);
    assert_eq!(bench.sent_on(2, MessageType::FollowUp, after).count(), 0);
    assert!(bench.sent_on(3, MessageType::Sync, after).count() >= 60);
}

#[test]
fn an_802_11_timereceiver_follows_the_grandmaster_its_ftm_bursts_carry() {
    // An access point that is the grandmaster, its radio's clock its time,
    // taking part in both ways; the relay's radio 2 s ahead of the
    // LocalClock and 8 ppm slow.
    let mut access_point = grandmaster();
    access_point.syncs = false;
    access_point.answers = false;
    let mut bench = Bench::with_clocks(
        std::vec![access_point.clone(), endpoint(2, 0x30)],
        [(2 * SECOND, 1.0 - 8e-6), (0, 1.0), (0, 1.0)],
    );
    bench
        .relay
        .set_wireless(bench.now, 1, Some(ACCESS_POINT), Support::BOTH);
    bench.drain();
    let (system, steps, path) = access_point.announces.clone().expect("announces");
    let mut responder = ftm::Responder::new();
    let mut asked = 0;
    let mut requests = 0;
    let mut sequence = 0;
    while bench.now < Duration::from_secs(10) {
        bench.now += Duration::from_millis(1);
        let now = bench.now;
        if now.subsec_millis() == 0 {
            bench.sequence = bench.sequence.wrapping_add(1);
            bench.announce(&access_point, &system, steps, &path);
        }
        bench.settle();
        // The relay's requests reach the access point, whose frames reach
        // the relay, each acked a SIFS after it arrived.
        let mut frames = Vec::new();
        for (request, _) in bench.actions_on(1, asked, is_action) {
            let request = FtmRequest::decode(&request).expect("an FTM Request");
            requests += 1;
            frames.extend(responder.request(now, &request));
        }
        asked = bench.sent.len();
        frames.extend(responder.poll_frame(now));
        for frame in frames {
            let left = local(now);
            let arrival = bench.stamp(1, left + AIR);
            let ack_departure = bench.stamp(1, left + AIR + SIFS).port;
            bench
                .relay
                .handle_measurement(now, 1, &frame.encode(), arrival, ack_departure);
            bench.drain();
            if frame.dialog_token != 0 {
                responder.confirmed(
                    frame.dialog_token,
                    access_point.clock(left),
                    access_point.clock(left + 2 * AIR + SIFS),
                );
                sequence += 1;
                let octets = follow_up_octets(&access_point, sequence, access_point.clock(left));
                responder.set_follow_up(frame.dialog_token, &octets);
            }
        }
    }
    assert_eq!(bench.relay.role(1), Some(Role::TimeReceiver));
    assert_eq!(
        bench.relay.grandmaster().map(|system| system.clock),
        Some(access_point.identity.clock)
    );
    // A burst asked for each Sync interval.
    assert!((70..=82).contains(&requests), "{requests} requests");
    // FTM's picoseconds leave the time good to the nanoseconds the bench
    // stamps in.
    let local_now = local(bench.now);
    let time = bench.relay.grandmaster_time(local_now).expect("time");
    let off = time - access_point.clock(local_now);
    assert!(off.abs() <= 5, "off by {off} ns");

    // And it relays that time on its wired port.
    let after = Duration::from_secs(6);
    let syncs: Vec<_> = bench.sent_on(2, MessageType::Sync, after).collect();
    let follow_ups: Vec<_> = bench.sent_on(2, MessageType::FollowUp, after).collect();
    assert!(follow_ups.len() >= 25, "{} follow ups", follow_ups.len());
    for follow_up in &follow_ups {
        let message = follow_up.message();
        let sync = syncs
            .iter()
            .find(|sync| sync.message().header.sequence_id == message.header.sequence_id)
            .expect("its sync");
        let Body::FollowUp { precise_origin } = message.body else {
            panic!("not a follow up");
        };
        let said = precise_origin.nanos() + correction_nanos(message.header.correction);
        let actual = access_point.clock(sync.left.expect("stamped"));
        assert!((said - actual).abs() <= 5, "off by {} ns", said - actual);
    }
}

/// Answers the FTM Requests sent on `port` from `from` on in the bench's
/// frames with a refusal, and says how many there were.
fn refuse_requests(bench: &mut Bench, port: u16, from: usize) -> usize {
    let requests = bench.actions_on(port, from, is_action);
    for (request, _) in &requests {
        let asked = FtmRequest::decode(request)
            .expect("an FTM Request")
            .parameters
            .expect("parameters");
        let refusal = FineTimingMeasurement {
            dialog_token: 0,
            follow_up_dialog_token: 0,
            tod: 0,
            toa: 0,
            tod_error: 0,
            toa_error: 0,
            parameters: Some(ftm::FtmParameters {
                status: ftm::STATUS_INCAPABLE,
                ftms_per_burst: asked.ftms_per_burst,
                ..Default::default()
            }),
            follow_up: None,
        };
        let arrival = bench.stamp(port, local(bench.now));
        bench
            .relay
            .handle_measurement(bench.now, port, &refusal.encode(), arrival, 0);
        bench.drain();
    }
    requests.len()
}

#[test]
fn ftm_alone_needs_the_neighbors_word_and_bursts_it_grants() {
    let mut access_point = grandmaster();
    access_point.syncs = false;
    access_point.answers = false;
    let (system, steps, path) = access_point.announces.clone().expect("announces");
    let mut bench = Bench::new(std::vec![access_point.clone()]);
    bench.relay.set_wireless(
        bench.now,
        1,
        Some(ACCESS_POINT),
        Support::FINE_TIMING_MEASUREMENT,
    );
    bench.drain();
    assert_eq!(
        bench.relay.incapable_because(1),
        Some(link::Incapable::NotGptpCapable)
    );
    bench.now += Duration::from_millis(1);
    bench.say_gptp_capable(&access_point);
    assert!(bench.relay.as_capable(1));
    bench.announce(&access_point, &system, steps, &path);
    assert_eq!(bench.relay.role(1), Some(Role::TimeReceiver));

    // Three frames refused, and then two: not asCapable.
    let start = bench.sent.len();
    bench.settle();
    assert_eq!(refuse_requests(&mut bench, 1, start), 1);
    let retry = bench.sent.len();
    bench.settle();
    assert_eq!(refuse_requests(&mut bench, 1, retry), 1);
    assert!(!bench.relay.as_capable(1));
    assert_eq!(
        bench.relay.incapable_because(1),
        Some(link::Incapable::FtmRefused)
    );
    assert_eq!(bench.relay.role(1), Some(Role::Disabled));

    // It asks again at the next interval, and a burst granted then makes
    // it asCapable again.
    let mut responder = ftm::Responder::new();
    let waiting = bench.sent.len();
    bench.now += Duration::from_millis(124);
    bench.settle();
    assert!(bench.actions_on(1, waiting, is_action).is_empty());
    bench.now += Duration::from_millis(1);
    bench.settle();
    let asked = bench.actions_on(1, waiting, is_action);
    assert_eq!(asked.len(), 1);
    let request = FtmRequest::decode(&asked[0].0).expect("an FTM Request");
    let granted = responder.request(bench.now, &request).expect("granted");
    let arrival = bench.stamp(1, local(bench.now));
    bench
        .relay
        .handle_measurement(bench.now, 1, &granted.encode(), arrival, 0);
    bench.drain();
    assert!(bench.relay.as_capable(1));

    // Without the neighbor's word for nine intervals, it is not.
    let lapse = bench.now + Duration::from_secs(10);
    while bench.now < lapse {
        bench.now += Duration::from_millis(5);
        bench.settle();
    }
    assert_eq!(
        bench.relay.incapable_because(1),
        Some(link::Incapable::NotGptpCapable)
    );
    assert!(!bench.relay.as_capable(1));
}

#[test]
fn bursts_refused_fall_back_to_timing_measurement_where_the_station_has_it() {
    let mut access_point = grandmaster();
    access_point.syncs = false;
    access_point.answers = false;
    let (system, steps, path) = access_point.announces.clone().expect("announces");
    let mut bench = Bench::new(std::vec![access_point.clone()]);
    bench
        .relay
        .set_wireless(bench.now, 1, Some(ACCESS_POINT), Support::BOTH);
    bench.drain();
    // Timing Measurement on domain 0 needs no word that it runs gPTP.
    assert!(bench.relay.as_capable(1));
    bench.now += Duration::from_millis(1);
    bench.announce(&access_point, &system, steps, &path);
    assert_eq!(bench.relay.role(1), Some(Role::TimeReceiver));
    let start = bench.sent.len();
    bench.settle();
    assert_eq!(refuse_requests(&mut bench, 1, start), 1);
    let retry = bench.sent.len();
    bench.settle();
    assert_eq!(refuse_requests(&mut bench, 1, retry), 1);
    // Still asCapable, with Timing Measurement now: no more requests.
    assert!(bench.relay.as_capable(1));
    assert_eq!(bench.relay.role(1), Some(Role::TimeReceiver));
    let after = bench.sent.len();
    while bench.now < Duration::from_secs(2) {
        bench.now += Duration::from_millis(5);
        bench.settle();
    }
    assert!(bench.actions_on(1, after, is_action).is_empty());
}
