use std::{marker::PhantomData, time::Duration};

use libc::{sockaddr_storage, socklen_t, timespec};
use socket2::SockAddr;

#[cfg(doc)]
use crate::op::*;
use crate::{
    buf::{AsIoSlices, AsIoSlicesMut, IntoInner, IoBuf, IoBufMut},
    driver::RawFd,
};

/// Read a file at specified position into specified buffer.
pub struct ReadAt<'arena, T: IoBufMut<'arena>> {
    pub(crate) fd: RawFd,
    pub(crate) offset: usize,
    pub(crate) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBufMut<'arena>> ReadAt<'arena, T> {
    /// Create [`ReadAt`].
    pub fn new(fd: RawFd, offset: usize, buffer: T) -> Self {
        Self {
            fd,
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
    pub(crate) fd: RawFd,
    pub(crate) offset: usize,
    pub(crate) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBuf<'arena>> WriteAt<'arena, T> {
    /// Create [`WriteAt`].
    pub fn new(fd: RawFd, offset: usize, buffer: T) -> Self {
        Self {
            fd,
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
    pub(crate) fd: RawFd,
    pub(crate) addr: SockAddr,
}

impl Connect {
    /// Create [`Connect`]. `fd` should be bound.
    pub fn new(fd: RawFd, addr: SockAddr) -> Self {
        Self { fd, addr }
    }
}

/// Accept a connection.
pub struct Accept {
    pub(crate) fd: RawFd,
    pub(crate) buffer: sockaddr_storage,
    pub(crate) addr_len: socklen_t,
}

impl Accept {
    /// Create [`Accept`].
    pub fn new(fd: RawFd) -> Self {
        Self {
            fd,
            buffer: unsafe { std::mem::zeroed() },
            addr_len: std::mem::size_of::<sockaddr_storage>() as _,
        }
    }

    /// Get the remote address from the inner buffer.
    pub fn into_addr(self) -> SockAddr {
        unsafe { SockAddr::new(self.buffer, self.addr_len) }
    }
}

/// Receive data from remote.
pub struct RecvImpl<'arena, T: AsIoSlicesMut<'arena>> {
    pub(crate) fd: RawFd,
    pub(crate) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlicesMut<'arena>> RecvImpl<'arena, T> {
    /// Create [`Recv`] or [`RecvVectored`].
    pub fn new(fd: RawFd, buffer: T) -> Self {
        Self {
            fd,
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
    pub(crate) fd: RawFd,
    pub(crate) buffer: T,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlices<'arena>> SendImpl<'arena, T> {
    /// Create [`Send`] or [`SendVectored`].
    pub fn new(fd: RawFd, buffer: T) -> Self {
        Self {
            fd,
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
pub struct RecvFromImpl<'arena, T: AsIoSlicesMut<'arena>> {
    pub(crate) fd: RawFd,
    pub(crate) buffer: T,
    pub(crate) addr: sockaddr_storage,
    pub(crate) msg: libc::msghdr,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlicesMut<'arena>> RecvFromImpl<'arena, T> {
    /// Create [`RecvFrom`] or [`RecvFromVectored`].
    pub fn new(fd: RawFd, buffer: T) -> Self {
        Self {
            fd,
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

impl<'arena, T: AsIoSlicesMut<'arena>> IntoInner for RecvFromImpl<'arena, T> {
    type Inner = (T, sockaddr_storage, socklen_t);

    fn into_inner(self) -> Self::Inner {
        (self.buffer, self.addr, self.msg.msg_namelen)
    }
}

/// Send data to specified address.
pub struct SendToImpl<'arena, T: AsIoSlices<'arena>> {
    pub(crate) fd: RawFd,
    pub(crate) buffer: T,
    pub(crate) addr: SockAddr,
    pub(crate) msg: libc::msghdr,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: AsIoSlices<'arena>> SendToImpl<'arena, T> {
    /// Create [`SendTo`] or [`SendToVectored`].
    pub fn new(fd: RawFd, buffer: T, addr: SockAddr) -> Self {
        Self {
            fd,
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

/// Timeout operation completes after th given relative timeout duration.
///
/// If supported by platform timeout operation will take into account the time
/// spent in low power modes or suspend (CLOCK_BOOTTIME). Otherwise
/// CLOCK_MONOTONIC is used.
///
/// Only io_uring driver supports CLOCK_BOOTTIME.
#[derive(Debug)]
pub struct Timeout {
    pub(crate) duration: timespec,
}

impl Timeout {
    /// Create [`Timeout`].
    pub fn new(duration: Duration) -> Self {
        let tv_sec = i64::try_from(duration.as_secs()).expect("duration not overflows i64");
        let tv_nsec = duration.subsec_nanos();

        Self {
            duration: timespec {
                tv_sec,
                tv_nsec: tv_nsec.into(),
            },
        }
    }
}
