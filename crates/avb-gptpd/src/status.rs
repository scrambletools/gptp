//! How gPTP stands, written to a status file to read with `cat`, and for
//! avb-bridged, which takes each port's asCapable from it: the first line
//! names this process, and under `ports` a port's line has `up,
//! asCapable` or `up, not asCapable` after its name, or `down`.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use avb_gptp::relay::Role;

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
