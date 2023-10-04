#[cfg(feature = "allocator_api")]
use std::alloc::Allocator;
#[doc(no_inline)]
pub use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, RawFd, OwnedFd};
use std::{
    collections::VecDeque,
    marker::PhantomData,
    io,
    time::Duration,
};
use bit_set::BitSet;
use std::convert::identity;

pub(crate) use libc::{sockaddr_storage, socklen_t};
use rustix::event::kqueue::{Event, kqueue, kevent, EventFlags, EventFilter};

#[cfg(feature = "time")]
use crate::driver::time::TimerWheel;
use crate::{
    driver::{CompleteIo, Entry, OpObject, Operation, unix::IntoFdOrFixed},
    vec_deque_alloc,
};

pub(crate) mod op;

/// Attached file descriptor.
///
/// Can't be moved between threads.
#[derive(Debug, Clone, Copy)]
pub struct Fd {
    raw_fd: RawFd,
    _not_send_not_sync: PhantomData<*const ()>
}

impl Fd {
    #[inline]
    const fn from_raw(raw_fd: RawFd) -> Self {
        Self { raw_fd, _not_send_not_sync: PhantomData }
    }

    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.raw_fd
    }
}

impl IntoFdOrFixed for Fd {
    type Target = Fd;

    #[inline]
    fn into(self) -> Self::Target {
        self
    }
}

/// Fixed fd is aliased to attached fd
pub type FixedFd = Fd;
/// FdOrFixed is aliased to attached fd
pub type FdOrFixed = Fd;

/// Invalid file descriptor value could be used as an initial value of uninitialized file descriptor
pub const INVALID_FD: FdOrFixed = Fd::from_raw(-1);

/// Abstraction of operations.
pub trait OpCode {

    /// Perform the operation before checking for readiness, and return [`Some(Result>`] to
    /// indicate whether the operation is completed instead of rescheduled
    fn operate(&mut self) -> Option<io::Result<usize>>;

    /// Construct kqueue Event for the operation with the provided user_data
    fn as_event(&self, user_data: usize) -> Event;

    /// Only timers implement this method
    #[cfg(feature = "time")]
    fn timer_delay(&self) -> std::time::Duration {
        unimplemented!("operation is not a timer")
    }
}


#[cfg(feature = "time")]
const TIMER_PENDING: usize = usize::MAX - 2;

/// Low-level driver based on kqueue.
pub struct Driver<'arena> {
    kqueue: OwnedFd,
    // submission queue
    squeue: Vec<OpObject<'arena>>,
    // pending io queue
    io_pending: VecDeque<OpObject<'arena>>,
    // kevent changelist
    events_to_change: Vec<Event>,
    // kevent ready events output
    ready_events: Vec<Event>,
    // Indexes from the front of io_pending ops queue corresponding to completed kqueue events
    completed_events_indices: BitSet,
    // kqueue identifies read/write filters by file descriptors
    //
    // The field is used to deduplicate changes to read filters using per fd bitset
    to_change_fd_reads: BitSet,
    // The field is used to deduplicate changes to write filters using per fd bitset
    to_change_fd_writes: BitSet,
    #[cfg(feature = "time")]
    timers: TimerWheel,
}

impl<'arena> Driver<'arena> {
    /// Create a new kqueue driver with 1024 entries.
    pub fn new() -> io::Result<Self> {
        Self::with(1024, 0)
    }

    /// Create a new kqueue driver with specified entries.
    ///
    /// File registration is implemented as dummy operation.
    pub fn with(entries: u32, files_to_register: u32) -> io::Result<Self> {
        let entries = entries as usize; // for the sake of consistency, use u32 like iour
        let initial_fd_capacity = entries.max(files_to_register as usize);

        Ok(Self {
            kqueue: kqueue()?,
            squeue: Vec::with_capacity(entries),
            io_pending: VecDeque::with_capacity(entries),
            events_to_change: Vec::with_capacity(entries),
            ready_events: Vec::with_capacity(entries),
            completed_events_indices: BitSet::with_capacity(entries),
            to_change_fd_reads: BitSet::with_capacity(initial_fd_capacity),
            to_change_fd_writes: BitSet::with_capacity(initial_fd_capacity),
            #[cfg(feature = "time")]
            timers: TimerWheel::with_capacity(16),
        })
    }

    // operate pushed operations
    fn operate_squeue(&mut self, entries: &mut impl Extend<Entry>) {
        let oneshot_completed_iter = self.squeue.drain(..).filter_map(|mut op| {
            let user_data = op.user_data();
            let opcode = op.opcode();
            // io buffers are Unpin so no need to pin
            match opcode.operate() {
                // no result => io is pending
                None => {
                    self.io_pending.push_back(op);
                    None
                },
                Some(res) => {
                    match res {
                        #[cfg(feature = "time")]
                        Ok(TIMER_PENDING) => {
                            self.timers.insert(user_data, opcode.timer_delay());
                            None
                        },
                        res => {
                            Some(Entry::new(user_data, res))
                        }
                    }
                }
            }
        });

        entries.extend(oneshot_completed_iter);
    }

