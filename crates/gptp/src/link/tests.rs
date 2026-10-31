use super::*;
use std::vec::Vec;

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

#[test]
fn an_endpoint_declares_itself_in_every_pdelay_message() {
    let mut endpoint = Link::new(
        ours(),
        Config {
            endpoint_declaration: true,
            ..Config::default()
        },
    );
    endpoint.set_enabled(Duration::ZERO, true);
    let request = endpoint.handle_timeout(Duration::ZERO).expect("request");
    let mut octets = [0; 128];
    let length = request.message.encode(&mut octets).expect("encodes");
    assert!(crate::lite::has_endpoint_tlv(&octets[..length], 54));
    let asked = Message {
        header: Header {
            source: theirs(),
            ..request.message.header
        },
        body: Body::PdelayReq { reserved: [0; 20] },
        tlvs: &[],
    };
    let response = endpoint.handle_message(&asked, SECOND).expect("response");
    assert_eq!(response.message.tlvs, crate::lite::ENDPOINT_TLV);
    let follow_up = endpoint
        .handle_transmit_time(
            MessageType::PdelayResp,
            response.message.header.sequence_id,
            SECOND + 50_000,
        )
        .expect("follow up");
    assert_eq!(follow_up.message.tlvs, crate::lite::ENDPOINT_TLV);
    // And as a bridge's port, none.
    let mut bridge = link();
    let plain = bridge.handle_timeout(Duration::ZERO).expect("request");
    assert!(plain.message.tlvs.is_empty());
}

#[test]
fn three_requests_in_a_row_answered_twice_stop_the_asking_for_five_minutes() {
    let mut link = link();
    let other = Neighbor {
        identity: PortIdentity {
            clock: ClockIdentity(0x8c1f_64ff_fe36_c130),
            port: 1,
        },
        ..switch()
    };
    for second in 0..3 {
        let request = link
            .handle_timeout(Duration::from_secs(second))
            .expect("request");
        let t1 = SECOND + i128::from(second) * SECOND;
        let sequence = request.message.header.sequence_id;
        link.handle_transmit_time(MessageType::PdelayReq, sequence, t1);
        let (response, follow_up, t4) = switch().answer(&request.message, t1);
        let (twice, _, _) = other.answer(&request.message, t1);
        link.handle_message(&response, t4);
        link.handle_message(&twice, t4 + 10);
        link.handle_message(&follow_up, t4 + 1_000);
    }
    assert!(link.handle_timeout(Duration::from_secs(3)).is_none());
    assert_eq!(link.poll_timeout(), Some(Duration::from_secs(303)));
    assert!(!link.as_capable());
    // It still answers others' requests meanwhile.
    let asked = Message {
        header: Header {
            source: theirs(),
            ..link.header(0, Flags::PTP_TIMESCALE, RESPONSE_CONTROL, 0)
        },
        body: Body::PdelayReq { reserved: [0; 20] },
        tlvs: &[],
    };
    assert!(link.handle_message(&asked, 10 * SECOND).is_some());
    // Five minutes on, or the link going down and up, and it asks again.
    assert!(link.handle_timeout(Duration::from_secs(303)).is_some());
    link.set_enabled(Duration::from_secs(304), false);
    link.set_enabled(Duration::from_secs(305), true);
    assert!(link.handle_timeout(Duration::from_secs(305)).is_some());
}

/// The neighbor's turnarounds, in nanoseconds, for 68 requests in a row
/// as a bridge answered them while busy: 25 us, then 0.2, 1.2 and 2.2 ms.
const BUSY_TURNAROUNDS: [i128; 68] = [
    24100, 27500, 25900, 24200, 25700, 24000, 28800, 26700, 25400, 28500, 212900, 218500, 224700,
    185400, 27400, 205700, 202400, 215800, 87800, 223500, 169700, 190900, 205900, 193900, 191300,
    199000, 29600, 215100, 223200, 1049900, 1215200, 1168200, 1206900, 975600, 1172800, 1210200,
    1221200, 1214000, 1208600, 1199900, 1204600, 1192200, 1029000, 1190400, 1214300, 1181400,
    1211800, 2178000, 2221500, 2230400, 2220200, 2222300, 2208700, 2222600, 2197700, 2215400,
    2184700, 2217900, 25700, 25000, 24200, 24600, 25100, 26900, 26600, 28600, 24300, 25100,
];

/// Runs one exchange a second from `start`, the neighbor turning each
/// around in `turnarounds` and saying its response left `early` sooner
/// than it did: whether the port was asCapable after each.
fn replay(link: &mut Link, start: u64, turnarounds: &[i128], early: &[i128]) -> Vec<bool> {
    let base = switch();
    let mut capable = Vec::new();
    for (index, (&turnaround, &early)) in turnarounds.iter().zip(early).enumerate() {
        let second = start + index as u64;
        let neighbor = Neighbor {
            delay: 4,
            turnaround,
            ..base
        };
        let request = link
            .handle_timeout(Duration::from_secs(second))
            .expect("a request each second");
        let t1 = SECOND + i128::from(second) * SECOND;
        link.handle_transmit_time(
            MessageType::PdelayReq,
            request.message.header.sequence_id,
            t1,
        );
        let (response, mut follow_up, t4) = neighbor.answer(&request.message, t1);
        if let Body::PdelayRespFollowUp {
            response_origin, ..
        } = &mut follow_up.body
        {
            *response_origin = Timestamp::from_nanos(response_origin.nanos() - early).expect("t3");
        }
        link.handle_message(&response, t4);
        link.handle_message(&follow_up, t4 + 1_000);
        capable.push(link.as_capable());
    }
    capable
}

