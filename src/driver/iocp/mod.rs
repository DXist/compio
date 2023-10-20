#[cfg(feature = "allocator_api")]
use std::alloc::Allocator;
use std::{
    fmt, io,
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
    Storage::FileSystem::SetFileCompletionNotificationModes,
    System::{
        SystemServices::ERROR_SEVERITY_ERROR,
        Threading::INFINITE,
        WindowsProgramming::{FILE_SKIP_COMPLETION_PORT_ON_SUCCESS, FILE_SKIP_SET_EVENT_ON_HANDLE},
        IO::{
            CreateIoCompletionPort, GetQueuedCompletionStatusEx, PostQueuedCompletionStatus,
            OVERLAPPED, OVERLAPPED_ENTRY,
        },
    },
};

#[cfg(feature = "time")]
use crate::driver::time::TimerWheel;
use crate::{
    driver::{CompleteIo, Entry, OpObject, Operation},
    syscall, vec_deque_alloc,
};

pub(crate) mod op;

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

/// Attached file descriptor.
///
/// Can't be moved between threads.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fd {
    raw_fd: RawFd,
    _not_send_not_sync: PhantomData<*const ()>,
}

impl Fd {
    #[inline]
    const fn from_raw(raw_fd: RawFd) -> Self {
        Self {
            raw_fd,
            _not_send_not_sync: PhantomData,
        }
    }

    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.raw_fd
    }
}

impl fmt::Debug for Fd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Fd").field(&self.raw_fd).finish()
    }
}

/// Fixed fd is aliased to attached fd
pub type FixedFd = Fd;
/// FdOrFixed is aliased to attached fd
pub type FdOrFixed = Fd;

/// Invalid file descriptor value could be used as an initial value of uninitialized file descriptor
pub const INVALID_FD: Fd = Fd::from_raw(unsafe { std::mem::transmute(INVALID_HANDLE_VALUE) });
/// Invalid fixed file descriptor value could be used as an initial value of uninitialized fixed
/// file descriptor
pub const INVALID_FIXED_FD: FixedFd = INVALID_FD;

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
    #[cfg(feature = "time")]
    fn timer_delay(&self) -> Duration {
        unimplemented!("operation is not a timer")
    }
}

const DEFAULT_CAPACITY: usize = 1024;

/// Low-level driver of IOCP.
pub struct Driver<'arena> {
    port: OwnedHandle,
    squeue: Vec<OpObject<'arena>>,
    // to protect undrained part of squeue from new pushes from processing of completed entries
    squeue_drained_till: usize,
    iocp_entries: Vec<OVERLAPPED_ENTRY>,
    #[cfg(feature = "time")]
    timers: TimerWheel,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena> Driver<'arena> {
    /// Create a new IOCP.
    pub fn new() -> io::Result<Self> {
        Self::with(DEFAULT_CAPACITY as _, 0)
    }

    /// Create a new IOCP driver with specified entries.
    ///
    /// File registration is implemented as attachment.
    pub fn with(entries: u32, _files_to_register: u32) -> io::Result<Self> {
        let port = syscall!(BOOL, CreateIoCompletionPort(INVALID_HANDLE_VALUE, 0, 0, 0))?;
        let port = unsafe { OwnedHandle::from_raw_handle(port as _) };
        let entries = entries as usize;
        Ok(Self {
            port,
            squeue: Vec::with_capacity(entries),
            squeue_drained_till: entries,
            iocp_entries: Vec::with_capacity(entries),
            #[cfg(feature = "time")]
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
        let res = syscall!(
            BOOL,
            GetQueuedCompletionStatusEx(
                self.port.as_raw_handle() as _,
                self.iocp_entries.as_mut_ptr(),
                self.iocp_entries.len() as _,
                &mut recv_count,
                timeout,
                0,
            )
        );
        unsafe {
            self.iocp_entries.set_len(recv_count as _);
        }
        match res.map(|_| ()) {
            // timeouts are expected
            Err(err) if err.kind() == io::ErrorKind::TimedOut => Ok(()),
            res => res,
        }
    }

    fn create_entry(iocp_entry: OVERLAPPED_ENTRY) -> Entry {
        let transferred = iocp_entry.dwNumberOfBytesTransferred;
        let overlapped_ptr = iocp_entry.lpOverlapped;
        let overlapped = unsafe { &*overlapped_ptr.cast::<Overlapped>() };
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
        Entry::new(overlapped.user_data, res)
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

#[cfg(feature = "time")]
const TIMER_PENDING: usize = usize::MAX - 2;

impl<'arena> CompleteIo<'arena> for Driver<'arena> {
    #[inline]
    fn attach(&mut self, fd: RawFd) -> io::Result<Fd> {
        syscall!(
            BOOL,
            CreateIoCompletionPort(fd as _, self.port.as_raw_handle() as _, 0, 0)
        )?;
        let flags =
            u8::try_from(FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE)
                .expect("within u8 range");

        syscall!(BOOL, SetFileCompletionNotificationModes(fd as _, flags))?;
        Ok(Fd::from_raw(fd))
    }

    #[inline]
    fn register_fd(&mut self, fd: RawFd, _id: u32) -> io::Result<FixedFd> {
        self.attach(fd)
    }

    #[inline]
    fn unregister_fd(&mut self, _fixed_fd: FixedFd) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn try_cancel(&mut self, user_data: usize) -> Result<(), ()> {
        if let Some(pos) = self
            .squeue
            .iter()
            .position(|operation| operation.user_data() == user_data)
        {
            // we assume cancellations are rare
            let _ = self.squeue.remove(pos);
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
        self.squeue_drained_till.saturating_sub(self.squeue.len())
    }

    unsafe fn submit(
        &mut self,
        timeout: Option<Duration>,
        entries: &mut impl Extend<Entry>,
    ) -> io::Result<()> {
        let oneshot_completed_iter =
            self.squeue
                .drain(..)
                .enumerate()
                .filter_map(|(idx, mut operation)| {
                    let user_data = operation.user_data();
                    // we require Unpin buffers - so no need to pin
                    let op = operation.opcode();
                    let result = op.operate(user_data);
                    match result {
                        #[cfg(feature = "time")]
                        Poll::Ready(Ok(TIMER_PENDING)) => {
                            self.timers.insert(user_data, op.timer_delay());
                            None
                        }
                        Poll::Ready(result) => {
                            self.squeue_drained_till = idx + 1;
                            Some(Entry::new(user_data, result))
                        }
                        _ => None,
                    }
                });

        entries.extend(oneshot_completed_iter);
        self.squeue_drained_till = self.squeue.capacity();

        #[cfg(feature = "time")]
        let timeout = self.timers.till_next_timer_or_timeout(timeout);

        let res = self.poll_impl(timeout);
        #[cfg(feature = "time")]
        self.timers.expire_timers(entries);

        entries.extend(
            self.iocp_entries
                .drain(..)
                .filter_map(|e| Some(Self::create_entry(e))),
        );

        res
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
