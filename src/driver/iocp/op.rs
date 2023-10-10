#[cfg(feature = "once_cell_try")]
use std::sync::OnceLock;
use std::{
    io,
    marker::PhantomData,
    os::raw::c_void,
    ptr::{copy, null, null_mut},
    task::Poll,
};

#[cfg(not(feature = "once_cell_try"))]
use once_cell::sync::OnceCell as OnceLock;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use windows_sys::{
    core::GUID,
    Win32::{
        Foundation::{
            CloseHandle, GetLastError, ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING,
            ERROR_NO_DATA, ERROR_PIPE_CONNECTED,
        },
        Networking::WinSock::{
            closesocket, getsockopt, setsockopt, socklen_t, WSAIoctl, WSARecv, WSARecvFrom,
            WSASend, WSASendTo, INVALID_SOCKET, LPFN_ACCEPTEX, LPFN_CONNECTEX,
            LPFN_GETACCEPTEXSOCKADDRS, SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKADDR,
            SOCKADDR_STORAGE, SOL_SOCKET, SO_ERROR, SO_UPDATE_ACCEPT_CONTEXT,
            SO_UPDATE_CONNECT_CONTEXT, WSAENOTSOCK, WSAID_ACCEPTEX, WSAID_CONNECTEX,
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
    buf::{AsIoSlices, AsIoSlicesMut, BufWrapper, BufWrapperMut, IntoInner, IoBuf, IoBufMut},
    driver::{iocp::Overlapped, Fd, FromRawFd, IntoRawFd, OpCode, RawFd},
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

/// Read a nonseekable file into specified buffer.
pub struct Read<'arena, T: IoBufMut<'arena>> {
    fd: Fd,
    buffer: T,
    overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBufMut<'arena>> Read<'arena, T> {
    /// Create [`Read`].
    pub fn new(fd: Fd, buffer: T) -> Self {
        Self {
            fd,
            buffer,
            overlapped: Overlapped::new(usize::MAX),
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: IoBufMut<'arena>> IntoInner for Read<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

impl<'arena, T: IoBufMut<'arena>> OpCode for Read<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.overlapped.user_data = user_data;

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

/// Read a file at specified position into specified buffer.
pub struct ReadAt<'arena, T: IoBufMut<'arena>> {
    fd: Fd,
    offset: usize,
    buffer: T,
    overlapped: Overlapped,
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

/// Write a nonseekable file from specified buffer.
pub struct Write<'arena, T: IoBuf<'arena>> {
    fd: Fd,
    buffer: T,
    overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBuf<'arena>> Write<'arena, T> {
    /// Create [`Write`].
    pub fn new(fd: Fd, buffer: T) -> Self {
        Self {
            fd,
            buffer,
            overlapped: Overlapped::new(usize::MAX),
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: IoBuf<'arena>> IntoInner for Write<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

impl<'arena, T: IoBuf<'arena>> OpCode for Write<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.overlapped.user_data = user_data;
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

/// Write a file at specified position from specified buffer.
pub struct WriteAt<'arena, T: IoBuf<'arena>> {
    fd: Fd,
    offset: usize,
    buffer: T,
    overlapped: Overlapped,
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
    fd: Fd,
    addr: SockAddr,
    overlapped: Overlapped,
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

    /// Post operation socket handling.
    ///
    /// Set SO_UPDATE_CONNECT_CONTEXT
    pub fn on_connect(self, result: io::Result<usize>) -> io::Result<()> {
        let _ = result?;
        self.update_context()?;
        Ok(())
    }

    /// Set SO_UPDATE_CONNECT_CONTEXT
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

impl Default for Connect {
    fn default() -> Self {
        Self {
            fd: INVALID_FD,
            addr: unsafe { std::mem::zeroed },
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
    fd: Fd,
    #[allow(dead_code)]
    datasync: bool,
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
    /// * kqueue: it is synchronized `fdatasync` or `fsync`.
    pub fn new(fd: Fd, datasync: bool) -> Self {
        Self { fd, datasync }
    }
}

impl OpCode for Sync {
    unsafe fn operate(&mut self, _user_data: usize) -> Poll<io::Result<usize>> {
        let res = FlushFileBuffers(self.fd.as_raw_fd() as _);
        win32_result(res, 0)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        unimplemented!("FlushFileBuffers is synchonous")
    }
}

static ACCEPT_EX: OnceLock<LPFN_ACCEPTEX> = OnceLock::new();
static GET_ADDRS: OnceLock<LPFN_GETACCEPTEXSOCKADDRS> = OnceLock::new();

/// Accept a connection.
///
/// It's possible to reinit the data structure and reuse it for the following accepts.
pub struct Accept {
    fd: Fd,
    accept_fd: RawFd,
    accept_sock_opts: Option<AcceptSocketOpts>,
    addr: SockAddr,
    overlapped: Overlapped,
}

struct AcceptSocketOpts {
    domain: Domain,
    ty: Type,
    protocol: Option<Protocol>,
}

impl Accept {
    const INVALID_SOCKET: RawFd = INVALID_SOCKET as RawFd;

    /// Create [`Accept`] with listen socket options.
    ///
    /// Accept socket will be created on operation execution.
    pub fn with_socket_opts(fd: Fd, domain: Domain, ty: Type, protocol: Option<Protocol>) -> Self {
        Self {
            fd,
            accept_fd: Self::INVALID_SOCKET,
            accept_sock_opts: Some(AcceptSocketOpts {
                domain,
                ty,
                protocol,
            }),
            addr: unsafe {
                SockAddr::new(
                    std::mem::zeroed(),
                    std::mem::size_of::<SOCKADDR_STORAGE>() as socklen_t,
                )
            },
            overlapped: Overlapped::new(usize::MAX),
        }
    }

    /// Init existing [`Accept`] for new accept operation.
    pub fn init_with_socket_opts(
        &mut self,
        fd: Fd,
        domain: Domain,
        ty: Type,
        protocol: Option<Protocol>,
    ) {
        self.fd = fd;
        self.accept_fd = Self::INVALID_SOCKET;
        self.accept_sock_opts = Some(AcceptSocketOpts {
            domain,
            ty,
            protocol,
        });
    }

    /// Create [`Accept`] with the provided accept socket fd. `accept_fd` should not be bound.
    pub fn new(fd: Fd, accept_fd: RawFd) -> Self {
        Self {
            fd,
            accept_fd,
            accept_sock_opts: None,
            addr: unsafe {
                SockAddr::new(
                    std::mem::zeroed(),
                    std::mem::size_of::<SOCKADDR_STORAGE>() as socklen_t,
                )
            },
            overlapped: Overlapped::new(usize::MAX),
        }
    }

    /// Post operation socket handling.
    ///
    /// Set SO_UPDATE_ACCEPT_CONTEXT
    /// Get remote address.
    pub fn on_accept(&mut self, result: io::Result<usize>) -> io::Result<(Socket, &SockAddr)> {
        let _ = result?;
        let accept_sock = unsafe { Socket::from_raw_fd(self.accept_fd) };
        self.update_context()?;
        let addr = self.as_sockaddr()?;
        Ok((accept_sock, addr))
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
    pub fn as_sockaddr(&mut self) -> io::Result<&SockAddr> {
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
                self.addr.as_ptr() as *const c_void,
                0,
                0,
                std::mem::size_of::<SOCKADDR_STORAGE>() as _,
                &mut local_addr,
                &mut local_addr_len,
                &mut remote_addr,
                &mut remote_addr_len,
            );
        }
        let addr_storage_ptr = self.addr.as_ptr() as *mut _;
        if remote_addr != addr_storage_ptr {
            unsafe { copy(remote_addr, addr_storage_ptr, remote_addr_len as usize) };
        }
        unsafe {
            self.addr.set_length(remote_addr_len);
        };
        Ok(&self.addr)
    }
}

impl OpCode for Accept {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        if self.accept_fd == Self::INVALID_SOCKET {
            if let Some(opts) = &self.accept_sock_opts {
                // create socket with accept socket options
                self.accept_fd = match Socket::new(opts.domain, opts.ty, opts.protocol) {
                    Ok(sock) => sock.into_raw_fd(),
                    Err(err) => return Poll::Ready(Err(err)),
                }
            }
        }
        self.overlapped.user_data = user_data;
        let accept_fn = ACCEPT_EX
            .get_or_try_init(|| get_wsa_fn(self.fd, WSAID_ACCEPTEX))?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::Unsupported, "cannot retrieve AcceptEx")
            })?;
        let storage_ptr = self.addr.as_ptr();
        let mut received = 0;
        let res = accept_fn(
            self.fd.as_raw_fd() as _,
            self.accept_fd as _,
            storage_ptr as *mut c_void,
            0,
            0,
            std::mem::size_of::<SOCKADDR_STORAGE>() as _,
            &mut received,
            &mut self.overlapped.base as *mut _,
        );
        win32_result(res, received)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        &mut self.overlapped.base
    }
}

/// Receive a single piece of data in a single buffer from remote.
pub struct Recv<'arena, T: IoBufMut<'arena>> {
    inner: RecvVectoredImpl<'arena, BufWrapperMut<'arena, T>>,
}

impl<'arena, T: IoBufMut<'arena>> Recv<'arena, T> {
    /// Create [`Recv`]
    pub fn new(fd: Fd, buffer: T) -> Self {
        // SAFETY: buffer is Unpin, IoSliceMut is Unpin as well
        Self {
            inner: RecvVectoredImpl::new(fd, BufWrapperMut::from(buffer)),
        }
    }
}

impl<'arena, T: IoBufMut<'arena>> IntoInner for Recv<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.inner.into_inner().into_inner()
    }
}

impl<'arena, T: IoBufMut<'arena>> OpCode for Recv<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.inner.operate(user_data)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        self.inner.overlapped()
    }
}

/// Receive a single piece of data into scattered buffers from remote.
pub struct RecvVectoredImpl<'arena, T: AsIoSlicesMut<'arena>> {
    fd: Fd,
    buffer: T,
    overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlicesMut<'arena>> RecvVectoredImpl<'arena, T> {
    /// Create [`RecvVectored`].
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

impl<'arena, T: AsIoSlicesMut<'arena>> IntoInner for RecvVectoredImpl<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvVectoredImpl<'arena, T> {
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

/// Send a single piece of data from a single buffer to remote.
pub struct Send<'arena, T: IoBuf<'arena>> {
    inner: SendVectoredImpl<'arena, BufWrapper<'arena, T>>,
}

impl<'arena, T: IoBuf<'arena>> Send<'arena, T> {
    /// Create [`Send`]
    pub fn new(fd: Fd, buffer: T) -> Self {
        // SAFETY: buffer is Unpin, IoSliceMut is Unpin as well
        Self {
            inner: SendVectoredImpl::new(fd, BufWrapper::from(buffer)),
        }
    }
}

impl<'arena, T: IoBuf<'arena>> IntoInner for Send<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.inner.into_inner().into_inner()
    }
}

impl<'arena, T: IoBuf<'arena>> OpCode for Send<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.inner.operate(user_data)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        self.inner.overlapped()
    }
}

/// Send a single piece of data to remote using scattered buffers.
pub struct SendVectoredImpl<'arena, T: AsIoSlices<'arena>> {
    fd: Fd,
    buffer: T,
    overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlices<'arena>> SendVectoredImpl<'arena, T> {
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

impl<'arena, T: AsIoSlices<'arena>> IntoInner for SendVectoredImpl<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendVectoredImpl<'arena, T> {
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

/// Receive a single piece of data and source address using a single buffer.
pub struct RecvFrom<'arena, T: IoBufMut<'arena>> {
    inner: RecvMsgImpl<'arena, BufWrapperMut<'arena, T>>,
}

impl<'arena, T: IoBufMut<'arena>> RecvFrom<'arena, T> {
    /// Create [`RecvFrom`]
    pub fn new(fd: Fd, buffer: T) -> Self {
        // SAFETY: buffer is Unpin, IoSliceMut is Unpin as well
        Self {
            inner: RecvMsgImpl::new(fd, BufWrapperMut::from(buffer)),
        }
    }
}

impl<'arena, T: IoBufMut<'arena>> IntoInner for RecvFrom<'arena, T> {
    type Inner = (T, SockAddr);

    fn into_inner(self) -> Self::Inner {
        let (bufwrapper, sockaddr) = self.inner.into_inner();
        (bufwrapper.into_inner(), sockaddr)
    }
}

impl<'arena, T: IoBufMut<'arena>> OpCode for RecvFrom<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.inner.operate(user_data)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        self.inner.overlapped()
    }
}

