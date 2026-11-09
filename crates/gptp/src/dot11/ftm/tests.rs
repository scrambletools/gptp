use super::*;
use std::vec;
use std::vec::Vec;

use crate::message::{Body, ClockIdentity, Flags, Header, PortIdentity, Timestamp};

const MILLISECOND: i128 = 1_000_000;

/// A Follow_Up whose sequence names the frame it is for.
fn follow_up(sequence: u16) -> [u8; FOLLOW_UP_LENGTH] {
    let mut tlv = [0; 32];
    let length = crate::message::Tlv::FollowUpInformation(Default::default())
        .encode(&mut tlv)
        .unwrap();
    let message = Message {
        header: Header {
            major_sdo_id: 1,
            minor_version: 1,
            version: 2,
            domain: 0,
            minor_sdo_id: 0,
            flags: Flags(Flags::PTP_TIMESCALE),
            correction: 0,
            type_specific: 0,
            source: PortIdentity {
                clock: ClockIdentity(0x8c1f_64ff_fe36_c100),
                port: 7,
            },
            sequence_id: sequence,
            control: 2,
            log_interval: -2,
        },
        body: Body::FollowUp {
            precise_origin: Timestamp::default(),
        },
        tlvs: &tlv[..length],
    };
    let mut out = [0; FOLLOW_UP_LENGTH];
    message.encode(&mut out).unwrap();
    out
}

#[test]
fn parameters_lay_out_as_802_11_numbers_their_bits() {
    let asked = FtmParameters::for_sync(-2, 3);
    // Burst duration 11 (128 ms) in B12-B15, Min Delta FTM 200 in B16-B23,
    // partial TSF 1, ASAP in B42 and three frames in B43-B47.
    assert_eq!(
        asked.encode(),
        [0x00, 0xb0, 0xc8, 0x01, 0x00, 0x1c, 0x00, 0x00, 0x00]
    );
    let every = FtmParameters {
        status: 3,
        value: 21,
        bursts_exponent: 9,
        burst_duration: 10,
        min_delta: 77,
        partial_tsf: 0xbeef,
        partial_tsf_no_preference: true,
        asap_capable: true,
        asap: false,
        ftms_per_burst: 31,
        format_and_bandwidth: 13,
        burst_period: 0x1234,
    };
    assert_eq!(FtmParameters::decode(&every.encode()), Some(every));
    assert_eq!(FtmParameters::decode(&[0; 8]), None);
    assert_eq!(asked.burst_length(), Some(Duration::from_millis(128)));
    assert_eq!(asked.min_delta_time(), Duration::from_millis(20));
    let fastest = FtmParameters::for_sync(-7, 3);
    assert_eq!(fastest.burst_length(), Some(Duration::from_millis(4)));
    assert_eq!(fastest.min_delta_time(), Duration::from_micros(600));
}

#[test]
fn a_request_carries_its_parameters_and_skips_location_requests() {
    let request = FtmRequest {
        trigger: 1,
        parameters: Some(FtmParameters::for_sync(-3, 3)),
    };
    let octets = request.encode();
    assert_eq!(octets[..5], [4, 32, 1, 206, 9]);
    assert_eq!(FtmRequest::decode(&octets), Ok(request));
    // An LCI Measurement Request element before the parameters.
    let mut with_lci = vec![4, 32, 1, 38, 3, 1, 0, 8];
    with_lci.extend_from_slice(&octets[3..]);
    assert_eq!(FtmRequest::decode(&with_lci), Ok(request));
    assert_eq!(
        FtmRequest::decode(&[4, 33, 1]),
        Err(Dot11Error::OtherAction {
            category: 4,
            action: 33
        })
    );
}

