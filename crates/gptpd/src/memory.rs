//! Keeping the daemon in RAM.

/// CAP_IPC_LOCK, as linux/capability.h numbers it.
const CAP_IPC_LOCK: u32 = 14;

/// Keeps the daemon in RAM, so none of it waits on the disk while a frame
/// is due, however hard the computer swaps: what is mapped now at once,
/// what is mapped later once touched. Where RLIMIT_MEMLOCK binds, as in
/// an unprivileged container, each later mapping would count against it
/// in full, a thread's whole stack and each new piece of heap, and fail
/// past it: there only what is mapped now.
#[allow(unsafe_code)]
pub fn lock() {
    let binding = binding_limit();
    // SAFETY: mlockall takes no memory.
    if unsafe { libc::mlockall(libc::MCL_CURRENT) } != 0 {
        eprintln!(
            "its memory may be paged out: mlockall: {}",
            std::io::Error::last_os_error()
        );
    }
    match binding {
        None => {
            // SAFETY: as above.
            if unsafe { libc::mlockall(libc::MCL_FUTURE | libc::MCL_ONFAULT) } != 0 {
                eprintln!(
                    "what it maps later may be paged out: mlockall: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        Some(limit) => eprintln!(
            "what it maps later may be paged out: RLIMIT_MEMLOCK is {} KiB, \
             and it lacks CAP_IPC_LOCK",
            limit >> 10
        ),
    }
}

/// RLIMIT_MEMLOCK in bytes where it binds.
#[allow(unsafe_code)]
fn binding_limit() -> Option<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is what getrlimit fills.
    if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &raw mut limit) } != 0 {
        return None;
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let uid_map = std::fs::read_to_string("/proc/self/uid_map").unwrap_or_default();
    binds(limit.rlim_cur, &status, &uid_map)
}

/// `limit` where it binds: finite, and the process without CAP_IPC_LOCK
/// in the first user namespace, by /proc/self/status and uid_map. Root
/// in an unprivileged container has its capabilities in its own only.
fn binds(limit: u64, status: &str, uid_map: &str) -> Option<u64> {
    if limit == libc::RLIM_INFINITY {
        return None;
    }
    let capable = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|caps| u64::from_str_radix(caps.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << CAP_IPC_LOCK) != 0);
    let first_namespace = uid_map.split_whitespace().eq(["0", "0", "4294967295"]);
    (!(capable && first_namespace)).then_some(limit)
}

/// Whether this is the child process `test` runs alone in, where it may
/// lock memory; from the parent, runs the child and checks it.
#[cfg(test)]
pub fn in_a_child(test: &str) -> bool {
    if std::env::var_os("GPTPD_TEST_CHILD").is_some() {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([test, "--exact", "--test-threads=1", "--nocapture"])
        .env("GPTPD_TEST_CHILD", "1")
        .output()
        .expect("child");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = "         0          0 4294967295\n";
    const CONTAINER: &str = "         0     100000      65536\n";
    const ROOT: &str = "Name:\tgptpd\nCapEff:\t000001ffffffffff\n";
    const USER: &str = "Name:\tgptpd\nCapEff:\t0000000000000000\n";

    #[test]
    fn the_limit_binds_without_cap_ipc_lock_in_the_first_namespace() {
        let eight = 8 << 20;
        assert_eq!(binds(libc::RLIM_INFINITY, USER, CONTAINER), None);
        assert_eq!(binds(eight, ROOT, FIRST), None);
        assert_eq!(binds(eight, "CapEff:\t0000000000004000\n", FIRST), None);
        assert_eq!(binds(eight, ROOT, CONTAINER), Some(eight));
        assert_eq!(binds(eight, USER, FIRST), Some(eight));
        assert_eq!(binds(eight, "", ""), Some(eight));
    }

    /// Where the limit binds, threads start past it.
    #[test]
    #[allow(unsafe_code)]
    fn where_the_limit_binds_what_is_mapped_later_goes_unlocked() {
        if !in_a_child("memory::tests::where_the_limit_binds_what_is_mapped_later_goes_unlocked") {
            return;
        }
        let Some(limit) = binding_limit() else {
            eprintln!("skipped: RLIMIT_MEMLOCK does not bind");
            return;
        };
        lock();
        let size = limit as usize + (1 << 20);
        // SAFETY: a new mapping, touching nothing.
        let past = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(
            past,
            libc::MAP_FAILED,
            "{}",
            std::io::Error::last_os_error()
        );
        // SAFETY: just mapped, and used by nothing.
        unsafe { libc::munmap(past, size) };
        let threads: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| std::thread::park_timeout(Default::default())))
            .collect();
        for thread in threads {
            thread.join().expect("join");
        }
    }
}
