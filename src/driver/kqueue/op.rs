use std::io;

#[cfg(feature = "time")]
pub use crate::driver::time::Timeout;
pub use crate::driver::unix::op::*;
use crate::{
    buf::{AsIoSlices, AsIoSlicesMut, IoBuf, IoBufMut},
    driver::{OpCode},
    syscall,
};
use rustix::event::kqueue::{Event, EventFilter, EventFlags};

macro_rules! add_event_flags {
    () => {
        EventFlags::ADD | EventFlags::ENABLE | EventFlags::ONESHOT
    }
}

macro_rules! read_filter_event {
    ($self:ident, $user_data:ident) => {
        Event::new(EventFilter::Read($self.fd.as_raw_fd()), add_event_flags!(), $user_data as isize)
    }
}
use read_filter_event;

macro_rules! write_filter_event {
    ($self:ident, $user_data:ident) => {
        Event::new(EventFilter::Write($self.fd.as_raw_fd()), add_event_flags!(), $user_data as isize)
    }
}
use write_filter_event;

impl<'arena, T: IoBufMut<'arena>> OpCode for ReadAt<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        let fd = self.fd.as_raw_fd();
        // SAFETY: slice into buffer is Unpin
        let slice = self.buffer.as_uninit_slice();

        syscall!(
            maybe_block pread(
                fd,
                slice.as_mut_ptr() as _,
                slice.len() as _,
                self.offset as _
            )
        )
    }

    fn as_event(&self, user_data: usize) -> Event {
        read_filter_event!(self, user_data)
    }

}

impl<'arena, T: IoBuf<'arena>> OpCode for WriteAt<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        // SAFETY: buffer is Unpin
        let slice = self.buffer.as_slice();

        syscall!(
            maybe_block pwrite(
                self.fd.as_raw_fd(),
                slice.as_ptr() as _,
                slice.len() as _,
                self.offset as _
            )
        )
    }

    fn as_event(&self, user_data: usize) -> Event {
        write_filter_event!(self, user_data)
    }
}

impl OpCode for Sync {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        Some(syscall!(fsync(self.fd.as_raw_fd())).map(|ok| usize::try_from(ok).expect("non negative")))
    }

    fn as_event(&self, _: usize) -> Event {
        unreachable!("Sync operation should complete in one shot")
    }
}

impl OpCode for Accept {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        // SAFETY: buffer is Unpin
        syscall!(
            maybe_block accept(
                self.fd.as_raw_fd(),
                &mut self.buffer as *mut _ as *mut _,
                &mut self.addr_len
            )
        )
    }

    fn as_event(&self, user_data: usize) -> Event {
        read_filter_event!(self, user_data)
    }
}

impl OpCode for Connect {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        if !self.initiated {
            self.initiated = true;
            syscall!(
                maybe_block connect(self.fd.as_raw_fd(), self.addr.as_ptr(), self.addr.len())
            )
        } else {
            let mut err: libc::c_int = 0;
            let mut err_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;

            if let Err(syscall_err) = syscall!(getsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut err as *mut _ as *mut _,
                &mut err_len
            )) {
                Some(Err(syscall_err))
            } else {
                if err == 0 {
                    Some(Ok(0))
                }
                else {
                    Some(Err(io::Error::from_raw_os_error(err)))
                }
            }
        }
    }

    fn as_event(&self, user_data: usize) -> Event {
        write_filter_event!(self, user_data)
    }

}

impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvImpl<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        let fd = self.fd;
        // SAFETY: IoSliceMut is Unpin
        let slices = unsafe { self.buffer.as_io_slices_mut() };
        syscall!(maybe_block readv(fd.as_raw_fd(), slices.as_mut_ptr() as _, slices.len() as _,))
    }

    fn as_event(&self, user_data: usize) -> Event {
        read_filter_event!(self, user_data)
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendImpl<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        // SAFETY: IoSlice is Unpin
        let slices = unsafe { self.buffer.as_io_slices() };
        syscall!(maybe_block writev(self.fd.as_raw_fd(), slices.as_ptr() as _, slices.len() as _,))
    }

    fn as_event(&self, user_data: usize) -> Event {
        write_filter_event!(self, user_data)
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvFromImpl<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        if self.msg.msg_namelen == 0 {
            let fd = self.fd;
            let msg = self.set_msg();
            syscall!(maybe_block recvmsg(fd.as_raw_fd(), msg, 0))
        } else {
            syscall!(maybe_block recvmsg(self.fd.as_raw_fd(), &mut self.msg, 0))
        }
    }

    fn as_event(&self, user_data: usize) -> Event {
        read_filter_event!(self, user_data)
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendToImpl<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        if self.msg.msg_namelen == 0 {
            let fd = self.fd;
            let msg = self.set_msg();
            syscall!(maybe_block sendmsg(fd.as_raw_fd(), msg, 0))
        } else {
            syscall!(maybe_block sendmsg(self.fd.as_raw_fd(), &self.msg, 0))
        }
    }

    fn as_event(&self, user_data: usize) -> Event {
        write_filter_event!(self, user_data)
    }
}

#[cfg(feature = "time")]
impl OpCode for Timeout {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        Some(Ok(super::TIMER_PENDING))
    }

    fn as_event(&self, _user_data: usize) -> Event {
        unimplemented!("not relevant to timers")
    }

    #[cfg(feature = "time")]
    fn timer_delay(&self) -> std::time::Duration {
        self.delay
    }
}
