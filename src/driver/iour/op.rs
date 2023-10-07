#[cfg(feature = "time")]
use std::time::Duration;

#[cfg(feature = "time")]
use io_uring::types::{TimeoutFlags, Timespec};
use io_uring::{
    opcode,
    squeue::Entry,
    types::{self, FsyncFlags},
};
use libc::sockaddr_storage;
use socket2::SockAddr;

pub use crate::driver::unix::op::*;
use crate::driver::unix::IntoFdOrFixed;
use crate::{
    buf::{AsIoSlices, AsIoSlicesMut, IoBuf, IoBufMut, IntoInner, BufWrapper, BufWrapperMut},
    driver::{Fd, FdOrFixed, IntoRawFd, OpCode},
};

macro_rules! apply_to_fd_or_fixed {
    ($opcode_new:path ; $fd:expr $(, $($arg: expr),*)?) => {
        match $fd {
            FdOrFixed::Fd(fd) => $opcode_new(types::Fd(fd.as_raw_fd()) $(, $($arg,)*)?),
            FdOrFixed::Fixed(fixed_fd) => $opcode_new(types::Fixed(fixed_fd.as_offset()) $(, $($arg,)*)?)
        }
    }

}

impl<'arena, T: IoBufMut<'arena>> OpCode for Read<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: slice into buffer is Unpin
        let slice = self.buffer.as_uninit_slice();
        apply_to_fd_or_fixed!(opcode::Read::new; self.fd, slice.as_mut_ptr() as _, slice.len() as _).build()
    }
}

impl<'arena, T: IoBufMut<'arena>> OpCode for ReadAt<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: slice into buffer is Unpin
        let slice = self.buffer.as_uninit_slice();
        apply_to_fd_or_fixed!(opcode::Read::new; self.fd, slice.as_mut_ptr() as _, slice.len() as _)
            .offset(self.offset as _)
            .build()
    }
}

impl<'arena, T: IoBuf<'arena>> OpCode for Write<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: slice into buffer is Unpin
        let slice = self.buffer.as_slice();
        apply_to_fd_or_fixed!(opcode::Write::new; self.fd, slice.as_ptr(), slice.len() as _).build()
    }
}

impl<'arena, T: IoBuf<'arena>> OpCode for WriteAt<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: slice into buffer is Unpin
        let slice = self.buffer.as_slice();
        apply_to_fd_or_fixed!(opcode::Write::new; self.fd, slice.as_ptr(), slice.len() as _)
            .offset(self.offset as _)
            .build()
    }
}

impl OpCode for Sync {
    fn create_entry(&mut self) -> Entry {
        apply_to_fd_or_fixed!(opcode::Fsync::new; self.fd)
            .flags(if self.datasync {
                FsyncFlags::DATASYNC
            } else {
                FsyncFlags::empty()
            })
            .build()
    }
}

impl OpCode for Accept {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: buffer is Unpin
        let buf_pointer = &mut self.buffer as *mut sockaddr_storage as *mut libc::sockaddr;
        apply_to_fd_or_fixed!(opcode::Accept::new; self.fd, buf_pointer, &mut self.addr_len).build()
    }
}

impl OpCode for Connect {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: SockAddr is Unpin
        apply_to_fd_or_fixed!(opcode::Connect::new; self.fd, self.addr.as_ptr(), self.addr.len())
            .build()
    }
}

impl<'arena, T: IoBufMut<'arena>> OpCode for Recv<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: IoSliceMut is Unpin
        let slice = self.buffer.as_uninit_slice();
        // From https://github.com/fredrikwidlund/libreactor/issues/5 :
        // "In my tests using the Techempower JSON benchmark running on an AWS
        // EC2 instance, I was able to achieve a performance improvement of a
        // little over 10% by using the send/recv functions (with the flags
        // param set to 0) in place of write/read."

        // "From the attached flamegraphs (see below) of the syscalls made during
        // the test, you can see that sys_read/sys_write call several intermediate
        // functions before finally calling inet_recvmsg and sock_sendmsg. So even
        // though the behavior is functionally identical, there is a performance
        // gain to be had that shows up tests like this."
        apply_to_fd_or_fixed!(opcode::Recv::new; self.fd, slice.as_mut_ptr() as _, slice.len() as _).build()
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvVectoredImpl<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: IoSliceMut is Unpin
        let slices = unsafe { self.buffer.as_io_slices_mut() };
        apply_to_fd_or_fixed!(opcode::Readv::new; self.fd, slices.as_mut_ptr() as _, slices.len() as _).build()
    }
}

