#[cfg(feature = "time")]
use std::time::Duration;

#[cfg(feature = "time")]
use io_uring::types::{TimeoutFlags, Timespec};
use io_uring::{
    opcode,
    squeue::Entry,
    types::{Fd, FsyncFlags},
};
use libc::sockaddr_storage;

pub use crate::driver::unix::op::*;
use crate::{
    buf::{AsIoSlices, AsIoSlicesMut, IoBuf, IoBufMut},
    driver::OpCode,
};

impl<'arena, T: IoBufMut<'arena>> OpCode for ReadAt<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        let fd = Fd(self.fd);
        // SAFETY: slice into buffer is Unpin
        let slice = self.buffer.as_uninit_slice();
        opcode::Read::new(fd, slice.as_mut_ptr() as _, slice.len() as _)
            .offset(self.offset as _)
            .build()
    }
}

impl<'arena, T: IoBuf<'arena>> OpCode for WriteAt<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: slice into buffer is Unpin
        let slice = self.buffer.as_slice();
        opcode::Write::new(Fd(self.fd), slice.as_ptr(), slice.len() as _)
            .offset(self.offset as _)
            .build()
    }
}

impl OpCode for Sync {
    fn create_entry(&mut self) -> Entry {
        opcode::Fsync::new(Fd(self.fd))
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
        opcode::Accept::new(
            Fd(self.fd),
            // SAFETY: buffer is Unpin
            &mut self.buffer as *mut sockaddr_storage as *mut libc::sockaddr,
            &mut self.addr_len,
        )
        .build()
    }
}

impl OpCode for Connect {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: SockAddr is Unpin
        opcode::Connect::new(Fd(self.fd), self.addr.as_ptr(), self.addr.len()).build()
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvImpl<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: IoSliceMut is Unpin
        let slices = unsafe { self.buffer.as_io_slices_mut() };
        opcode::Readv::new(Fd(self.fd), slices.as_mut_ptr() as _, slices.len() as _).build()
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendImpl<'arena, T> {
    fn create_entry(&mut self) -> Entry {
        // SAFETY: IoSlice is Unpin
        let slices = unsafe { self.buffer.as_io_slices() };
        opcode::Writev::new(Fd(self.fd), slices.as_ptr() as _, slices.len() as _).build()
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvFromImpl<'arena, T> {
    #[allow(clippy::no_effect)]
    fn create_entry(&mut self) -> Entry {
        let fd = self.fd;
        let msg = self.set_msg();
        opcode::RecvMsg::new(Fd(fd), msg as *mut _).build()
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendToImpl<'arena, T> {
    #[allow(clippy::no_effect)]
    fn create_entry(&mut self) -> Entry {
        let fd = self.fd;
        let msg = self.set_msg();
        opcode::SendMsg::new(Fd(fd), msg).build()
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
