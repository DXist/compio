use std::{io, marker::PhantomData};

use libc::{sockaddr_storage, socklen_t};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::{
    buf::{AsIoSlices, AsIoSlicesMut, IntoInner, IoBuf, IoBufMut},
    driver::{unix::IntoFdOrFixed, FdOrFixed, FromRawFd, RawFd},
};

/// Read a nonseekable file into specified buffer.
pub struct Read<'arena, T: IoBufMut<'arena>> {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBufMut<'arena>> Read<'arena, T> {
    /// Create [`Read`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            buffer,
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

/// Read a file at specified position into specified buffer.
pub struct ReadAt<'arena, T: IoBufMut<'arena>> {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) offset: usize,
    pub(in crate::driver) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBufMut<'arena>> ReadAt<'arena, T> {
    /// Create [`ReadAt`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, offset: usize, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            offset,
            buffer,
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

/// Write a nonseekable file from specified buffer.
pub struct Write<'arena, T: IoBuf<'arena>> {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBuf<'arena>> Write<'arena, T> {
    /// Create [`Write`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            buffer,
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

/// Write a file at specified position from specified buffer.
pub struct WriteAt<'arena, T: IoBuf<'arena>> {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) offset: usize,
    pub(in crate::driver) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBuf<'arena>> WriteAt<'arena, T> {
    /// Create [`WriteAt`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, offset: usize, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            offset,
            buffer,
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

/// Connect to a remote address.
pub struct Connect {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) addr: SockAddr,
    #[cfg(not(target_os = "linux"))]
    pub(in crate::driver) initiated: bool,
}

impl Connect {
    /// Create [`Connect`]. `fd` should be bound.
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, addr: SockAddr) -> Self {
        #[cfg(target_os = "linux")]
        let this = Self {
            fd: fd.into(),
            addr,
        };
        #[cfg(not(target_os = "linux"))]
        let this = {
            let initiated = false;
            Self {
                fd: fd.into(),
                addr,
                initiated,
            }
        };
        this
    }

    /// Post operation socket handling.
    ///
    /// For compatibility with IOCP.
    pub fn on_connect(self, result: io::Result<usize>) -> io::Result<()> {
        result.map(|_| ())
    }
}

/// Accept a connection.
///
/// It's possible to reinit the data structure and reuse it for the following accepts.
pub struct Accept {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) addr: SockAddr,
    pub(in crate::driver) addr_len: socklen_t,
}

impl Accept {
    /// Create [`Accept`] with the provided accept socket options.
    ///
    /// Similar interface between IOCP/unix
    pub fn with_socket_opts(
        fd: impl IntoFdOrFixed<Target = FdOrFixed>,
        _domain: Domain,
        _ty: Type,
        _protocol: Option<Protocol>,
    ) -> Self {
        Self::new(fd)
    }

    /// Create [`Accept`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>) -> Self {
        Self {
            fd: fd.into(),
            addr: unsafe {
                SockAddr::new(
                    std::mem::zeroed(),
                    std::mem::size_of::<sockaddr_storage>() as socklen_t,
                )
            },
            addr_len: std::mem::size_of::<sockaddr_storage>() as socklen_t,
        }
    }
    ///
    /// Init existing [`Accept`] for new accept operation.
    pub fn init_with_socket_opts(
        &mut self,
        fd: impl IntoFdOrFixed<Target = FdOrFixed>,
        _domain: Domain,
        _ty: Type,
        _protocol: Option<Protocol>,
    ) {
        self.fd = fd.into();
    }

    /// Post operation socket handling.
    ///
    /// Set nonblocking for kqueue.
    /// Get remote address.
    pub fn on_accept(&mut self, result: io::Result<usize>) -> io::Result<(Socket, &SockAddr)> {
        let accept_sock = unsafe { Socket::from_raw_fd(result? as RawFd) };
        #[cfg(all(unix, not(target_os = "linux")))]
        accept_sock.set_nonblocking(true)?;
        let addr = self.as_sockaddr();
        Ok((accept_sock, addr))
    }

    /// Get the remote address from the inner buffer.
    pub fn as_sockaddr(&mut self) -> &SockAddr {
        unsafe { self.addr.set_length(self.addr_len) };
        &self.addr
    }
}

/// Sync data to the disk.
pub struct Sync {
    pub(in crate::driver) fd: FdOrFixed,
    #[allow(dead_code)]
    pub(in crate::driver) datasync: bool,
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
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, datasync: bool) -> Self {
        Self {
            fd: fd.into(),
            datasync,
        }
    }
}

/// Receive a single piece of data in a single buffer from remote.
pub struct Recv<'arena, T: IoBufMut<'arena>> {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBufMut<'arena>> Recv<'arena, T> {
    /// Create [`Recv`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            buffer,
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: IoBufMut<'arena>> IntoInner for Recv<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

/// Receive a single piece of data into scattered buffers from remote.
pub struct RecvVectoredImpl<'arena, T: AsIoSlicesMut<'arena>> {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlicesMut<'arena>> RecvVectoredImpl<'arena, T> {
    /// Create [`RecvVectored`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            buffer,
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

/// Send a single piece of data from a single buffer to remote.
pub struct Send<'arena, T: IoBuf<'arena>> {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBuf<'arena>> Send<'arena, T> {
    /// Create [`Send`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            buffer,
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: IoBuf<'arena>> IntoInner for Send<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

/// Send a single piece of data to remote using scattered buffers.
pub struct SendVectoredImpl<'arena, T: AsIoSlices<'arena>> {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlices<'arena>> SendVectoredImpl<'arena, T> {
    /// Create [`SendVectored`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            buffer,
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

/// Receive a single piece of data and source address using scattered buffers.
pub struct RecvMsgImpl<'arena, T: AsIoSlicesMut<'arena>> {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) buffer: T,
    pub(in crate::driver) addr: sockaddr_storage,
    pub(in crate::driver) msg: libc::msghdr,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlicesMut<'arena>> RecvMsgImpl<'arena, T> {
    /// Create [`RecvFromVectored`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            buffer,
            addr: unsafe { std::mem::zeroed() },
            msg: unsafe { std::mem::zeroed() },
            _lifetime: PhantomData,
        }
    }