impl<'arena, T: IoBuf<'arena>> OpCode for Send<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: IoSlice is Unpin
        let slice = self.buffer.as_slice();
        apply_to_fd_or_fixed!(opcode::Send::new; self.fd, slice.as_ptr() as _, slice.len() as _).build()
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendVectoredImpl<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: IoSlice is Unpin
        let slices = unsafe { self.buffer.as_io_slices() };
        apply_to_fd_or_fixed!(opcode::Writev::new; self.fd, slices.as_ptr() as _, slices.len() as _)
            .build()
    }
}

// SendTo/RecvFrom opcodes are in progress - https://github.com/axboe/liburing/issues/397
// We fallback to reuse SendMsg/RcvMsg

/// Receive a single piece of data and source address using a single buffer.
pub struct RecvFrom<'arena, T: IoBufMut<'arena>> {
    inner: RecvMsgImpl<'arena, BufWrapperMut<'arena, T>>
}

impl<'arena, T: IoBufMut<'arena>> RecvFrom<'arena, T> {
    /// Create [`RecvFrom`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            inner: RecvMsgImpl::new(fd, BufWrapperMut::from(buffer))
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
    fn create_entry(&mut self) -> Entry {
        self.inner.create_entry()
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvMsgImpl<'arena, T> {
    #[allow(clippy::no_effect)]
    fn create_entry(&mut self) -> Entry {
        let fd = self.fd;
        let msg = self.set_msg();
        apply_to_fd_or_fixed!(opcode::RecvMsg::new; fd, msg as *mut _).build()
    }
}

/// Send a single piece of data from a single buffer to the specified address.
pub struct SendTo<'arena, T: IoBuf<'arena>> {
    inner: SendMsgImpl<'arena, BufWrapper<'arena, T>>
}

impl<'arena, T: IoBuf<'arena>> SendTo<'arena, T> {
    /// Create [`SendTo`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T, addr: SockAddr) -> Self {
        Self {
            inner: SendMsgImpl::new(fd, BufWrapper::from(buffer), addr)
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
    fn create_entry(&mut self) -> Entry {
        self.inner.create_entry()
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendMsgImpl<'arena, T> {
    #[allow(clippy::no_effect)]
    fn create_entry(&mut self) -> Entry {
        let fd = self.fd;
        let msg = self.set_msg();
        apply_to_fd_or_fixed!(opcode::SendMsg::new; fd, msg).build()
    }
}

/// Timeout operation completes after the given relative timeout duration.
///
/// If supported by platform timeout operation will take into account the time
/// spent in low power modes or suspend (CLOCK_BOOTTIME). Otherwise
/// CLOCK_MONOTONIC is used.
///
/// Only io_uring driver supports waiting using CLOCK_BOOTTIME clock.
#[cfg(feature = "time")]
#[repr(transparent)]
pub struct Timeout {
    timespec: Timespec,
}

#[cfg(feature = "time")]
impl Timeout {
    // ETIME_SUCCESS seems not to work on Linux 5.15
    const FLAGS: TimeoutFlags = unsafe {
        TimeoutFlags::from_bits_unchecked(
            TimeoutFlags::BOOTTIME.bits() | TimeoutFlags::ETIME_SUCCESS.bits(),
        )
    };

    /// Create `Timeout` with the provided duration.
    pub fn new(delay: Duration) -> Self {
        let timespec = Timespec::from(delay);
        Self { timespec }
    }
}

#[cfg(feature = "time")]
impl OpCode for Timeout {
    fn create_entry(&mut self) -> Entry {
        opcode::Timeout::new(&self.timespec as *const Timespec)
            .flags(Self::FLAGS)
            .build()
    }
}

/// Close attached file descriptor.
impl OpCode for Fd {
    fn create_entry(&mut self) -> Entry {
        opcode::Close::new(types::Fd(self.as_raw_fd())).build()
    }
}

/// Close some file or socket and set it to None.
///
/// If option value is None the operation panics.
impl<T: IntoRawFd> OpCode for Option<T> {
    fn create_entry(&mut self) -> Entry {
        let maybe_into_raw_fd = std::mem::replace(self, None);
        if let Some(into_raw_fd) = maybe_into_raw_fd {
            let fd = into_raw_fd.into_raw_fd();
            opcode::Close::new(types::Fd(fd)).build()
        } else {
            panic!("Option<T: IntoRawFd> is None. Can't create Close operation.")
        }
    }
}
