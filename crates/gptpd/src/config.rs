//! An end station's settings, from `/etc/gptpd/<interface>.conf`, and
//! the check before it starts that no other PTP daemon holds the
//! interface: two answering one Pdelay_Req, even once, make a strict
//! neighbor fall back to plain PTP.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// priority1 where the file says nothing: grandmaster capable, as Milan
/// asks of an endpoint.
pub const PRIORITY1: u8 = 248;

/// syncReceiptTimeout where the file says nothing: 802.1AS's 3.
pub const SYNC_RECEIPT_TIMEOUT: u8 = 3;

/// Which PTP profile an end station runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// gPTP, falling back to AVB Lite where the Pdelay exchange says no
    /// AVB bridge is the neighbor (AVB Lite profile 2.2).
    #[default]
    Auto,
    /// gPTP only.
    Gptp,
    /// AVB Lite, as an operator asked.
    AvbLite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// 255 for never grandmaster.
    pub priority1: u8,
    pub profile: Profile,
    /// Whether the port says each second that it runs gPTP (802.1AS-2020
    /// 10.6.4.4).
    pub gptp_capable: bool,
    /// Whether the port's hardware clock is kept near CLOCK_TAI, for cards
    /// that send frames at launch times on it (steer.rs).
    pub follow_tai: bool,
    /// How many Sync intervals with no Sync from the grandmaster before
    /// the port lets it go (802.1AS-2020 10.7.3.1, syncReceiptTimeout).
    pub sync_receipt_timeout: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            priority1: PRIORITY1,
            profile: Profile::Auto,
            gptp_capable: true,
            follow_tai: false,
            sync_receipt_timeout: SYNC_RECEIPT_TIMEOUT,
        }
    }
}

pub fn path(interface: &str) -> PathBuf {
    PathBuf::from(format!("/etc/gptpd/{interface}.conf"))
}

/// Where `--check` leaves the reason it refused, for the applications to
/// show.
pub fn refused_path(interface: &str) -> PathBuf {
    PathBuf::from(format!("/run/gptpd/{interface}.refused"))
}

/// The settings in `text`: `key value` lines, `#` starting a comment.
pub fn parse(text: &str) -> Result<Settings, String> {
    let mut settings = Settings::default();
    for (number, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let value = value.trim();
        match key {
            "priority1" => {
                settings.priority1 = value
                    .parse()
                    .ok()
                    .filter(|priority1| *priority1 > 0)
                    .ok_or_else(|| {
                        format!(
                            "line {}: priority1 takes 1 to 255, not {value:?}",
                            number + 1
                        )
                    })?;
            }
            "profile" => {
                settings.profile = match value {
                    "auto" => Profile::Auto,
                    "gptp" => Profile::Gptp,
                    "avb-lite" => Profile::AvbLite,
                    _ => {
                        return Err(format!(
                            "line {}: profile takes auto, gptp or avb-lite, not {value:?}",
                            number + 1
                        ));
                    }
                };
            }
            "gptp-capable" => {
                settings.gptp_capable = match value {
                    "yes" => true,
                    "no" => false,
                    _ => {
                        return Err(format!(
                            "line {}: gptp-capable takes yes or no, not {value:?}",
                            number + 1
                        ));
                    }
                };
            }
            "follow-tai" => {
                settings.follow_tai = match value {
                    "yes" => true,
                    "no" => false,
                    _ => {
                        return Err(format!(
                            "line {}: follow-tai takes yes or no, not {value:?}",
                            number + 1
                        ));
                    }
                };
            }
            "sync-receipt-timeout" => {
                settings.sync_receipt_timeout = value
                    .parse()
                    .ok()
                    .filter(|intervals| *intervals > 0)
                    .ok_or_else(|| {
                        format!(
                            "line {}: sync-receipt-timeout takes 1 to 255, not {value:?}",
                            number + 1
                        )
                    })?;
            }
            _ => return Err(format!("line {}: unknown setting {key}", number + 1)),
        }
    }
    Ok(settings)
}

/// The settings at `path`, or the defaults where there is no file.
pub fn load(path: &Path) -> Result<Settings, String> {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|error| format!("{}: {error}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// `text` with its priority1 set to `priority1`, other lines kept.
pub fn with_priority1(text: &str, priority1: u8) -> String {
    let mut found = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            let setting = line.split('#').next().unwrap_or("").trim();
            if setting.split_whitespace().next() == Some("priority1") {
                found = true;
                format!("priority1 {priority1}")
            } else {
                line.to_owned()
            }
        })
        .collect();
    if !found {
        lines.push(format!("priority1 {priority1}"));
    }
    lines.join("\n") + "\n"
}

