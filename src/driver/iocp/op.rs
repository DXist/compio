#[cfg(feature = "once_cell_try")]
use std::sync::OnceLock;
use std::{
    io,
    marker::PhantomData,
    ptr::{null, null_mut},
    task::Poll,
};

#[cfg(not(feature = "once_cell_try"))]
use once_cell::sync::OnceCell as OnceLock;
use socket2::SockAddr;
use windows_sys::{
    core::GUID,
    Win32::{
        Foundation::{
            GetLastError, ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_NO_DATA,
            ERROR_PIPE_CONNECTED,
        },
        Networking::WinSock::{
            setsockopt, socklen_t, WSAIoctl, WSARecv, WSARecvFrom, WSASend, WSASendTo,
            LPFN_ACCEPTEX, LPFN_CONNECTEX, LPFN_GETACCEPTEXSOCKADDRS,
            SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKADDR, SOCKADDR_STORAGE, SOL_SOCKET,
            SO_UPDATE_ACCEPT_CONTEXT, SO_UPDATE_CONNECT_CONTEXT, WSAID_ACCEPTEX, WSAID_CONNECTEX,
            WSAID_GETACCEPTEXSOCKADDRS,
        },
        Storage::FileSystem::{FlushFileBuffers, ReadFile, WriteFile},
        System::{Pipes::ConnectNamedPipe, IO::OVERLAPPED},
    },
};

#[cfg(feature = "time")]
use crate::driver::iocp::TIMER_PENDING;
#[cfg(feature = "time")]
pub use crate::driver::time::Timeout;
use crate::{
    buf::{AsIoSlices, AsIoSlicesMut, IntoInner, IoBuf, IoBufMut},
    driver::{iocp::Overlapped, OpCode, Fd, RawFd},
    syscall,
};

#[inline]
unsafe fn winapi_result(transferred: u32) -> Poll<io::Result<usize>> {
    let error = GetLastError();
    assert_ne!(error, 0);
    match error {
        ERROR_IO_PENDING => Poll::Pending,
        ERROR_IO_INCOMPLETE | ERROR_HANDLE_EOF | ERROR_PIPE_CONNECTED | ERROR_NO_DATA => {
            Poll::Ready(Ok(transferred as _))
        }
        _ => Poll::Ready(Err(io::Error::from_raw_os_error(error as _))),
    }
}

#[inline]
unsafe fn win32_result(res: i32, transferred: u32) -> Poll<io::Result<usize>> {
    if res == 0 {
        winapi_result(transferred)
    } else {
        Poll::Ready(Ok(transferred as _))
    }
}

// read, write, send and recv functions may return immediately, indicate that
// the task is completed, but the overlapped result is also posted to the IOCP.
// To make our driver easy, simply return Pending and query the result later.

#[inline]
unsafe fn win32_pending_result(res: i32) -> Poll<io::Result<usize>> {
    if res == 0 {
        winapi_result(0)
    } else {
        Poll::Pending
    }
}

#[inline]
unsafe fn winsock_result(res: i32, transferred: u32) -> Poll<io::Result<usize>> {
    if res != 0 {
        winapi_result(transferred)
    } else {
        Poll::Pending
    }
}

unsafe fn get_wsa_fn<F>(handle: Fd, fguid: GUID) -> io::Result<Option<F>> {
    let mut fptr = None;
    let mut returned = 0;
    syscall!(
        SOCKET,
        WSAIoctl(
            handle.as_raw_fd() as _,
            SIO_GET_EXTENSION_FUNCTION_POINTER,
            std::ptr::addr_of!(fguid).cast(),
            std::mem::size_of_val(&fguid) as _,
            std::ptr::addr_of_mut!(fptr).cast(),
            std::mem::size_of::<F>() as _,
            &mut returned,
            null_mut(),
            None,
        )
    )?;
    Ok(fptr)
}

