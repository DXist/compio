use std::marker::PhantomData;

use libc::{sockaddr_storage, socklen_t};
use socket2::SockAddr;

use crate::{
    buf::{AsIoSlices, AsIoSlicesMut, IntoInner, IoBuf, IoBufMut},
    driver::{unix::IntoFdOrFixed, FdOrFixed},
};

/// Read a file at specified position into specified buffer.
pub struct ReadAt<'arena, T: IoBufMut<'arena>> {
    pub(crate) fd: FdOrFixed,
    pub(crate) offset: usize,
    pub(crate) buffer: T,
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

/// Write a file at specified position from specified buffer.
pub struct WriteAt<'arena, T: IoBuf<'arena>> {
    pub(crate) fd: FdOrFixed,
    pub(crate) offset: usize,
    pub(crate) buffer: T,
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
    pub(crate) fd: FdOrFixed,
    pub(crate) addr: SockAddr,
    #[cfg(not(target_os = "linux"))]
    pub(crate) initiated: bool,
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
}

/// Accept a connection.
pub struct Accept {
    pub(crate) fd: FdOrFixed,
    pub(crate) buffer: sockaddr_storage,
    pub(crate) addr_len: socklen_t,
}

impl Accept {
    /// Create [`Accept`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>) -> Self {
        Self {
            fd: fd.into(),
            buffer: unsafe { std::mem::zeroed() },
            addr_len: std::mem::size_of::<sockaddr_storage>() as _,
        }
    }

    /// Get the remote address from the inner buffer.
    pub fn into_addr(self) -> SockAddr {
        unsafe { SockAddr::new(self.buffer, self.addr_len) }
    }
}

/// Sync data to the disk.
pub struct Sync {
    pub(crate) fd: FdOrFixed,
    #[allow(dead_code)]
    pub(crate) datasync: bool,
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

/// Receive data from remote.
pub struct RecvImpl<'arena, T: AsIoSlicesMut<'arena>> {
    pub(crate) fd: FdOrFixed,
    pub(crate) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlicesMut<'arena>> RecvImpl<'arena, T> {
    /// Create [`Recv`] or [`RecvVectored`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            buffer,
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

/// Send data to remote.
pub struct SendImpl<'arena, T: AsIoSlices<'arena>> {
    pub(crate) fd: FdOrFixed,
    pub(crate) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlices<'arena>> SendImpl<'arena, T> {
    /// Create [`Send`] or [`SendVectored`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            buffer,
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

/// Receive data and source address.
pub struct RecvMsgImpl<'arena, T: AsIoSlicesMut<'arena>> {
    pub(crate) fd: FdOrFixed,
    pub(crate) buffer: T,
    pub(crate) addr: sockaddr_storage,
    pub(crate) msg: libc::msghdr,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlicesMut<'arena>> RecvMsgImpl<'arena, T> {
    /// Create [`RecvFrom`] or [`RecvFromVectored`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            buffer,
            addr: unsafe { std::mem::zeroed() },
            msg: unsafe { std::mem::zeroed() },
            _lifetime: PhantomData,
        }
    }

    pub(crate) fn set_msg(&mut self) -> &mut libc::msghdr {
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
    type Inner = (T, sockaddr_storage, socklen_t);

    fn into_inner(self) -> Self::Inner {
        (self.buffer, self.addr, self.msg.msg_namelen)
    }
}

/// Send data to specified address.
pub struct SendToImpl<'arena, T: AsIoSlices<'arena>> {
    pub(crate) fd: FdOrFixed,
    pub(crate) buffer: T,
    pub(crate) addr: SockAddr,
    pub(crate) msg: libc::msghdr,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlices<'arena>> SendToImpl<'arena, T> {
    /// Create [`SendTo`] or [`SendToVectored`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T, addr: SockAddr) -> Self {
        Self {
            fd: fd.into(),
            buffer,
            addr,
            msg: unsafe { std::mem::zeroed() },
            _lifetime: PhantomData,
        }
    }

    pub(crate) fn set_msg(&mut self) -> &libc::msghdr {
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

impl<'arena, T: AsIoSlices<'arena>> IntoInner for SendToImpl<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}
