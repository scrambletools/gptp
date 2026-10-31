use super::*;
use std::vec::Vec;

fn octets(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex"))
        .collect()
}

fn encoded(message: &Message) -> Vec<u8> {
    let mut out = [0; 256];
    let length = message.encode(&mut out).expect("encodes");
    out[..length].to_vec()
}

/// Messages heard on a bridge's ports, after their Ethernet headers: one
/// of each type from an AVB switch, an audio interface, a Mac and two
/// ESP32 endpoints.
#[rustfmt::skip]
const RECORDED: &[(&str, &str)] = &[
    ("switch announce", "1b02004c00000008000000000000000000000000000a92fffe026e0500045258050000000000000000000000002100f6f821436af8000a92fffe026e050000a000080008000a92fffe026e05"),
    ("switch follow up", "1802004c00000008000000000000000000000000000a92fffe026e050004d24102fd00000003ec7b35272c720003001c0080c200000100000000000000000000000000000000000000000000"),
    ("switch pdelay request", "1202003600000008000000000000000000000000000a92fffe026e0500047aea05000000000000000000000000000000000000000000"),
    ("switch pdelay response", "1302003600000208000000000000000000000000000a92fffe026e0500040000057f00000003ec7a356fd3698c1f64fffe36c1020004"),
    ("switch pdelay response follow up", "1a02003600000008000000000000000000000000000a92fffe026e0500040000057f00000003ec7a3572eca28c1f64fffe36c1020004"),
    ("switch sync", "1002002c00000208000000000000000000000000000a92fffe026e050004d24100fd00000000000000000000"),
    ("interface announce", "1b02004c0000020e0000000000000000000000000001f2fffe0162e50001002b050000000000000c0e50bca0002300f8f8f8436aee0001f2fffe0162e50000a0000800080001f2fffe016200"),
    ("interface follow up", "1802004c000002000000000000000000000000000001f2fffe0162e50001009102fd00000000000c0e5244e90003001c0080c200000100000000000000000000000000000000000000000000"),
    ("interface pdelay response", "13020036000002000000000000000000000000000001f2fffe0162e500010000050000044b761c2803a8add38c1f64fffe36c1020002"),
    ("interface sync", "1002002c000002000000000000000000000000000001f2fffe0162e50001009100fd00000000000c0e51ceb8"),
    ("mac announce", "1b02004c00000008000000000000000000000000eab69c57836b000200010146050000000000000000000000002500f8f821436aebeab69c57836b00020000a000080008eab69c57836b0002"),
    ("mac follow up", "1802006000000008000000021b41000000000000eab69c57836b0002000109bd02fd0000001f6e5f12cc6f3c0003001c0080c20000010000000000000000000000066790f7714f610112103700030010000d93000004eab69c57836b00020000"),
    ("mac signaling", "1c02006a00000008000000000000000000000000eab69c57836b00020001015e05808c1f64fffe36c102000500030016000d930000010003010100002710000027100000000000030020000d93000005000f010100000000271000002710000027100000271000000000"),
    ("endpoint announce", "1b02004c000000080000000000000000000000008c1f64fffe36c13000010006000000000000000000000000000000f6f8fefffff88c1f64fffe36c1300000a0000800088c1f64fffe36c130"),
    ("endpoint pdelay request", "12020042000000080000000000000000000000008c1f64fffe36c13000010127007f0000000000000000000000000000000000000000800000088c1f6436c0010100"),
    ("endpoint pdelay response", "13020042000002080000000000000000000000008c1f64fffe36c13000010000000600000003ec7a32cf69b48c1f64fffe36c1020003800000088c1f6436c0010100"),
    ("endpoint signaling", "1c02003c000000000000000000000000000000008c1f64fffe36c13000010001007fffffffffffffffffffff8000000c0080c2000004000000000000"),
];

fn recorded(name: &str) -> Vec<u8> {
    let (_, hex) = RECORDED
        .iter()
        .find(|(each, _)| *each == name)
        .expect("recorded");
    octets(hex)
}

