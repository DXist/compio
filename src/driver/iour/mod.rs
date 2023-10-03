#[cfg(feature = "allocator_api")]
use std::alloc::Allocator;
#[doc(no_inline)]
pub use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::{io, marker::PhantomData, time::Duration};

use io_uring::{
    cqueue,
    opcode::{self, AsyncCancel, FilesUpdate},
    squeue,
    Probe,
    register::SKIP_FILE,
    types::{SubmitArgs, Timespec},
    IoUring,
};
pub(crate) use libc::{sockaddr_storage, socklen_t};

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

/// Fixed fd is offset in registered files array
#[derive(Debug, Clone, Copy)]
pub struct FixedFd {
    offset: u32,
    _not_send_not_sync: PhantomData<*const ()>
}

impl FixedFd {
    #[inline]
    const fn from_offset(offset: u32) -> Self {
        Self { offset, _not_send_not_sync: PhantomData }
    }

    #[inline]
    fn as_offset(&self) -> u32 {
        self.offset
    }
}

/// FdOrFixed is either raw fd or fixed id/offset in registered files array
#[derive(Debug, Clone, Copy)]
pub enum FdOrFixed {
    /// Attached Fd
    Fd(Fd),
    /// Attached Fd with fixed id
    Fixed(FixedFd)
}

impl IntoFdOrFixed for Fd {
    type Target = FdOrFixed;

    #[inline]
    fn into(self) -> Self::Target {
        FdOrFixed::Fd(self)
    }
}

impl IntoFdOrFixed for FixedFd {
    type Target = FdOrFixed;

    #[inline]
    fn into(self) -> Self::Target {
        FdOrFixed::Fixed(self)
    }
}

/// Invalid file descriptor value could be used as an initial value of uninitialized file descriptor
pub const INVALID_FD: FdOrFixed = FdOrFixed::Fd(Fd::from_raw(-1));

/// Abstraction of io-uring operations.
pub trait OpCode {
    /// Create submission entry.
    fn create_entry(&mut self) -> squeue::Entry;
}

/// Low-level driver of io-uring.
pub struct Driver<'arena> {
    inner: IoUring,
    squeue_buffer: Vec<squeue::Entry>,
    files_update_fds: Vec<RawFd>,
    // in progress FilesUpdate state
    files_update_state: FilesUpdateState,
    _lifetime: PhantomData<&'arena ()>,
}

#[derive(Debug, Clone, Copy)]
enum FilesUpdateState {
    NoUpdateInProgress,
    // async FilesUpdate is pushed to submission queue
    Pushed,
    // async FilesUpdate is submitted but not completed yet
    Submitted
}

impl<'arena> Driver<'arena> {
    const FILES_UPDATE_KEY: u64 = u64::MAX;

    /// Create a new io-uring driver with 1024 entries and without registered files.
    pub fn new() -> io::Result<Self> {
        Self::with(1024, 0)
    }

    /// Create a new io-uring driver with specified entries and files to register.
    pub fn with(entries: u32, files_to_register: u32) -> io::Result<Self> {
        let inner = IoUring::new(entries)?;
        let files_update_fds = if files_to_register > 0 {
            let submitter = inner.submitter();
            let mut probe = Probe::new();
            submitter.register_probe(&mut probe)?;
            if probe.is_supported(opcode::Socket::CODE) {
                // register_files_sparse available since Linux 5.19
                submitter.register_files_sparse(files_to_register)?;
                vec![SKIP_FILE; files_to_register as usize]
            } else {
                let mut files = vec![-1; files_to_register as usize];
                submitter.register_files(&files)?;
                for f in &mut files {
                    *f = SKIP_FILE
                }
                files
            }
        }
        else {
            Vec::new()
        };

        Ok(Self {
            inner,
            squeue_buffer: Vec::with_capacity(entries as usize),
            files_update_fds,
            files_update_state: FilesUpdateState::NoUpdateInProgress,
            _lifetime: PhantomData,
        })
    }