/// Read a file at specified position into specified buffer.
pub struct ReadAt<'arena, T: IoBufMut<'arena>> {
    pub(crate) fd: Fd,
    pub(crate) offset: usize,
    pub(crate) buffer: T,
    pub(super) overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBufMut<'arena>> ReadAt<'arena, T> {
    /// Create [`ReadAt`].
    pub fn new(fd: Fd, offset: usize, buffer: T) -> Self {
        Self {
            fd,
            offset,
            buffer,
            overlapped: Overlapped::new(usize::MAX),
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: IoBufMut<'arena>> IntoInner for ReadAt<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

impl<'arena, T: IoBufMut<'arena>> OpCode for ReadAt<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.overlapped.user_data = user_data;
        self.overlapped().Anonymous.Anonymous.Offset = (self.offset & 0xFFFFFFFF) as _;
        #[cfg(target_pointer_width = "64")]
        {
            self.overlapped().Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as _;
        }

        let fd = self.fd.as_raw_fd() as _;
        // SAFETY: buffer is Unpin
        let slice = self.buffer.as_uninit_slice();
        let res = ReadFile(
            fd,
            slice.as_mut_ptr() as _,
            slice.len() as _,
            null_mut(),
            self.overlapped() as *mut _,
        );
        win32_pending_result(res)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        &mut self.overlapped.base
    }
}

/// Write a file at specified position from specified buffer.
pub struct WriteAt<'arena, T: IoBuf<'arena>> {
    pub(crate) fd: Fd,
    pub(crate) offset: usize,
    pub(crate) buffer: T,
    pub(super) overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBuf<'arena>> WriteAt<'arena, T> {
    /// Create [`WriteAt`].
    pub fn new(fd: Fd, offset: usize, buffer: T) -> Self {
        Self {
            fd,
            offset,
            buffer,
            overlapped: Overlapped::new(usize::MAX),
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: IoBuf<'arena>> IntoInner for WriteAt<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

impl<'arena, T: IoBuf<'arena>> OpCode for WriteAt<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.overlapped.user_data = user_data;
        self.overlapped().Anonymous.Anonymous.Offset = (self.offset & 0xFFFFFFFF) as _;
        #[cfg(target_pointer_width = "64")]
        {
            self.overlapped().Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as _;
        }
        // SAFETY: buffer is Unpin
        let slice = self.buffer.as_slice();
        let res = WriteFile(
            self.fd.as_raw_fd() as _,
            slice.as_ptr() as _,
            slice.len() as _,
            null_mut(),
            self.overlapped() as *mut _,
        );
        win32_pending_result(res)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        &mut self.overlapped.base
    }
}

static CONNECT_EX: OnceLock<LPFN_CONNECTEX> = OnceLock::new();

/// Connect to a remote address.
pub struct Connect {
    pub(crate) fd: Fd,
    pub(crate) addr: SockAddr,
    pub(super) overlapped: Overlapped,
}

impl Connect {
    /// Update connect context.
    pub fn update_context(&self) -> io::Result<()> {
        syscall!(
            SOCKET,
            setsockopt(
                self.fd.as_raw_fd() as _,
                SOL_SOCKET,
                SO_UPDATE_CONNECT_CONTEXT,
                null(),
                0,
            )
        )?;
        Ok(())
    }
}

impl Connect {
    /// Create [`Connect`]. `fd` should be bound.
    pub fn new(fd: Fd, addr: SockAddr) -> Self {
        Self {
            fd,
            addr,
            overlapped: Overlapped::new(usize::MAX),
        }
    }
}

impl OpCode for Connect {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.overlapped.user_data = user_data;
        let connect_fn = CONNECT_EX
            .get_or_try_init(|| get_wsa_fn(self.fd, WSAID_CONNECTEX))?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::Unsupported, "cannot retrieve ConnectEx")
            })?;
        let mut sent = 0;
        let res = connect_fn(
            self.fd.as_raw_fd() as _,
            // SAFETY: SockAddr is Unpin - https://docs.rs/socket2/latest/socket2/struct.SockAddr.html#impl-Unpin-for-SockAddr
            self.addr.as_ptr(),
            self.addr.len(),
            null(),
            0,
            &mut sent,
            &mut self.overlapped.base as *mut _,
        );
        win32_result(res, sent)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        &mut self.overlapped.base
    }
}

/// Sync data to the disk.
pub struct Sync {
    pub(crate) fd: Fd,
    #[allow(dead_code)]
    pub(crate) datasync: bool,
    pub(super) overlapped: Overlapped,
}

impl Sync {
    /// Create [`Sync`].
    ///
    /// If `datasync` is `true`, the file metadata may not be synchronized.
    ///
    /// ## Platform specific
    ///
    /// * IOCP: it is synchronized operation, and calls `FlushFileBuffers`.
    /// * io-uring: `fdatasync` if `datasync` specified, otherwise `fsync`.
    /// * mio: it is synchronized `fdatasync` or `fsync`.
    pub fn new(fd: Fd, datasync: bool) -> Self {
        Self {
            fd,
            datasync,
            overlapped: Overlapped::new(usize::MAX),
        }
    }
}
impl OpCode for Sync {
    unsafe fn operate(&mut self, _user_data: usize) -> Poll<io::Result<usize>> {
        let res = FlushFileBuffers(self.fd.as_raw_fd() as _);
        win32_result(res, 0)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        &mut self.overlapped.base
    }
}