#[test]
fn wild_delays_from_a_busy_neighbor_cost_neither_the_delay_nor_as_capable() {
    let mut link = link();
    exchange(
        &mut link,
        &Neighbor {
            delay: 4,
            ..switch()
        },
        0,
        8,
        SECOND,
    );
    assert!(link.as_capable());
    // While busy, some responses say they left 3 to 20 us before they
    // did: two to three in every eight, never more than two in a row.
    let mut early = [0; 68];
    for (index, micros) in [
        (10, 12),
        (11, 3),
        (13, 20),
        (16, 8),
        (19, 15),
        (21, 6),
        (24, 9),
        (29, 18),
        (30, 4),
        (33, 11),
        (36, 7),
        (37, 16),
        (41, 5),
        (44, 13),
        (48, 10),
        (51, 19),
        (52, 6),
        (55, 14),
    ] {
        early[index] = micros * 1_000;
    }
    let capable = replay(&mut link, 8, &BUSY_TURNAROUNDS, &early);
    assert!(capable.iter().all(|&capable| capable), "{capable:?}");
    let delay = link.measurement().expect("measured").mean_link_delay;
    assert!((delay - 4.0).abs() < 2.0, "{delay}");
}

#[test]
fn wild_delays_are_reported_with_the_median_they_stood_from() {
    let mut link = link();
    exchange(
        &mut link,
        &Neighbor {
            delay: 4,
            ..switch()
        },
        0,
        8,
        SECOND,
    );
    let mut early = [0; 2];
    early[1] = 10_000;
    replay(&mut link, 8, &[25_000, 1_200_000], &early);
    let (delay, median) = link.take_outlier().expect("an outlier");
    // 5 us more, less what the early time does to that interval's rate.
    assert!((delay - 5_004.0).abs() < 10.0, "{delay}");
    assert!((median - 4.0).abs() < 2.0, "{median}");
    assert_eq!(link.take_outlier(), None);
}

#[test]
fn a_link_delay_that_stays_changed_is_taken_after_the_outliers_allowed() {
    let mut link = link();
    exchange(
        &mut link,
        &Neighbor {
            delay: 4,
            ..switch()
        },
        0,
        8,
        SECOND,
    );
    // A longer path, still short enough: left out fifteen times, then
    // taken, the rate kept right across the change.
    exchange(
        &mut link,
        &Neighbor {
            delay: 600,
            ..switch()
        },
        8,
        15,
        SECOND,
    );
    let delay = link.measurement().expect("measured").mean_link_delay;
    assert!((delay - 4.0).abs() < 2.0, "{delay}");
    exchange(
        &mut link,
        &Neighbor {
            delay: 600,
            ..switch()
        },
        23,
        2,
        SECOND,
    );
    let measurement = link.measurement().expect("measured");
    assert!(
        (measurement.mean_link_delay - 600.0).abs() < 2.0,
        "{measurement:?}"
    );
    assert!((measurement.neighbor_rate_ratio - switch().rate).abs() < 1e-9);
    assert!(link.as_capable());
    // One too long: no longer asCapable once it stays.
    exchange(
        &mut link,
        &Neighbor {
            delay: 5_000,
            ..switch()
        },
        25,
        15,
        SECOND,
    );
    assert!(link.as_capable());
    exchange(
        &mut link,
        &Neighbor {
            delay: 5_000,
            ..switch()
        },
        40,
        1,
        SECOND,
    );
    assert!(!link.as_capable());
}

#[test]
fn a_run_of_bad_stamps_from_a_busy_neighbor_costs_neither_as_capable_nor_the_rate() {
    // As at 12:25:32 on the desktop: six responses in a row whose time
    // stood 9.76 us early, while the neighbor took 4 ms to answer.
    let mut link = link();
    exchange(
        &mut link,
        &Neighbor {
            delay: 4,
            ..switch()
        },
        0,
        8,
        SECOND,
    );
    let mut early = [0; 9];
    for slot in &mut early[1..7] {
        *slot = 9_758;
    }
    let capable = replay(&mut link, 8, &[4_000_000; 9], &early);
    assert!(capable.iter().all(|&capable| capable), "{capable:?}");
    // The first good exchange after the run: its rate spans only good ones.
    let measurement = link.measurement().expect("measured");
    assert!(
        (measurement.neighbor_rate_ratio - switch().rate).abs() < 1e-9,
        "{measurement:?}"
    );
    assert!(
        (measurement.mean_link_delay - 4.0).abs() < 2.0,
        "{measurement:?}"
    );
    let capable = replay(&mut link, 17, &[25_000; 8], &[0; 8]);
    assert!(capable.iter().all(|&capable| capable), "{capable:?}");
}
