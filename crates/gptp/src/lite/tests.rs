use super::*;

const IDENTITY: ClockIdentity = ClockIdentity(0xa010_a3ff_fea5_1726);
const BRIDGE: ClockIdentity = ClockIdentity(0x0001_f2ff_feff_3b14);

/// A Pdelay_Resp from `responder` to request `sequence` of `requester`,
/// with the Endpoint Declaration TLV or not.
fn response(
    responder: ClockIdentity,
    requester: ClockIdentity,
    sequence: u16,
    endpoint: bool,
) -> Vec<u8> {
    let mut message = vec![0; PDELAY_LEN];
    message[0] = 0x10 | PDELAY_RESP;
    message[1] = 0x02;
    let length = PDELAY_LEN + if endpoint { ENDPOINT_TLV.len() } else { 0 };
    message[2..4].copy_from_slice(&(length as u16).to_be_bytes());
    message[20..28].copy_from_slice(&responder.0.to_be_bytes());
    message[28..30].copy_from_slice(&1u16.to_be_bytes());
    message[30..32].copy_from_slice(&sequence.to_be_bytes());
    message[44..52].copy_from_slice(&requester.0.to_be_bytes());
    message[52..54].copy_from_slice(&1u16.to_be_bytes());
    if endpoint {
        message.extend_from_slice(&ENDPOINT_TLV);
    }
    message
}

/// A Pdelay_Req from `identity` without the Endpoint Declaration TLV, as
/// a bridge's port or a plain gPTP device asks.
fn plain_request(identity: ClockIdentity, sequence: u16) -> Vec<u8> {
    let mut message = pdelay_request(identity, sequence);
    message.truncate(PDELAY_LEN);
    message[2..4].copy_from_slice(&(PDELAY_LEN as u16).to_be_bytes());
    message
}

/// The link asking once a second from second `from` to `until`, each
/// request answered with `answer`.
fn run(fallback: &mut Fallback, from: u16, until: u16, answer: impl Fn(u16) -> Vec<Vec<u8>>) {
    for second in from..until {
        let now = Duration::from_secs(u64::from(second));
        fallback.handle_timeout(now);
        fallback.handle_request(now, second);
        for message in answer(second) {
            fallback.handle_message(now, &message);
        }
    }
}

/// An Announce from `grandmaster` with a Grandmaster Link TLV giving
/// `speed`, after a TLV of another kind.
fn announce(grandmaster: ClockIdentity, speed: u32) -> Vec<u8> {
    let mut message = vec![0; ANNOUNCE_LEN];
    message[0] = ANNOUNCE;
    message[1] = 0x02;
    message[53..61].copy_from_slice(&grandmaster.0.to_be_bytes());
    // A path trace TLV, then the Grandmaster Link TLV.
    message.extend_from_slice(&[0x00, 0x08, 0x00, 0x08]);
    message.extend_from_slice(&grandmaster.0.to_be_bytes());
    message.extend_from_slice(&grandmaster_link_tlv(speed));
    let length = message.len() as u16;
    message[2..4].copy_from_slice(&length.to_be_bytes());
    message
}

#[test]
fn an_announce_tells_the_grandmasters_link_speed() {
    let grandmaster = ClockIdentity(0xe8f6_0aff_fee0_9220);
    let message = announce(grandmaster, 100);
    assert_eq!(grandmaster_link(&message), Some((grandmaster, 100)));
    // Without the TLV, or from another kind of message, nothing.
    assert_eq!(grandmaster_link(&message[..ANNOUNCE_LEN]), None);
    assert_eq!(grandmaster_link(&pdelay_request(IDENTITY, 1)), None);
}

/// The profile's example: a 1 Gb/s endpoint following a 100 Mb/s
/// grandmaster settles 2.3 us behind it; the other way round, ahead; at
/// one speed, neither.
#[test]
fn mixed_link_speeds_make_an_asymmetry() {
    assert_eq!(link_asymmetry(100, 1000), 2304);
    assert_eq!(link_asymmetry(1000, 100), -2304);
    assert_eq!(link_asymmetry(1000, 1000), 0);
    assert_eq!(link_asymmetry(100, 10_000), 2534);
}

