/// Timer wheel using CLOCK_BOOTTIME instants if available
use boot_time::Instant;

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
        self.deadline.cmp(&other.deadline)
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
        let timer = Timer { key, dealine };
        self.0.push(timer);
    }

    pub fn duration_till_next_timer(&self) -> Option<Duration> {
        self.0
            .peek()
            .map(|timer| timer.deadline.saturating_duration_since(Instant::now()))
    }
}
