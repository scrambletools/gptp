use super::*;
use crate::message::ClockIdentity;
use std::vec::Vec;

const SECOND: i128 = 1_000_000_000;
const LOCAL_BASE: i128 = 2_000 * SECOND;

fn ours() -> PortIdentity {
    PortIdentity {
        clock: ClockIdentity(0xa010_a3ff_fea5_1726),
        port: 1,
    }
}

/// A grandmaster through a switch knowing nothing of PTP: its clock runs
/// at `rate` times the endpoint's from `offset`; frames take `delay` plus
/// `asymmetry` to the endpoint and `delay` less it back.
struct Transmitter {
    identity: PortIdentity,
    system: SystemIdentity,
    rate: f64,
    offset: i128,
    delay: i128,
    asymmetry: i128,
    sequence: u16,
}

impl Transmitter {
    fn new(low: u64, priority1: u8) -> Self {
        let clock = ClockIdentity(0x8c1f_64ff_fe36_c100 | low);
        Self {
            identity: PortIdentity { clock, port: 1 },
            system: SystemIdentity::grandmaster_capable(clock, priority1, 248),
            rate: 1.0 + 15e-6,
            offset: 40 * SECOND,
            delay: 50_000,
            asymmetry: 120,
            sequence: 0,
        }
    }

    fn clock(&self, local: i128) -> i128 {
        self.offset + local + (local as f64 * (self.rate - 1.0)) as i128
    }