#[test]
fn a_frame_carries_its_times_in_picoseconds_and_the_follow_up() {
    let frame = FineTimingMeasurement {
        dialog_token: 5,
        follow_up_dialog_token: 4,
        tod: 0x0605_0403_0201,
        toa: 0x0c0b_0a09_0807,
        tod_error: 0x0102,
        toa_error: 0x0304,
        parameters: Some(grant(&FtmParameters::for_sync(-2, 3))),
        follow_up: Some(follow_up(4)),
    };
    let octets = frame.encode();
    assert_eq!(
        octets[..20],
        [
            4, 33, 5, 4, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 2, 1, 4, 3
        ]
    );
    assert_eq!(octets[20..22], [206, 9]);
    assert_eq!(octets[31..37], [221, 80, 0x00, 0x80, 0xc2, 0]);
    assert_eq!(FineTimingMeasurement::decode(&octets), Ok(frame.clone()));
    assert_eq!(frame.follow_up_message().unwrap().header.sequence_id, 4);
}

#[test]
fn picoseconds_wrap_at_48_bits() {
    let wrap = 1u64 << 48;
    assert_eq!(picoseconds_between(wrap - 5, 7), 12);
    assert_eq!(picoseconds_between(7, wrap - 5), -12);
    assert_eq!(picoseconds(1), 1_000);
}

#[test]
fn the_responder_paces_a_burst_and_ends_it_with_token_zero() {
    let mut responder = Responder::new();
    let start = Duration::from_secs(1);
    let initial = responder
        .request(
            start,
            &FtmRequest {
                trigger: 1,
                parameters: Some(FtmParameters::for_sync(-2, 3)),
            },
        )
        .expect("the initial frame");
    let granted = initial.parameters.expect("parameters");
    assert_eq!(granted.status, STATUS_SUCCESSFUL);
    assert!(granted.asap && granted.asap_capable);
    assert_eq!(granted.ftms_per_burst, 3);
    assert_ne!(initial.dialog_token, 0);
    assert_eq!(initial.follow_up_dialog_token, 0);

    // Nothing until its Ack, and then not before Min Delta FTM (20 ms).
    let later = start + Duration::from_millis(25);
    assert_eq!(responder.poll_frame(later), None);
    responder.confirmed(initial.dialog_token, 1_000_000, 1_016_100);
    responder.set_follow_up(initial.dialog_token, &follow_up(1));
    assert_eq!(
        responder.poll_timeout(),
        Some(start + Duration::from_millis(20))
    );
    assert_eq!(
        responder.poll_frame(start + Duration::from_millis(10)),
        None
    );
    let second = responder.poll_frame(later).expect("the second frame");
    assert_eq!(second.follow_up_dialog_token, initial.dialog_token);
    assert_eq!(second.tod, 1_000_000_000);
    assert_eq!(second.toa, 1_016_100_000);
    assert_eq!(second.follow_up, Some(follow_up(1)));
    assert_eq!(second.parameters, None);

    responder.confirmed(second.dialog_token, 2_000_000, 2_016_100);
    responder.set_follow_up(second.dialog_token, &follow_up(2));
    let third = responder
        .poll_frame(later + Duration::from_millis(20))
        .expect("the third frame");
    assert_eq!(third.dialog_token, 0);
    assert_eq!(third.follow_up_dialog_token, second.dialog_token);
    assert_eq!(third.follow_up, Some(follow_up(2)));
    assert_eq!(responder.poll_timeout(), None);
}

#[test]
fn a_frame_without_its_follow_up_is_followed_up_by_its_times_alone() {
    let mut responder = Responder::new();
    let start = Duration::from_secs(1);
    let initial = responder
        .request(
            start,
            &FtmRequest {
                trigger: 1,
                parameters: Some(FtmParameters::for_sync(-2, 3)),
            },
        )
        .expect("the initial frame");
    // Not synchronized, the relay has no Follow_Up for it: a station that
    // only ranges still gets its times.
    responder.confirmed(initial.dialog_token, 1_000_000, 1_016_100);
    let second = responder
        .poll_frame(start + Duration::from_millis(20))
        .expect("the second frame");
    assert_eq!(second.follow_up_dialog_token, initial.dialog_token);
    assert_eq!(second.tod, 1_000_000_000);
    assert_eq!(second.toa, 1_016_100_000);
    assert_eq!(second.follow_up, None);
}