    pub(in crate::driver) fn set_msg(&mut self) -> &mut libc::msghdr {
        // SAFETY: IoSliceMut is Unpin
        let (slices, len) = unsafe {
            let slices = self.buffer.as_io_slices_mut();
            let len = slices.len();
            (slices.as_mut_ptr(), len)
        };
        self.msg = libc::msghdr {
            msg_name: &mut self.addr as *mut _ as _,
            msg_namelen: std::mem::size_of_val(&self.addr) as _,
            msg_iov: slices as _,
            msg_iovlen: len as _,
            msg_control: std::ptr::null_mut(),
            msg_controllen: 0,
            msg_flags: 0,
        };
        &mut self.msg
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> IntoInner for RecvMsgImpl<'arena, T> {
    type Inner = (T, SockAddr);

    fn into_inner(self) -> Self::Inner {
        (self.buffer, unsafe {
            SockAddr::new(self.addr, self.msg.msg_namelen)
        })
    }
}

/// Send a single piece of data from scattered buffers to the specified address.
pub struct SendMsgImpl<'arena, T: AsIoSlices<'arena>> {
    pub(in crate::driver) fd: FdOrFixed,
    pub(in crate::driver) buffer: T,
    pub(in crate::driver) addr: SockAddr,
    pub(in crate::driver) msg: libc::msghdr,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlices<'arena>> SendMsgImpl<'arena, T> {
    /// Create [`SendToVectored`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T, addr: SockAddr) -> Self {
        Self {
            fd: fd.into(),
            buffer,
            addr,
            msg: unsafe { std::mem::zeroed() },
            _lifetime: PhantomData,
        }
    }

    pub(in crate::driver) fn set_msg(&mut self) -> &libc::msghdr {
        // SAFETY: IoSlice is Unpin
        let (slices, len) = unsafe {
            let slices = self.buffer.as_io_slices();
            let len = slices.len();
            (slices.as_ptr(), len)
        };
        self.msg = libc::msghdr {
            msg_name: self.addr.as_ptr() as _,
            msg_namelen: self.addr.len(),
            msg_iov: slices as _,
            msg_iovlen: len as _,
            msg_control: std::ptr::null_mut(),
            msg_controllen: 0,
            msg_flags: 0,
        };
        &self.msg
    }
}

impl<'arena, T: AsIoSlices<'arena>> IntoInner for SendMsgImpl<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}
