use super::*;
use std::vec;
use std::vec::Vec;

use crate::message::{
    Body, ClockIdentity, Flags, FollowUpInformation, Header, PortIdentity, Timestamp, Tlv,
};

const MILLISECOND: i128 = 1_000_000;

/// A Follow_Up as a relay sends it, with `correction` in 2^-16 ns.
fn follow_up(sequence: u16, correction: i64) -> [u8; FOLLOW_UP_LENGTH] {
    let information = FollowUpInformation {
        cumulative_scaled_rate_offset: -41_943,
        gm_time_base_indicator: 3,
        last_gm_phase_change: 0,
        scaled_last_gm_freq_change: 0,
    };
    let mut tlv = [0; 32];
    let length = Tlv::FollowUpInformation(information)
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
            correction,
            type_specific: 0,
            source: PortIdentity {
                clock: ClockIdentity(0x8c1f_64ff_fe36_c100),
                port: 7,
            },
            sequence_id: sequence,
            control: 2,
            log_interval: -3,
        },
        body: Body::FollowUp {
            precise_origin: Timestamp {
                seconds: 1_793_600_000,
                nanoseconds: 125_000_000,
            },
        },
        tlvs: &tlv[..length],
    };
    let mut out = [0; FOLLOW_UP_LENGTH];
    assert_eq!(message.encode(&mut out), Ok(FOLLOW_UP_LENGTH));
    out
}

#[test]
fn a_frame_encodes_as_802_11_lays_it_out() {
    let frame = TimingMeasurement {
        dialog_token: 7,
        follow_up_dialog_token: 6,
        tod: 0x0403_0201,
        toa: 0x0807_0605,
        max_tod_error: 2,
        max_toa_error: 3,
        follow_up: Some(follow_up(41, 0)),
    };
    let octets = frame.encode();
    // TOD and TOA little-endian, as 802.11's fields are.
    assert_eq!(octets[..14], [11, 1, 7, 6, 1, 2, 3, 4, 5, 6, 7, 8, 2, 3]);
    // The Vendor Specific element of 802.1AS-2020 12.7: length 80, IEEE
    // 802.1's OUI, type 0, then the Follow_Up.
    assert_eq!(octets[14..20], [221, 80, 0x00, 0x80, 0xc2, 0]);
    assert_eq!(octets.len(), 14 + 2 + 80);
    assert_eq!(TimingMeasurement::decode(&octets), Ok(frame.clone()));
    let message = frame.follow_up_message().unwrap();
    assert_eq!(message.header.sequence_id, 41);
    assert_eq!(
        message
            .follow_up_information()
            .unwrap()
            .gm_time_base_indicator,
        3
    );
}

#[test]
fn a_frame_without_a_follow_up_ends_after_its_errors() {
    let frame = TimingMeasurement {
        dialog_token: 1,
        follow_up_dialog_token: 0,
        tod: 0,
        toa: 0,
        max_tod_error: 0,
        max_toa_error: 0,
        follow_up: None,
    };
    assert_eq!(frame.encode().len(), 14);
    assert_eq!(TimingMeasurement::decode(&frame.encode()), Ok(frame));
}

