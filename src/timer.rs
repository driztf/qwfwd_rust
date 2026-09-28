//! A deadline timer for the event loop with better than millisecond
//! precision where the platform offers it.
//!
//! tokio's timer wheel has a resolution of 1 ms and rounds deadlines up, so
//! a paced release waited on with it lands up to a millisecond late, and
//! because the pacer counts slots from when the previous one was due rather
//! than from when it fired, that lateness shows up as jitter in the spacing
//! between packets. On Linux a timerfd is driven by the kernel's
//! high-resolution timers instead, with tens of microseconds of slack, and
//! tokio can wait for it to become readable like any other file descriptor.
//! Elsewhere, and on a Linux that refuses a timerfd (a seccomp profile,
//! say), the timer falls back to tokio's own.

use std::time::Instant;

use crate::cprint;

pub struct Timer {
    fine: Option<fine::Fine>,
}

impl Timer {
    pub fn new() -> Self {
        let fine = match fine::Fine::new() {
            Ok(fine) => fine,
            Err(err) => {
                if let Some(err) = err {
                    cprint!(
                        "no high-resolution timer ({err}); releases are timed to the millisecond\n"
                    );
                }
                None
            }
        };
        Timer { fine }
    }

    /// What times the releases, for the startup message.
    pub fn description(&self) -> &'static str {
        if self.fine.is_some() {
            "the kernel's high-resolution timer"
        } else {
            "the runtime's millisecond timer"
        }
    }

    /// Waits until `at`; a deadline already passed returns at once.
    pub async fn sleep_until(&mut self, at: Instant) {
        if let Some(fine) = &mut self.fine
            && fine.sleep_until(at).await.is_ok()
        {
            return;
        }
        tokio::time::sleep_until(at.into()).await;
    }
}

#[cfg(target_os = "linux")]
mod fine {
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, RawFd};
    use std::time::{Duration, Instant};

    use nix::sys::time::TimeSpec;
    use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;

    /// tokio registers file descriptors by raw number, which nix's timer
    /// only exposes through `AsFd`.
    struct Fd(TimerFd);

    impl AsRawFd for Fd {
        fn as_raw_fd(&self) -> RawFd {
            self.0.as_fd().as_raw_fd()
        }
    }

    pub struct Fine {
        fd: AsyncFd<Fd>,
        /// The deadline the timer is armed for, so waiting for the same one
        /// again (the loop rebuilds its wait after every event) costs no
        /// system call.
        armed: Option<Instant>,
    }

    impl Fine {
        /// `Err(None)` means the platform has no such timer; `Err(Some)` that
        /// it refused one.
        pub fn new() -> Result<Option<Self>, Option<io::Error>> {
            let flags = TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC;
            let fd = TimerFd::new(ClockId::CLOCK_MONOTONIC, flags).map_err(io::Error::from)?;
            let fd = AsyncFd::with_interest(Fd(fd), Interest::READABLE)?;
            Ok(Some(Fine { fd, armed: None }))
        }

        fn arm(&mut self, at: Instant) -> io::Result<()> {
            // A zero expiration would disarm the timer, hence the nanosecond
            // floor for a deadline already passed.
            let wait = at
                .saturating_duration_since(Instant::now())
                .max(Duration::from_nanos(1));
            let expiration = Expiration::OneShot(TimeSpec::from_duration(wait));
            self.fd
                .get_ref()
                .0
                .set(expiration, TimerSetTimeFlags::empty())?;
            self.armed = Some(at);
            Ok(())
        }

        pub async fn sleep_until(&mut self, at: Instant) -> io::Result<()> {
            if self.armed != Some(at) {
                self.arm(at)?;
            }
            loop {
                let mut guard = self.fd.readable().await?;
                let read = guard.try_io(|fd| {
                    let mut count = [0u8; 8];
                    nix::unistd::read(&fd.get_ref().0, &mut count).map_err(io::Error::from)
                });
                match read {
                    Ok(Ok(_)) => {
                        // One-shot: nothing more to read until it is armed again.
                        guard.clear_ready();
                        self.armed = None;
                        if Instant::now() >= at {
                            return Ok(());
                        }
                        // Woken early (the expiration of a wait abandoned
                        // before this one, say): arm for what is left.
                        self.arm(at)?;
                    }
                    Ok(Err(err)) => return Err(err),
                    // Readiness left over from an earlier read; wait again.
                    Err(_would_block) => continue,
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod fine {
    use std::io;
    use std::time::Instant;

    pub enum Fine {}

    impl Fine {
        pub fn new() -> Result<Option<Self>, Option<io::Error>> {
            Err(None)
        }

        pub async fn sleep_until(&mut self, _at: Instant) -> io::Result<()> {
            match *self {}
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::time::{Duration, Instant};

    use super::Timer;

    #[tokio::test]
    async fn wakes_within_a_fraction_of_a_millisecond() {
        let mut timer = Timer::new();
        assert!(timer.fine.is_some(), "no timerfd on this Linux?");

        // Best of several tries, so a preempted thread does not fail the test.
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let start = Instant::now();
            timer.sleep_until(start + Duration::from_micros(300)).await;
            let elapsed = start.elapsed();
            assert!(elapsed >= Duration::from_micros(300), "{elapsed:?}");
            best = best.min(elapsed);
        }
        assert!(
            best < Duration::from_micros(900),
            "woke {best:?} after a 300 µs deadline at best: millisecond-grained?"
        );

        // A deadline already passed fires at once.
        let start = Instant::now();
        timer.sleep_until(start - Duration::from_millis(5)).await;
        assert!(start.elapsed() < Duration::from_millis(2));

        // A wait abandoned before it fired leaves nothing behind for the
        // next one, whose deadline is honoured in full.
        let abandoned = tokio::time::timeout(
            Duration::from_millis(1),
            timer.sleep_until(Instant::now() + Duration::from_millis(20)),
        )
        .await;
        assert!(abandoned.is_err(), "the wait should have been cut short");
        tokio::time::sleep(Duration::from_millis(25)).await;
        let start = Instant::now();
        timer.sleep_until(start + Duration::from_millis(2)).await;
        assert!(start.elapsed() >= Duration::from_millis(2));
    }
}
