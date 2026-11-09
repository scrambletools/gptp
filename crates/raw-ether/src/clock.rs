//! A network interface's PTP hardware clock, which gPTP keeps on the
//! network's time, read from Linux's `/dev/ptpN`, and two such clocks
//! compared. Elsewhere opening fails as unsupported.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

/// One of this computer's clocks, to compare a hardware clock with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemClock {
    /// The wall clock, which NTP steps and slews.
    Realtime,
    /// The clock `std::time::Instant` and sleeps run on: slewed by NTP,
    /// never stepped.
    Monotonic,
    /// The oscillator as it runs, which nothing adjusts.
    MonotonicRaw,
    /// International atomic time: the wall clock and the kernel's TAI
    /// offset, which is zero until something such as chrony sets it.
    Tai,
}

/// A PTP hardware clock.
pub struct HardwareClock {
    inner: platform::HardwareClock,
}

impl HardwareClock {
    /// Opens the clock with `index`, as an interface reports it in
    /// [`crate::Interface::hardware_clock`].
    pub fn open(index: u32) -> io::Result<Self> {
        Ok(Self {
            inner: platform::HardwareClock::open(index, false)?,
        })
    }

    /// Opens the clock with `index` to adjust it as well as read it.
    pub fn open_adjustable(index: u32) -> io::Result<Self> {
        Ok(Self {
            inner: platform::HardwareClock::open(index, true)?,
        })
    }

    /// How far the clock's frequency is adjusted from its oscillator's, in
    /// parts per billion.
    pub fn frequency_adjustment(&self) -> io::Result<f64> {
        self.inner.frequency_adjustment()
    }

    /// Adjusts the clock's frequency to `ppb` parts per billion from its
    /// oscillator's: 0 lets it run free. Needs the clock opened with
    /// [`open_adjustable`](Self::open_adjustable).
    pub fn set_frequency_adjustment(&self, ppb: f64) -> io::Result<()> {
        self.inner.set_frequency_adjustment(ppb)
    }

    /// The clock's time, from the PTP epoch.
    pub fn now(&self) -> io::Result<Duration> {
        self.inner.now()
    }

    /// How far this clock is ahead of `other`, in nanoseconds: each read
    /// by its driver between two readings of the system clock, the
    /// tightest of several pairs kept, so the system clock drops out.
    /// Linux only; elsewhere unsupported.
    pub fn offset_from(&self, other: &HardwareClock) -> io::Result<i128> {
        self.reading_from(other).map(|reading| reading.offset)
    }

    /// [`offset_from`](Self::offset_from) with its span: the two clocks'
    /// spans together.
    pub fn reading_from(&self, other: &HardwareClock) -> io::Result<Reading> {
        let ours = self.system_reading(SystemClock::Realtime)?;
        let theirs = other.system_reading(SystemClock::Realtime)?;
        Ok(Reading {
            offset: ours.offset - theirs.offset,
            span: ours.span.saturating_add(theirs.span),
        })
    }