#[test]
fn other_elements_are_skipped_and_other_frames_refused() {
    let mut octets = vec![11, 1, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    // Another vendor's element, then the Follow_Up's.
    octets.extend_from_slice(&[221, 4, 0x00, 0x50, 0xf2, 9]);
    octets.extend_from_slice(&[221, 80, 0x00, 0x80, 0xc2, 0]);
    octets.extend_from_slice(&follow_up(5, 0));
    let frame = TimingMeasurement::decode(&octets).unwrap();
    assert_eq!(frame.follow_up, Some(follow_up(5, 0)));

    assert_eq!(
        TimingMeasurement::decode(&[4, 33, 0, 0]),
        Err(Dot11Error::OtherAction {
            category: 4,
            action: 33
        })
    );
    assert_eq!(
        TimingMeasurement::decode(&[11, 1, 0]),
        Err(Dot11Error::TooShort(3))
    );
    let mut truncated = vec![11, 1, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    truncated.extend_from_slice(&[221, 80, 0x00]);
    assert_eq!(
        TimingMeasurement::decode(&truncated),
        Err(Dot11Error::Truncated)
    );
}

#[test]
fn dialog_tokens_skip_zero() {
    let mut transmitter = Transmitter::new();
    let tokens: Vec<u8> = (0..300)
        .map(|_| transmitter.next_frame().dialog_token)
        .collect();
    assert_eq!(tokens[0], 1);
    assert_eq!(tokens[254], 255);
    assert_eq!(tokens[255], 1);
    assert!(tokens.iter().all(|&token| token != 0));
}

#[test]
fn a_frame_carries_the_last_one_complete() {
    let mut transmitter = Transmitter::new();
    let first = transmitter.next_frame();
    assert_eq!(first.follow_up_dialog_token, 0);
    transmitter.confirmed(first.dialog_token, 1_000_005, 1_016_515);
    transmitter.set_follow_up(first.dialog_token, &follow_up(1, 0));
    let second = transmitter.next_frame();
    assert_eq!(second.follow_up_dialog_token, first.dialog_token);
    assert_eq!(second.tod, 100_000);
    assert_eq!(second.toa, 101_651);
    assert_eq!(second.follow_up, Some(follow_up(1, 0)));

    // Its Ack lost, the second frame is never followed up; the third goes
    // without, and the fourth carries the third's.
    let third = transmitter.next_frame();
    assert_eq!(third.follow_up_dialog_token, 0);
    transmitter.confirmed(third.dialog_token, 2_000_000, 2_016_500);
    transmitter.set_follow_up(third.dialog_token, &follow_up(3, 0));
    let fourth = transmitter.next_frame();
    assert_eq!(fourth.follow_up_dialog_token, third.dialog_token);
}

/// Two radios: the transmitter's clock runs at `rate` times the
/// receiver's from `offset`; a frame takes `delay` over the air and the
/// receiver acks it `turnaround` after it arrives, in nanoseconds of the
/// receiver's clock.
struct Air {
    rate: f64,
    offset: i128,
    delay: i128,
    turnaround: i128,
}

impl Air {
    fn transmitter_clock(&self, receiver: i128) -> i128 {
        self.offset + (receiver as f64 * self.rate) as i128
    }

    /// Sends frames every `interval` from `start` (receiver time) and
    /// hands the receiver each, returning what it measured.
    fn run(&self, start: i128, interval: i128, frames: usize) -> Vec<Measurement> {
        let mut transmitter = Transmitter::new();
        let mut receiver = Receiver::new();
        let mut measured = Vec::new();
        for index in 0..frames {
            let left = start + index as i128 * interval;
            let arrived = left + self.delay;
            let ack_left = arrived + self.turnaround;
            let ack_arrived = ack_left + self.delay;
            let frame = transmitter.next_frame();
            let octets = frame.encode();
            let heard = TimingMeasurement::decode(&octets).unwrap();
            if let Some(measurement) = receiver.indication(&heard, Stamp::local(arrived), ack_left)
            {
                measured.push(measurement);
            }
            transmitter.confirmed(
                frame.dialog_token,
                self.transmitter_clock(left),
                self.transmitter_clock(ack_arrived),
            );
            transmitter.set_follow_up(frame.dialog_token, &follow_up(index as u16, 0));
        }
        measured
    }
}

#[test]
fn the_receiver_measures_the_neighbor_rate_and_the_delay() {
    let air = Air {
        rate: 1.0 + 25e-6,
        offset: 7 * MILLISECOND,
        delay: 40,
        turnaround: 16_000,
    };
    let measured = air.run(1_000 * MILLISECOND, 125 * MILLISECOND, 40);
    // The first follow-up only starts the rate's line.
    assert_eq!(measured.len(), 38);
    for measurement in &measured {
        // 10 ns units over 125 ms: the rate to within about 1e-7.
        let rate_error = measurement.neighbor_rate_ratio - air.rate;
        assert!(
            rate_error.abs() < 2e-7,
            "rate {}",
            measurement.neighbor_rate_ratio
        );
        // The delay to within the units' 10 ns.
        let delay_error = measurement.mean_link_delay - air.delay as f64;
        assert!(
            delay_error.abs() < 10.0,
            "delay {}",
            measurement.mean_link_delay
        );
    }
    // Each names the frame it follows up, by its arrival.
    let first = measured[0];
    assert_eq!(
        first.arrival.local,
        1_000 * MILLISECOND + 125 * MILLISECOND + 40
    );
    assert_eq!(first.follow_up_message().unwrap().header.sequence_id, 1);
}

#[test]
fn times_wrap_without_harm() {
    // The transmitter's clock passes 2^32 units of 10 ns (about 42.9 s)
    // midway.
    let wrap = (1i128 << 32) * 10;
    let air = Air {
        rate: 1.0 - 12e-6,
        offset: wrap - 2_000 * MILLISECOND,
        delay: 25,
        turnaround: 16_000,
    };
    let measured = air.run(0, 125 * MILLISECOND, 40);
    assert_eq!(measured.len(), 38);
    for measurement in &measured {
        assert!((measurement.neighbor_rate_ratio - air.rate).abs() < 2e-7);
        assert!((measurement.mean_link_delay - air.delay as f64).abs() < 10.0);
    }
}

#[test]
fn a_rate_beyond_the_limit_is_no_measurement() {
    let air = Air {
        rate: 1.002,
        offset: 0,
        delay: 40,
        turnaround: 16_000,
    };
    assert!(air.run(0, 125 * MILLISECOND, 10).is_empty());
}

#[test]
fn a_reset_receiver_starts_the_rate_again() {
    let air = Air {
        rate: 1.0,
        offset: 0,
        delay: 40,
        turnaround: 16_000,
    };
    let mut transmitter = Transmitter::new();
    let mut receiver = Receiver::new();
    let mut measured = 0;
    for index in 0..6 {
        if index == 3 {
            receiver.reset();
        }
        let left = index as i128 * 125 * MILLISECOND;
        let frame = transmitter.next_frame();
        if receiver
            .indication(
                &frame,
                Stamp::local(left + air.delay),
                left + air.delay + air.turnaround,
            )
            .is_some()
        {
            measured += 1;
        }
        transmitter.confirmed(
            frame.dialog_token,
            left,
            left + 2 * air.delay + air.turnaround,
        );
        transmitter.set_follow_up(frame.dialog_token, &follow_up(index as u16, 0));
    }
    // Frame 1 follows up frame 0 and starts the rate's line, and frame 2
    // measures. The reset forgets frame 2, so frame 3 measures nothing,
    // frame 4 starts the line again and frame 5 measures.
    assert_eq!(measured, 2);
}
