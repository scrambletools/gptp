//! Raw sockets from a helper, so the program using them holds no
//! capability. On Linux a raw Ethernet socket takes CAP_NET_RAW only to be
//! created; once open it works for whoever holds it. A program holding the
//! capability itself runs as one other programs may not look into: the
//! desktop portals refuse it file dialogs and screen sharing, and
//! debuggers and crash reports pass it by. So an app ships a tiny helper
//! of its own, the one file granted the capability, whose `main` calls
//! [`serve`]; the app starts it with [`Broker::start`] and [`install`]s
//! it, and every raw socket this crate opens comes from it from then on.
//!
//! The helper takes requests on its standard input, a Unix socket the app
//! keeps the other end of: one octet, the kind of socket. It answers each
//! with four octets, zero or the error number, and on success the new
//! socket's descriptor beside them. It makes nothing but packet sockets,
//! and stops when the app closes its end. A helper granted CAP_NET_ADMIN
//! as well makes [`Kind::Launch`] sockets, whose frames a card sends at
//! their times; one without answers EPERM for them.

#![allow(unsafe_code)]

use std::io::{self, Read};
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};

/// A kind of packet socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kind {
    /// Frames' payloads, the link-layer header taken off and put on.
    Datagram = 1,
    /// Whole frames, header and all.
    Raw = 2,
    /// Whole frames, each sent at the launch time it carries on CLOCK_TAI
    /// (SO_TXTIME), for the ETF queue discipline to hand to a card that
    /// sends it at that time, and the frames ETF drops reported on the
    /// socket's error queue. A departure time on CLOCK_TAI takes
    /// CAP_NET_ADMIN as well as CAP_NET_RAW: a helper without it answers
    /// EPERM, as one older than this kind answers EINVAL, and the app
    /// takes a [`Kind::Raw`] socket instead. The drops come on the error
    /// queue as SO_EE_ORIGIN_TXTIME, so an app also taking transmit time
    /// stamps on it tells the two apart by origin. A launch time with no
    /// ETF on the frame's queue is ignored, and the frame sent at once.
    Launch = 3,
}

impl Kind {
    fn from_octet(octet: u8) -> Option<Self> {
        match octet {
            1 => Some(Self::Datagram),
            2 => Some(Self::Raw),
            3 => Some(Self::Launch),
            _ => None,
        }
    }

    fn socket_type(self) -> libc::c_int {
        match self {
            Self::Datagram => libc::SOCK_DGRAM,
            Self::Raw | Self::Launch => libc::SOCK_RAW,
        }
    }
}

/// A packet socket of `kind` for no protocol, made here: it takes
/// CAP_NET_RAW, and CAP_NET_ADMIN too for [`Kind::Launch`]. The helper
/// answers with the error number alone, so each stays the system's.
fn make(kind: Kind) -> io::Result<OwnedFd> {
    let fd = packet_socket(kind)?;
    if kind == Kind::Launch {
        launch_on_tai(&fd)?;
    }
    Ok(fd)
}