    /// The driver's reading against `system`, or where it has none, one
    /// read as [`reading_against`](Self::reading_against) reads it.
    /// iwlwifi's radio clock has none: its PTP_SYS_OFFSET_PRECISE is a
    /// firmware command of about 1 ms that logs a line each time.
    fn system_reading(&self, system: SystemClock) -> io::Result<Reading> {
        match self.inner.reading_from_system(system) {
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                self.reading_against(system)
            }
            read => read,
        }
    }

    /// How far this clock is ahead of `system`, in nanoseconds, from its
    /// driver's tightest of several readings between two of `system`'s
    /// (PTP_SYS_OFFSET_EXTENDED). A kernel too old to take a clock other
    /// than the wall clock for it refuses the others, and then
    /// [`offset_read_against`](Self::offset_read_against) serves.
    pub fn offset_from_system_clock(&self, system: SystemClock) -> io::Result<i128> {
        self.reading_from_system_clock(system)
            .map(|reading| reading.offset)
    }

    /// [`offset_from_system_clock`](Self::offset_from_system_clock) with
    /// the span of the reading kept.
    pub fn reading_from_system_clock(&self, system: SystemClock) -> io::Result<Reading> {
        self.system_reading(system)
    }

    /// How far the clock is ahead of this computer's monotonic clock, in
    /// nanoseconds, read between two readings of the monotonic clock: of
    /// a few tries, the closest-read of those near their median, as a
    /// clock can now and then be read wrong (a 2^32 ns tear has been seen
    /// on one card's).
    pub fn offset_from_monotonic(&self) -> io::Result<i128> {
        self.offset_read_against(SystemClock::Monotonic)
    }

    /// How far the clock is ahead of `system`, in nanoseconds, read as
    /// [`offset_from_monotonic`](Self::offset_from_monotonic) reads it:
    /// looser than the driver's readings, but on every kernel.
    pub fn offset_read_against(&self, system: SystemClock) -> io::Result<i128> {
        self.reading_against(system).map(|reading| reading.offset)
    }

    /// [`offset_read_against`](Self::offset_read_against) with the span of
    /// the try kept. Where none stands near the median, the median's.
    pub fn reading_against(&self, system: SystemClock) -> io::Result<Reading> {
        let mut tries = [Reading { offset: 0, span: 0 }; 5];
        for slot in &mut tries {
            let before = system_now(system);
            let clock = self.now()?;
            let after = system_now(system);
            let span = after.saturating_sub(before).as_nanos();
            let middle = before + (after - before) / 2;
            *slot = Reading {
                offset: clock.as_nanos() as i128 - middle.as_nanos() as i128,
                span: u64::try_from(span).unwrap_or(u64::MAX),
            };
        }
        let mut sorted = tries;
        sorted.sort_unstable_by_key(|reading| reading.offset);
        let median = sorted[sorted.len() / 2];
        Ok(tries
            .into_iter()
            .filter(|reading| (reading.offset - median.offset).abs() <= 10_000)
            .min_by_key(|reading| reading.span)
            .unwrap_or(median))
    }
}

/// A hardware clock against another clock, read between two of the
/// other's readings: how far it was ahead, and how long the reading took
/// from the other's first to its second. The hardware clock is latched
/// somewhere in that span, so a reading is off by up to half of it; a
/// slow one, held up on the way to the latch or after it, by up to half
/// of what it took beyond the usual. Comparisons that must hold to less
/// than a microsecond can leave such readings out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    /// The hardware clock less the other, in nanoseconds.
    pub offset: i128,
    /// How long the reading took, in nanoseconds.
    pub span: u64,
}

/// How far back comparisons count towards the rate, in nanoseconds: long
/// enough that each comparison's own error hardly moves it.
const SPAN: i128 = 2_000_000_000;

/// Two clocks compared now and then, with the rate between them, so that
/// a time on one is put on the other as the two stood when it was taken,
/// not as at the last comparison: clocks some ppm apart drift hundreds of
/// ns between comparisons. A line fitted through the comparisons kept
/// gives both, so no one comparison's error carries over whole.
#[derive(Debug, Clone, Default)]
pub struct Comparison {
    /// (when, on the clock compared, in ns; its time less the other's)
    kept: VecDeque<(i128, i128)>,
}

impl Comparison {
    /// The clock compared read `at`, and `offset` ahead of the other.
    pub fn update(&mut self, at: i128, offset: i128) {
        if self.kept.back().is_some_and(|(last, _)| at <= *last) {
            return;
        }
        self.kept.push_back((at, offset));
        while self
            .kept
            .front()
            .is_some_and(|(first, _)| at - first > SPAN)
            && self.kept.len() > 2
        {
            self.kept.pop_front();
        }
    }

    /// How far ahead of the other the clock compared was at `at`, a time
    /// on it.
    pub fn offset_at(&self, at: i128) -> i128 {
        let Some(&(last_at, last_offset)) = self.kept.back() else {
            return 0;
        };
        let Some((mean_at, mean_offset, rate)) = self.fit() else {
            return last_offset;
        };
        last_offset + (mean_offset + rate * ((at - last_at) as f64 - mean_at)).round() as i128
    }

