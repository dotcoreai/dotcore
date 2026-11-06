//! Supplies the backend's time source: monotonic instants for leases, ages and expiries, wall time
//! for timestamps aligned across machines, and waits that end when that time reaches a deadline.
//!
//! Production reads system time. Backend tests can compose a manual clock instead, which follows
//! real time plus an offset they advance, so a lease expires without waiting for it in real time;
//! any wait on that clock wakes as soon as an advance reaches its deadline. Unit tests that must
//! decide exactly when each wait wakes use a virtual clock, whose time moves only when advanced. Code that reads this
//! clock waits through it too, never through a separate timer, so both stay on one timeline.

use std::{
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::time::Instant;

/// Future returned by [`Clock::sleep_until`].
pub type ClockSleep = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Time source of one composed backend, so every decision it compares against the same time
/// reads one timeline.
pub trait Clock: Send + Sync {
    /// Monotonic time, for leases, ages and expiries.
    fn now(&self) -> Instant;

    /// Wall time, for timestamps compared across machines; it can jump when the host clock is
    /// set, so never measure durations with it.
    fn wall_now(&self) -> SystemTime;

    /// Completes once [`Self::now`] has reached `deadline`.
    fn sleep_until(&self, deadline: Instant) -> ClockSleep;
}

/// Returns the latest multiple of `interval` since the Unix epoch at or before `wall`, so every
/// node cuts time into the same slots. A zero `interval` counts as one nanosecond.
pub fn slot_start(wall: SystemTime, interval: Duration) -> SystemTime {
    let interval = interval.as_nanos().max(1);
    let since_epoch = wall
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    UNIX_EPOCH + nanos(since_epoch - since_epoch % interval)
}

/// Returns the first multiple of `interval` since the Unix epoch strictly after `wall`.
pub fn next_boundary(wall: SystemTime, interval: Duration) -> SystemTime {
    slot_start(wall, interval) + interval.max(Duration::from_nanos(1))
}

fn nanos(nanos: u128) -> Duration {
    Duration::new(
        u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX),
        (nanos % 1_000_000_000) as u32,
    )
}

/// Production clock: Tokio monotonic time, which also stops under Tokio's paused test time.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wall_now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep_until(&self, deadline: Instant) -> ClockSleep {
        Box::pin(tokio::time::sleep_until(deadline))
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use manual::ManualClock;
#[cfg(any(test, feature = "test-support"))]
pub use virtual_time::VirtualClock;

#[cfg(any(test, feature = "test-support"))]
mod manual {
    use std::{
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use tokio::{sync::watch, time::Instant};

    use super::{Clock, ClockSleep};

    /// Test clock: real time plus an offset that only grows, for both monotonic and wall time.
    /// Its wall time starts at the host's, or at a fixed origin, so wall-aligned schedules start
    /// at a known phase.
    ///
    /// Clones share one offset, so a test keeps a handle to the clock it composes a backend with
    /// and advances that backend's time through it.
    #[derive(Clone)]
    pub struct ManualClock {
        offset: Arc<watch::Sender<Duration>>,

        /// Wall time at `origin.1`, when the clock has a fixed origin.
        origin: Option<(SystemTime, Instant)>,
    }

    impl ManualClock {
        /// Creates a clock that starts at real time.
        pub fn new() -> Self {
            Self {
                offset: Arc::new(watch::Sender::new(Duration::ZERO)),
                origin: None,
            }
        }

        /// Creates a clock whose wall time starts at `wall` and then follows real time.
        pub fn starting_at(wall: SystemTime) -> Self {
            Self {
                origin: Some((wall, Instant::now())),
                ..Self::new()
            }
        }

        /// Moves this clock `by` ahead of real time and wakes every wait it reaches.
        pub fn advance(&self, by: Duration) {
            self.offset.send_modify(|offset| *offset += by);
        }

        /// Returns how far this clock is ahead of real time.
        pub fn offset(&self) -> Duration {
            *self.offset.borrow()
        }
    }

    impl Default for ManualClock {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            Instant::now() + self.offset()
        }

        fn wall_now(&self) -> SystemTime {
            let real = match self.origin {
                Some((wall, started)) => wall + started.elapsed(),
                None => SystemTime::now(),
            };
            real + self.offset()
        }

        fn sleep_until(&self, deadline: Instant) -> ClockSleep {
            // Subscribing before the first read means an advance made at any later point marks
            // the receiver changed, so it cannot be missed between the check and the wait.
            let mut offset = self.offset.subscribe();
            Box::pin(async move {
                loop {
                    let current = *offset.borrow_and_update();
                    let now = Instant::now();
                    if now + current >= deadline {
                        return;
                    }
                    let Some(real_deadline) = deadline.checked_sub(current) else {
                        return;
                    };
                    tokio::select! {
                        () = tokio::time::sleep_until(real_deadline) => {}
                        changed = offset.changed() => {
                            if changed.is_err() {
                                // The clock is gone, so its offset is final: wait on real time
                                // alone.
                                tokio::time::sleep_until(real_deadline).await;
                                return;
                            }
                        }
                    }
                }
            })
        }
    }