#[test]
fn requests_carry_the_endpoint_declaration() {
    let request = pdelay_request(IDENTITY, 7);
    assert_eq!(request.len(), 66);
    assert_eq!(&request[..4], &[0x12, 0x02, 0x00, 0x42]);
    assert_eq!(&request[54..58], &[0x80, 0x00, 0x00, 0x08]);
    assert!(has_endpoint_tlv(&request, PDELAY_LEN));
    assert!(!has_endpoint_tlv(&request[..PDELAY_LEN], PDELAY_LEN));
    // An endpoint of the profile's first revision, its TLV of type 0x0003
    // and seven octets long with no pad, is still recognized.
    let mut older = request[..PDELAY_LEN].to_vec();
    older[2..4].copy_from_slice(&65u16.to_be_bytes());
    older.extend_from_slice(&[0x00, 0x03, 0x00, 0x07]);
    older.extend_from_slice(&ENDPOINT_TLV[4..11]);
    assert!(has_endpoint_tlv(&older, PDELAY_LEN));
}

#[test]
fn reasons_have_the_status_names() {
    for reason in [
        Reason::EndpointTlv,
        Reason::PdelayUnanswered,
        Reason::MultipleResponders,
        Reason::Configured,
    ] {
        assert_eq!(Reason::from_name(reason.name()), Some(reason));
    }
    assert_eq!(Reason::PdelayUnanswered.name(), "pdelay-unanswered");
    assert_eq!(Reason::from_name("none"), None);
}

#[test]
fn a_single_bridge_answering_keeps_avb() {
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    run(&mut fallback, 0, 60, |sequence| {
        vec![
            response(BRIDGE, IDENTITY, sequence, false),
            plain_request(BRIDGE, sequence),
        ]
    });
    assert_eq!(fallback.reason(), None);
    assert_eq!(fallback.gptp_peers(), 1);
    assert_eq!(fallback.poll_transmit(), None);
}

#[test]
fn nine_unanswered_requests_fall_back() {
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    // The ninth request unanswered is known as the tenth goes out.
    run(&mut fallback, 0, 9, |_| Vec::new());
    assert_eq!(fallback.reason(), None);
    run(&mut fallback, 9, 10, |_| Vec::new());
    assert_eq!(fallback.reason(), Some(Reason::PdelayUnanswered));
}

/// The count starts again at every answer, so eight lost in a row now
/// and then never fall back.
#[test]
fn an_answer_starts_the_count_again() {
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    run(&mut fallback, 0, 90, |sequence| {
        if sequence % 9 == 8 {
            vec![response(BRIDGE, IDENTITY, sequence, false)]
        } else {
            Vec::new()
        }
    });
    assert_eq!(fallback.reason(), None);
}

#[test]
fn two_responders_or_an_endpoint_fall_back() {
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    run(&mut fallback, 0, 2, |sequence| {
        vec![
            response(BRIDGE, IDENTITY, sequence, false),
            response(
                ClockIdentity(0x2222_22ff_fe22_2222),
                IDENTITY,
                sequence,
                false,
            ),
        ]
    });
    assert_eq!(fallback.reason(), Some(Reason::MultipleResponders));

    // Another endpoint's request, heard through a plain switch.
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    fallback.handle_message(
        Duration::ZERO,
        &pdelay_request(ClockIdentity(0x3333_33ff_fe33_3333), 1),
    );
    assert_eq!(fallback.reason(), Some(Reason::EndpointTlv));
    // An endpoint's answer says so too.
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    run(&mut fallback, 0, 1, |sequence| {
        vec![response(BRIDGE, IDENTITY, sequence, true)]
    });
    assert_eq!(fallback.reason(), Some(Reason::EndpointTlv));
}

/// Answers to another clock's requests, or to an older request of this
/// one, are no second responder.
#[test]
fn only_answers_to_the_latest_request_count() {
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    run(&mut fallback, 0, 20, |sequence| {
        vec![
            response(BRIDGE, IDENTITY, sequence, false),
            response(
                ClockIdentity(0x2222_22ff_fe22_2222),
                ClockIdentity(0x4444_44ff_fe44_4444),
                sequence,
                false,
            ),
            response(
                ClockIdentity(0x2222_22ff_fe22_2222),
                IDENTITY,
                sequence.wrapping_sub(1),
                false,
            ),
        ]
    });
    assert_eq!(fallback.reason(), None);
}

