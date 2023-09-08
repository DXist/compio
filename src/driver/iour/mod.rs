#[doc(no_inline)]
pub use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::{
    cell::{RefCell, UnsafeCell},
    io,
    marker::PhantomData,
    mem::MaybeUninit,
    num::Wrapping,
    time::Duration,
};

use io_uring::{
    cqueue, squeue,
    types::{SubmitArgs, Timespec},
    IoUring,
};
pub(crate) use libc::{sockaddr_storage, socklen_t};

use crate::driver::{
    queue_with_capacity,
    registered_fd::{DriverRegisteredFileDescriptors, RegisteredFd, RegisteredFileDescriptors},
    Entry, Poller, Queue,
};

pub(crate) mod op;

/// Abstraction of io-uring operations.
pub trait OpCode {
    /// Create submission entry.
    fn create_entry(&mut self) -> squeue::Entry;
}

/// Low-level driver of io-uring.
pub struct Driver {
    inner: IoUring,
    squeue: Queue<squeue::Entry>,
    cqueue: Queue<Entry>,
    attached_files: UnsafeCell<Vec<RawFd>>,
    last_attached_file_idx: UnsafeCell<Wrapping<u32>>,
    _p: PhantomData<RefCell<()>>,
}

impl Driver {
    /// Create a new io-uring driver with 1024 entries.
    pub fn new() -> io::Result<Self> {
        Self::with_entries(1024)
    }

    /// Create a new io-uring driver with specified entries.
    pub fn with_entries(entries: u32) -> io::Result<Self> {
        Ok(Self {
            inner: IoUring::new(entries)?,
            squeue: queue_with_capacity(entries as usize),
            cqueue: queue_with_capacity(entries as usize),
            attached_files: UnsafeCell::new(vec![-1 as RawFd; 1024]),
            last_attached_file_idx: UnsafeCell::new(Wrapping(u32::MAX)),
            _p: PhantomData,
        })
    }

    unsafe fn submit(&self, timeout: Option<Duration>) -> io::Result<()> {
        let mut inner_squeue = self.inner.submission_shared();
        // Anyway we need to submit once, no matter there are entries in squeue.
        loop {
            while !inner_squeue.is_full() {
                if let Some(entry) = self.squeue.pop() {
                    inner_squeue.push(&entry).unwrap();
                } else {
                    break;
                }
            }
            inner_squeue.sync();

            let res = if self.squeue.is_empty() {
                // Last part of submission queue, wait till timeout.
                if let Some(duration) = timeout {
                    let timespec = timespec(duration);
                    let args = SubmitArgs::new().timespec(&timespec);
                    self.inner.submitter().submit_with_args(1, &args)
                } else {
                    self.inner.submit_and_wait(1)
                }
            } else {
                self.inner.submit()
            };
            match res {
                Ok(_) => Ok(()),
                Err(e) => match e.raw_os_error() {
                    Some(libc::ETIME) => Err(io::Error::from_raw_os_error(libc::ETIMEDOUT)),
                    Some(libc::EBUSY) => Ok(()),
                    _ => Err(e),
                },
            }?;
            inner_squeue.sync();

            for entry in self.inner.completion_shared() {
                self.cqueue.push(create_entry(entry));
            }

            if self.squeue.is_empty() && inner_squeue.is_empty() {
                break;
            }
        }
        Ok(())
    }

    fn poll_entries(&self, entries: &mut [MaybeUninit<Entry>]) -> usize {
        let len = self.cqueue.len().min(entries.len());
        for entry in &mut entries[..len] {
            entry.write(self.cqueue.pop().unwrap());
        }
        len
    }
}

impl Poller for Driver {
    fn attach(&self, fd: RawFd) -> io::Result<RegisteredFd> {
        // SAFETY: we expect single thread execution
        Ok(unsafe {
            let idx: &mut Wrapping<u32> = &mut *self.last_attached_file_idx.get();
            *idx += Wrapping(1);
            let slots: &mut Vec<RawFd> = &mut *self.attached_files.get();
            if let Some(file_slot) = slots.get_mut(idx.0 as usize) {
                *file_slot = fd
            } else {
                // push and resize
                slots.push(fd)
            }
            RegisteredFd::new(idx.0)
        })
    }

    unsafe fn push(&self, op: &mut (impl OpCode + 'static), user_data: usize) -> io::Result<()> {
        let entry = op.create_entry().user_data(user_data as _);
        self.squeue.push(entry);
        Ok(())
    }

    fn post(&self, user_data: usize, result: usize) -> io::Result<()> {
        self.cqueue.push(Entry::new(user_data, Ok(result)));
        Ok(())
    }

    fn poll(
        &self,
        timeout: Option<Duration>,
        entries: &mut [MaybeUninit<Entry>],
    ) -> io::Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        let len = self.poll_entries(entries);
        if len > 0 {
            return Ok(len);
        }
        unsafe { self.submit(timeout) }?;
        let len = self.poll_entries(entries);
        Ok(len)
    }
}

fn create_entry(entry: cqueue::Entry) -> Entry {
    let result = entry.result();
    let result = if result < 0 {
        Err(io::Error::from_raw_os_error(-result))
    } else {
        Ok(result as _)
    };
    Entry::new(entry.user_data() as _, result)
}

fn timespec(duration: std::time::Duration) -> Timespec {
    Timespec::new()
        .sec(duration.as_secs())
        .nsec(duration.subsec_nanos())
}

impl RegisteredFileDescriptors for Driver {
    fn register_attached_files(&self) -> io::Result<()> {
        let attached_files = unsafe { &*self.attached_files.get() };
        self.inner.submitter().register_files(attached_files)
    }
}

impl DriverRegisteredFileDescriptors for Driver {
    // reference to registered files slice
    fn registered_files(&self) -> &[RawFd] {
        unsafe { &*self.attached_files.get() }
    }

    // mutable reference to registered files slice
    fn registered_files_mut(&self) -> &mut [RawFd] {
        unsafe { &mut *self.attached_files.get() }
    }
}
