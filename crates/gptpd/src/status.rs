//! How gPTP stands, written to a status file to read with `cat`, and for
//! avb-bridged, which takes each port's asCapable from it: the first line
//! names this process, and under `ports` a port's line has `up,
//! asCapable` or `up, not asCapable` after its name, or `down`; an
//! 802.11 port's names its station, which a beacon carrier leaves the
//! port's Announce to.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gptp::relay::Role;

use crate::{Daemon, number};

pub fn role(role: Role) -> &'static str {
    match role {
        Role::Disabled => "disabled",
        Role::TimeReceiver => "timereceiver port",
        Role::TimeTransmitter => "timetransmitter port",
        Role::Passive => "passive port",
    }
}

/// Nanoseconds as seconds, to the nanosecond.
fn seconds(nanoseconds: i128) -> String {
    let sign = if nanoseconds < 0 { "-" } else { "" };
    let size = nanoseconds.unsigned_abs();
    format!("{sign}{}.{:09}", size / 1_000_000_000, size % 1_000_000_000)
}

fn ppm(rate: f64) -> String {
    format!("{:+.3} ppm", rate * 1e6)
}

pub(crate) fn text(daemon: &Daemon) -> String {
    let relay = &daemon.relay;
    let mut text = format!(
        "time-aware relay, process {}, as clock {}\nLocalClock: {}'s hardware clock\n",
        std::process::id(),
        daemon.identity,
        daemon.clocks.local_name()
    );
    match relay.grandmaster() {
        Some(grandmaster) => {
            let _ = writeln!(
                text,
                "grandmaster {}, priority1 {}, clock class {}",
                grandmaster.clock, grandmaster.priority1, grandmaster.clock_class
            );
            if let Some(rate) = relay.rate_ratio() {
                let _ = writeln!(text, "  its rate {} from the LocalClock's", ppm(rate - 1.0));
            }
            if let Ok(local) = daemon.clocks.now()
                && let Some(time) = relay.grandmaster_time(local)
            {
                let _ = writeln!(
                    text,
                    "  its time {} at LocalClock {}",
                    seconds(time),
                    seconds(local)
                );
            }
        }
        None => text.push_str("no grandmaster\n"),
    }
    text.push_str("\nports\n");
    for (index, port) in daemon.ports.iter().enumerate() {
        let number = number(index);
        let _ = write!(text, "  {:<10}", port.name);
        if !port.up {
            text.push_str(" down\n");
            continue;
        }
        let _ = write!(
            text,
            " up, {}, {}",
            if relay.as_capable(number) {
                "asCapable"
            } else {
                "not asCapable"
            },
            role(relay.role(number).unwrap_or(Role::Disabled))
        );
        if let Some(station) = port.wifi.as_ref().and_then(|wifi| wifi.peer()) {
            let _ = write!(text, ", station {station}");
        }
        if let Some(link) = relay.link(number) {
            let _ = write!(
                text,
                ", link delay {:.0} ns, neighbor {} at {}",
                link.mean_link_delay,
                link.neighbor,
                ppm(link.neighbor_rate_ratio - 1.0)
            );
        }
        if let Some(rate) = daemon.clocks.rate(index) {
            let _ = write!(text, ", its clock {} from the LocalClock", ppm(rate));
        }
        text.push('\n');
    }
    text
}