/// A packet socket of `kind`'s type for no protocol: it takes CAP_NET_RAW.
fn packet_socket(kind: Kind) -> io::Result<OwnedFd> {
    // SAFETY: plain system call with integer arguments.
    let raw = unsafe { libc::socket(libc::AF_PACKET, kind.socket_type() | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a socket this function just opened and owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Has `fd`'s frames carry launch times: on CLOCK_TAI, ETF's clock,
/// without deadline mode, as a frame should leave at its time and not
/// before, and each frame dropped reported. It takes CAP_NET_ADMIN.
fn launch_on_tai(fd: &OwnedFd) -> io::Result<()> {
    let timing = libc::sock_txtime {
        clockid: libc::CLOCK_TAI,
        flags: libc::SOF_TXTIME_REPORT_ERRORS,
    };
    // SAFETY: `timing` lives through the call, and the length passed is
    // its size.
    let set = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TXTIME,
            (&raw const timing).cast(),
            size_of::<libc::sock_txtime>() as libc::socklen_t,
        )
    };
    if set < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The error for a [`Kind::Launch`] socket refused for want of
/// CAP_NET_ADMIN, naming the program that needs it.
fn launch_capability(program: Option<&Path>) -> io::Error {
    let grant = match program {
        Some(program) => format!(
            "; grant it with: sudo setcap cap_net_raw,cap_net_admin+ep {}",
            program.display()
        ),
        None => String::new(),
    };
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("launch times on CLOCK_TAI need CAP_NET_ADMIN as well as CAP_NET_RAW{grant}"),
    )
}

/// The helper started and installed, and where it is, once there is one.
struct Installed {
    broker: Mutex<Broker>,
    program: PathBuf,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// The helper [`use_helper`] found last, started or not, for the error to
/// name.
static FOUND: Mutex<Option<PathBuf>> = Mutex::new(None);

/// A packet socket of `kind`: from the helper when one is installed, else
/// made here. The error for one CAP_NET_RAW is missing for carries
/// [`crate::Blocked::RawCapability`]; a [`Kind::Launch`] one refused for
/// want of CAP_NET_ADMIN is PermissionDenied, naming that capability. For
/// crates on this one opening sockets of their own, as avb-net does for
/// AVTP streams.
pub fn open(kind: Kind) -> io::Result<OwnedFd> {
    let result = match INSTALLED.get() {
        Some(installed) => {
            let made = installed
                .broker
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .socket(kind);
            match made {
                // The helper made a socket as it started, so it holds
                // CAP_NET_RAW: EPERM for a Launch one is CAP_NET_ADMIN.
                Err(error) if kind == Kind::Launch && error.raw_os_error() == Some(libc::EPERM) => {
                    return Err(launch_capability(Some(&installed.program)));
                }
                made => made,
            }
        }
        None => match packet_socket(kind) {
            Ok(fd) if kind == Kind::Launch => match launch_on_tai(&fd) {
                Ok(()) => Ok(fd),
                Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                    let program = std::env::current_exe().ok();
                    return Err(launch_capability(program.as_deref()));
                }
                Err(error) => Err(error),
            },
            made => made,
        },
    };
    result.map_err(|error| match error.raw_os_error() {
        Some(libc::EPERM | libc::EACCES) => crate::socket::raw_capability(),
        _ => error,
    })
}

/// The program that must hold CAP_NET_RAW for raw sockets to open: the
/// installed helper, or the one found that could not start, else this
/// one. An app names it in the command it offers to grant the capability.
pub fn needs_capability() -> Option<PathBuf> {
    INSTALLED
        .get()
        .map(|installed| installed.program.clone())
        .or_else(|| FOUND.lock().unwrap_or_else(PoisonError::into_inner).clone())
        .or_else(|| std::env::current_exe().ok())
}

/// Has every raw socket this crate opens from now on come from `broker`.
/// Only the first broker installed is kept; a later one is dropped, its
/// helper stopping.
pub fn install(broker: Broker) {
    let program = broker.program.clone();
    let _ = INSTALLED.set(Installed {
        broker: Mutex::new(broker),
        program,
    });
}

/// Whether a helper is installed.
pub fn installed() -> bool {
    INSTALLED.get().is_some()
}

/// Starts and installs the app's helper, `name`, once: found beside the
/// running program, or beside the folder it is in, as for an example, or
/// in one of `folders`, where packages put it. Later calls do nothing
/// once one is installed, and try again after one failed, as after the
/// helper is granted the capability.
pub fn use_helper(name: &str, folders: &[&Path]) -> io::Result<()> {
    static STARTING: Mutex<()> = Mutex::new(());
    let _one_at_a_time = STARTING.lock().unwrap_or_else(PoisonError::into_inner);
    if installed() {
        return Ok(());
    }
    let program = find(name, folders).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("the raw Ethernet helper, {name}, is not installed beside this program"),
        )
    })?;
    *FOUND.lock().unwrap_or_else(PoisonError::into_inner) = Some(program.clone());
    install(Broker::start(&program)?);
    Ok(())
}