    /// How much faster the clock compared runs than the other, as a
    /// fraction, once compared twice.
    pub fn rate(&self) -> Option<f64> {
        self.fit().map(|(_, _, rate)| rate)
    }

    /// The least-squares line through the comparisons kept: their mean
    /// time and offset, each from the last's so as to keep the precision,
    /// and the slope.
    fn fit(&self) -> Option<(f64, f64, f64)> {
        let &(last_at, last_offset) = self.kept.back()?;
        if self.kept.len() < 2 {
            return None;
        }
        let count = self.kept.len() as f64;
        let points = || {
            self.kept
                .iter()
                .map(move |&(at, offset)| ((at - last_at) as f64, (offset - last_offset) as f64))
        };
        let mean_at = points().map(|(at, _)| at).sum::<f64>() / count;
        let mean_offset = points().map(|(_, offset)| offset).sum::<f64>() / count;
        let (spread, covariance) =
            points().fold((0.0, 0.0), |(spread, covariance), (at, offset)| {
                let at = at - mean_at;
                (spread + at * at, covariance + at * (offset - mean_offset))
            });
        (spread > 0.0).then(|| (mean_at, mean_offset, covariance / spread))
    }

    /// Whether the clocks have been compared yet.
    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }

    /// Forgets all but the last `count` comparisons, as when the rate
    /// between the clocks changed and the older ones would only drag the
    /// line.
    pub fn keep_last(&mut self, count: usize) {
        while self.kept.len() > count {
            self.kept.pop_front();
        }
    }

    /// Forgets every comparison, as after one of the clocks was set.
    pub fn clear(&mut self) {
        self.kept.clear();
    }
}

/// This computer's monotonic clock: the clock `std::time::Instant` reads,
/// as a duration from its unspecified start.
pub fn monotonic_now() -> Duration {
    platform::monotonic_now()
}

/// One of this computer's clocks, as a duration from its epoch. Elsewhere
/// than on Linux the monotonic clock stands in for all three.
pub fn system_now(system: SystemClock) -> Duration {
    platform::system_now(system)
}

/// How long this computer has been suspended since it started: what the
/// boot clock has counted beyond the monotonic clock. It grows at each
/// resume. Elsewhere than on Linux it stays zero.
pub fn suspended() -> Duration {
    platform::suspended()
}