#[test]
fn a_station_with_no_spacing_gets_one_its_burst_holds() {
    let asking = |burst_duration, ftms_per_burst| FtmParameters {
        burst_duration,
        min_delta: 0,
        ftms_per_burst,
        partial_tsf_no_preference: true,
        format_and_bandwidth: FORMAT_NON_HT_20,
        ..FtmParameters::default()
    };
    // 8 frames in 32 ms, as an ESP32 ranging asks: 4 ms apart, not 20.
    let ranging = grant(&asking(9, 8));
    assert_eq!(ranging.min_delta, 40);
    assert_eq!(ranging.ftms_per_burst, 8);
    // A burst of no preference, 128 ms, holds 3 frames 20 ms apart.
    let unspecified = grant(&asking(15, 0));
    assert_eq!(unspecified.burst_duration, 11);
    assert_eq!(unspecified.min_delta, 200);
    assert_eq!(unspecified.ftms_per_burst, 3);
    // As short as can be: 250 µs for 2 frames, 100 µs apart at the least.
    assert_eq!(grant(&asking(2, 2)).min_delta, 1);
}

#[test]
fn an_unacked_frame_ends_the_burst_at_its_length() {
    let mut responder = Responder::new();
    let start = Duration::from_secs(1);
    responder.request(
        start,
        &FtmRequest {
            trigger: 1,
            parameters: Some(FtmParameters::for_sync(-2, 3)),
        },
    );
    assert_eq!(
        responder.poll_timeout(),
        Some(start + Duration::from_millis(128))
    );
    assert_eq!(
        responder.poll_frame(start + Duration::from_millis(129)),
        None
    );
    assert_eq!(responder.poll_timeout(), None);
    // A request to stop ends one under way.
    responder.request(
        start,
        &FtmRequest {
            trigger: 1,
            parameters: Some(FtmParameters::for_sync(-2, 3)),
        },
    );
    responder.request(
        start,
        &FtmRequest {
            trigger: 0,
            parameters: None,
        },
    );
    assert_eq!(responder.poll_timeout(), None);
}

/// Two radios: the responder's clock runs at `rate` times the initiator's
/// from `offset`; the initiator acks `turnaround` after a frame arrives.
/// Times in nanoseconds of the initiator's clock.
struct Air {
    rate: f64,
    offset: i128,
    turnaround: i128,
}

impl Air {
    fn responder_clock(&self, initiator: i128) -> i128 {
        self.offset + (initiator as f64 * self.rate) as i128
    }

    /// Runs the initiator and the responder for `bursts` sync intervals,
    /// each frame of a burst over the air in the delay `delays` gives for
    /// its place, both ways. Returns the measurements and when each frame
    /// arrived.
    fn run(&self, bursts: usize, delays: [i128; 3]) -> (Vec<Measurement>, Vec<i128>) {
        let mut initiator = Initiator::new(-2);
        let mut responder = Responder::new();
        let mut measured = Vec::new();
        let mut arrivals = Vec::new();
        let base = 1_000 * MILLISECOND;
        let mut now = Duration::ZERO;
        let mut sequence = 0;
        for _ in 0..bursts * 300 {
            now += Duration::from_millis(1);
            let local = base + now.as_nanos() as i128;
            let mut frame = initiator
                .poll_request(now)
                .and_then(|request| responder.request(now, &request));
            if frame.is_none() {
                frame = responder.poll_frame(now);
            }
            let Some(frame) = frame else {
                continue;
            };
            let place = arrivals.len() % 3;
            let delay = delays[place];
            let arrived = local + delay;
            let ack_left = arrived + self.turnaround;
            let ack_arrived = ack_left + delay;
            arrivals.push(arrived);
            let heard = FineTimingMeasurement::decode(&frame.encode()).unwrap();
            if let Some(measurement) = initiator.frame(now, &heard, Stamp::local(arrived), ack_left)
            {
                measured.push(measurement);
            }
            if frame.dialog_token != 0 {
                responder.confirmed(
                    frame.dialog_token,
                    self.responder_clock(local),
                    self.responder_clock(ack_arrived),
                );
                sequence += 1;
                responder.set_follow_up(frame.dialog_token, &follow_up(sequence));
            }
        }
        (measured, arrivals)
    }
}

