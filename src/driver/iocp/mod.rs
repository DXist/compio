#[cfg(feature = "allocator_api")]
use std::alloc::Allocator;
use std::{
    collections::HashSet,
    io,
    marker::PhantomData,
    os::windows::prelude::{
        AsRawHandle, AsRawSocket, FromRawHandle, FromRawSocket, IntoRawHandle, IntoRawSocket,
        OwnedHandle, RawHandle,
    },
    task::Poll,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{
        RtlNtStatusToDosError, ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE, ERROR_NO_DATA,
        FACILITY_NTWIN32, INVALID_HANDLE_VALUE, NTSTATUS, STATUS_PENDING, STATUS_SUCCESS,
    },
    System::{
        SystemServices::ERROR_SEVERITY_ERROR,
        Threading::INFINITE,
        IO::{
            CreateIoCompletionPort, GetQueuedCompletionStatusEx, PostQueuedCompletionStatus,
            OVERLAPPED, OVERLAPPED_ENTRY,
        },
    },
};

use crate::{
    driver::{CompleteIo, Entry, OpObject, Operation},
    syscall, vec_deque_alloc,
};
#[cfg(feature="time")]
use crate::driver::time::TimerWheel;

pub(crate) mod op;

pub(crate) use windows_sys::Win32::Networking::WinSock::{
    socklen_t, SOCKADDR_STORAGE as sockaddr_storage,
};

/// On windows, handle and socket are in the same size.
/// Both of them could be attached to an IOCP.
/// Therefore, both could be seen as fd.
pub type RawFd = RawHandle;

/// Extracts raw fds.
pub trait AsRawFd {
    /// Extracts the raw fd.
    fn as_raw_fd(&self) -> RawFd;
}

/// Contruct IO objects from raw fds.
pub trait FromRawFd {
    /// Constructs a new IO object from the specified raw fd.
    ///
    /// # Safety
    ///
    /// The `fd` passed in must:
    ///   - be a valid open handle or socket,
    ///   - be opened with `FILE_FLAG_OVERLAPPED` if it's a file handle,
    ///   - have been attached to a driver.
    unsafe fn from_raw_fd(fd: RawFd) -> Self;
}

/// Consumes an object and acquire ownership of its raw fd.
pub trait IntoRawFd {
    /// Consumes this object, returning the raw underlying fd.
    fn into_raw_fd(self) -> RawFd;
}

impl AsRawFd for std::fs::File {
    fn as_raw_fd(&self) -> RawFd {
        self.as_raw_handle()
    }
}

impl AsRawFd for socket2::Socket {
    fn as_raw_fd(&self) -> RawFd {
        self.as_raw_socket() as _
    }
}

impl FromRawFd for std::fs::File {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self::from_raw_handle(fd)
    }
}

impl FromRawFd for socket2::Socket {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self::from_raw_socket(fd as _)
    }
}

impl IntoRawFd for std::fs::File {
    fn into_raw_fd(self) -> RawFd {
        self.into_raw_handle()
    }
}

impl IntoRawFd for socket2::Socket {
    fn into_raw_fd(self) -> RawFd {
        self.into_raw_socket() as _
    }
}

/// Abstraction of IOCP operations.
pub trait OpCode {
    /// Perform Windows API call.
    ///
    /// # Safety
    ///
    /// `self` attributes must be Unpin to ensure safe operation.
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>>;

    /// Return mut reference on OVERLAPPED structure
    fn overlapped(&mut self) -> &mut OVERLAPPED;

    /// Only timers implement this method
    #[cfg(feature="time")]
    fn timer_delay(&self) -> Duration {
        unimplemented!("operation is not a timer")
    }
}

const DEFAULT_CAPACITY: usize = 1024;

