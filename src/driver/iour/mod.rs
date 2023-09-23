#[cfg(feature = "allocator_api")]
use std::alloc::Allocator;
#[doc(no_inline)]
pub use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::{
    io,
    marker::PhantomData,
    time::Duration,
};

use io_uring::{
    cqueue,
    opcode::AsyncCancel,
    squeue,
    types::{SubmitArgs, Timespec},
    IoUring,
};
pub(crate) use libc::{sockaddr_storage, socklen_t};

use crate::{driver::{Entry, Operation, Poller, OpObject}, vec_deque_alloc};

pub(crate) mod op;

/// Abstraction of io-uring operations.
pub trait OpCode {
    /// Create submission entry.
    fn create_entry(&mut self) -> squeue::Entry;
}

/// Low-level driver of io-uring.
pub struct Driver<'arena> {
    inner: IoUring,
    squeue_buffer: Vec<squeue::Entry>,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena> Driver<'arena> {
    /// Create a new io-uring driver with 1024 entries.
    pub fn new() -> io::Result<Self> {
        Self::with_entries(1024)
    }

    /// Create a new io-uring driver with specified entries.
    pub fn with_entries(entries: u32) -> io::Result<Self> {
        Ok(Self {
            inner: IoUring::new(entries)?,
            squeue_buffer: Vec::with_capacity(entries as usize),
            _lifetime: PhantomData,
        })
    }

    // Submit and wait for completions until `timeout` is passed
    fn submit(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        loop {
            let res = match timeout {
                None => self.inner.submit_and_wait(1),
                Some(Duration::ZERO) => self.inner.submit(),
                Some(duration) => {
                    // Wait till timeout.
                    let timespec = timespec(duration);
                    let args = SubmitArgs::new().timespec(&timespec);
                    self.inner.submitter().submit_with_args(1, &args)
                }
            };
            match res {
                Ok(_) => break Ok(()),
                Err(e) => match e.raw_os_error() {
                    // retry on interrupt
                    Some(libc::EINTR) => continue,
                    Some(libc::ETIME) => break Err(io::Error::from_raw_os_error(libc::ETIMEDOUT)),
                    // break to process completions
                    Some(libc::EBUSY) | Some(libc::EAGAIN) => break Ok(()),
                    _ => break Err(e),
                },
            }
        }
    }

    fn complete_entries(&mut self, entries: &mut impl Extend<Entry>) {
        const NO_ENTRY: i32 = -libc::ENOENT;
        const NOT_CANCELLABLE: i32 = -libc::EALREADY;

        let completed_entries = self.inner.completion().filter_map(|entry| {
            // https://man7.org/linux/man-pages/man3/io_uring_prep_cancel.3.html
            match entry.result() {
                // The request identified by user_data could not be located.
                // This could be because it completed before the cancelation
                // request was issued, or if an invalid identifier is used.
                NO_ENTRY |
                // The execution state of the request has progressed far
                // enough that cancelation is no longer possible. This should
                // normally mean that it will complete shortly, either
                // successfully, or interrupted due to the cancelation.
                NOT_CANCELLABLE => None,
                _ => Some(create_entry(entry)),
            }
        });
        entries.extend(completed_entries);
    }
}

impl<'arena> Poller<'arena> for Driver<'arena> {
    #[inline]
    fn attach(&mut self, _fd: RawFd) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn try_cancel(&mut self, user_data: usize) -> Result<(), ()> {
        let squeue_entry = AsyncCancel::new(user_data as u64).build().user_data(user_data as u64);
        unsafe { self.inner.submission().push(&squeue_entry) }.map_err(|_| ())
    }

    #[inline]
    fn try_push<O: OpCode>(&mut self, mut op: Operation<'arena, O>) -> Result<(), Operation<'arena, O>> {
        let user_data = op.user_data();
        let squeue_entry = op.opcode().create_entry().user_data(user_data as _);
        unsafe { self.inner.submission().push(&squeue_entry) }.map_err(|_| op)
    }

    #[inline]
    fn try_push_dyn(&mut self, mut op: OpObject<'arena>) -> Result<(), OpObject<'arena>> {
        let user_data = op.user_data();
        let squeue_entry = op.opcode().create_entry().user_data(user_data as _);
        unsafe { self.inner.submission().push(&squeue_entry) }.map_err(|_| op)
    }

    #[inline]
    fn push_queue<#[cfg(feature = "allocator_api")] A: Allocator + Unpin + 'arena>(&mut self, ops_queue: &mut vec_deque_alloc!(OpObject<'arena>, A)) {
        let to_drain = self.capacity_left().min(ops_queue.len());
        if to_drain == 0 { return };

        let mut squeue = self.inner.submission();
        let drain_iter = ops_queue.drain(..to_drain).map(|mut op| {
            let user_data = op.user_data();
            op.opcode().create_entry().user_data(user_data as _)
        });
        self.squeue_buffer.clear();
        self.squeue_buffer.extend(drain_iter);
        unsafe { squeue.push_multiple(&self.squeue_buffer).expect("in capacity") };
    }

    #[inline]
    fn capacity_left(&self) -> usize {
        let squeue = unsafe { self.inner.submission_shared() };
        squeue.capacity() - squeue.len()
    }

    unsafe fn submit_and_wait_completed(
        &mut self,
        timeout: Option<Duration>,
        completed: &mut impl Extend<Entry>,
    ) -> io::Result<()> {
        // Anyway we need to submit once, no matter there are entries in squeue.
        self.inner.submission().sync();
        self.submit(timeout)?;
        // if new submission entries are pushed during completion, runtime has to submit and wait again
        self.complete_entries(completed);
        Ok(())
    }
}

impl AsRawFd for Driver<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[inline]
fn create_entry(entry: cqueue::Entry) -> Entry {
    let result = entry.result();
    let result = if result < 0 {
        Err(io::Error::from_raw_os_error(-result))
    } else {
        Ok(result as _)
    };
    Entry::new(entry.user_data() as _, result)
}

#[inline]
fn timespec(duration: std::time::Duration) -> Timespec {
    Timespec::new()
        .sec(duration.as_secs())
        .nsec(duration.subsec_nanos())
}
