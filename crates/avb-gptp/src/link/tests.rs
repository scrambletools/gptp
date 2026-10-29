use super::*;
use crate::message::ClockIdentity;

const SECOND: i128 = 1_000_000_000;

fn ours() -> PortIdentity {
    PortIdentity {
        clock: ClockIdentity(0x8c1f_64ff_fe36_c102),
        port: 4,
    }
}

fn theirs() -> PortIdentity {
    PortIdentity {
        clock: ClockIdentity(0x000a_92ff_fe02_6e05),
        port: 4,
    }
}

/// A neighbor answering this port's requests: its clock runs at `rate`
/// times ours from `offset`, the link takes `delay` each way and it turns
/// a request around in `turnaround`, all in nanoseconds of our clock.
struct Neighbor {
    identity: PortIdentity,
    rate: f64,
    offset: i128,
    delay: i128,
    turnaround: i128,
}

impl Neighbor {
    fn clock(&self, ours: i128) -> i128 {
        self.offset + (ours as f64 * self.rate) as i128
    }

    /// The response and follow up to a request that left at `t1`, and
    /// when the response arrives back here.
    fn answer(&self, request: &Message, t1: i128) -> (Message<'static>, Message<'static>, i128) {
        let at_neighbor = t1 + self.delay;
        let leaves = at_neighbor + self.turnaround;
        let header = Header {
            source: self.identity,
            ..request.header
        };
        let response = Message {
            header,
            body: Body::PdelayResp {
                request_receipt: Timestamp::from_nanos(self.clock(at_neighbor)).expect("t2"),
                requesting: request.header.source,
            },
            tlvs: &[],
        };
        let follow_up = Message {
            header,
            body: Body::PdelayRespFollowUp {
                response_origin: Timestamp::from_nanos(self.clock(leaves)).expect("t3"),
                requesting: request.header.source,
            },
            tlvs: &[],
        };
        (response, follow_up, leaves + self.delay)
    }
}

fn switch() -> Neighbor {
    Neighbor {
        identity: theirs(),
        rate: 1.0 + 25e-6,
        offset: 3 * SECOND,
        delay: 350,
        turnaround: 4_000_000,
    }
}

/// Runs `seconds` of exchanges from `start`, our clock reading `clock`
/// plus the monotonic time.
fn exchange(link: &mut Link, neighbor: &Neighbor, start: u64, seconds: u64, clock: i128) {
    for second in start..start + seconds {
        let now = Duration::from_secs(second);
        let request = link.handle_timeout(now).expect("a request each second");
        assert!(request.stamp);
        let t1 = clock + i128::from(second) * SECOND;
        link.handle_transmit_time(
            MessageType::PdelayReq,
            request.message.header.sequence_id,
            t1,
        );
        let (response, follow_up, t4) = neighbor.answer(&request.message, t1);
        assert!(link.handle_message(&response, t4).is_none());
        assert!(link.handle_message(&follow_up, t4 + 1_000).is_none());
    }
}

fn link() -> Link {
    let mut link = Link::new(ours(), Config::default());
    link.set_enabled(Duration::ZERO, true);
    link
}

#[test]
fn a_neighbors_rate_and_the_links_delay_are_measured() {
    let mut link = link();
    let neighbor = switch();
    exchange(&mut link, &neighbor, 0, 1, SECOND);
    assert!(!link.as_capable(), "no rate after one exchange");
    exchange(&mut link, &neighbor, 1, 5, SECOND);
    assert!(link.as_capable());
    let measurement = link.measurement().expect("measured");
    assert!((measurement.neighbor_rate_ratio - neighbor.rate).abs() < 1e-9);
    assert!(
        (measurement.mean_link_delay - 350.0).abs() < 2.0,
        "{measurement:?}"
    );
    assert_eq!(measurement.neighbor, theirs());
}

#[test]
fn a_long_link_is_not_as_capable() {
    let mut link = link();
    let neighbor = Neighbor {
        delay: 5_000,
        ..switch()
    };
    exchange(&mut link, &neighbor, 0, 6, SECOND);
    assert!(!link.as_capable());
    assert!(link.measurement().is_some());
}

#[test]
fn a_neighbors_clock_step_costs_one_measurement_not_as_capable() {
    let mut link = link();
    let neighbor = switch();
    exchange(&mut link, &neighbor, 0, 5, SECOND);
    assert!(link.as_capable());
    // The neighbor steps its clock by a year, as a bridge does at boot.
    let stepped = Neighbor {
        offset: neighbor.offset + 31_536_000 * SECOND,
        ..switch()
    };
    exchange(&mut link, &stepped, 5, 1, SECOND);
    assert!(link.as_capable(), "the last good rate is kept");
    exchange(&mut link, &stepped, 6, 3, SECOND);
    assert!(link.as_capable());
    let measurement = link.measurement().expect("measured");
    assert!((measurement.neighbor_rate_ratio - neighbor.rate).abs() < 1e-9);
    assert!(
        (measurement.mean_link_delay - 350.0).abs() < 2.0,
        "{measurement:?}"
    );
}