/// Where the helper `name` is, as [`use_helper`] looks for it.
fn find(name: &str, folders: &[&Path]) -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|program| program.parent().map(Path::to_path_buf));
    let mut places = Vec::new();
    if let Some(folder) = beside {
        places.push(folder.join(name));
        if let Some(parent) = folder.parent() {
            places.push(parent.join(name));
        }
    }
    places.extend(folders.iter().map(|folder| folder.join(name)));
    places.into_iter().find(|place| place.is_file())
}

/// A running helper, and the socket to it. Dropping it stops the helper.
pub struct Broker {
    stream: UnixStream,
    child: Child,
    program: PathBuf,
}

impl Broker {
    /// Starts the helper at `program` and asks it for a first socket, so a
    /// helper that lacks the capability is found out here: the error then
    /// carries [`crate::Blocked::RawCapability`], naming the helper.
    pub fn start(program: &Path) -> io::Result<Self> {
        let (ours, theirs) = UnixStream::pair()?;
        let child = Command::new(program)
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .spawn()?;
        let mut broker = Self {
            stream: ours,
            child,
            program: program.to_owned(),
        };
        match broker.socket(Kind::Datagram) {
            Ok(_) => Ok(broker),
            Err(error) if matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES)) => {
                let program = program.display();
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "raw Ethernet needs CAP_NET_RAW on the helper; grant it with: sudo \
                         setcap cap_net_raw+ep {program}"
                    ),
                ))
            }
            Err(error) => Err(error),
        }
    }

    /// Asks the helper for a socket of `kind`.
    fn socket(&mut self, kind: Kind) -> io::Result<OwnedFd> {
        use std::io::Write;
        self.stream.write_all(&[kind as u8])?;
        let (status, fd) = receive(self.stream.as_raw_fd())?;
        match (status, fd) {
            (0, Some(fd)) => Ok(fd),
            (0, None) => Err(io::Error::other("the helper sent no socket")),
            (errno, _) => Err(io::Error::from_raw_os_error(errno)),
        }
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        // Closing our end ends the helper's requests.
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        let _ = self.child.wait();
    }
}

/// The room a message with one descriptor beside it takes.
fn control_room() -> usize {
    // SAFETY: CMSG_SPACE only computes a length.
    unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) as usize }
}

/// Sends `status`, and `fd` beside it when there is one.
fn send(socket: RawFd, status: i32, fd: Option<RawFd>) -> io::Result<()> {
    let mut data = status.to_ne_bytes();
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let mut control = vec![0u8; control_room()];
    // SAFETY: msghdr is plain data, valid when zeroed.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &raw mut iov;
    message.msg_iovlen = 1;
    if let Some(fd) = fd {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len() as _;
        // SAFETY: the control buffer has room for one header and one
        // descriptor, as CMSG_SPACE said, and lives through the call.
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&raw const message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
            libc::CMSG_DATA(header).cast::<RawFd>().write_unaligned(fd);
        }
    }
    // SAFETY: `message` points at buffers that live through the call.
    if unsafe { libc::sendmsg(socket, &raw const message, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receives a status, and the descriptor beside it if any, close on exec.
fn receive(socket: RawFd) -> io::Result<(i32, Option<OwnedFd>)> {
    let mut data = [0u8; 4];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let mut control = vec![0u8; control_room()];
    // SAFETY: msghdr is plain data, valid when zeroed.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &raw mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;
    // SAFETY: `message` points at buffers that live through the call.
    let read = unsafe { libc::recvmsg(socket, &raw mut message, libc::MSG_CMSG_CLOEXEC) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    if read as usize != data.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the helper stopped",
        ));
    }
    let mut fd = None;
    // SAFETY: the kernel filled the control buffer and set its length;
    // the macros walk the headers within it.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&raw const message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let raw = libc::CMSG_DATA(header).cast::<RawFd>().read_unaligned();
                fd = Some(OwnedFd::from_raw_fd(raw));
            }
            header = libc::CMSG_NXTHDR(&raw const message, header);
        }
    }
    Ok((i32::from_ne_bytes(data), fd))
}

