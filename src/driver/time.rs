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

    pub fn duration_till_next_timer(&self) -> Option<Duration> {
        self.0
            .peek()
            .map(|timer| timer.deadline.saturating_duration_since(Instant::now()))
    }

    pub fn expire_timers(&mut self, entries: &mut impl Extend<Entry>) {
        let now = Instant::now();
        while let Some(timer) = self.0.peek() {
            if timer.deadline.saturating_duration_since(now) == Duration::ZERO {
                let timer = self.0.pop().expect("timer present");
                entries.extend(Some(Entry::new(timer.key, Ok(0))));
            }
            else {
                break
            }
        }
    }
}
