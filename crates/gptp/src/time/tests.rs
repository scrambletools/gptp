use super::*;

fn mapping() -> Mapping {
    Mapping {
        generation: 7,
        state: State::Following,
        grandmaster: ClockIdentity(0x000a_92ff_fe02_6e05),
        updated: 5_000_000_000,
        monotonic: 4_900_000_000,
        at_monotonic: 1_793_426_562_742_980_346,
        monotonic_rate: 1.0 + 9.3e-6,
        local: 378_780_622_685_984,
        at_local: 1_793_426_562_700_000_000,
        local_rate: 1.0 - 1.4e-6,
        asymmetry_included: true,
        residual: Some(-42),
        tai: Some(-1_234),
    }
}

#[test]
fn a_record_decodes_as_it_was_encoded() {
    let (decoded, sequence) = Mapping::decode(&mapping().encode(3)).expect("record");
    assert_eq!(decoded, mapping());
    assert_eq!(sequence, 3);
    let none = Mapping {
        state: State::None,
        residual: None,
        tai: None,
        asymmetry_included: false,
        ..mapping()
    };
    assert_eq!(Mapping::decode(&none.encode(4)).expect("record").0, none);
}

#[test]
fn a_record_written_while_read_fails_its_crc() {
    let mut record = mapping().encode(1);
    record[50] ^= 0x10;
    assert_eq!(Mapping::decode(&record), Err(RecordError::Checksum));
    assert_eq!(Mapping::decode(&record[..60]), Err(RecordError::Short));
}

#[test]
fn other_records_are_refused() {
    let record = mapping().encode(1);
    let mut magic = record;
    magic[0] = b'X';
    assert_eq!(Mapping::decode(&magic), Err(RecordError::Magic));
    let mut version = record;
    version[4] = 2;
    assert_eq!(Mapping::decode(&version), Err(RecordError::Version(2)));
    let mut length = record;
    length[6] = 0;
    assert_eq!(Mapping::decode(&length), Err(RecordError::Length(0x0000)));
}

#[test]
fn the_crc_is_ethernets() {
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
}

#[test]
fn the_grandmasters_time_follows_its_rate() {
    let mapping = mapping();
    let second = 1_000_000_000;
    assert_eq!(
        mapping.grandmaster_at(mapping.monotonic),
        mapping.at_monotonic
    );
    // 9.3 ppm faster: 9300 ns more in a second, 33.48 ms more in an hour.
    assert_eq!(
        mapping.grandmaster_at(mapping.monotonic + second),
        mapping.at_monotonic + second as i128 + 9300
    );
    let hour = 3600 * second;
    assert_eq!(
        mapping.grandmaster_at(mapping.monotonic + hour),
        mapping.at_monotonic + i128::from(hour) + 33_480_000
    );
    assert_eq!(
        mapping.grandmaster_at(mapping.monotonic - second),
        mapping.at_monotonic - second as i128 - 9300
    );
}

#[test]
fn each_clock_and_the_grandmasters_time_map_back() {
    let mapping = mapping();
    for elapsed in [
        0,
        1,
        125_000_000,
        1_000_000_000,
        3_600_000_000_000,
        -2_000_000_000,
    ] {
        let monotonic = mapping.monotonic + elapsed;
        let back = mapping.monotonic_at(mapping.grandmaster_at(monotonic));
        assert!(
            (back - monotonic).abs() <= 1,
            "{elapsed}: {back} {monotonic}"
        );
        let local = mapping.local + i128::from(elapsed);
        let back = mapping.local_at(mapping.grandmaster_at_local(local));
        assert!((back - local).abs() <= 1, "{elapsed}: {back} {local}");
    }
}

#[test]
fn a_mapping_older_than_a_second_is_stale() {
    let mapping = mapping();
    assert!(!mapping.stale(mapping.updated));
    assert!(!mapping.stale(mapping.updated + STALE_NANOS));
    assert!(mapping.stale(mapping.updated + STALE_NANOS + 1));
}

#[cfg(all(feature = "std", unix))]
mod reader {
    use std::format;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::{Reader, path};
    use super::*;

    fn scratch() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "gptp-time-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn the_reader_reads_whats_written_and_nothing_stale() {
        let file = scratch();
        let mut reader = Reader::at(file.clone());
        assert_eq!(reader.read(0).expect("read"), None);
        let mapping = mapping();
        fs::write(&file, mapping.encode(1)).expect("write");
        assert_eq!(reader.read(mapping.updated).expect("read"), Some(mapping));
        assert_eq!(
            reader
                .read(mapping.updated + 2 * STALE_NANOS)
                .expect("read"),
            None
        );
        // A restarted gptpd's new file is read, the reader having let the
        // stale one go.
        let newer = Mapping {
            generation: 8,
            updated: mapping.updated + 3 * STALE_NANOS,
            ..mapping
        };
        let replacement = file.with_extension("new");
        fs::write(&replacement, newer.encode(1)).expect("write");
        fs::rename(&replacement, &file).expect("rename");
        assert_eq!(reader.read(newer.updated).expect("read"), Some(newer));
        fs::remove_file(&file).expect("remove");
    }

    #[test]
    fn a_torn_record_reads_as_none_and_others_as_errors() {
        let file = scratch();
        let mut torn = mapping().encode(1);
        torn[70] ^= 1;
        fs::write(&file, torn).expect("write");
        let mut reader = Reader::at(file.clone());
        assert_eq!(reader.read(mapping().updated).expect("read"), None);
        let mut other = mapping().encode(1);
        other[0] = b'X';
        fs::write(&file, other).expect("write");
        assert!(reader.read(mapping().updated).is_err());
        fs::remove_file(&file).expect("remove");
    }

    #[test]
    fn only_interface_names_make_paths() {
        assert_eq!(
            path("enp2s0").expect("path"),
            PathBuf::from("/run/gptpd/enp2s0.time")
        );
        for wrong in ["", "../etc/passwd", "a b", "sixteen-letters!"] {
            assert!(path(wrong).is_err(), "{wrong}");
        }
    }
}

#[test]
fn a_reader_of_the_first_records_reads_one_that_says_how_far_tai_stands() {
    // The TAI offset takes octets that were padding: the version, length
    // and every other field stay where they were.
    let record = mapping().encode(5);
    assert_eq!(u16::from_le_bytes([record[4], record[5]]), 1);
    assert_eq!(record.len(), 120);
    let without = Mapping {
        tai: None,
        ..mapping()
    }
    .encode(5);
    assert_eq!(record[20..116], without[20..116]);
}