/// Receive a single piece of data and source address using scattered buffers.
pub struct RecvMsgImpl<'arena, T: AsIoSlicesMut<'arena>> {
    fd: Fd,
    buffer: T,
    addr: SOCKADDR_STORAGE,
    addr_len: socklen_t,
    overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlicesMut<'arena>> RecvMsgImpl<'arena, T> {
    /// Create [`RecvFromVectored`].
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

impl<'arena, T: AsIoSlicesMut<'arena>> IntoInner for RecvMsgImpl<'arena, T> {
    type Inner = (T, SockAddr);

    fn into_inner(self) -> Self::Inner {
        (self.buffer, unsafe {
            SockAddr::new(self.addr, self.addr_len)
        })
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvMsgImpl<'arena, T> {
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

/// Send a single piece of data from a single buffer to the specified address.
pub struct SendTo<'arena, T: IoBuf<'arena>> {
    inner: SendMsgImpl<'arena, BufWrapper<'arena, T>>,
}

impl<'arena, T: IoBuf<'arena>> SendTo<'arena, T> {
    /// Create [`Send`]
    pub fn new(fd: Fd, buffer: T, addr: SockAddr) -> Self {
        // SAFETY: buffer is Unpin, IoSliceMut is Unpin as well
        Self {
            inner: SendMsgImpl::new(fd, BufWrapper::from(buffer), addr),
        }
    }
}

impl<'arena, T: IoBuf<'arena>> IntoInner for SendTo<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.inner.into_inner().into_inner()
    }
}