/// Has the calling thread's sleeps end as close to when they were asked
/// for as the system can, for a thread that paces frames: on Linux a
/// timer slack of a nanosecond in place of 50 microseconds. Elsewhere it
/// does nothing.
pub fn precise_sleeps() {
    platform::precise_sleeps();
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod platform {
    use super::{Reading, SystemClock};
    use std::io;
    use std::mem::size_of;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Duration;

    pub struct HardwareClock {
        _fd: OwnedFd,
        id: libc::clockid_t,
    }

    fn read(id: libc::clockid_t) -> io::Result<Duration> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `time` is a valid timespec that lives through the call.
        if unsafe { libc::clock_gettime(id, &raw mut time) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Duration::new(time.tv_sec as u64, time.tv_nsec as u32))
    }

    impl HardwareClock {
        pub fn open(index: u32, adjustable: bool) -> io::Result<Self> {
            let path = format!("/dev/ptp{index}\0");
            let mode = if adjustable {
                libc::O_RDWR
            } else {
                libc::O_RDONLY
            };
            // SAFETY: `path` is NUL-terminated and lives through the call.
            let raw = unsafe { libc::open(path.as_ptr().cast(), mode | libc::O_CLOEXEC) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `raw` is a descriptor this function just opened.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            // A dynamic clock's ID is made from its descriptor
            // (FD_TO_CLOCKID: CLOCKFD in the low bits).
            let id = ((!fd.as_raw_fd()) << 3) | 3;
            Ok(Self { _fd: fd, id })
        }

        pub fn now(&self) -> io::Result<Duration> {
            read(self.id)
        }

        /// clock_adjtime on the clock: sets what `modes` names, here no
        /// more than the frequency, and gives the adjustments as they stand.
        fn adjust(&self, modes: libc::c_uint, frequency: libc::c_long) -> io::Result<libc::timex> {
            // SAFETY: timex is plain data, valid when zeroed.
            let mut time: libc::timex = unsafe { std::mem::zeroed() };
            time.modes = modes;
            time.freq = frequency;
            // SAFETY: `time` is a valid timex that lives through the call.
            if unsafe { libc::clock_adjtime(self.id, &raw mut time) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(time)
        }

        /// timex's freq is in ppm with 16 fractional bits.
        const SCALED_PPB: f64 = 65.536;

        pub fn frequency_adjustment(&self) -> io::Result<f64> {
            Ok(self.adjust(0, 0)?.freq as f64 / Self::SCALED_PPB)
        }

        pub fn set_frequency_adjustment(&self, ppb: f64) -> io::Result<()> {
            self.adjust(
                libc::ADJ_FREQUENCY,
                (ppb * Self::SCALED_PPB).round() as libc::c_long,
            )
            .map(|_| ())
        }

        /// The clock less the system clock, from its driver's tightest of
        /// several readings between two of the system clock
        /// (PTP_SYS_OFFSET_EXTENDED), or unsupported where it has none.
        pub fn reading_from_system(&self, system: SystemClock) -> io::Result<Reading> {
            self.extended_reading(system).map_err(|error| {
                if error.raw_os_error() == Some(libc::EOPNOTSUPP) {
                    io::Error::new(io::ErrorKind::Unsupported, error)
                } else {
                    error
                }
            })
        }

        fn extended_reading(&self, system: SystemClock) -> io::Result<Reading> {
            /// struct ptp_clock_time, from linux/ptp_clock.h.
            #[repr(C)]
            #[derive(Clone, Copy, Default)]
            struct Time {
                seconds: i64,
                nanoseconds: u32,
                reserved: u32,
            }
            /// struct ptp_sys_offset_extended: system, clock and system
            /// again, for each sample.
            #[repr(C)]
            struct Extended {
                samples: u32,
                clock: i32,
                reserved: [u32; 2],
                times: [[Time; 3]; 25],
            }
            /// _IOWR('=', 9, struct ptp_sys_offset_extended).
            const PTP_SYS_OFFSET_EXTENDED: libc::c_ulong =
                0xc000_0000 | ((size_of::<Extended>() as libc::c_ulong) << 16) | (0x3d << 8) | 9;
            let mut request = Extended {
                samples: 9,
                clock: id_of(system),
                reserved: [0; 2],
                times: [[Time::default(); 3]; 25],
            };
            // SAFETY: `request` is the struct the ioctl takes, and lives
            // through the call.
            if unsafe {
                libc::ioctl(
                    self._fd.as_raw_fd(),
                    PTP_SYS_OFFSET_EXTENDED as _,
                    &raw mut request,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            let nanos = |time: Time| {
                i128::from(time.seconds) * 1_000_000_000 + i128::from(time.nanoseconds)
            };
            request.times[..request.samples.min(25) as usize]
                .iter()
                .map(|[before, clock, after]| {
                    let (before, after) = (nanos(*before), nanos(*after));
                    Reading {
                        offset: nanos(*clock) - (before + after) / 2,
                        span: u64::try_from(after - before).unwrap_or(u64::MAX),
                    }
                })
                .min_by_key(|reading| reading.span)
                .ok_or_else(|| io::Error::other("the clock gave no samples"))
        }
    }

    pub fn monotonic_now() -> Duration {
        read(libc::CLOCK_MONOTONIC).unwrap_or_default()
    }

    fn id_of(system: SystemClock) -> libc::clockid_t {
        match system {
            SystemClock::Realtime => libc::CLOCK_REALTIME,
            SystemClock::Monotonic => libc::CLOCK_MONOTONIC,
            SystemClock::MonotonicRaw => libc::CLOCK_MONOTONIC_RAW,
            SystemClock::Tai => libc::CLOCK_TAI,
        }
    }

    pub fn system_now(system: SystemClock) -> Duration {
        read(id_of(system)).unwrap_or_default()
    }

    pub fn suspended() -> Duration {
        let boot = read(libc::CLOCK_BOOTTIME).unwrap_or_default();
        boot.saturating_sub(monotonic_now())
    }

    pub fn precise_sleeps() {
        // SAFETY: prctl with PR_SET_TIMERSLACK takes plain integers and
        // changes only the calling thread.
        unsafe {
            libc::prctl(libc::PR_SET_TIMERSLACK, 1 as libc::c_ulong);
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::io;
    use std::time::Duration;

    pub struct HardwareClock;

    impl HardwareClock {
        pub fn open(_index: u32, _adjustable: bool) -> io::Result<Self> {
            Err(unsupported())
        }

        pub fn now(&self) -> io::Result<Duration> {
            Ok(Duration::ZERO)
        }

        pub fn frequency_adjustment(&self) -> io::Result<f64> {
            Err(unsupported())
        }

        pub fn set_frequency_adjustment(&self, _ppb: f64) -> io::Result<()> {
            Err(unsupported())
        }

        pub fn reading_from_system(
            &self,
            _system: super::SystemClock,
        ) -> io::Result<super::Reading> {
            Err(unsupported())
        }
    }

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "hardware clocks are not supported on this system yet",
        )
    }

    pub fn monotonic_now() -> Duration {
        use std::sync::OnceLock;
        static START: OnceLock<std::time::Instant> = OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed()
    }

    pub fn system_now(_system: super::SystemClock) -> Duration {
        monotonic_now()
    }

    pub fn suspended() -> Duration {
        Duration::ZERO
    }

    pub fn precise_sleeps() {}
}

#[cfg(test)]
mod comparison_tests {
    use super::*;

    #[test]
    fn a_steady_drift_is_followed_between_comparisons() {
        let mut comparison = Comparison::default();
        // 6 ppm apart: 600 ns more every 100 ms.
        for step in 0..30i128 {
            comparison.update(step * 100_000_000, 1_000 + step * 600);
        }
        let last = 29 * 100_000_000;
        assert_eq!(comparison.offset_at(last), 1_000 + 29 * 600);
        assert_eq!(
            comparison.offset_at(last + 50_000_000),
            1_000 + 29 * 600 + 300
        );
        assert_eq!(
            comparison.offset_at(last - 50_000_000),
            1_000 + 29 * 600 - 300
        );
        assert!((comparison.rate().unwrap() - 6e-6).abs() < 1e-12);
    }

    #[test]
    fn the_error_of_one_comparison_is_mostly_filtered_out() {
        let mut comparison = Comparison::default();
        // 6 ppm apart, each comparison 50 ns off one way or the other.
        for step in 0..20i128 {
            let error = if step % 2 == 0 { 50 } else { -50 };
            comparison.update(step * 100_000_000, step * 600 + error);
        }
        let last = 19 * 100_000_000;
        let off = comparison.offset_at(last) - 19 * 600;
        assert!(off.abs() <= 15, "off by {off} ns");
    }

    #[test]
    fn one_comparison_is_taken_as_it_is() {
        let mut comparison = Comparison::default();
        assert!(comparison.is_empty());
        assert_eq!(comparison.offset_at(5), 0);
        comparison.update(10, 42);
        assert_eq!(comparison.offset_at(1_000_000), 42);
        assert_eq!(comparison.rate(), None);
        comparison.clear();
        assert!(comparison.is_empty());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_hardware_clock_reads_when_there_is_one() {
        // Computers without /dev/ptp0, or not allowed to read it, skip it.
        let Ok(clock) = HardwareClock::open(0) else {
            return;
        };
        // The grandmaster keeps whatever timescale it has; the clock only
        // has to run on, at the monotonic clock's pace give or take.
        let first = clock.offset_from_monotonic().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let second = clock.offset_from_monotonic().unwrap();
        assert!((second - first).abs() < 100_000, "{first} then {second}");
    }

    #[test]
    fn the_monotonic_clock_moves_on() {
        let before = monotonic_now();
        std::thread::sleep(Duration::from_millis(2));
        assert!(monotonic_now() > before);
    }
}
