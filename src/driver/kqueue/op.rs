use std::{io, marker::PhantomData, mem::size_of};

use libc::{sockaddr, sockaddr_storage, socklen_t};
use rustix::event::kqueue::{Event, EventFilter, EventFlags};
use socket2::SockAddr;

#[cfg(feature = "time")]
pub use crate::driver::time::Timeout;
pub use crate::driver::unix::op::*;
use crate::{
    buf::{AsIoSlices, AsIoSlicesMut, IntoInner, IoBuf, IoBufMut},
    driver::{unix::IntoFdOrFixed, Fd, FdOrFixed, IntoRawFd, OpCode, RawFd},
    syscall,
};

macro_rules! add_event_flags {
    () => {
        EventFlags::ADD | EventFlags::ENABLE | EventFlags::ONESHOT
    };
}

macro_rules! read_filter_event {
    ($self:ident, $user_data:ident) => {
        Event::new(
            EventFilter::Read($self.fd.as_raw_fd()),
            add_event_flags!(),
            $user_data as isize,
        )
    };
}
use read_filter_event;

macro_rules! write_filter_event {
    ($self:ident, $user_data:ident) => {
        Event::new(
            EventFilter::Write($self.fd.as_raw_fd()),
            add_event_flags!(),
            $user_data as isize,
        )
    };
}
use write_filter_event;

impl<'arena, T: IoBufMut<'arena>> OpCode for Read<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        let fd = self.fd.as_raw_fd();
        // SAFETY: slice into buffer is Unpin
        let slice = self.buffer.as_uninit_slice();

        syscall!(
            maybe_block read(
                fd,
                slice.as_mut_ptr() as _,
                slice.len() as _,
            )
        )
    }

    fn as_event(&self, user_data: usize) -> Event {
        read_filter_event!(self, user_data)
    }
}

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

impl<'arena, T: IoBuf<'arena>> OpCode for Write<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        // SAFETY: buffer is Unpin
        let slice = self.buffer.as_slice();

        syscall!(
            maybe_block write(
                self.fd.as_raw_fd(),
                slice.as_ptr() as _,
                slice.len() as _,
            )
        )
    }

    fn as_event(&self, user_data: usize) -> Event {
        write_filter_event!(self, user_data)
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
        Some(
            syscall!(fsync(self.fd.as_raw_fd()))
                .map(|ok| usize::try_from(ok).expect("non negative")),
        )
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
                self.addr.as_ptr() as *mut sockaddr,
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
                } else {
                    Some(Err(io::Error::from_raw_os_error(err)))
                }
            }
        }
    }

    fn as_event(&self, user_data: usize) -> Event {
        write_filter_event!(self, user_data)
    }
}

impl<'arena, T: IoBufMut<'arena>> OpCode for Recv<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        let fd = self.fd;
        // SAFETY: IoBufMut is Unpin
        let slice = self.buffer.as_uninit_slice();
        syscall!(maybe_block recv(fd.as_raw_fd(), slice.as_mut_ptr() as _, slice.len() as _, 0))
    }

    fn as_event(&self, user_data: usize) -> Event {
        read_filter_event!(self, user_data)
    }
}

impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvVectoredImpl<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        let fd = self.fd;
        // SAFETY: IoSliceMut is Unpin
        let slices = unsafe { self.buffer.as_io_slices_mut() };
        syscall!(maybe_block readv(fd.as_raw_fd(), slices.as_mut_ptr() as _, slices.len() as _))
    }

    fn as_event(&self, user_data: usize) -> Event {
        read_filter_event!(self, user_data)
    }
}

impl<'arena, T: IoBuf<'arena>> OpCode for Send<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        // SAFETY: IoBuf is Unpin
        let slice = self.buffer.as_slice();
        syscall!(maybe_block send(self.fd.as_raw_fd(), slice.as_ptr() as _, slice.len() as _, 0))
    }

    fn as_event(&self, user_data: usize) -> Event {
        write_filter_event!(self, user_data)
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendVectoredImpl<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        // SAFETY: IoSlice is Unpin
        let slices = unsafe { self.buffer.as_io_slices() };
        syscall!(maybe_block writev(self.fd.as_raw_fd(), slices.as_ptr() as _, slices.len() as _,))
    }

    fn as_event(&self, user_data: usize) -> Event {
        write_filter_event!(self, user_data)
    }
}