/// An end station's state, as clients read it from its socket.
pub(crate) fn snapshot(daemon: &Daemon) -> gptp::status::Status {
    use gptp::status::{Counters, Fallback, Grandmaster, Port, Profile, Status};
    let relay = &daemon.relay;
    let end = daemon.end_station.as_ref();
    let grandmaster = relay.grandmaster().map(|grandmaster| Grandmaster {
        identity: grandmaster.clock,
        priority1: grandmaster.priority1,
        clock_class: grandmaster.clock_class,
        clock_accuracy: grandmaster.clock_accuracy,
        offset_scaled_log_variance: grandmaster.offset_scaled_log_variance,
        priority2: grandmaster.priority2,
        steps_removed: relay.steps_removed(),
        is_us: grandmaster.clock == daemon.identity,
    });
    let ports = daemon
        .ports
        .iter()
        .enumerate()
        .map(|(index, port)| {
            let number = number(index);
            let link = relay.link(number);
            Port {
                name: port.name.clone(),
                up: port.up,
                as_capable: relay.as_capable(number),
                role: relay.role(number).unwrap_or(Role::Disabled),
                link_delay: link.map(|link| link.mean_link_delay.round() as i64),
                neighbor: link.map(|link| link.neighbor),
                neighbor_rate_ratio: link.map(|link| link.neighbor_rate_ratio),
            }
        })
        .collect();
    let lite = end.map(|end| &end.lite);
    let fallback = match lite.and_then(crate::lite::Lite::reason) {
        None => Fallback::None,
        Some(gptp::lite::Reason::EndpointTlv) => Fallback::EndpointTlv,
        Some(gptp::lite::Reason::PdelayUnanswered) => Fallback::PdelayUnanswered,
        Some(gptp::lite::Reason::MultipleResponders) => Fallback::MultipleResponders,
        Some(gptp::lite::Reason::Configured) => Fallback::Configured,
    };
    if let Some(lite) = lite.filter(|lite| lite.active()) {
        return lite_snapshot(daemon, lite, fallback);
    }
    Status {
        clock: daemon.identity,
        domain: 0,
        priority1: relay.identity().priority1,
        profile: Profile::Gptp,
        fallback,
        grandmaster,
        path: relay.path_trace(),
        generation: end.map_or(0, |end| end.station.generation()),
        residual: end.and_then(|end| end.station.residual()),
        ports,
        counters: Counters {
            grandmaster_changes: daemon.counts.grandmaster_changes,
            link_ups: daemon.counts.link_ups,
            link_downs: daemon.counts.link_downs,
            sync_timeouts: daemon.counts.sync_timeouts,
        },
    }
}

/// An end station's state while it runs the AVB Lite profile: the
/// grandmaster its end-to-end client follows, and the path's delay.
fn lite_snapshot(
    daemon: &Daemon,
    lite: &crate::lite::Lite,
    fallback: gptp::status::Fallback,
) -> gptp::status::Status {
    use gptp::status::{Counters, Grandmaster, Port, Profile, Status};
    let client = &lite.client;
    let grandmaster = client.grandmaster().map(|grandmaster| Grandmaster {
        identity: grandmaster.clock,
        priority1: grandmaster.priority1,
        clock_class: grandmaster.clock_class,
        clock_accuracy: grandmaster.clock_accuracy,
        offset_scaled_log_variance: grandmaster.offset_scaled_log_variance,
        priority2: grandmaster.priority2,
        steps_removed: 1,
        is_us: false,
    });
    let end = daemon.end_station.as_ref();
    let port = &daemon.ports[0];
    Status {
        clock: daemon.identity,
        domain: 0,
        priority1: 255,
        profile: Profile::AvbLite,
        fallback,
        grandmaster,
        path: grandmaster
            .map(|grandmaster| vec![grandmaster.identity, daemon.identity])
            .unwrap_or_else(|| vec![daemon.identity]),
        generation: end.map_or(0, |end| end.station.generation()),
        residual: end.and_then(|end| end.station.residual()),
        ports: vec![Port {
            name: port.name.clone(),
            up: port.up,
            // AVB Lite has no asCapable: the path is measured end to end.
            as_capable: false,
            role: if grandmaster.is_some() {
                Role::TimeReceiver
            } else {
                Role::Disabled
            },
            link_delay: client.mean_path_delay().map(|delay| delay.round() as i64),
            neighbor: None,
            neighbor_rate_ratio: None,
        }],
        counters: Counters {
            grandmaster_changes: daemon.counts.grandmaster_changes,
            link_ups: daemon.counts.link_ups,
            link_downs: daemon.counts.link_downs,
            sync_timeouts: daemon.counts.sync_timeouts,
        },
    }
}

/// Writes `text` to `path` whole, through a file beside it.
pub fn write(path: &Path, text: &str) -> io::Result<()> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }
    let mut partial = PathBuf::from(path);
    partial.set_extension("new");
    fs::write(&partial, text)?;
    fs::rename(&partial, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_show_in_seconds_to_the_nanosecond() {
        assert_eq!(seconds(1_793_300_000_000_000_005), "1793300000.000000005");
        assert_eq!(seconds(-1), "-0.000000001");
    }
}
