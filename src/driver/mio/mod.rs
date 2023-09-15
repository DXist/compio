#[cfg(feature = "allocator_api")]
use std::alloc::Allocator;
#[doc(no_inline)]
pub use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::{
    collections::{HashMap, HashSet},
    io,
    ops::ControlFlow,
    time::Duration,
};

pub(crate) use libc::{sockaddr_storage, socklen_t};
use mio::{
    event::{Event, Source},
    unix::SourceFd,
    Events, Interest, Poll, Registry, Token,
};

#[cfg(feature = "time")]
use crate::driver::time::TimerWheel;
use crate::{
    driver::{CompleteIo, Entry, OpObject, Operation},
    vec_deque_alloc,
};

pub(crate) mod op;

/// Invalid file descriptor value could be used as an initial value of uninitialized file descriptor
pub const INVALID_FD: RawFd = -1;

/// Abstraction of operations.
pub trait OpCode {
    /// Perform the operation before submit, and return [`Decision`] to
    /// indicate whether submitting the operation to mio is required.
    fn pre_submit(self: &mut Self) -> io::Result<Decision>;

    /// Perform the operation after received corresponding
    /// event.
    fn on_event(self: &mut Self, event: &Event) -> io::Result<ControlFlow<usize>>;

    /// Only timers implement this method
    #[cfg(feature = "time")]
    fn timer_delay(&self) -> std::time::Duration {
        unimplemented!("operation is not a timer")
    }
}

/// Result of [`OpCode::pre_submit`].
pub enum Decision {
    /// Instant operation, no need to submit
    Completed(usize),
    /// Async operation, needs to submit
    Wait(WaitArg),
}

impl Decision {
    /// Decide to wait for the given fd with the given interest.
    pub fn wait_for(fd: RawFd, interest: Interest) -> Self {
        Self::Wait(WaitArg { fd, interest })
    }

    /// Decide to wait for the given fd to be readable.
    pub fn wait_readable(fd: RawFd) -> Self {
        Self::wait_for(fd, Interest::READABLE)
    }

    /// Decide to wait for the given fd to be writable.
    pub fn wait_writable(fd: RawFd) -> Self {
        Self::wait_for(fd, Interest::WRITABLE)
    }
}

/// Meta of mio operations.
#[derive(Debug, Clone, Copy)]
pub struct WaitArg {
    fd: RawFd,
    interest: Interest,
}

#[cfg(feature = "time")]
const TIMER_PENDING: usize = usize::MAX - 2;

/// Low-level driver of mio.
pub struct Driver<'arena> {
    squeue: Vec<OpObject<'arena>>,
    events: Events,
    #[cfg(feature = "time")]
    timers: TimerWheel,
    poll: Poll,
    waiting: HashMap<usize, WaitEntry<'arena>>,
    cancelled: HashSet<usize>,
}

/// Entry waiting for events
struct WaitEntry<'arena> {
    op: &'arena mut dyn OpCode,
    arg: WaitArg,
    user_data: usize,
}

impl<'arena> WaitEntry<'arena> {
    fn new(op: &'arena mut dyn OpCode, user_data: usize, arg: WaitArg) -> Self {
        Self { op, arg, user_data }
    }
}

impl<'arena> Driver<'arena> {
    /// Create a new mio driver with 1024 entries.
    pub fn new() -> io::Result<Self> {
        Self::with_entries(1024)
    }

    /// Create a new mio driver with the given number of entries.
    pub fn with_entries(entries: u32) -> io::Result<Self> {
        let entries = entries as usize; // for the sake of consistency, use u32 like iour

        Ok(Self {
            squeue: Vec::with_capacity(entries),
            events: Events::with_capacity(entries),
            #[cfg(feature = "time")]
            timers: TimerWheel::with_capacity(16),
            poll: Poll::new()?,
            waiting: HashMap::new(),
            cancelled: HashSet::new(),
        })
    }

    fn submit(
        cancelled: &mut HashSet<usize>,
        waiting: &mut HashMap<usize, WaitEntry<'arena>>,
        registry: &Registry,
        op: &'arena mut dyn OpCode,
        user_data: usize,
        arg: WaitArg,
    ) -> io::Result<()> {
        if !cancelled.remove(&user_data) {
            let token = Token(user_data);

            SourceFd(&arg.fd).register(registry, token, arg.interest)?;

            // Only insert the entry after it was registered successfully
            waiting.insert(user_data, WaitEntry::new(op, user_data, arg));
        }
        Ok(())
    }
}

