use std::{
    io,
    os::fd::{AsRawFd, BorrowedFd},
};

use mio::unix::pipe::{Receiver, Sender};

use crate::{
    driver::{AsRegisteredFd, RegisteredFd},
    op::Recv,
    task::RUNTIME,
};

/// An event that won't wake until [`EventHandle::notify`] is called
/// successfully.
#[derive(Debug)]
pub struct Event {
    sender: Sender,
    receiver: Receiver,
    registered_fd: RegisteredFd,
}

impl Event {
    /// Create [`Event`].
    pub fn new() -> io::Result<Self> {
        let (sender, receiver) = mio::unix::pipe::new()?;
        let registered_fd = RUNTIME.with(|runtime| runtime.register_fd(receiver.as_raw_fd()))?;

        Ok(Self {
            sender,
            receiver,
            registered_fd,
        })
    }

    /// Get a notify handle.
    pub fn handle(&self) -> EventHandle {
        EventHandle::new(unsafe { BorrowedFd::borrow_raw(self.sender.as_raw_fd()) })
    }

    /// Wait for [`EventHandle::notify`] called.
    pub async fn wait(&self) -> io::Result<()> {
        let buffer = Vec::with_capacity(8);
        // Trick: Recv uses readv which doesn't seek.
        let op = Recv::new(self.registered_fd, buffer);
        let (res, _) = RUNTIME.with(|runtime| runtime.submit(op)).await;
        res?;
        Ok(())
    }
}

impl AsRegisteredFd for Event {
    fn as_registered_fd(&self) -> RegisteredFd {
        self.registered_fd
    }
}

/// A handle to [`Event`].
pub struct EventHandle<'a> {
    fd: BorrowedFd<'a>,
}

impl<'a> EventHandle<'a> {
    pub(crate) fn new(fd: BorrowedFd<'a>) -> Self {
        Self { fd }
    }

    /// Notify the event.
    pub fn notify(&self) -> io::Result<()> {
        let data = 1u64;
        let res = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                &data as *const _ as *const _,
                std::mem::size_of::<u64>(),
            )
        };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
