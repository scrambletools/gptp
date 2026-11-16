//! The status socket, `/run/gptpd/<interface>.sock` (gptp::status): a
//! snapshot to each client as it connects, at every change and every
//! 2 s, and its commands answered. Anyone may read; commands are taken
//! from root and from members of the group `gptpd`.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gptp::status::{Block, Command};

/// How often a client gets a snapshot when nothing changes.
const EVERY: Duration = Duration::from_secs(2);

/// How long a command waits for the daemon's answer.
const ANSWER: Duration = Duration::from_secs(5);

/// The group whose members may send commands.
const GROUP: &str = "gptpd";

/// The stack of each of the socket's threads, which hold little: where
/// RLIMIT_MEMLOCK binds, a stack counts against it in full.
const STACK: usize = 64 << 10;

/// A command from a client, with where its answer goes.
pub struct Request {
    pub command: Command,
    pub answer: Sender<Result<(), String>>,
}

struct Shared {
    /// The latest snapshot, numbered by its last real change.
    snapshot: Mutex<(u64, String)>,
    changed: Condvar,
}

/// A snapshot's lines but for its measurements, which change at each Sync
/// and Pdelay exchange: those go out with the snapshots every 2 s.
fn significant(snapshot: &str) -> String {
    snapshot
        .lines()
        .filter(|line| {
            !line.starts_with("residual ")
                && !line.starts_with("port.link_delay ")
                && !line.starts_with("port.neighbor_rate_ratio ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct Server {
    shared: Arc<Shared>,
    requests: Receiver<Request>,
}

impl Server {
    /// Listens on `path`, in place of what an earlier gptpd left there.
    pub fn start(path: &Path) -> io::Result<Self> {
        if let Some(folder) = path.parent() {
            fs::create_dir_all(folder)?;
        }
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
        let shared = Arc::new(Shared {
            snapshot: Mutex::new((0, String::new())),
            changed: Condvar::new(),
        });
        let (sender, requests) = mpsc::channel();
        let accepting = Arc::clone(&shared);
        let path = PathBuf::from(path);
        spawn("status", move || {
            for stream in listener.incoming() {
                let served = stream.and_then(|stream| {
                    serve(stream, Arc::clone(&accepting), sender.clone())
                        .map_err(|error| io::Error::other(format!("a client dropped: {error}")))
                });
                if let Err(error) = served {
                    eprintln!("{}: {error}", path.display());
                    // What refused it, as too many files open or no
                    // room for a thread, stays a while: not at once again.
                    thread::sleep(Duration::from_millis(100));
                }
            }
        })?;
        Ok(Self { shared, requests })
    }

    /// Keeps `snapshot` for the clients, and hands it to them at once
    /// where more than its measurements changed.
    pub fn publish(&self, snapshot: String) {
        let mut latest = self
            .shared
            .snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if significant(&latest.1) != significant(&snapshot) {
            *latest = (latest.0 + 1, snapshot);
            self.shared.changed.notify_all();
        } else {
            latest.1 = snapshot;
        }
    }

    /// The commands clients sent since last asked.
    pub fn requests(&self) -> impl Iterator<Item = Request> + '_ {
        self.requests.try_iter()
    }
}

/// Starts a thread of the socket's, on a stack of [`STACK`].
fn spawn(name: &str, work: impl FnOnce() + Send + 'static) -> io::Result<()> {
    thread::Builder::new()
        .name(name.to_owned())
        .stack_size(STACK)
        .spawn(work)
        .map(drop)
}

/// Serves one client: snapshots from one thread, commands from another.
/// Where either cannot start, the client goes.
fn serve(stream: UnixStream, shared: Arc<Shared>, requests: Sender<Request>) -> io::Result<()> {
    let reading = stream.try_clone()?;
    let writer = Arc::new(Mutex::new(stream));
    let replying = Arc::clone(&writer);
    let sending = Arc::clone(&writer);
    let started = spawn("status-reader", move || {
        answer(reading, &replying, &requests)
    })
    .and_then(|()| spawn("status-writer", move || send(&sending, &shared)));
    if started.is_err() {
        // Ends the reader, if it started.
        let _ = lock(&writer).shutdown(Shutdown::Both);
    }
    started
}

/// The client's stream, though a thread panicked holding it.
fn lock(stream: &Mutex<UnixStream>) -> std::sync::MutexGuard<'_, UnixStream> {
    stream.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Answers the client's commands.
fn answer(reading: UnixStream, replying: &Mutex<UnixStream>, requests: &Sender<Request>) {
    let permitted = permitted(&reading);
    for line in BufReader::new(reading).lines() {
        let Ok(line) = line else {
            return;
        };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match (&permitted, Command::parse(&line)) {
            (_, Err(error)) => Err(error.0),
            (Err(why), Ok(_)) => Err(why.clone()),
            (Ok(()), Ok(command)) => {
                let (answer, answered) = mpsc::channel();
                if requests.send(Request { command, answer }).is_err() {
                    return;
                }
                answered
                    .recv_timeout(ANSWER)
                    .unwrap_or_else(|_| Err("gptpd did not answer".to_owned()))
            }
        };
        let mut reply = String::new();
        let _ = Block::encode_reply(
            &mut reply,
            answer.as_ref().map(|_| ()).map_err(String::as_str),
        );
        if lock(replying).write_all(reply.as_bytes()).is_err() {
            return;
        }
    }
}

/// Sends the client each snapshot.
fn send(writer: &Mutex<UnixStream>, shared: &Shared) {
    let mut sent = 0;
    loop {
        let (number, snapshot) = {
            let latest = shared
                .snapshot
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let deadline = Instant::now() + EVERY;
            let (latest, _) = shared
                .changed
                .wait_timeout_while(latest, deadline - Instant::now(), |latest| {
                    latest.0 == sent && sent != 0
                })
                .unwrap_or_else(|poison| poison.into_inner());
            latest.clone()
        };
        if snapshot.is_empty() {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        sent = number;
        if lock(writer).write_all(snapshot.as_bytes()).is_err() {
            return;
        }
    }
}

/// Whether the client on `stream` may send commands: root, or a member of
/// [`GROUP`].
fn permitted(stream: &UnixStream) -> Result<(), String> {
    let (uid, gid) =
        peer::credentials(stream).map_err(|error| format!("cannot tell who is asking: {error}"))?;
    if uid == 0 {
        return Ok(());
    }
    let groups = fs::read_to_string("/etc/group").unwrap_or_default();
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    if member(&groups, &passwd, uid, gid) {
        Ok(())
    } else {
        Err(format!(
            "uid {uid} is neither root nor in the group {GROUP}"
        ))
    }
}

/// Whether `uid`, with primary group `gid`, is in [`GROUP`], by
/// `/etc/group` and `/etc/passwd`.
fn member(groups: &str, passwd: &str, uid: u32, gid: u32) -> bool {
    let Some(group) = groups
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.first() == Some(&GROUP))
    else {
        return false;
    };
    if group.get(2).and_then(|id| id.parse::<u32>().ok()) == Some(gid) {
        return true;
    }
    let name = passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.get(2).and_then(|id| id.parse::<u32>().ok()) == Some(uid))
        .and_then(|fields| fields.first().map(|name| (*name).to_owned()));
    let members = group.get(3).copied().unwrap_or("");
    name.is_some_and(|name| members.split(',').any(|member| member == name))
}

#[allow(unsafe_code)]
mod peer {
    use std::io;
    use std::mem::size_of;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    /// The user and group of the process at the other end (SO_PEERCRED).
    pub fn credentials(stream: &UnixStream) -> io::Result<(u32, u32)> {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `credentials` and `length` are what SO_PEERCRED fills,
        // and live through the call.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credentials).cast(),
                &raw mut length,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok((credentials.uid, credentials.gid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUPS: &str = "root:x:0:\nwheel:x:998:user\ngptpd:x:961:alice,user\n";
    const PASSWD: &str = "root:x:0:0::/root:/bin/bash\nuser:x:1000:1000::/home/user:/bin/bash\n\
                          bob:x:1001:961::/home/bob:/bin/sh\neve:x:1002:1002::/home/eve:/bin/sh\n";

    #[test]
    fn measurements_alone_are_no_change_to_send_at_once() {
        let one = "version 1\nclock a\ngeneration 3\nresidual 12\nport x\nport.up x true\n\
                   port.link_delay x 2\nport.neighbor_rate_ratio x 1.0000126\n";
        let measured = one
            .replace("residual 12", "residual -3")
            .replace("port.link_delay x 2", "port.link_delay x 1");
        assert_eq!(significant(one), significant(&measured));
        assert_ne!(
            significant(one),
            significant(&one.replace("generation 3", "generation 4"))
        );
        assert_ne!(
            significant(one),
            significant(&one.replace("port.up x true", "port.up x false"))
        );
    }

    #[test]
    fn members_of_the_group_may_command() {
        assert!(member(GROUPS, PASSWD, 1000, 1000));
        assert!(member(GROUPS, PASSWD, 1001, 961));
        assert!(!member(GROUPS, PASSWD, 1002, 1002));
        assert!(!member("wheel:x:998:user\n", PASSWD, 1000, 1000));
    }

    #[test]
    fn the_server_hands_out_snapshots_and_takes_commands_from_its_own() {
        let socket = std::env::temp_dir().join(format!("gptpd-server-{}.sock", std::process::id()));
        let server = Server::start(&socket).expect("start");
        server.publish("version 1\nclock 8c1f64.fffe.36c102\n\n".to_owned());
        let mut client = gptp::status::Client::connect_at(&socket).expect("connect");
        let Some(Block::Status(status)) = client.next(Some(Duration::from_secs(5))).expect("next")
        else {
            panic!("no snapshot");
        };
        assert_eq!(status.clock.to_string(), "8c1f64.fffe.36c102");
        server.publish("version 1\nclock 8c1f64.fffe.36c102\npriority1 246\n\n".to_owned());
        let Some(Block::Status(status)) = client.next(Some(Duration::from_secs(5))).expect("next")
        else {
            panic!("no snapshot");
        };
        assert_eq!(status.priority1, 246);
        client.set_priority1(255).expect("send");
        // The test runs as a user in no gptpd group, unless it is root.
        let deadline = Instant::now() + Duration::from_secs(5);
        let reply = loop {
            for request in server.requests() {
                let _ = request.answer.send(Ok(()));
            }
            match client.next(Some(Duration::from_millis(50))).expect("next") {
                Some(Block::Reply(reply)) => break reply,
                _ if Instant::now() > deadline => panic!("no reply"),
                _ => {}
            }
        };
        let groups = fs::read_to_string("/etc/group").unwrap_or_default();
        let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
        let uid = fs::metadata("/proc/self").map(|meta| std::os::unix::fs::MetadataExt::uid(&meta));
        let allowed = uid.as_ref().is_ok_and(|uid| *uid == 0)
            || uid.is_ok_and(|uid| member(&groups, &passwd, uid, u32::MAX));
        assert_eq!(reply.is_ok(), allowed, "{reply:?}");
        fs::remove_file(&socket).expect("remove");
    }

    /// Address space mapped, under MCL_FUTURE, until RLIMIT_MEMLOCK has
    /// room left for small allocations but for no thread's stack; given
    /// back when dropped. A thread with no malloc arena, as none can be
    /// made then, maps each allocation on its own.
    struct Filler(*mut libc::c_void, usize);

    impl Filler {
        /// The room left: a page for each of a few allocations, less than
        /// the socket's 64 KiB thread stacks.
        const ROOM: u64 = 56 << 10;

        /// None where the limit does not bind, as for root.
        #[allow(unsafe_code)]
        fn to_the_limit() -> Option<Self> {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: `limit` is what getrlimit fills.
            if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &raw mut limit) } != 0
                || limit.rlim_cur == libc::RLIM_INFINITY
            {
                return None;
            }
            // SAFETY: sysconf takes no memory.
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
            // Another thread may map something between reading and mapping.
            for _ in 0..10 {
                let status = fs::read_to_string("/proc/self/status").expect("status");
                let locked = status
                    .lines()
                    .find_map(|line| line.strip_prefix("VmLck:"))
                    .and_then(|kb| kb.trim().trim_end_matches(" kB").parse::<u64>().ok())
                    .expect("VmLck")
                    * 1024;
                let size = limit.rlim_cur.checked_sub(locked + Self::ROOM)? / page * page;
                let filler = Self(map(size as usize)?, size as usize);
                if filler.0 == libc::MAP_FAILED {
                    continue;
                }
                // A stack's worth more mapped all the same: it does not bind.
                let stack = map(68 << 10)?;
                if stack != libc::MAP_FAILED {
                    // SAFETY: just mapped, and used by nothing.
                    unsafe { libc::munmap(stack, 68 << 10) };
                    return None;
                }
                return Some(filler);
            }
            None
        }
    }

    /// An anonymous mapping of `size` bytes no one may touch.
    #[allow(unsafe_code)]
    fn map(size: usize) -> Option<*mut libc::c_void> {
        if size == 0 {
            return None;
        }
        // SAFETY: a new mapping, touching nothing.
        Some(unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        })
    }

    impl Drop for Filler {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            if self.0 != libc::MAP_FAILED {
                // SAFETY: mapped by `to_the_limit`, and used by nothing.
                unsafe { libc::munmap(self.0, self.1) };
            }
        }
    }

    /// A client gets a snapshot, and an answer to a command, so all the
    /// socket's threads so far are running.
    fn served(server: &Server, client: &mut gptp::status::Client) {
        let Some(Block::Status(_)) = client.next(Some(Duration::from_secs(5))).expect("next")
        else {
            panic!("no snapshot");
        };
        client.set_priority1(255).expect("send");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for request in server.requests() {
                let _ = request.answer.send(Ok(()));
            }
            match client.next(Some(Duration::from_millis(50))).expect("next") {
                Some(Block::Reply(_)) => return,
                _ if Instant::now() > deadline => panic!("no reply"),
                _ => {}
            }
        }
    }

    /// In a container, where RLIMIT_MEMLOCK binds and the daemon locks
    /// what it maps later, a thread may fail to start: the client it was
    /// for goes, and the socket goes on taking the next.
    #[test]
    #[allow(unsafe_code)]
    fn a_client_no_thread_can_start_for_goes_and_the_next_is_served() {
        if !crate::memory::in_a_child(
            "server::tests::a_client_no_thread_can_start_for_goes_and_the_next_is_served",
        ) {
            return;
        }
        // SAFETY: mlockall takes no memory.
        if unsafe { libc::mlockall(libc::MCL_FUTURE | libc::MCL_ONFAULT) } != 0 {
            eprintln!("skipped: mlockall: {}", io::Error::last_os_error());
            return;
        }
        let socket = std::env::temp_dir().join(format!("gptpd-full-{}.sock", std::process::id()));
        let server = Server::start(&socket).expect("start");
        server.publish("version 1\nclock 8c1f64.fffe.36c102\n\n".to_owned());
        let mut first = gptp::status::Client::connect_at(&socket).expect("connect");
        served(&server, &mut first);
        let Some(filler) = Filler::to_the_limit() else {
            eprintln!("skipped: RLIMIT_MEMLOCK does not bind");
            fs::remove_file(&socket).expect("remove");
            return;
        };
        let mut refused = UnixStream::connect(&socket).expect("connect");
        refused
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut byte = [0];
        let ended = io::Read::read(&mut refused, &mut byte);
        drop(filler);
        assert_eq!(ended.expect("dropped, not left waiting"), 0);
        let mut next = gptp::status::Client::connect_at(&socket).expect("connect");
        served(&server, &mut next);
        fs::remove_file(&socket).expect("remove");
    }
}