#[test]
fn recorded_messages_encode_as_they_were_received() {
    for (name, hex) in RECORDED {
        let received = octets(hex);
        let message = Message::decode(&received).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(message.encoded_len(), received.len(), "{name}");
        assert_eq!(encoded(&message), received, "{name}");
        // Each TLV, encoded on its own, is the octets it came from.
        let mut tlvs = Vec::new();
        for tlv in message.tlvs() {
            let mut out = [0; 128];
            let length = tlv.encode(&mut out).expect("tlv encodes");
            assert_eq!(length, tlv.encoded_len(), "{name}");
            tlvs.extend_from_slice(&out[..length]);
        }
        assert_eq!(tlvs, message.tlvs, "{name}");
    }
}

#[test]
fn ethernet_padding_after_the_message_is_left_behind() {
    let mut received = recorded("switch sync");
    let length = received.len();
    received.extend_from_slice(&[0; 16]);
    let message = Message::decode(&received).expect("sync");
    assert_eq!(message.encoded_len(), length);
}

#[test]
fn an_announce_reads_as_the_btca_compares_it() {
    let received = recorded("switch announce");
    let message = Message::decode(&received).expect("announce");
    let grandmaster = ClockIdentity(0x000a_92ff_fe02_6e05);
    assert_eq!(
        message.header.source,
        PortIdentity {
            clock: grandmaster,
            port: 4
        }
    );
    assert_eq!(message.header.sequence_id, 0x5258);
    assert_eq!(message.header.flags, Flags(Flags::PTP_TIMESCALE));
    let Body::Announce(announce) = message.body else {
        panic!("not an announce");
    };
    assert_eq!(announce.priority1, 246);
    assert_eq!(announce.clock_class, 248);
    assert_eq!(announce.clock_accuracy, 0x21);
    assert_eq!(announce.offset_scaled_log_variance, 0x436a);
    assert_eq!(announce.priority2, 248);
    assert_eq!(announce.grandmaster, grandmaster);
    assert_eq!(announce.steps_removed, 0);
    let path = message.path_trace().expect("path trace");
    assert_eq!(path.len(), 1);
    assert_eq!(path.iter().collect::<Vec<_>>(), [grandmaster]);
}

#[test]
fn a_follow_up_carries_its_time_and_the_grandmasters_rate() {
    let received = recorded("mac follow up");
    let message = Message::decode(&received).expect("follow up");
    assert_eq!(message.header.correction, 0x0000_0002_1b41_0000);
    assert_eq!(
        message.body,
        Body::FollowUp {
            precise_origin: Timestamp {
                seconds: 0x1f_6e5f,
                nanoseconds: 0x12cc_6f3c
            },
        }
    );
    let information = message.follow_up_information().expect("information");
    assert_eq!(information.cumulative_scaled_rate_offset, 0);
    assert_eq!(information.last_gm_phase_change, 0x0006_6790_f771_4f61);
    assert_eq!(information.scaled_last_gm_freq_change, 0x0112_1037);
    // The vendor's own TLV after it is kept as it came.
    let vendor = message.tlvs().nth(1).expect("second tlv");
    assert!(
        matches!(vendor, Tlv::Other { tlv_type: 0x0003, value } if value[..3] == [0x00, 0x0d, 0x93])
    );
}

#[test]
fn a_negative_phase_change_keeps_its_sign() {
    let information = FollowUpInformation {
        last_gm_phase_change: -(1 << 70),
        ..FollowUpInformation::default()
    };
    let mut out = [0; 32];
    Tlv::FollowUpInformation(information)
        .encode(&mut out)
        .expect("encodes");
    let value = &out[4..];
    assert_eq!(
        Tlv::read(ORGANIZATION_EXTENSION, value),
        Tlv::FollowUpInformation(information)
    );
}

#[test]
fn a_pdelay_response_names_the_request_it_answers() {
    let received = recorded("switch pdelay response");
    let message = Message::decode(&received).expect("response");
    assert!(message.message_type().event());
    assert!(message.header.flags.two_step());
    assert_eq!(
        message.body,
        Body::PdelayResp {
            request_receipt: Timestamp {
                seconds: 0x03_ec7a,
                nanoseconds: 0x356f_d369
            },
            requesting: PortIdentity {
                clock: ClockIdentity(0x8c1f_64ff_fe36_c102),
                port: 4
            },
        }
    );
}