/// Low-level driver of IOCP.
pub struct Driver<'arena> {
    port: OwnedHandle,
    squeue: Vec<OpObject<'arena>>,
    iocp_entries: Vec<OVERLAPPED_ENTRY>,
    cancelled: HashSet<usize>,
    #[cfg(feature="time")]
    timers: TimerWheel,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena> Driver<'arena> {
    /// Create a new IOCP.
    pub fn new() -> io::Result<Self> {
        Self::with_entries(DEFAULT_CAPACITY as _)
    }

    /// The same as [`Driver::new`].
    pub fn with_entries(entries: u32) -> io::Result<Self> {
        let port = syscall!(BOOL, CreateIoCompletionPort(INVALID_HANDLE_VALUE, 0, 0, 0))?;
        let port = unsafe { OwnedHandle::from_raw_handle(port as _) };
        Ok(Self {
            port,
            squeue: Vec::with_capacity(entries as usize),
            iocp_entries: Vec::with_capacity(entries as usize),
            cancelled: HashSet::default(),
            #[cfg(feature="time")]
            timers: TimerWheel::with_capacity(16),
            _lifetime: PhantomData,
        })
    }

    #[inline]
    fn poll_impl(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let mut recv_count = 0;
        let timeout = match timeout {
            Some(timeout) => timeout.as_millis() as u32,
            None => INFINITE,
        };
        syscall!(
            BOOL,
            GetQueuedCompletionStatusEx(
                self.port.as_raw_handle() as _,
                self.iocp_entries.as_mut_ptr(),
                self.iocp_entries.len() as _,
                &mut recv_count,
                timeout,
                0,
            )
        )?;
        unsafe {
            self.iocp_entries.set_len(recv_count as _);
        }
        Ok(())
    }

    fn create_entry(cancelled: &mut HashSet<usize>, iocp_entry: OVERLAPPED_ENTRY) -> Option<Entry> {
        let transferred = iocp_entry.dwNumberOfBytesTransferred;
        let overlapped_ptr = iocp_entry.lpOverlapped;
        let overlapped = unsafe { &*overlapped_ptr.cast::<Overlapped>() };
        if cancelled.remove(&overlapped.user_data) {
            return None;
        }
        let res = if matches!(
            overlapped.base.Internal as NTSTATUS,
            STATUS_SUCCESS | STATUS_PENDING
        ) {
            Ok(transferred as _)
        } else {
            let error = unsafe { RtlNtStatusToDosError(overlapped.base.Internal as _) };
            match error {
                ERROR_IO_INCOMPLETE | ERROR_HANDLE_EOF | ERROR_NO_DATA => Ok(0),
                _ => Err(io::Error::from_raw_os_error(error as _)),
            }
        };
        Some(Entry::new(overlapped.user_data, res))
    }
}

/// # Safety
///
/// * The handle should be valid.
/// * The overlapped_ptr should be non-null.
pub(crate) unsafe fn post_driver_raw(
    handle: RawFd,
    result: io::Result<usize>,
    overlapped: &mut OVERLAPPED,
) -> io::Result<()> {
    if let Err(e) = &result {
        overlapped.Internal = ntstatus_from_win32(e.raw_os_error().unwrap_or_default()) as _;
    }
    syscall!(
        BOOL,
        PostQueuedCompletionStatus(
            handle as _,
            result.unwrap_or_default() as _,
            0,
            overlapped as *mut _,
        )
    )?;
    Ok(())
}

fn ntstatus_from_win32(x: i32) -> NTSTATUS {
    if x <= 0 {
        x
    } else {
        (x & 0x0000FFFF) | (FACILITY_NTWIN32 << 16) as NTSTATUS | ERROR_SEVERITY_ERROR as NTSTATUS
    }
}

#[cfg(feature="time")]
const TIMER_PENDING: usize = usize::MAX - 2;

impl<'arena> CompleteIo<'arena> for Driver<'arena> {
    #[inline]
    fn attach(&mut self, fd: RawFd) -> io::Result<()> {
        syscall!(
            BOOL,
            CreateIoCompletionPort(fd as _, self.port.as_raw_handle() as _, 0, 0)
        )?;
        Ok(())
    }

    #[inline]
    fn try_cancel(&mut self, user_data: usize) -> Result<(), ()> {
        self.cancelled.insert(user_data);
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
        for mut operation in self.squeue.drain(..) {
            let user_data = operation.user_data();
            if !self.cancelled.remove(&user_data) {
                // we require Unpin buffers - so no need to pin
                let op = operation.opcode();
                let result = op.operate(user_data);
                match result {
                    #[cfg(feature="time")]
                    Poll::Ready(Ok(TIMER_PENDING)) => self.timers.insert(user_data, op.timer_delay()),
                    Poll::Ready(result) => {
                        post_driver_raw(self.port.as_raw_handle(), result, op.overlapped())?;
                    },
                    _ => {}
                }
            }
        }

        #[cfg(feature="time")]
        let timeout = self.timers.till_next_timer_or_timeout(timeout);

        self.poll_impl(timeout)?;
        #[cfg(feature="time")]
        self.timers.expire_timers(entries);

        {
            let cancelled = &mut self.cancelled;
            entries.extend(
                self.iocp_entries
                    .drain(..)
                    .filter_map(|e| Self::create_entry(cancelled, e)),
            );
        }

        Ok(())
    }
}

impl AsRawFd for Driver<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.port.as_raw_handle()
    }
}

#[repr(C)]
pub(crate) struct Overlapped {
    #[allow(dead_code)]
    pub base: OVERLAPPED,
    pub user_data: usize,
}

impl Overlapped {
    pub fn new(user_data: usize) -> Self {
        Self {
            base: unsafe { std::mem::zeroed() },
            user_data,
        }
    }
}