/// Stores `priority1` in the file at `path`, whole through a file beside
/// it.
pub fn save_priority1(path: &Path, priority1: u8) -> io::Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }
    let partial = path.with_extension("conf.new");
    fs::write(&partial, with_priority1(&text, priority1))?;
    fs::rename(&partial, path)
}

/// Another PTP daemon running on `interface`: a ptp4l given it with `-i`
/// or in its configuration file, as triib's and plaa's units and one run
/// by hand do.
pub fn holder(interface: &str) -> Option<String> {
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let pid = entry.file_name();
        let pid = pid.to_string_lossy();
        if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(command) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let arguments: Vec<String> = command
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(|argument| String::from_utf8_lossy(argument).into_owned())
            .collect();
        if runs_on(&arguments, interface, |file| fs::read_to_string(file).ok()) {
            return Some(format!("ptp4l (process {pid}) runs on {interface}"));
        }
    }
    None
}

/// Whether the command `arguments` runs ptp4l on `interface`, reading its
/// configuration file with `read`.
fn runs_on(arguments: &[String], interface: &str, read: impl Fn(&str) -> Option<String>) -> bool {
    let Some(program) = arguments.first() else {
        return false;
    };
    if Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        != Some("ptp4l")
    {
        return false;
    }
    let mut rest = arguments[1..].iter();
    while let Some(argument) = rest.next() {
        let value = match argument.as_str() {
            "-i" | "--interface" | "-f" => rest.next().map(String::as_str),
            other => other
                .strip_prefix("-i")
                .or_else(|| other.strip_prefix("--interface="))
                .filter(|value| !value.is_empty()),
        };
        let Some(value) = value else {
            continue;
        };
        if argument == "-f" {
            // A configuration file names its interfaces as sections.
            let section = format!("[{interface}]");
            if read(value).is_some_and(|text| text.lines().any(|line| line.trim() == section)) {
                return true;
            }
        } else if value == interface {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn settings_are_lines_of_key_and_value() {
        assert_eq!(parse(""), Ok(Settings::default()));
        assert_eq!(
            parse("# never grandmaster\npriority1 255 # here\nprofile avb-lite\n"),
            Ok(Settings {
                priority1: 255,
                profile: Profile::AvbLite,
                gptp_capable: true,
                follow_tai: false,
                sync_receipt_timeout: 3,
            })
        );
        assert_eq!(
            parse("sync-receipt-timeout 16")
                .expect("settings")
                .sync_receipt_timeout,
            16
        );
        assert!(parse("sync-receipt-timeout 0").is_err());
        assert!(parse("sync-receipt-timeout 256").is_err());
        assert!(parse("follow-tai yes").expect("settings").follow_tai);
        assert!(parse("follow-tai 1").is_err());
        assert!(parse("profile 1588").is_err());
        assert!(!parse("gptp-capable no").expect("settings").gptp_capable);
        assert!(parse("priority1 0").is_err());
        assert!(parse("priority1 256").is_err());
        assert!(parse("priority2 1").is_err());
    }

    #[test]
    fn priority1_is_set_in_place_or_added() {
        assert_eq!(
            with_priority1("# ours\npriority1 248\n", 246),
            "# ours\npriority1 246\n"
        );
        assert_eq!(with_priority1("# ours\n", 255), "# ours\npriority1 255\n");
        assert_eq!(with_priority1("", 255), "priority1 255\n");
    }

    #[test]
    fn a_ptp4l_on_the_interface_is_found() {
        let none = |_: &str| None;
        assert!(runs_on(
            &command("/usr/sbin/ptp4l -i enp2s0 -2"),
            "enp2s0",
            none
        ));
        assert!(runs_on(&command("ptp4l -ienp2s0"), "enp2s0", none));
        assert!(runs_on(
            &command("ptp4l --interface=enp2s0"),
            "enp2s0",
            none
        ));
        assert!(!runs_on(&command("ptp4l -i enp3s0"), "enp2s0", none));
        assert!(!runs_on(&command("phc2sys -s enp2s0"), "enp2s0", none));
        let config = |file: &str| (file == "/etc/x.cfg").then(|| "[global]\n[enp2s0]\n".to_owned());
        assert!(runs_on(&command("ptp4l -f /etc/x.cfg"), "enp2s0", config));
        assert!(!runs_on(&command("ptp4l -f /etc/x.cfg"), "enp3s0", config));
    }
}