    impl PartialEq for ManualClock {
        fn eq(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.offset, &other.offset)
        }
    }

    impl Eq for ManualClock {}

    impl std::fmt::Debug for ManualClock {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("ManualClock")
                .field("offset", &self.offset())
                .finish()
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
mod virtual_time {
    use std::{
        collections::BTreeMap,
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use tokio::{sync::watch, time::Instant};

    use super::{Clock, ClockSleep};

    /// Test clock whose monotonic and wall time move only when the test advances them, and which
    /// tells the test when its next wait is due, so code under test wakes exactly at its deadlines
    /// whatever real time passes. Wall time can also step back or forward alone, like a corrected host clock.
    ///
    /// Clones share one timeline.
    #[derive(Clone)]
    pub struct VirtualClock {
        base: Instant,
        base_wall: SystemTime,
        state: Arc<watch::Sender<VirtualTime>>,
    }

    struct VirtualTime {
        offset: Duration,
        wall_step_back: Duration,
        wall_step_forward: Duration,

        /// Deadlines of the pending waits, with how many wait on each.
        sleepers: BTreeMap<Instant, usize>,
    }

    impl VirtualClock {
        /// Creates a clock whose wall time starts at `wall`.
        pub fn starting_at(wall: SystemTime) -> Self {
            Self {
                base: Instant::now(),
                base_wall: wall,
                state: Arc::new(watch::Sender::new(VirtualTime {
                    offset: Duration::ZERO,
                    wall_step_back: Duration::ZERO,
                    wall_step_forward: Duration::ZERO,
                    sleepers: BTreeMap::new(),
                })),
            }
        }

        /// Moves monotonic and wall time `by` ahead and wakes every wait it reaches.
        pub fn advance(&self, by: Duration) {
            self.state.send_modify(|time| time.offset += by);
        }

        /// Steps wall time `by` back while monotonic time stays.
        pub fn step_wall_back(&self, by: Duration) {
            self.state.send_modify(|time| time.wall_step_back += by);
        }

        /// Steps wall time `by` ahead while monotonic time stays.
        pub fn step_wall_forward(&self, by: Duration) {
            self.state.send_modify(|time| time.wall_step_forward += by);
        }

        /// Waits until some wait is pending past the current time, then advances exactly to the
        /// earliest such deadline and returns it.
        pub async fn advance_to_next_wake(&self) -> Instant {
            self.advance_to_next_wake_within(Duration::MAX).await
        }

        /// Like [`Self::advance_to_next_wake`], but only considers waits due within `window` of
        /// the current time, so far-off safety bounds are never the wake that gets advanced to.
        pub async fn advance_to_next_wake_within(&self, window: Duration) -> Instant {
            let mut state = self.state.subscribe();
            let base = self.base;
            let deadline = {
                let time = state
                    .wait_for(|time| {
                        let now = base + time.offset;
                        time.sleepers
                            .range(now..)
                            .any(|(deadline, _)| *deadline > now && within(*deadline, now, window))
                    })
                    .await
                    .expect("the clock owns its sender");
                let now = base + time.offset;
                *time
                    .sleepers
                    .keys()
                    .find(|deadline| **deadline > now)
                    .expect("a pending wait")
            };
            self.state.send_modify(|time| time.offset = deadline - base);
            deadline
        }
    }

    fn within(deadline: Instant, now: Instant, window: Duration) -> bool {
        deadline - now <= window
    }

    impl Clock for VirtualClock {
        fn now(&self) -> Instant {
            self.base + self.state.borrow().offset
        }

        fn wall_now(&self) -> SystemTime {
            let time = self.state.borrow();
            self.base_wall + time.offset + time.wall_step_forward - time.wall_step_back
        }

        fn sleep_until(&self, deadline: Instant) -> ClockSleep {
            let state = Arc::clone(&self.state);
            let base = self.base;
            Box::pin(async move {
                let _registration = SleepRegistration::new(Arc::clone(&state), deadline);
                let mut changes = state.subscribe();
                loop {
                    if base + changes.borrow_and_update().offset >= deadline {
                        return;
                    }
                    if changes.changed().await.is_err() {
                        return;
                    }
                }
            })
        }
    }

    /// Lists one pending wait's deadline for as long as the wait lives.
    struct SleepRegistration {
        state: Arc<watch::Sender<VirtualTime>>,
        deadline: Instant,
    }

    impl SleepRegistration {
        fn new(state: Arc<watch::Sender<VirtualTime>>, deadline: Instant) -> Self {
            state.send_modify(|time| *time.sleepers.entry(deadline).or_default() += 1);
            Self { state, deadline }
        }
    }

    impl Drop for SleepRegistration {
        fn drop(&mut self) {
            self.state.send_modify(|time| {
                if let Some(count) = time.sleepers.get_mut(&self.deadline) {
                    *count -= 1;
                    if *count == 0 {
                        time.sleepers.remove(&self.deadline);
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn manual_sleep_completes_at_its_real_deadline() {
        let clock = ManualClock::new();
        let start = Instant::now();
        clock.sleep_until(start + Duration::from_secs(10)).await;

        assert_eq!(Instant::now() - start, Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn an_advance_completes_a_sleep_without_real_time_passing() {
        let clock = ManualClock::new();
        let start = Instant::now();
        let sleep = tokio::spawn(clock.sleep_until(start + Duration::from_secs(10)));
        tokio::task::yield_now().await;

        clock.advance(Duration::from_secs(10));
        sleep.await.expect("sleep task");

        assert_eq!(Instant::now(), start);
        assert_eq!(clock.now(), start + Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn an_advance_before_the_sleep_is_polled_is_not_lost() {
        let clock = ManualClock::new();
        let start = Instant::now();
        let sleep = clock.sleep_until(start + Duration::from_secs(10));

        clock.advance(Duration::from_secs(10));
        sleep.await;

        assert_eq!(Instant::now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn a_partial_advance_keeps_waiting_for_the_rest() {
        let clock = ManualClock::new();
        let start = Instant::now();
        let sleep = tokio::spawn(clock.sleep_until(start + Duration::from_secs(10)));
        tokio::task::yield_now().await;

        clock.advance(Duration::from_secs(4));
        tokio::task::yield_now().await;
        assert!(!sleep.is_finished());

        tokio::time::sleep(Duration::from_secs(6)).await;
        sleep.await.expect("sleep task");
        assert_eq!(Instant::now() - start, Duration::from_secs(6));
    }

    #[test]
    fn an_advance_moves_wall_time_by_the_same_offset() {
        let clock = ManualClock::new();
        let before = SystemTime::now();

        clock.advance(Duration::from_secs(60));

        let wall = clock.wall_now();
        assert!(wall >= before + Duration::from_secs(60));
        assert!(wall <= SystemTime::now() + Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn a_fixed_origin_starts_wall_time_there_and_follows_real_time_and_advances() {
        let origin = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let clock = ManualClock::starting_at(origin);
        assert_eq!(clock.wall_now(), origin);

        tokio::time::sleep(Duration::from_secs(2)).await;
        clock.advance(Duration::from_secs(3));

        assert_eq!(clock.wall_now(), origin + Duration::from_secs(5));
    }

    #[test]
    fn clones_share_one_offset() {
        let clock = ManualClock::new();
        let clone = clock.clone();

        clone.advance(Duration::from_secs(3));

        assert_eq!(clock.offset(), Duration::from_secs(3));
        assert_eq!(clock, clone);
        assert_ne!(clock, ManualClock::new());
    }

    #[tokio::test]
    async fn a_virtual_clock_wakes_a_wait_exactly_at_its_deadline() {
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let clock = VirtualClock::starting_at(wall);
        let start = clock.now();
        let sleep = tokio::spawn(clock.sleep_until(start + Duration::from_millis(750)));

        let woke_at = clock.advance_to_next_wake().await;
        sleep.await.expect("sleep task");

        assert_eq!(woke_at, start + Duration::from_millis(750));
        assert_eq!(clock.now(), woke_at);
        assert_eq!(clock.wall_now(), wall + Duration::from_millis(750));
    }

    #[test]
    fn a_virtual_wall_step_back_leaves_monotonic_time() {
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let clock = VirtualClock::starting_at(wall);
        let start = clock.now();

        clock.advance(Duration::from_secs(10));
        clock.step_wall_back(Duration::from_secs(60));

        assert_eq!(clock.now(), start + Duration::from_secs(10));
        assert_eq!(clock.wall_now(), wall - Duration::from_secs(50));
    }
}