impl<'arena, T: IoBuf<'arena>> OpCode for SendTo<'arena, T> {
    unsafe fn operate(&mut self, user_data: usize) -> Poll<io::Result<usize>> {
        self.inner.operate(user_data)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        self.inner.overlapped()
    }
}

/// Send a single piece of data from scattered buffers to the specified address.
pub struct SendMsgImpl<'arena, T: AsIoSlices<'arena>> {
    fd: Fd,
    buffer: T,
    addr: SockAddr,
    overlapped: Overlapped,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlices<'arena>> SendMsgImpl<'arena, T> {
    /// Create [`SendToVectored`].
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

impl<'arena, T: AsIoSlices<'arena>> IntoInner for SendMsgImpl<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendMsgImpl<'arena, T> {
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
    fd: Fd,
    overlapped: Overlapped,
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
        let res = ConnectNamedPipe(
            self.fd.as_raw_fd() as _,
            &mut self.overlapped.base as *mut _,
        );
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

fn get_sockopt_error(fd: RawFd) -> Result<(), i32> {
    let mut err_code = 0;
    let mut err_len = std::mem::size_of::<i32>() as i32;

    let rc = unsafe {
        getsockopt(
            fd as _,
            SOL_SOCKET,
            SO_ERROR,
            &mut err_code as *mut _ as *mut u8,
            &mut err_len as *mut _,
        )
    };

    debug_assert!(err_len == 4);
    if rc != 0 {
        Err(rc)
    } else {
        if err_code == 0 { Ok(()) } else { Err(err_code) }
    }
}

fn close_raw_fd(fd: RawFd) -> io::Result<usize> {
    let res = get_sockopt_error(fd);
    match res {
        Err(WSAENOTSOCK) => {
            // close file handle
            let closed = unsafe { CloseHandle(fd as _) };
            if closed == 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(0)
            }
        }
        Err(_) => Err(std::io::Error::last_os_error()),
        Ok(()) => {
            // close socket
            let rc = unsafe { closesocket(fd as _) };
            if rc != 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(0)
            }
        }
    }
}