    // Submit and wait for completions until `timeout` is passed
    fn submit(&mut self, timeout: Option<Duration>) -> io::Result<()> {
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
            Ok(_) => Ok(()),
            Err(e) => match e.raw_os_error() {
                // timeouts are expected
                Some(libc::ETIME) |
                // break to process completions
                Some(libc::EBUSY) | Some(libc::EAGAIN) => Ok(()),
                _ => Err(e),
            },
        }
    }

    fn complete_entries(&mut self, entries: &mut impl Extend<Entry>) {
        const TIMER_EXPIRED: i32 = -libc::ETIME;
        const NO_ENTRY: i32 = -libc::ENOENT;
        const NOT_CANCELLABLE: i32 = -libc::EALREADY;

        let completed_entries = self.inner.completion().filter_map(|entry| {
            match entry.user_data() {
                Self::FILES_UPDATE_KEY => {
                    // async FilesUpdate operation has finished - reset files update state
                    for f in self.files_update_fds.iter_mut() {
                        *f = SKIP_FILE
                    }
                    self.files_update_state = FilesUpdateState::NoUpdateInProgress;
                    // we processed CQE
                    None
                },
                _ => match entry.result() {
                    // https://man7.org/linux/man-pages/man3/io_uring_prep_cancel.3.html
                    // The specified timeout occurred and triggered the completion event.,
                    TIMER_EXPIRED => Some(Entry::new(entry.user_data() as usize, Ok(0))),
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
            }
        });
        entries.extend(completed_entries);
    }

    #[inline]
    fn register_fd_impl(&mut self, fd: RawFd, id: u32) -> io::Result<()> {
        debug_assert!(self.files_update_fds.len() > 0, "files_to_register is nonzero");
        debug_assert!(( id as usize ) < self.files_update_fds.len(), "registered fixed file index is within [0; files_to_register) range");

        let is_squeue_full = unsafe {
            self.inner.submission_shared().is_full()
        };

        match (is_squeue_full, self.files_update_state) {
            (true, _) | (false, FilesUpdateState::Submitted) => {
                // fallback to synchronous registration when squeue is full or async files_update is not completed yet
                self.inner.submitter().register_files_update(id, &[fd])?;
            }
            (false, FilesUpdateState::NoUpdateInProgress) => {
                // set the initial file update
                self.files_update_fds[id as usize] = fd;
                // create and push update operation for all registered file descriptors
                let len = u32::try_from(self.files_update_fds.len()).expect("in range");
                let fds_ptr = self.files_update_fds.as_ptr();
                let squeue_entry = FilesUpdate::new(fds_ptr, len)
                    .build().user_data(Self::FILES_UPDATE_KEY);
                let mut squeue = self.inner.submission();
                unsafe { squeue.push(&squeue_entry) }.expect("squeue is not full");
                self.files_update_state = FilesUpdateState::Pushed;
            }
            (false, FilesUpdateState::Pushed) => {
                // accumulate more updates in `files_update_fds`
                self.files_update_fds[id as usize] = fd;
            }
        }
        Ok(())
    }
}

impl<'arena> CompleteIo<'arena> for Driver<'arena> {
    #[inline]
    fn attach(&mut self, fd: RawFd) -> io::Result<Fd> {
        Ok(Fd::from_raw(fd))
    }

    #[inline]
    fn register_fd(&mut self, fd: RawFd, id: u32) -> io::Result<FixedFd> {
        self.register_fd_impl(fd, id).map(|_| FixedFd::from_offset(id))
    }

    #[inline]
    fn unregister_fd(&mut self, fixed_fd: FixedFd) -> io::Result<()> {
        self.register_fd_impl(-1, fixed_fd.as_offset())
    }

    #[inline]
    fn try_cancel(&mut self, user_data: usize) -> Result<(), ()> {
        let squeue_entry = AsyncCancel::new(user_data as u64)
            .build()
            .user_data(user_data as u64);
        unsafe { self.inner.submission().push(&squeue_entry) }.map_err(|_| ())
    }

    #[inline]
    fn try_push<O: OpCode>(
        &mut self,
        mut op: Operation<'arena, O>,
    ) -> Result<(), Operation<'arena, O>> {
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
    fn push_queue<#[cfg(feature = "allocator_api")] A: Allocator + Unpin + 'arena>(
        &mut self,
        ops_queue: &mut vec_deque_alloc!(OpObject<'arena>, A),
    ) {
        let to_drain = self.capacity_left().min(ops_queue.len());
        if to_drain == 0 {
            return;
        };

        let mut squeue = self.inner.submission();
        let drain_iter = ops_queue.drain(..to_drain).map(|mut op| {
            let user_data = op.user_data();
            op.opcode().create_entry().user_data(user_data as _)
        });
        self.squeue_buffer.clear();
        self.squeue_buffer.extend(drain_iter);
        unsafe {
            squeue
                .push_multiple(&self.squeue_buffer)
                .expect("in capacity")
        };
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

        if let FilesUpdateState::Pushed = self.files_update_state {
            self.files_update_state = FilesUpdateState::Submitted
        }

        let res = self.submit(timeout);
        // if new submission entries are pushed during completion, runtime has to submit
        // and wait again
        self.complete_entries(completed);
        res
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
