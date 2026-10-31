use alloc::string::String;
use alloc::vec;

use super::*;

fn status() -> Status {
    Status {
        clock: ClockIdentity(0x8c1f_64ff_fe36_c102),
        domain: 0,
        priority1: 248,
        profile: Profile::Gptp,
        fallback: Fallback::None,
        grandmaster: Some(Grandmaster {
            identity: ClockIdentity(0x000a_92ff_fe02_6e05),
            priority1: 246,
            clock_class: 248,
            clock_accuracy: 0xfe,
            offset_scaled_log_variance: 0x436a,
            priority2: 248,
            steps_removed: 2,
            is_us: false,
        }),
        path: vec![
            ClockIdentity(0x000a_92ff_fe02_6e05),
            ClockIdentity(0x8c1f_64ff_fe36_c100),
            ClockIdentity(0x8c1f_64ff_fe36_c102),
        ],
        generation: 6,
        residual: Some(-17),
        ports: vec![Port {
            name: "enp2s0.2".into(),
            up: true,
            as_capable: true,
            role: Role::TimeReceiver,
            link_delay: Some(487),
            neighbor: Some(PortIdentity {
                clock: ClockIdentity(0x8c1f_64ff_fe36_c100),
                port: 4,
            }),
            neighbor_rate_ratio: Some(1.000_008_416),
        }],
        counters: Counters {
            grandmaster_changes: 2,
            link_ups: 1,
            link_downs: 0,
            sync_timeouts: 3,
        },
    }
}

fn encoded(status: &Status) -> String {
    let mut text = String::new();
    status.encode(&mut text).expect("encode");
    text
}

#[test]
fn a_snapshot_parses_as_it_was_written() {
    let text = encoded(&status());
    assert!(text.starts_with("version 1\nclock 8c1f64.fffe.36c102\n"));
    assert!(text.ends_with("\n\n"));
    assert!(text.contains("port.role enp2s0.2 timereceiver\n"));
    assert_eq!(Block::parse(&text), Ok(Block::Status(status())));
}

#[test]
fn a_snapshot_without_a_grandmaster_or_ports_parses() {
    let alone = Status {
        grandmaster: None,
        path: vec![],
        residual: None,
        ports: vec![],
        profile: Profile::AvbLite,
        fallback: Fallback::PdelayUnanswered,
        ..status()
    };
    let text = encoded(&alone);
    assert!(!text.contains("\ngrandmaster"));
    assert_eq!(Block::parse(&text), Ok(Block::Status(alone)));
}

#[test]
fn unknown_keys_and_ports_are_ignored() {
    let mut text = encoded(&status());
    text.insert_str(text.len() - 1, "future.key 1 2 3\nport.up enp9s0 false\n");
    assert_eq!(Block::parse(&text), Ok(Block::Status(status())));
    assert!(Block::parse("version 1\ndomain 0\n\n").is_err());
    assert!(Block::parse("clock 8c1f64.fffe.36c102\npriority1 x\n\n").is_err());
}

#[test]
fn replies_parse_as_they_were_written() {
    let mut ok = String::new();
    Block::encode_reply(&mut ok, Ok(())).expect("encode");
    assert_eq!(ok, "reply ok\n\n");
    assert_eq!(Block::parse(&ok), Ok(Block::Reply(Ok(()))));
    let mut refused = String::new();
    Block::encode_reply(&mut refused, Err("not permitted\nfor uid 1000")).expect("encode");
    assert_eq!(refused, "reply error not permitted for uid 1000\n\n");
    assert_eq!(
        Block::parse(&refused),
        Ok(Block::Reply(Err("not permitted for uid 1000".into())))
    );
}

#[test]
fn commands_parse_as_they_are_shown() {
    assert_eq!(
        Command::parse("priority1 248\n"),
        Ok(Command::Priority1(248))
    );
    assert_eq!(Command::Priority1(255).to_string(), "priority1 255");
    for wrong in ["priority1 0", "priority1 256", "priority1", "reboot now"] {
        assert!(Command::parse(wrong).is_err(), "{wrong}");
    }
}

#[cfg(all(feature = "std", unix))]
mod client {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use std::{format, fs, thread};

    use super::super::{Client, path};
    use super::*;

    #[test]
    fn the_client_reads_snapshots_and_replies_and_sends_commands() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let socket = std::env::temp_dir().join(format!(
            "gptp-status-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = UnixListener::bind(&socket).expect("bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Written in two pieces, the second ending the snapshot.
            let text = encoded(&status());
            let (first, second) = text.split_at(20);
            stream.write_all(first.as_bytes()).expect("write");
            thread::sleep(Duration::from_millis(50));
            stream.write_all(second.as_bytes()).expect("write");
            let mut command = String::new();
            BufReader::new(&stream)
                .read_line(&mut command)
                .expect("read");
            let mut reply = String::new();
            match Command::parse(&command) {
                Ok(Command::Priority1(255)) => Block::encode_reply(&mut reply, Ok(())),
                _ => Block::encode_reply(&mut reply, Err("unexpected")),
            }
            .expect("encode");
            stream.write_all(reply.as_bytes()).expect("write");
            thread::sleep(Duration::from_millis(200));
        });
        let mut client = Client::connect_at(&socket).expect("connect");
        assert_eq!(
            client.next(Some(Duration::from_secs(5))).expect("next"),
            Some(Block::Status(status()))
        );
        assert_eq!(
            client.next(Some(Duration::from_millis(20))).expect("next"),
            None
        );
        client.set_priority1(255).expect("send");
        assert_eq!(
            client.next(Some(Duration::from_secs(5))).expect("next"),
            Some(Block::Reply(Ok(())))
        );
        server.join().expect("server");
        assert!(client.next(Some(Duration::from_secs(5))).is_err());
        fs::remove_file(&socket).expect("remove");
    }

    #[test]
    fn the_socket_is_beside_the_time_file() {
        assert_eq!(
            path("enp2s0").expect("path"),
            std::path::PathBuf::from("/run/gptpd/enp2s0.sock")
        );
        assert!(path("a/b").is_err());
    }
}