#[test]
fn an_endpoints_own_tlv_on_its_pdelay_request_is_kept() {
    let received = recorded("endpoint pdelay request");
    let message = Message::decode(&received).expect("request");
    let tlvs: Vec<_> = message.tlvs().collect();
    assert_eq!(
        tlvs,
        [Tlv::Other {
            tlv_type: 0x8000,
            value: &[0x8c, 0x1f, 0x64, 0x36, 0xc0, 0x01, 0x01, 0x00],
        }]
    );
}

#[test]
fn a_message_interval_request_reads_as_its_three_intervals() {
    let request = IntervalRequest {
        tlv_type: ORGANIZATION_EXTENSION,
        link_delay: 0,
        time_sync: -3,
        announce: 127,
        flags: 0b11,
        reserved: [0; 2],
    };
    let mut tlv = [0; 16];
    let length = Tlv::IntervalRequest(request)
        .encode(&mut tlv)
        .expect("encodes");
    assert_eq!(&tlv[..length], octets("0003000c0080c200000200fd7f030000"));
    let signaling = recorded("endpoint signaling");
    let message = Message {
        tlvs: &tlv[..length],
        ..Message::decode(&signaling).expect("signaling")
    };
    let sent = encoded(&message);
    let heard = Message::decode(&sent).expect("decodes");
    assert_eq!(
        heard.tlvs().collect::<Vec<_>>(),
        [Tlv::IntervalRequest(request)]
    );
}

#[test]
fn a_buffer_too_small_is_an_error() {
    let received = recorded("switch announce");
    let message = Message::decode(&received).expect("announce");
    let mut out = [0; 40];
    assert_eq!(
        message.encode(&mut out),
        Err(EncodeError::BufferTooSmall {
            needed: received.len(),
            available: 40
        })
    );
}

#[test]
fn timestamps_count_nanoseconds_from_the_epoch() {
    let time = Timestamp {
        seconds: 0x03_ec7a,
        nanoseconds: 999_999_999,
    };
    assert_eq!(Timestamp::from_nanos(time.nanos()), Some(time));
    assert_eq!(
        Timestamp::from_nanos(time.nanos() + 1),
        Some(Timestamp {
            seconds: 0x03_ec7b,
            nanoseconds: 0
        })
    );
    assert_eq!(Timestamp::from_nanos(-1), None);
    assert_eq!(
        Timestamp::from_nanos(i128::from(1u64 << 48) * 1_000_000_000),
        None
    );
}

#[test]
fn what_is_not_a_gptp_message_says_why() {
    let sync = recorded("switch sync");
    assert_eq!(
        Message::decode(&sync[..20]),
        Err(DecodeError::Truncated {
            needed: 44,
            available: 20
        })
    );
    let mut version_one = sync.clone();
    version_one[1] = 0x01;
    assert_eq!(
        Message::decode(&version_one),
        Err(DecodeError::UnsupportedVersion(1))
    );
    let mut delay_request = sync.clone();
    delay_request[0] = 0x11;
    assert_eq!(
        Message::decode(&delay_request),
        Err(DecodeError::WrongMessageType(1))
    );
    let mut no_room = sync.clone();
    no_room[3] = 40;
    assert_eq!(
        Message::decode(&no_room),
        Err(DecodeError::Truncated {
            needed: 44,
            available: 40
        })
    );
    let mut long_tlv = recorded("endpoint pdelay request");
    let at = long_tlv.len() - 9;
    long_tlv[at] = 0x40;
    assert_eq!(Message::decode(&long_tlv), Err(DecodeError::BadTlv));
}

#[test]
fn no_cut_or_corrupted_message_panics() {
    for (_, hex) in RECORDED {
        let received = octets(hex);
        for end in 0..received.len() {
            let _ = Message::decode(&received[..end]);
        }
        for at in 0..received.len() {
            for value in [0x00, 0x01, 0x7f, 0x80, 0xff] {
                let mut corrupted = received.clone();
                corrupted[at] = value;
                if let Ok(message) = Message::decode(&corrupted) {
                    let _ = message.tlvs().count();
                    let _ = encoded(&message);
                }
            }
        }
    }
}