static ACCEPT_EX: OnceLock<LPFN_ACCEPTEX> = OnceLock::new();
static GET_ADDRS: OnceLock<LPFN_GETACCEPTEXSOCKADDRS> = OnceLock::new();

/// Accept a connection.
pub struct Accept {
    pub(crate) fd: Fd,
    pub(crate) accept_fd: RawFd,
    pub(crate) buffer: SOCKADDR_STORAGE,
    pub(super) overlapped: Overlapped,
}

impl Accept {
    /// Create [`Accept`]. `accept_fd` should not be bound.
    pub fn new(fd: Fd, accept_fd: RawFd) -> Self {
        Self {
            fd,
            accept_fd,
            buffer: unsafe { std::mem::zeroed() },
            overlapped: Overlapped::new(usize::MAX),
        }
    }

    /// Update accept context.
    pub fn update_context(&self) -> io::Result<()> {
        syscall!(
            SOCKET,
            setsockopt(
                self.accept_fd as _,
                SOL_SOCKET,
                SO_UPDATE_ACCEPT_CONTEXT,
                &self.fd.as_raw_fd() as *const _ as _,
                std::mem::size_of_val(&self.fd) as _,
            )
        )?;
        Ok(())
    }

    /// Get the remote address from the inner buffer.
    pub fn into_addr(self) -> io::Result<SockAddr> {
        let get_addrs_fn = GET_ADDRS
            .get_or_try_init(|| unsafe { get_wsa_fn(self.fd, WSAID_GETACCEPTEXSOCKADDRS) })?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "cannot retrieve GetAcceptExSockAddrs",
                )
            })?;
        let mut local_addr: *mut SOCKADDR = null_mut();
        let mut local_addr_len = 0;
        let mut remote_addr: *mut SOCKADDR = null_mut();
        let mut remote_addr_len = 0;
        unsafe {
            get_addrs_fn(
                &self.buffer as *const _ as *const _,
                0,
                0,
                std::mem::size_of_val(&self.buffer) as _,
                &mut local_addr,
                &mut local_addr_len,
                &mut remote_addr,
                &mut remote_addr_len,
            );
        }
        Ok(unsafe { SockAddr::new(*remote_addr.cast::<SOCKADDR_STORAGE>(), remote_addr_len) })
    }
}

impl OpCode for Accept {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.overlapped.user_data = user_data;
        let accept_fn = ACCEPT_EX
            .get_or_try_init(|| get_wsa_fn(self.fd, WSAID_ACCEPTEX))?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::Unsupported, "cannot retrieve AcceptEx")
            })?;
        let mut received = 0;
        let res = accept_fn(
            self.fd.as_raw_fd() as _,
            self.accept_fd as _,
            &mut self.buffer as *mut _ as *mut _,
            0,
            0,
            std::mem::size_of_val(&self.buffer) as _,
            &mut received,
            &mut self.overlapped.base as *mut _,
        );
        win32_result(res, received)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        &mut self.overlapped.base
    }
}