#[test]
fn a_neighbor_off_rate_from_the_start_is_never_as_capable() {
    let mut link = link();
    let neighbor = Neighbor {
        rate: 1.0 - 0.0625,
        ..switch()
    };
    exchange(&mut link, &neighbor, 0, 10, SECOND);
    assert!(!link.as_capable());
}

#[test]
fn a_neighbor_staying_off_rate_loses_as_capable_after_the_outliers_allowed() {
    let mut link = link();
    exchange(&mut link, &switch(), 0, 5, SECOND);
    let railed = Neighbor {
        rate: 1.0 - 0.0625,
        ..switch()
    };
    exchange(&mut link, &railed, 5, 3, SECOND);
    assert!(link.as_capable());
    exchange(&mut link, &railed, 8, 2, SECOND);
    assert!(!link.as_capable());
}

#[test]
fn unanswered_requests_lose_as_capable() {
    let mut link = link();
    exchange(&mut link, &switch(), 0, 5, SECOND);
    for second in 5..9 {
        link.handle_timeout(Duration::from_secs(second))
            .expect("request");
        assert!(link.as_capable(), "lost {} of {}", second - 5, 3);
    }
    link.handle_timeout(Duration::from_secs(9))
        .expect("request");
    assert!(!link.as_capable());
    // It comes back once the neighbor answers again.
    exchange(&mut link, &switch(), 10, 4, SECOND);
    assert!(link.as_capable());
}

#[test]
fn two_ports_answering_one_request_is_not_as_capable() {
    let mut link = link();
    exchange(&mut link, &switch(), 0, 5, SECOND);
    let other = Neighbor {
        identity: PortIdentity {
            clock: ClockIdentity(0x8c1f_64ff_fe36_c130),
            port: 1,
        },
        ..switch()
    };
    let request = link
        .handle_timeout(Duration::from_secs(5))
        .expect("request");
    let t1 = 6 * SECOND;
    link.handle_transmit_time(
        MessageType::PdelayReq,
        request.message.header.sequence_id,
        t1,
    );
    let (response, follow_up, t4) = switch().answer(&request.message, t1);
    let (second, _, _) = other.answer(&request.message, t1);
    link.handle_message(&response, t4);
    link.handle_message(&second, t4 + 10);
    link.handle_message(&follow_up, t4 + 1_000);
    assert!(!link.as_capable());
}

#[test]
fn a_request_is_answered_with_this_ports_own_clock() {
    let mut link = link();
    let request = Message {
        header: Header {
            source: theirs(),
            sequence_id: 0x7aea,
            ..link.header(0, Flags::PTP_TIMESCALE, RESPONSE_CONTROL, 0)
        },
        body: Body::PdelayReq { reserved: [0; 20] },
        tlvs: &[],
    };
    let t2 = 3 * SECOND + 123;
    let response = link.handle_message(&request, t2).expect("response");
    assert!(response.stamp);
    assert!(response.message.header.flags.two_step());
    assert_eq!(response.message.header.sequence_id, 0x7aea);
    assert_eq!(response.message.header.minor_version, 0);
    assert_eq!(
        response.message.body,
        Body::PdelayResp {
            request_receipt: Timestamp::from_nanos(t2).expect("t2"),
            requesting: theirs(),
        }
    );
    let t3 = t2 + 50_000;
    let follow_up = link
        .handle_transmit_time(MessageType::PdelayResp, 0x7aea, t3)
        .expect("follow up");
    assert!(!follow_up.stamp);
    assert_eq!(
        follow_up.message.body,
        Body::PdelayRespFollowUp {
            response_origin: Timestamp::from_nanos(t3).expect("t3"),
            requesting: theirs(),
        }
    );
    assert!(
        link.handle_transmit_time(MessageType::PdelayResp, 0x7aea, t3)
            .is_none()
    );
}

#[test]
fn a_port_down_neither_asks_nor_answers() {
    let mut link = Link::new(ours(), Config::default());
    assert_eq!(link.poll_timeout(), None);
    let request = Message {
        header: link.header(1, Flags::PTP_TIMESCALE, RESPONSE_CONTROL, 0),
        body: Body::PdelayReq { reserved: [0; 20] },
        tlvs: &[],
    };
    assert!(link.handle_message(&request, SECOND).is_none());
    link.set_enabled(Duration::from_secs(4), true);
    assert_eq!(link.poll_timeout(), Some(Duration::from_secs(4)));
    exchange(&mut link, &switch(), 4, 5, SECOND);
    assert!(link.as_capable());
    link.set_enabled(Duration::from_secs(9), false);
    assert!(!link.as_capable());
    assert_eq!(link.measurement(), None);
}

#[test]
fn requests_keep_their_cadence_and_skip_what_a_stall_missed() {
    let mut link = link();
    link.handle_timeout(Duration::ZERO).expect("first");
    assert_eq!(link.poll_timeout(), Some(Duration::from_secs(1)));
    assert!(link.handle_timeout(Duration::from_millis(500)).is_none());
    link.handle_timeout(Duration::from_millis(1_200))
        .expect("second");
    assert_eq!(link.poll_timeout(), Some(Duration::from_secs(2)));
    link.handle_timeout(Duration::from_secs(10))
        .expect("after a stall");
    assert_eq!(link.poll_timeout(), Some(Duration::from_secs(11)));
}