    fn header(&self, sequence: u16, flags: u16, control: u8, log_interval: i8) -> Header {
        Header {
            major_sdo_id: 0,
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

fn encoded(message: &Message) -> Vec<u8> {
    let mut frame = std::vec![0; message.encoded_len()];
    message.encode(&mut frame).expect("encodes");
    frame
}

struct Bench {
    client: Client,
    transmitters: Vec<Transmitter>,
    now: Duration,
}

fn local(now: Duration) -> i128 {
    LOCAL_BASE + now.as_nanos() as i128
}

impl Bench {
    fn new(transmitters: Vec<Transmitter>, asymmetry: i64) -> Self {
        let mut client = Client::new(Config {
            asymmetry,
            ..Config::avb_lite(ours())
        });
        client.set_link(Duration::ZERO, true);
        Self {
            client,
            transmitters,
            now: Duration::ZERO,
        }
    }

    /// Runs to `until` in Sync intervals: each timetransmitter announcing each
    /// second and syncing each step, the client asking its delay.
    fn run(&mut self, until: Duration) {
        let step = Duration::from_millis(125);
        while self.now + step <= until {
            self.now += step;
            let now = self.now;
            for index in 0..self.transmitters.len() {
                self.transmitters[index].sequence =
                    self.transmitters[index].sequence.wrapping_add(1);
                let transmitter = &self.transmitters[index];
                if now.subsec_millis() == 0 {
                    let announce = Message {
                        header: transmitter.header(
                            transmitter.sequence,
                            Flags::PTP_TIMESCALE,
                            5,
                            0,
                        ),
                        body: Body::Announce(crate::message::Announce {
                            origin: Timestamp::default(),
                            current_utc_offset: 37,
                            reserved: 0,
                            priority1: transmitter.system.priority1,
                            clock_class: transmitter.system.clock_class,
                            clock_accuracy: transmitter.system.clock_accuracy,
                            offset_scaled_log_variance: transmitter
                                .system
                                .offset_scaled_log_variance,
                            priority2: transmitter.system.priority2,
                            grandmaster: transmitter.system.clock,
                            steps_removed: 0,
                            time_source: 0xa0,
                        }),
                        tlvs: &[],
                    };
                    let frame = encoded(&announce);
                    self.client.handle_frame(now, &frame, 0);
                }
                // The Sync leaves the timetransmitter at local(now) and arrives a
                // path later.
                let left = local(now);
                let arrives = left + transmitter.delay + transmitter.asymmetry;
                let sync = Message {
                    header: transmitter.header(
                        transmitter.sequence,
                        Flags::TWO_STEP | Flags::PTP_TIMESCALE,
                        0,
                        -3,
                    ),
                    body: Body::Sync {
                        origin: Timestamp::default(),
                    },
                    tlvs: &[],
                };
                let follow_up = Message {
                    header: transmitter.header(transmitter.sequence, Flags::PTP_TIMESCALE, 2, -3),
                    body: Body::FollowUp {
                        precise_origin: Timestamp::from_nanos(transmitter.clock(left))
                            .expect("origin"),
                    },
                    tlvs: &[],
                };
                let (sync, follow_up) = (encoded(&sync), encoded(&follow_up));
                self.client.handle_frame(now, &sync, arrives);
                self.client.handle_frame(now, &follow_up, 0);
            }
            while self.client.poll_timeout().is_some_and(|due| due <= now) {
                self.client.handle_timeout(now);
            }
            self.answer();
        }
    }

    /// Answers the client's Delay_Req as each timetransmitter would, its own
    /// requests leaving now.
    fn answer(&mut self) {
        while let Some(transmit) = self.client.poll_transmit() {
            let left = local(self.now) + 10_000;
            let Some((message_type, sequence)) = transmit.stamp else {
                continue;
            };
            self.client
                .handle_transmit_time(message_type, sequence, left);
            let request = Message::decode(&transmit.frame).expect("decodes");
            assert_eq!(request.message_type(), MessageType::DelayReq);
            assert_eq!(request.header.major_sdo_id, 0);
            for transmitter in &self.transmitters {
                let arrived = transmitter.clock(left + transmitter.delay - transmitter.asymmetry);
                let response = Message {
                    header: transmitter.header(sequence, 0, 3, 0),
                    body: Body::DelayResp {
                        receive: Timestamp::from_nanos(arrived).expect("t4"),
                        requesting: request.header.source,
                    },
                    tlvs: &[],
                };
                let frame = encoded(&response);
                assert_eq!(requesting(&frame), Some(ours().clock));
                self.client.handle_frame(self.now, &frame, 0);
            }
        }
    }
}

#[test]
fn the_client_follows_the_grandmasters_time_end_to_end() {
    let mut bench = Bench::new(std::vec![Transmitter::new(0x30, 246)], 120);
    assert_eq!(bench.client.time_line(), None);
    bench.run(Duration::from_secs(6));
    let transmitter = &bench.transmitters[0];
    assert_eq!(
        bench.client.grandmaster().map(|system| system.clock),
        Some(transmitter.system.clock)
    );
    let delay = bench.client.mean_path_delay().expect("delay");
    assert!((delay - 50_000.0).abs() < 5.0, "{delay}");
    let rate = bench.client.rate_ratio().expect("rate");
    assert!((rate - transmitter.rate).abs() < 1e-9, "{rate}");
    for later in [0, 1_000_000, 500_000_000] {
        let at = local(bench.now) + later;
        let time = bench.client.grandmaster_time(at).expect("time");
        assert!(
            (time - transmitter.clock(at)).abs() < 5,
            "{later}: {}",
            time - transmitter.clock(at)
        );
    }
}

#[test]
fn without_its_asymmetry_the_client_stands_that_far_off() {
    let mut bench = Bench::new(std::vec![Transmitter::new(0x30, 246)], 0);
    bench.run(Duration::from_secs(6));
    let at = local(bench.now);
    let off = bench.client.grandmaster_time(at).expect("time") - bench.transmitters[0].clock(at);
    assert!((off + 120).abs() < 5, "{off}");
}

#[test]
fn the_best_announce_is_followed_and_one_gone_quiet_ages_out() {
    let mut bench = Bench::new(
        std::vec![Transmitter::new(0x30, 248), Transmitter::new(0x40, 246)],
        120,
    );
    bench.run(Duration::from_secs(4));
    assert_eq!(
        bench.client.grandmaster().map(|system| system.clock),
        Some(bench.transmitters[1].system.clock)
    );
    assert!(
        core::iter::from_fn(|| bench.client.poll_event())
            .any(|event| { event == Event::Grandmaster(Some(bench.transmitters[1].system)) })
    );
    bench.transmitters.remove(1);
    bench.run(Duration::from_secs(9));
    assert_eq!(
        bench.client.grandmaster().map(|system| system.clock),
        Some(bench.transmitters[0].system.clock)
    );
    assert!(bench.client.time_line().is_some());
}

#[test]
fn a_link_going_down_forgets_the_grandmaster() {
    let mut bench = Bench::new(std::vec![Transmitter::new(0x30, 246)], 120);
    bench.run(Duration::from_secs(3));
    assert!(bench.client.grandmaster().is_some());
    let now = bench.now;
    bench.client.set_link(now, false);
    assert_eq!(bench.client.grandmaster(), None);
    assert_eq!(bench.client.time_line(), None);
}