/// Receive data from remote.
pub struct RecvImpl<'arena, T: AsIoSlicesMut<'arena>> {
    pub(crate) fd: Fd,
    pub(crate) buffer: T,
    pub(super) overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlicesMut<'arena>> RecvImpl<'arena, T> {
    /// Create [`Recv`] or [`RecvVectored`].
    pub fn new(fd: Fd, buffer: T) -> Self {
        // SAFETY: buffer is Unpin, IoSliceMut is Unpin as well
        Self {
            fd,
            buffer,
            overlapped: Overlapped::new(usize::MAX),
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> IntoInner for RecvImpl<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvImpl<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.overlapped.user_data = user_data;
        // SAFETY: IoSliceMut is Unpin
        let slices = unsafe { self.buffer.as_io_slices_mut() };
        let fd = self.fd.as_raw_fd();
        let mut flags = 0;
        let mut received = 0;
        let res = WSARecv(
            fd as _,
            slices.as_ptr() as _,
            slices.len() as _,
            &mut received,
            &mut flags,
            &mut self.overlapped.base as *mut _,
            None,
        );
        winsock_result(res, received)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        &mut self.overlapped.base
    }
}

/// Send data to remote.
pub struct SendImpl<'arena, T: AsIoSlices<'arena>> {
    pub(crate) fd: Fd,
    pub(crate) buffer: T,
    pub(super) overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlices<'arena>> SendImpl<'arena, T> {
    /// Create [`Send`] or [`SendVectored`].
    pub fn new(fd: Fd, buffer: T) -> Self {
        Self {
            fd,
            buffer,
            overlapped: Overlapped::new(usize::MAX),
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: AsIoSlices<'arena>> IntoInner for SendImpl<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendImpl<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.overlapped.user_data = user_data;
        // SAFETY: buffer is Unpin, IoSlice is Unpin as well
        let slices = unsafe { self.buffer.as_io_slices() };
        let mut sent = 0;
        let res = WSASend(
            self.fd.as_raw_fd() as _,
            slices.as_ptr() as _,
            slices.len() as _,
            &mut sent,
            0,
            &mut self.overlapped.base as *mut _,
            None,
        );
        winsock_result(res, sent)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        &mut self.overlapped.base
    }
}

/// Receive data and source address.
pub struct RecvFromImpl<'arena, T: AsIoSlicesMut<'arena>> {
    pub(crate) fd: Fd,
    pub(crate) buffer: T,
    pub(crate) addr: SOCKADDR_STORAGE,
    pub(crate) addr_len: socklen_t,
    pub(super) overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlicesMut<'arena>> RecvFromImpl<'arena, T> {
    /// Create [`RecvFrom`] or [`RecvFromVectored`].
    pub fn new(fd: Fd, buffer: T) -> Self {
        Self {
            fd,
            buffer,
            addr: unsafe { std::mem::zeroed() },
            addr_len: std::mem::size_of::<SOCKADDR_STORAGE>() as _,
            overlapped: Overlapped::new(usize::MAX),
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> IntoInner for RecvFromImpl<'arena, T> {
    type Inner = (T, SOCKADDR_STORAGE, socklen_t);

    fn into_inner(self) -> Self::Inner {
        (self.buffer, self.addr, self.addr_len)
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvFromImpl<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.overlapped.user_data = user_data;
        let fd = self.fd.as_raw_fd();
        // SAFETY: buffer is Unpin, IoSliceMut is Unpin as well
        let slices = unsafe { self.buffer.as_io_slices_mut() };
        let mut flags = 0;
        let mut received = 0;
        let res = WSARecvFrom(
            fd as _,
            slices.as_ptr() as _,
            slices.len() as _,
            &mut received,
            &mut flags,
            &mut self.addr as *mut _ as *mut SOCKADDR,
            &mut self.addr_len,
            &mut self.overlapped.base as *mut _,
            None,
        );
        winsock_result(res, received)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        &mut self.overlapped.base
    }
}

/// Send data to specified address.
pub struct SendToImpl<'arena, T: AsIoSlices<'arena>> {
    pub(crate) fd: Fd,
    pub(crate) buffer: T,
    pub(crate) addr: SockAddr,
    pub(super) overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlices<'arena>> SendToImpl<'arena, T> {
    /// Create [`SendTo`] or [`SendToVectored`].
    pub fn new(fd: Fd, buffer: T, addr: SockAddr) -> Self {
        Self {
            fd,
            buffer,
            addr,
            overlapped: Overlapped::new(usize::MAX),
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: AsIoSlices<'arena>> IntoInner for SendToImpl<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendToImpl<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.overlapped.user_data = user_data;
        // SAFETY: buffer is Unpin, IoSliceMut is Unpin as well
        let slices = unsafe { self.buffer.as_io_slices() };
        let mut sent = 0;
        let res = WSASendTo(
            self.fd.as_raw_fd() as _,
            slices.as_ptr() as _,
            slices.len() as _,
            &mut sent,
            0,
            self.addr.as_ptr(),
            self.addr.len(),
            &mut self.overlapped.base as *mut _,
            None,
        );
        winsock_result(res, sent)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        &mut self.overlapped.base
    }
}

/// Connect a named pipe server.
pub struct ConnectNamedPipe {
    pub(crate) fd: Fd,
    pub(super) overlapped: Overlapped,
}

impl ConnectNamedPipe {
    /// Create [`ConnectNamedPipe`](struct@ConnectNamedPipe).
    pub fn new(fd: Fd) -> Self {
        Self {
            fd,
            overlapped: Overlapped::new(usize::MAX),
        }
    }
}

impl OpCode for ConnectNamedPipe {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.overlapped.user_data = user_data;
        let res = ConnectNamedPipe(self.fd.as_raw_fd() as _, &mut self.overlapped.base as *mut _);
        win32_result(res, 0)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        &mut self.overlapped.base
    }
}

#[cfg(feature = "time")]
impl OpCode for Timeout {
    unsafe fn operate(&mut self, _user_data: usize) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(TIMER_PENDING))
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        unimplemented!("not used for timers")
    }

    #[cfg(feature = "time")]
    fn timer_delay(&self) -> std::time::Duration {
        self.delay
    }
}
