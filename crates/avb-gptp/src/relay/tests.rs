use super::*;
use std::vec::Vec;

const SECOND: i128 = 1_000_000_000;
/// The LocalClock's reading at monotonic zero.
const LOCAL_BASE: i128 = 1_000 * SECOND;
/// How long the relay's frames take from being queued to leaving.
const TRANSMIT_LATENCY: i128 = 20_000;

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
}

impl Sent {
    fn message(&self) -> Message<'_> {
        Message::decode(&self.frame).expect("the relay sends what decodes")
    }
}

struct Bench {
    relay: Relay,
    neighbors: Vec<Neighbor>,
    now: Duration,
    sent: Vec<Sent>,
    sequence: u16,
}

impl Bench {
    fn new(neighbors: Vec<Neighbor>) -> Self {
        let mut bench = Self {
            relay: Relay::new(Config::new(ours()), 3),
            neighbors,
            now: Duration::ZERO,
            sent: Vec::new(),
            sequence: 0,
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

    fn deliver(&mut self, port: u16, message: &Message, received: i128) {
        let mut frame = [0; 128];
        let length = message.encode(&mut frame).expect("encodes");
        self.relay
            .handle_frame(self.now, port, &frame[..length], received);
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
        self.deliver(neighbor.port, &sync, received);
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
            let left = transmit.stamp.map(|_| local(self.now) + TRANSMIT_LATENCY);
            if let (Some((message_type, sequence)), Some(left)) = (transmit.stamp, left) {
                self.relay
                    .handle_transmit_time(transmit.port, message_type, sequence, left);
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
            sent.port == port && sent.now > after && sent.message().message_type() == message_type
        })
    }

    fn events(&mut self) -> Vec<Event> {
        core::iter::from_fn(|| self.relay.poll_event()).collect()
    }
}

#[test]
fn a_grandmasters_time_is_relayed_on_every_master_port() {
    let mut bench = Bench::new(std::vec![
        grandmaster(),
        endpoint(2, 0x30),
        endpoint(3, 0x40)
    ]);
    bench.run(Duration::from_secs(10));
    assert_eq!(bench.relay.role(1), Some(Role::Slave));
    assert_eq!(bench.relay.role(2), Some(Role::Master));
    assert_eq!(bench.relay.role(3), Some(Role::Master));
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
    assert_eq!(bench.relay.role(1), Some(Role::Master));
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
    assert_eq!(bench.relay.role(2), Some(Role::Slave));
    assert_eq!(bench.relay.role(1), Some(Role::Master));
    assert_eq!(bench.relay.role(3), Some(Role::Master));
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
    assert_eq!(bench.relay.role(1), Some(Role::Master));
}

#[test]
fn a_grandmaster_that_stops_syncing_loses_the_slave_port_until_its_next_announce() {
    let mut bench = Bench::new(std::vec![grandmaster(), endpoint(2, 0x30)]);
    bench.run(Duration::from_millis(6_500));
    bench.events();
    bench.neighbors[0].syncs = false;
    // The last Sync came at 6.5 s: three Sync intervals on, at 6.875 s,
    // the slave port's information ages.
    bench.run(Duration::from_millis(6_750));
    assert!(bench.relay.grandmaster().is_some(), "375 ms not yet past");
    bench.run(Duration::from_millis(6_875));
    assert_eq!(bench.relay.grandmaster(), None);
    bench.run(Duration::from_secs(7));
    assert!(
        bench.relay.grandmaster().is_some(),
        "its next Announce qualifies it again"
    );
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