#[test]
fn the_initiator_measures_from_the_fastest_frame_and_ack() {
    // The responder's clock 30 ppm slow: over Min Delta FTM (20 ms) it
    // drifts 600 ns, which would make the first frame look the fastest
    // unless its rate is taken out.
    let air = Air {
        rate: 1.0 - 30e-6,
        offset: 9 * MILLISECOND,
        turnaround: 16_000,
    };
    // The second frame of each burst crosses fastest.
    let (measured, arrivals) = air.run(12, [60, 40, 70]);
    assert!(measured.len() >= 10, "{} measurements", measured.len());
    // The first measurement still picks its frame at rate 1, and the next
    // takes its rate from that pick; from the third on, every frame and
    // Ack used is the fastest.
    for measurement in &measured[2..] {
        let rate_error = measurement.neighbor_rate_ratio - air.rate;
        assert!(
            rate_error.abs() < 1e-8,
            "rate {}",
            measurement.neighbor_rate_ratio
        );
        // The fastest frame and Ack, 40 ns each way, to within the
        // nanoseconds the radios stamp in.
        assert!(
            (measurement.mean_link_delay - 40.0).abs() < 2.0,
            "delay {}",
            measurement.mean_link_delay
        );
        // The frame chosen is a burst's second, and its Follow_Up came.
        let place = arrivals
            .iter()
            .position(|&arrived| arrived == measurement.arrival.local)
            .expect("an arrival");
        assert_eq!(place % 3, 1);
        assert!(measurement.follow_up_message().is_some());
    }
}

#[test]
fn a_refused_burst_of_three_is_asked_again_as_two() {
    let mut initiator = Initiator::new(-2);
    let now = Duration::from_secs(1);
    let first = initiator.poll_request(now).expect("a request");
    assert_eq!(first.parameters.unwrap().ftms_per_burst, 3);
    let refusal = |frames| FineTimingMeasurement {
        dialog_token: 0,
        follow_up_dialog_token: 0,
        tod: 0,
        toa: 0,
        tod_error: 0,
        toa_error: 0,
        parameters: Some(FtmParameters {
            status: STATUS_INCAPABLE,
            ftms_per_burst: frames,
            ..Default::default()
        }),
        follow_up: None,
    };
    assert_eq!(initiator.frame(now, &refusal(3), Stamp::local(0), 0), None);
    let retry = initiator.poll_request(now).expect("asked again at once");
    assert_eq!(retry.parameters.unwrap().ftms_per_burst, 2);
    assert_eq!(initiator.capable(), None);
    initiator.frame(now, &refusal(2), Stamp::local(0), 0);
    assert_eq!(initiator.capable(), Some(false));
    // Back to three at the next interval.
    assert_eq!(initiator.poll_request(now), None);
    let next = initiator
        .poll_request(now + Duration::from_millis(250))
        .expect("at the next interval");
    assert_eq!(next.parameters.unwrap().ftms_per_burst, 3);
}

#[test]
fn an_unanswered_request_is_asked_again_after_10_ms() {
    let mut initiator = Initiator::new(-2);
    let now = Duration::from_secs(1);
    assert!(initiator.poll_request(now).is_some());
    assert_eq!(
        initiator.poll_timeout(),
        Some(now + Duration::from_millis(10))
    );
    assert_eq!(initiator.poll_request(now + Duration::from_millis(9)), None);
    assert!(
        initiator
            .poll_request(now + Duration::from_millis(10))
            .is_some()
    );
}