/// Receive a single piece of data and source address using a single buffer.
pub struct RecvFrom<'arena, T: IoBufMut<'arena>> {
    fd: FdOrFixed,
    buffer: T,
    addr: sockaddr_storage,
    socklen: socklen_t,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBufMut<'arena>> RecvFrom<'arena, T> {
    /// Create [`RecvFrom`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T) -> Self {
        Self {
            fd: fd.into(),
            buffer,
            addr: unsafe { std::mem::zeroed() },
            socklen: size_of::<sockaddr_storage>() as socklen_t,
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: IoBufMut<'arena>> IntoInner for RecvFrom<'arena, T> {
    type Inner = (T, SockAddr);

    fn into_inner(self) -> Self::Inner {
        (self.buffer, unsafe {
            SockAddr::new(self.addr, self.socklen)
        })
    }
}

impl<'arena, T: IoBufMut<'arena>> OpCode for RecvFrom<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        let slice = self.buffer.as_uninit_slice();
        syscall!(maybe_block recvfrom(self.fd.as_raw_fd(), slice.as_mut_ptr() as *mut libc::c_void, slice.len(), 0, &mut self.addr as *mut sockaddr_storage as *mut sockaddr, &mut self.socklen as *mut socklen_t))
    }

    fn as_event(&self, user_data: usize) -> Event {
        read_filter_event!(self, user_data)
    }
}
impl<'arena, T: AsIoSlicesMut<'arena>> OpCode for RecvMsgImpl<'arena, T> {
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

/// Send a single piece of data from a single buffer to the specified address.
pub struct SendTo<'arena, T: IoBuf<'arena>> {
    fd: FdOrFixed,
    buffer: T,
    addr: SockAddr,
    _lifetime: PhantomData<&'arena ()>,
}

impl<'arena, T: IoBuf<'arena>> SendTo<'arena, T> {
    /// Create [`SendTo`].
    pub fn new(fd: impl IntoFdOrFixed<Target = FdOrFixed>, buffer: T, addr: SockAddr) -> Self {
        Self {
            fd: fd.into(),
            buffer,
            addr,
            _lifetime: PhantomData,
        }
    }
}

impl<'arena, T: IoBuf<'arena>> IntoInner for SendTo<'arena, T> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

impl<'arena, T: IoBuf<'arena>> OpCode for SendTo<'arena, T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        let slice = self.buffer.as_slice();
        syscall!(maybe_block sendto(self.fd.as_raw_fd(), slice.as_ptr() as *const libc::c_void, slice.len(), 0, self.addr.as_ptr(), self.addr.len()))
    }

    fn as_event(&self, user_data: usize) -> Event {
        write_filter_event!(self, user_data)
    }
}

impl<'arena, T: AsIoSlices<'arena>> OpCode for SendMsgImpl<'arena, T> {
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

fn close_raw_fd(raw_fd: RawFd) -> io::Result<usize> {
    syscall!(close(raw_fd)).map(|ok| usize::try_from(ok).expect("non negative"))
}

/// Close attached file descriptor.
///
/// io_uring: it closes in async fashion regular file descriptor
/// kqueue: runs `close` syscall
/// IOCP: it checks whether file is socket and executes either `closesocket` or `CloseHandle`
impl OpCode for Fd {
    fn operate(&mut self) -> Option<io::Result<usize>> {
        Some(close_raw_fd(self.as_raw_fd()))
    }

    fn as_event(&self, _: usize) -> Event {
        unreachable!("Close operation should complete in one shot")
    }
}

/// Close some file or socket and set it to None.
///
/// io_uring: it closes in async fashion regular file descriptor
/// kqueue: runs `close` syscall
/// IOCP: it checks whether file is socket and executes either `closesocket` or `CloseHandle`
impl<T: IntoRawFd> OpCode for Option<T> {
    fn operate(&mut self) -> Option<io::Result<usize>> {
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
        Some(result)
    }

    fn as_event(&self, _: usize) -> Event {
        unreachable!("Close operation should complete in one shot")
    }
}
