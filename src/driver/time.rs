/// Timer wheel using CLOCK_BOOTTIME instants if available
use std::collections::BinaryHeap;
use std::time::Duration;

use boot_time::Instant;

use crate::driver::Entry;

#[derive(Debug)]
struct Timer {
    key: usize,
    deadline: Instant,
}

impl PartialEq for Timer {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for Timer {}

impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // we reverse ordering to work with MaxHeap
        other.deadline.cmp(&self.deadline)
    }
}

#[repr(transparent)]
pub(super) struct TimerWheel(BinaryHeap<Timer>);

impl TimerWheel {
    pub(super) fn with_capacity(cap: usize) -> Self {
        Self(BinaryHeap::with_capacity(cap))
    }

    pub(super) fn insert(&mut self, key: usize, delay: Duration) {
        let deadline = Instant::now() + delay;
        let timer = Timer { key, deadline };
        self.0.push(timer);
    }

    pub(super) fn duration_till_next_timer(&self) -> Option<Duration> {
        self.0
            .peek()
            .map(|timer| timer.deadline.saturating_duration_since(Instant::now()))
    }

    pub(super) fn till_next_timer_or_timeout(&self, timeout: Option<Duration>) -> Option<Duration> {
        if let Some(next_timer_delay) = self.duration_till_next_timer() {
            Some(
                timeout
                    .map(|t| t.min(next_timer_delay))
                    .unwrap_or(next_timer_delay),
            )
        } else {
            timeout
        }
    }

    /// Return expired flag and duration till the next timer
    pub(super) fn expire_timers(&mut self, entries: &mut impl Extend<Entry>) {
        let now = Instant::now();

        while let Some(timer) = self.0.peek() {
            let duration_till_next_timer = timer.deadline.saturating_duration_since(now);
            if duration_till_next_timer == Duration::ZERO {
                let timer = self.0.pop().expect("timer present");
                entries.extend(Some(Entry::new(timer.key, Ok(0))));
            } else {
                break;
            }
        }
    }
}

impl Extend<(usize, Duration)> for TimerWheel {
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=(usize, Duration)> {
        let now = Instant::now();
        self.0.extend(iter.into_iter().map(|(key, delay)| Timer { key, deadline: now + delay }));
    }
}

/// Timeout operation completes after the given relative timeout duration.
///
/// If supported by platform timeout operation will take into account the time
/// spent in low power modes or suspend (CLOCK_BOOTTIME). Otherwise
/// CLOCK_MONOTONIC is used.
///
/// Only io_uring driver supports waiting using CLOCK_BOOTTIME clock.
#[repr(transparent)]
pub struct Timeout {
    pub(crate) delay: std::time::Duration,
}

impl Timeout {
    /// Create `Timeout` with the provided duration.
    pub fn new(delay: std::time::Duration) -> Self {
        Self { delay }
    }
}
