use std::{
    collections::BinaryHeap,
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use slab::Slab;

pub struct TimerRuntime {
    time: Instant,
    tasks: Slab<Option<Waker>>,
}

impl TimerRuntime {
    pub fn new() -> Self {
        Self {
            time: Instant::now(),
            tasks: Slab::default(),
            wheel: BinaryHeap::default(),
        }
    }

    pub fn contains(&self, key: usize) -> bool {
        self.tasks.contains(key)
    }

    pub fn update_waker(&mut self, key: usize, waker: Waker) {
        if let Some(w) = self.tasks.get_mut(key) {
            *w = Some(waker);
        }
    }

    pub fn cancel(&mut self, key: usize) {
        self.tasks.remove(key);
    }

    pub fn wake(&mut self) {
        let elapsed = self.time.elapsed();
        while let Some(entry) = self.wheel.pop() {
            if entry.delay <= elapsed {
                if self.tasks.contains(entry.key) {
                    if let Some(waker) = self.tasks.remove(entry.key) {
                        waker.wake();
                    }
                }
            } else {
                self.wheel.push(entry);
                break;
            }
        }
    }
}

pub struct TimerFuture {
    key: usize,
    completed: bool,
}

impl TimerFuture {
    pub fn new(key: usize) -> Self {
        Self {
            key,
            completed: false,
        }
    }
}

impl Future for TimerFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = crate::task::RUNTIME.with(|runtime| runtime.poll_timer(cx, self.key));
        if res.is_ready() {
            self.get_mut().completed = true;
        }
        res
    }
}

impl Drop for TimerFuture {
    fn drop(&mut self) {
        if !self.completed {
            crate::task::RUNTIME.with(|runtime| runtime.cancel_timer(self.key));
        }
    }
}