impl<'arena> CompleteIo<'arena> for Driver<'arena> {
    #[inline]
    fn attach(&mut self, _fd: RawFd) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn try_cancel(&mut self, user_data: usize) -> Result<(), ()> {
        if let Some(entry) = self.waiting.remove(&user_data) {
            self.poll
                .registry()
                .deregister(&mut SourceFd(&entry.arg.fd))
                .ok();
        } else {
            self.cancelled.insert(user_data);
        }
        Ok(())
    }

    #[inline]
    fn try_push<O: OpCode>(
        &mut self,
        op: Operation<'arena, O>,
    ) -> Result<(), Operation<'arena, O>> {
        if self.capacity_left() > 0 {
            self.squeue.push(OpObject::from(op));
            Ok(())
        } else {
            Err(op)
        }
    }

    #[inline]
    fn try_push_dyn(&mut self, op: OpObject<'arena>) -> Result<(), OpObject<'arena>> {
        if self.capacity_left() > 0 {
            self.squeue.push(op);
            Ok(())
        } else {
            Err(op)
        }
    }

    #[inline]
    fn push_queue<#[cfg(feature = "allocator_api")] A: Allocator + Unpin + 'arena>(
        &mut self,
        ops_queue: &mut vec_deque_alloc!(OpObject<'arena>, A),
    ) {
        let till = self.capacity_left().min(ops_queue.len());
        self.squeue.extend(ops_queue.drain(..till));
    }

    #[inline]
    fn capacity_left(&self) -> usize {
        self.squeue.capacity() - self.squeue.len()
    }

    unsafe fn submit_and_wait_completed(
        &mut self,
        timeout: Option<Duration>,
        entries: &mut impl Extend<Entry>,
    ) -> io::Result<()> {
        let cancelled = &mut self.cancelled;
        let waiting = &mut self.waiting;

        let mut at_least_one_completion = false;

        let submit_squeue_iter = {
            let registry = self.poll.registry();

            self.squeue.drain(..).filter_map(|entry| {
                let user_data = entry.user_data;
                let op = entry.op;
                // io buffers are Unpin so no need to pin
                match op.pre_submit() {
                    Ok(Decision::Wait(arg)) => {
                        if let Err(err) =
                            Self::submit(cancelled, waiting, registry, op, user_data, arg)
                        {
                            at_least_one_completion = true;
                            Some(Entry::new(user_data, Err(err)))
                        } else {
                            None
                        }
                    }
                    #[cfg(feature = "time")]
                    Ok(Decision::Completed(TIMER_PENDING)) => {
                        self.timers.insert(user_data, op.timer_delay());
                        None
                    }
                    Ok(Decision::Completed(res)) => {
                        at_least_one_completion = true;
                        Some(Entry::new(user_data, Ok(res)))
                    }
                    Err(err) => {
                        at_least_one_completion = true;
                        Some(Entry::new(user_data, Err(err)))
                    }
                }
            })
        };

        entries.extend(submit_squeue_iter);

        if at_least_one_completion {
            return Ok(());
        };

        // poll only when nothing was completed

        loop {
            #[cfg(feature = "time")]
            let timeout = self.timers.till_next_timer_or_timeout(timeout);

            match self.poll.poll(&mut self.events, timeout) {
                Ok(_) => break Ok(()),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => break Err(err),
            };
        }?;

        #[cfg(feature = "time")]
        self.timers.expire_timers(entries);

        let registry = self.poll.registry();
        let completed_iter = self.events.iter().filter_map(|event| {
            let token = event.token();
            let (user_data, fd, maybe_result) = {
                let entry = waiting
                    .get_mut(&token.0)
                    .expect("Unknown token returned by mio"); // XXX: Should this be silently ignored?
                let maybe_result = match entry.op.on_event(event) {
                    Ok(ControlFlow::Continue(_)) => None,
                    Ok(ControlFlow::Break(res)) => Some(Ok(res)),
                    Err(err) => Some(Err(err)),
                };
                (entry.user_data, &entry.arg.fd, maybe_result)
            };
            if let Some(result) = maybe_result {
                let res = if let Err(err) = registry.deregister(&mut SourceFd(fd)) {
                    Err(err)
                } else {
                    result
                };
                waiting.remove(&token.0);
                Some(Entry::new(user_data, res))
            } else {
                None
            }
        });
        entries.extend(completed_iter);
        Ok(())
    }
}

impl AsRawFd for Driver<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.poll.as_raw_fd()
    }
}