    fn operate_completed_and_requeue(&mut self, io_pending_scanned_till: usize, entries: &mut impl Extend<Entry>) {
        // stash indices of ready events
        self.completed_events_indices.clear();

        let completed_ops_iter = self.ready_events.drain(..).filter_map(|event| {
            let index = event.udata() as usize;
            let op = self.io_pending.get_mut(index).expect("in range");
            let user_data = op.user_data();

            if event.flags().contains(EventFlags::ERROR) {
                self.completed_events_indices.insert(index);

                let c_err = i32::try_from(event.data()).expect("system error in i32 range");
                let err = io::Error::from_raw_os_error(c_err);
                Some(Entry::new(user_data, Err(err)))
            }
            else {
                // operate the ready op
                match op.opcode().operate() {
                    // still pending, will requeue later
                    None => None,
                    Some(res) => {
                        self.completed_events_indices.insert(index);
                        Some(Entry::new(user_data, res))
                    }
                }
            }
        });

        entries.extend(completed_ops_iter);

        // operations not in `self.completed_events_indices` are still pending - reschedule to the back of the queue
        for index in 0..io_pending_scanned_till {
            let op = self.io_pending.pop_front().expect("front exists");
            if !self.completed_events_indices.contains(index) {
                self.io_pending.push_back(op)
            }
            // else operation is completed
        }

    }

    // On success returns index of self.io_pending a scan stopped at
    fn check_readiness(&mut self, timeout: Option<Duration>, entries: &mut impl Extend<Entry>) -> io::Result<usize> {
        self.events_to_change.clear();
        self.to_change_fd_reads.clear();
        self.to_change_fd_writes.clear();

        let max_events_to_change = self.events_to_change.capacity();

        let mut scanned_till = 0_usize;
        let change_event_iter = self.io_pending.iter().enumerate().scan(0, |events_to_change, (index, op)| {
            scanned_till = index;
            // iterate till hit events_to_change capacity
            if *events_to_change < max_events_to_change {
                let event = op.opcode_ref().as_event(index);
                let fd_absent = match event.filter() {
                    EventFilter::Read(raw_fd) => {
                        debug_assert!(raw_fd >=0);
                        self.to_change_fd_reads.insert(raw_fd as usize)
                    },
                    EventFilter::Write(raw_fd) => {
                        self.to_change_fd_writes.insert(raw_fd as usize)
                    },
                    _ => unreachable!("only Read/Write filters are supported")
                };
                let maybe_event = if fd_absent {
                    *events_to_change += 1;
                    Some(event)
                } else {
                    // postpone the same io operations with the same fd to next `submit` iterations
                    None
                };
                // will filter skipped events via identity func
                Some(maybe_event)
            }
            else {
                None
            }

        }).filter_map(identity);
        self.events_to_change.extend(change_event_iter);

        #[cfg(feature = "time")]
        let timeout = self.timers.till_next_timer_or_timeout(timeout);
        // kevent blocks indefinitely when timeout is NULL
        // we provide interface that unblocks when at least one operation is completed
        let timeout = timeout.unwrap_or(Duration::ZERO);
        let res = unsafe { kevent(self.kqueue.as_fd(), &self.events_to_change, &mut self.ready_events, Some(timeout)) };

        #[cfg(feature = "time")]
        self.timers.expire_timers(entries);

        Ok(res.map(|_| scanned_till)?)
    }
}

impl<'arena> CompleteIo<'arena> for Driver<'arena> {
    #[inline]
    fn attach(&mut self, fd: RawFd) -> io::Result<Fd> {
        Ok(Fd::from_raw(fd))
    }
    #[inline]
    fn register_fd(&mut self, fd: RawFd, _id: u32) -> io::Result<FixedFd> {
        Ok(FixedFd::from_raw(fd))
    }

    #[inline]
    fn unregister_fd(&mut self, _fixed_fd: FixedFd) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn try_cancel(&mut self, user_data: usize) -> Result<(), ()> {
        // we assume cancellations are rare
        if let Some(pos) = self.squeue.iter().position(|operation| operation.user_data() == user_data) {
            let _ = self.squeue.remove(pos);
        }
        if let Some(pos) = self.io_pending.iter().position(|operation| operation.user_data() == user_data) {
            let _ = self.io_pending.remove(pos);
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

    unsafe fn submit(
        &mut self,
        timeout: Option<Duration>,
        entries: &mut impl Extend<Entry>,
    ) -> io::Result<()> {
        let ops_pushed = self.squeue.len() > 0;

        self.operate_squeue(entries);

        // when io is pushed and completed and there is no pending io
        // let the caller to process completed operations
        if ops_pushed && self.io_pending.is_empty() {
            #[cfg(feature = "time")]
            self.timers.expire_timers(entries);
            return Ok(());
        }
        // either caller doesn't have new io or there is pending io

        // on any error there is no ready events
        let io_pending_scanned_till = self.check_readiness(timeout, entries)?;
        self.operate_completed_and_requeue(io_pending_scanned_till, entries);

        Ok(())
    }
}

impl AsRawFd for Driver<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.kqueue.as_raw_fd()
    }
}
