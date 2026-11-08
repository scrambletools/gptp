use super::*;

const NL80211: u16 = 0x1c;
const INTERFACE: u32 = 7;
const AP: MacAddress = MacAddress([0x28, 0x0c, 0x50, 0x6b, 0x33, 0x70]);
const STATION: MacAddress = MacAddress([0x8c, 0x1f, 0x64, 0x36, 0xc1, 0x50]);

/// Attributes laid out as netlink nests them.
fn nest(attributes: &[(u16, &[u8])]) -> Vec<u8> {
    request(0, 0, 0, attributes)[NETLINK_HEADER + GENERIC_HEADER..].to_vec()
}

#[test]
fn a_request_is_laid_out_as_generic_netlink_asks() {
    let message = request(
        NL80211,
        CMD_REGISTER_FRAME,
        9,
        &[
            (ATTR_IFINDEX, &INTERFACE.to_ne_bytes()),
            (ATTR_FRAME_MATCH, &[11, 1, 0]),
        ],
    );
    // Header, command, then each attribute padded to four octets.
    assert_eq!(message.len(), 16 + 4 + 8 + 8);
    assert_eq!(u32_at(&message, 0), Some(36));
    assert_eq!(u16_at(&message, 4), Some(NL80211));
    assert_eq!(
        u16_at(&message, 6),
        Some((libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16)
    );
    assert_eq!(sequence_of(&message), Some(9));
    let (command, octets) = generic(&message).unwrap();
    assert_eq!(command, CMD_REGISTER_FRAME);
    let found: Vec<(u16, Vec<u8>)> = attributes(octets)
        .map(|(kind, payload)| (kind, payload.to_vec()))
        .collect();
    assert_eq!(
        found,
        [
            (ATTR_IFINDEX, INTERFACE.to_ne_bytes().to_vec()),
            (ATTR_FRAME_MATCH, vec![11, 1, 0]),
        ]
    );
}

#[test]
fn one_read_splits_into_its_messages() {
    let first = request(NL80211, CMD_FRAME, 1, &[(ATTR_COOKIE, &5u64.to_ne_bytes())]);
    let second = request(NL80211, CMD_FRAME, 2, &[]);
    let buffer = [first.clone(), second.clone()].concat();
    let split: Vec<&[u8]> = messages(&buffer).collect();
    assert_eq!(split, [&first[..], &second[..]]);
    // A length running past the read ends the list.
    let mut cut = first.clone();
    cut.truncate(20);
    assert_eq!(messages(&cut).count(), 0);
}

#[test]
fn an_acknowledgement_and_an_error_are_told_apart() {
    let mut ack = vec![0; 36];
    ack[..4].copy_from_slice(&36u32.to_ne_bytes());
    ack[4..6].copy_from_slice(&(libc::NLMSG_ERROR as u16).to_ne_bytes());
    assert_eq!(error_of(&ack), Some(0));
    let mut refused = ack.clone();
    refused[16..20].copy_from_slice(&(-libc::EALREADY).to_ne_bytes());
    assert_eq!(error_of(&refused), Some(-libc::EALREADY));
    assert_eq!(error_of(&request(NL80211, CMD_FRAME, 1, &[])), None);
}

#[test]
fn the_family_and_its_mlme_group_come_from_the_controller() {
    let config = nest(&[
        (CTRL_ATTR_MCAST_GRP_NAME, b"config\0"),
        (CTRL_ATTR_MCAST_GRP_ID, &5u32.to_ne_bytes()),
    ]);
    let mlme = nest(&[
        (CTRL_ATTR_MCAST_GRP_NAME, b"mlme\0"),
        (CTRL_ATTR_MCAST_GRP_ID, &7u32.to_ne_bytes()),
    ]);
    let groups = nest(&[(1, &config), (2, &mlme)]);
    let answer = request(
        GENL_ID_CTRL,
        1,
        3,
        &[
            (CTRL_ATTR_FAMILY_ID, &NL80211.to_ne_bytes()),
            (CTRL_ATTR_FAMILY_NAME, b"nl80211\0"),
            (CTRL_ATTR_MCAST_GROUPS, &groups),
        ],
    );
    assert_eq!(family_of(&answer, "mlme"), Some((NL80211, 7)));
    assert_eq!(family_of(&answer, "scan"), None);
}

#[test]
fn an_action_frame_is_a_management_frame_of_subtype_13() {
    let frame = action_frame(STATION, AP, AP, &[11, 1, 4, 3]);
    assert_eq!(frame[..2], [0xd0, 0x00]);
    assert_eq!(frame[4..10], STATION.0);
    assert_eq!(frame[10..16], AP.0);
    assert_eq!(frame[16..22], AP.0);
    assert_eq!(frame[24..], [11, 1, 4, 3]);
}

#[test]
fn a_received_frame_comes_with_its_arrival_and_its_acks_departure() {
    let frame = action_frame(AP, STATION, AP, &[11, 1, 2, 1]);
    let event = request(
        NL80211,
        CMD_FRAME,
        0,
        &[
            (ATTR_IFINDEX, &INTERFACE.to_ne_bytes()),
            (ATTR_FRAME, &frame),
            (ATTR_RX_HW_TIMESTAMP, &1_000_000_040u64.to_ne_bytes()),
            (ATTR_TX_HW_TIMESTAMP, &1_000_016_040u64.to_ne_bytes()),
        ],
    );
    assert_eq!(
        event_of(&event, NL80211, INTERFACE),
        Some(Event::Frame {
            source: STATION,
            action: vec![11, 1, 2, 1],
            received: Some(1_000_000_040),
            ack_sent: Some(1_000_016_040),
        })
    );
    // Another interface's, another family's, or not an Action frame.
    assert_eq!(event_of(&event, NL80211, INTERFACE + 1), None);
    assert_eq!(event_of(&event, NL80211 + 1, INTERFACE), None);
    let mut beacon = frame.clone();
    beacon[0] = 0x80;
    let other = request(
        NL80211,
        CMD_FRAME,
        0,
        &[
            (ATTR_IFINDEX, &INTERFACE.to_ne_bytes()),
            (ATTR_FRAME, &beacon),
        ],
    );
    assert_eq!(event_of(&other, NL80211, INTERFACE), None);
}

#[test]
fn a_frames_status_tells_whether_its_ack_came_and_when() {
    let frame = action_frame(STATION, AP, AP, &[11, 1, 2, 1]);
    let acked = request(
        NL80211,
        CMD_FRAME_TX_STATUS,
        0,
        &[
            (ATTR_IFINDEX, &INTERFACE.to_ne_bytes()),
            (ATTR_FRAME, &frame),
            (ATTR_COOKIE, &77u64.to_ne_bytes()),
            (ATTR_ACK, &[]),
            (ATTR_TX_HW_TIMESTAMP, &2_000_000_000u64.to_ne_bytes()),
            (ATTR_RX_HW_TIMESTAMP, &2_000_016_080u64.to_ne_bytes()),
        ],
    );
    assert_eq!(
        event_of(&acked, NL80211, INTERFACE),
        Some(Event::TxStatus {
            cookie: 77,
            acked: true,
            sent: Some(2_000_000_000),
            ack_received: Some(2_000_016_080),
        })
    );
    let lost = request(
        NL80211,
        CMD_FRAME_TX_STATUS,
        0,
        &[
            (ATTR_IFINDEX, &INTERFACE.to_ne_bytes()),
            (ATTR_COOKIE, &78u64.to_ne_bytes()),
        ],
    );
    assert_eq!(
        event_of(&lost, NL80211, INTERFACE),
        Some(Event::TxStatus {
            cookie: 78,
            acked: false,
            sent: None,
            ack_received: None,
        })
    );
}

#[test]
fn stations_joining_and_leaving_are_told() {
    for (command, joined) in [(CMD_NEW_STATION, true), (CMD_DEL_STATION, false)] {
        let event = request(
            NL80211,
            command,
            0,
            &[
                (ATTR_IFINDEX, &INTERFACE.to_ne_bytes()),
                (ATTR_MAC, &STATION.0),
            ],
        );
        assert_eq!(
            event_of(&event, NL80211, INTERFACE),
            Some(Event::Station {
                peer: STATION,
                joined
            })
        );
    }
}

#[test]
fn an_interface_and_its_stations_are_read_from_the_answers() {
    let access_point = request(
        NL80211,
        CMD_GET_INTERFACE,
        0,
        &[
            (ATTR_IFINDEX, &INTERFACE.to_ne_bytes()),
            (ATTR_IFTYPE, &IFTYPE_AP.to_ne_bytes()),
        ],
    );
    assert!(is_access_point(&access_point));
    let managed = request(
        NL80211,
        CMD_GET_INTERFACE,
        0,
        &[
            (ATTR_IFINDEX, &INTERFACE.to_ne_bytes()),
            (ATTR_IFTYPE, &2u32.to_ne_bytes()),
        ],
    );
    assert!(!is_access_point(&managed));
    let station = request(
        NL80211,
        CMD_GET_STATION,
        0,
        &[
            (ATTR_IFINDEX, &INTERFACE.to_ne_bytes()),
            (ATTR_MAC, &STATION.0),
        ],
    );
    assert_eq!(station_in(&station), Some(STATION));
    assert_eq!(station_in(&managed), None);
}