/// The helper's whole program: answers the requests on its standard
/// input until the app closes it. Returns the exit code.
pub fn serve() -> i32 {
    let input = 0;
    // A Unix socket the app started us with, or nothing to serve.
    let mut kind: libc::c_int = 0;
    let mut length = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `kind` and `length` live through the call and fit it.
    let is_socket = unsafe {
        libc::getsockopt(
            input,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&raw mut kind).cast(),
            &raw mut length,
        )
    } == 0;
    if !is_socket {
        eprintln!("this helper opens raw Ethernet sockets for the app that starts it");
        return 2;
    }
    // SAFETY: standard input is ours for the life of the program.
    let mut requests = unsafe { UnixStream::from_raw_fd(input) };
    let mut octet = [0u8];
    loop {
        match requests.read(&mut octet) {
            Ok(0) | Err(_) => return 0,
            Ok(_) => {}
        }
        let made = Kind::from_octet(octet[0])
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))
            .and_then(make);
        let sent = match &made {
            Ok(fd) => send(requests.as_raw_fd(), 0, Some(fd.as_raw_fd())),
            Err(error) => send(
                requests.as_raw_fd(),
                error.raw_os_error().unwrap_or(libc::EIO),
                None,
            ),
        };
        // Ours closes once sent: the app holds its own copy.
        drop(made);
        if sent.is_err() {
            return 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A descriptor and a status go across a socket pair as the helper
    /// sends them, the descriptor arriving as one of the receiver's own.
    #[test]
    fn a_descriptor_crosses() {
        let (helper, app) = UnixStream::pair().unwrap();
        let (left, mut right) = UnixStream::pair().unwrap();
        send(helper.as_raw_fd(), 0, Some(left.as_raw_fd())).unwrap();
        let (status, fd) = receive(app.as_raw_fd()).unwrap();
        assert_eq!(status, 0);
        let mut received = UnixStream::from(fd.expect("a descriptor"));
        use std::io::Write;
        received.write_all(b"hi").unwrap();
        let mut heard = [0u8; 2];
        right.read_exact(&mut heard).unwrap();
        assert_eq!(&heard, b"hi");
        // An error goes alone.
        send(helper.as_raw_fd(), libc::EPERM, None).unwrap();
        let (status, fd) = receive(app.as_raw_fd()).unwrap();
        assert_eq!(status, libc::EPERM);
        assert!(fd.is_none());
    }

    #[test]
    fn only_packet_sockets_are_asked_for() {
        assert_eq!(Kind::from_octet(1), Some(Kind::Datagram));
        assert_eq!(Kind::from_octet(2), Some(Kind::Raw));
        assert_eq!(Kind::from_octet(3), Some(Kind::Launch));
        assert_eq!(Kind::from_octet(4), None);
        for kind in [Kind::Datagram, Kind::Raw, Kind::Launch] {
            assert_eq!(Kind::from_octet(kind as u8), Some(kind));
        }
    }

    #[test]
    fn a_launch_socket_refused_names_cap_net_admin() {
        let error = launch_capability(Some(Path::new("/usr/lib/app/helper")));
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        // No error number, so open() does not take it for CAP_NET_RAW.
        assert_eq!(error.raw_os_error(), None);
        let text = error.to_string();
        assert!(text.contains("CAP_NET_ADMIN"), "{text}");
        assert!(
            text.ends_with("setcap cap_net_raw,cap_net_admin+ep /usr/lib/app/helper"),
            "{text}"
        );
        assert!(!launch_capability(None).to_string().contains("setcap"));
    }
}