/// Close attached file descriptor.
///
/// io_uring: it closes in async fashion regular file descriptor
/// kqueue: runs `close` syscall
/// IOCP: it checks whether file is socket and executes either `closesocket` or `CloseHandle`
impl OpCode for Fd {
    unsafe fn operate(&mut self, _user_data: usize) -> Poll<io::Result<usize>> {
        let fd = self.as_raw_fd();
        Poll::Ready(close_raw_fd(fd))
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        unimplemented!("Fd close is synchonous")
    }
}

/// Close some file or socket and set it to None.
///
/// If `Option` value then `io::ErrorKind::InvalidInput` is returned.
///
/// io_uring: it closes in async fashion regular file descriptor
/// kqueue: runs `close` syscall
/// IOCP: it checks whether file is socket and executes either `closesocket` or `CloseHandle`
impl<T: IntoRawFd> OpCode for Option<T> {
    unsafe fn operate(&mut self, _user_data: usize) -> Poll<io::Result<usize>> {
        let maybe_into_raw_fd = std::mem::replace(self, None);
        let result = if let Some(into_raw_fd) = maybe_into_raw_fd {
            let fd = into_raw_fd.into_raw_fd();
            close_raw_fd(fd)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Value is None. File descriptor is already closed?",
            ))
        };
        Poll::Ready(result)
    }

    fn overlapped(&mut self) -> &mut OVERLAPPED {
        unimplemented!("Option<T: IntoRawFd> close is synchonous")
    }
}
