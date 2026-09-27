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
//! Elsewhere the timer falls back to tokio's own.

pub use imp::Timer;

#[cfg(target_os = "linux")]
mod imp {
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

    pub struct Timer {
        fd: AsyncFd<Fd>,
    }

    impl Timer {
        pub fn new() -> io::Result<Self> {
            let flags = TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC;
            let fd = TimerFd::new(ClockId::CLOCK_MONOTONIC, flags)?;
            Ok(Timer {
                fd: AsyncFd::with_interest(Fd(fd), Interest::READABLE)?,
            })
        }

        /// Waits until `at`; a deadline already passed returns at once.
        pub async fn sleep_until(&mut self, at: Instant) -> io::Result<()> {
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
            loop {
                let mut guard = self.fd.readable().await?;
                let read = guard.try_io(|fd| {
                    let mut count = [0u8; 8];
                    nix::unistd::read(&fd.get_ref().0, &mut count).map_err(io::Error::from)
                });
                match read {
                    // An expiration left behind by a wait abandoned earlier
                    // reads the same as the real one; the clock tells them apart.
                    Ok(Ok(_)) if Instant::now() >= at => return Ok(()),
                    Ok(Ok(_)) => continue,
                    Ok(Err(err)) => return Err(err),
                    // Readiness left over from an earlier read; wait again.
                    Err(_would_block) => continue,
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::io;
    use std::time::Instant;

    pub struct Timer;

    impl Timer {
        pub fn new() -> io::Result<Self> {
            Ok(Timer)
        }

        pub async fn sleep_until(&mut self, at: Instant) -> io::Result<()> {
            tokio::time::sleep_until(at.into()).await;
            Ok(())
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::time::{Duration, Instant};

    use super::Timer;

    #[tokio::test]
    async fn wakes_within_a_fraction_of_a_millisecond() {
        let mut timer = Timer::new().unwrap();
        let start = Instant::now();
        timer
            .sleep_until(start + Duration::from_micros(300))
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_micros(300), "{elapsed:?}");
        assert!(
            elapsed < Duration::from_micros(900),
            "woke {elapsed:?} after a 300 µs deadline: millisecond-grained?"
        );

        // A deadline already passed fires at once, and an abandoned wait
        // leaves nothing behind for the next one.
        let start = Instant::now();
        timer
            .sleep_until(start - Duration::from_millis(5))
            .await
            .unwrap();
        assert!(start.elapsed() < Duration::from_micros(500));
        let abandoned = tokio::time::timeout(
            Duration::from_millis(1),
            timer.sleep_until(Instant::now() + Duration::from_millis(20)),
        )
        .await;
        assert!(abandoned.is_err(), "the wait should have been cut short");
        // The abandoned timer expires unobserved in the meantime.
        tokio::time::sleep(Duration::from_millis(25)).await;
        let start = Instant::now();
        timer
            .sleep_until(start + Duration::from_millis(2))
            .await
            .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(2));
    }
}