/// Two peers asking without the TLV are a switch flooding the
/// bridge-group address; this system's own requests heard back are not a
/// peer.
#[test]
fn two_peers_asking_fall_back() {
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    for second in 0..20u16 {
        let now = Duration::from_secs(u64::from(second));
        fallback.handle_request(now, second);
        fallback.handle_message(now, &response(BRIDGE, IDENTITY, second, false));
        fallback.handle_message(now, &plain_request(BRIDGE, second));
        fallback.handle_message(now, &plain_request(IDENTITY, second));
    }
    assert_eq!(fallback.gptp_peers(), 1);
    assert_eq!(fallback.reason(), None);
    fallback.handle_message(
        Duration::from_secs(20),
        &plain_request(ClockIdentity(0x3333_33ff_fe33_3333), 1),
    );
    assert_eq!(fallback.reason(), Some(Reason::MultipleResponders));
}

/// It has weighed long enough 10 s after it starts or the link comes up,
/// and never while the link is down.
#[test]
fn it_settles_after_ten_seconds_of_link() {
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    assert!(!fallback.settled(Duration::from_secs(9)));
    assert!(fallback.settled(Duration::from_secs(10)));
    fallback.set_link(Duration::from_secs(20), false);
    assert!(!fallback.settled(Duration::from_secs(40)));
    fallback.set_link(Duration::from_secs(40), true);
    assert!(!fallback.settled(Duration::from_secs(49)));
    assert!(fallback.settled(Duration::from_secs(50)));
}

/// With the cable out nothing answers, which is no reason to fall back:
/// nothing is weighed until the link comes up, and then afresh.
#[test]
fn nothing_is_weighed_while_the_link_is_down() {
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    fallback.set_link(Duration::ZERO, false);
    run(&mut fallback, 0, 30, |_| Vec::new());
    assert_eq!(fallback.reason(), None);
    fallback.set_link(Duration::from_secs(30), true);
    run(&mut fallback, 30, 39, |_| Vec::new());
    assert_eq!(fallback.reason(), None);
    run(&mut fallback, 39, 40, |_| Vec::new());
    assert_eq!(fallback.reason(), Some(Reason::PdelayUnanswered));
}

/// The fallback holds while the link stays up, whatever answers later
/// (2.2.1), and the link coming up again weighs afresh in gPTP.
#[test]
fn the_fallback_holds_until_the_link_comes_up_again() {
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    run(&mut fallback, 0, 10, |_| Vec::new());
    assert_eq!(fallback.reason(), Some(Reason::PdelayUnanswered));
    run(&mut fallback, 10, 60, |sequence| {
        vec![response(BRIDGE, IDENTITY, sequence, false)]
    });
    assert_eq!(fallback.reason(), Some(Reason::PdelayUnanswered));
    fallback.set_link(Duration::from_secs(60), false);
    fallback.set_link(Duration::from_secs(61), true);
    assert_eq!(fallback.reason(), None);
    run(&mut fallback, 61, 90, |sequence| {
        vec![response(BRIDGE, IDENTITY, sequence, false)]
    });
    assert_eq!(fallback.reason(), None);

    // An operator's fallback holds across the link too.
    let mut fallback = Fallback::new(IDENTITY, true, Duration::ZERO);
    fallback.set_link(Duration::from_secs(1), false);
    fallback.set_link(Duration::from_secs(2), true);
    assert_eq!(fallback.reason(), Some(Reason::Configured));
}

#[test]
fn after_falling_back_a_beacon_goes_out_every_3_s() {
    let mut fallback = Fallback::new(IDENTITY, true, Duration::ZERO);
    assert_eq!(fallback.reason(), Some(Reason::Configured));
    let mut beacons = Vec::new();
    // At 0, 3, 6 and 9 s.
    for tick in 0..120 {
        let now = Duration::from_millis(tick * 100);
        fallback.handle_timeout(now);
        while let Some(beacon) = fallback.poll_transmit() {
            assert!(has_endpoint_tlv(&beacon, PDELAY_LEN));
            beacons.push(u16::from_be_bytes([beacon[30], beacon[31]]));
        }
    }
    assert_eq!(beacons, [0, 1, 2, 3]);
    // None while the link is down.
    fallback.set_link(Duration::from_secs(12), false);
    for second in 12..30 {
        fallback.handle_timeout(Duration::from_secs(second));
    }
    assert_eq!(fallback.poll_transmit(), None);
}

/// Before any fallback there is no beacon and nothing to wait for.
#[test]
fn no_beacon_in_gptp() {
    let mut fallback = Fallback::new(IDENTITY, false, Duration::ZERO);
    assert_eq!(fallback.poll_timeout(), None);
    run(&mut fallback, 0, 30, |sequence| {
        vec![response(BRIDGE, IDENTITY, sequence, false)]
    });
    assert_eq!(fallback.poll_transmit(), None);
}
