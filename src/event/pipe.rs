use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
    process::{ChildStderr, ChildStdout},
};

use arrayvec::ArrayVec;

use crate::{buf::BufWrapperMut, impl_raw_fd, op::Recv, syscall, task::RUNTIME};

/// An event that won't wake until [`EventHandle::notify`] is called
/// successfully.
#[derive(Debug)]
pub struct Event {
    sender: OwnedFd,
    receiver: OwnedFd,
}

impl Event {
    /// Create [`Event`].
    pub fn new() -> io::Result<Self> {
        let (sender, receiver) = pipe_new()?;
        sender.set_nonblocking(false)?;
        let sender = unsafe { OwnedFd::from_raw_fd(sender.into_raw_fd()) };
        let receiver = unsafe { OwnedFd::from_raw_fd(receiver.into_raw_fd()) };
        Ok(Self { sender, receiver })
    }

    /// Get a notify handle.
    pub fn handle(&self) -> io::Result<EventHandle> {
        Ok(EventHandle::new(self.sender.try_clone()?))
    }

    /// Wait for [`EventHandle::notify`] called.
    pub async fn wait(&self) -> io::Result<()> {
        let buffer = ArrayVec::<u8, 1>::new();
        let fd = RUNTIME.with(|runtime| runtime.attach(self.receiver.as_raw_fd()))?;
        // Trick: Recv uses readv which doesn't seek.
        let op = Recv::new(fd, BufWrapperMut::from(buffer));
        let (res, _) = RUNTIME.with(|runtime| runtime.submit(op)).await;
        res?;
        Ok(())
    }
}

impl AsRawFd for Event {
    fn as_raw_fd(&self) -> RawFd {
        self.receiver.as_raw_fd()
    }
}

/// A handle to [`Event`].
pub struct EventHandle {
    fd: OwnedFd,
}

impl EventHandle {
    pub(crate) fn new(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// Notify the event.
    pub fn notify(&mut self) -> io::Result<()> {
        let data = &[1];
        syscall!(write(self.fd.as_raw_fd(), data.as_ptr() as _, 1))?;
        Ok(())
    }
}

impl_raw_fd!(EventHandle, fd);

/// From mio::unix::pipe
fn pipe_new_raw() -> io::Result<[RawFd; 2]> {
    let mut fds: [RawFd; 2] = [-1, -1];

    #[cfg(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "illumos",
        target_os = "redox",
    ))]
    unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    #[cfg(any(
        target_os = "aix",
        target_os = "ios",
        target_os = "macos",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "espidf",
    ))]
    unsafe {
        // For platforms that don't have `pipe2(2)` we need to manually set the
        // correct flags on the file descriptor.
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }

        for fd in &fds {
            if libc::fcntl(*fd, libc::F_SETFL, libc::O_NONBLOCK) != 0
                || libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC) != 0
            {
                let err = io::Error::last_os_error();
                // Don't leak file descriptors. Can't handle closing error though.
                let _ = libc::close(fds[0]);
                let _ = libc::close(fds[1]);
                return Err(err);
            }
        }
    }

    Ok(fds)
}

fn pipe_new() -> io::Result<(Sender, Receiver)> {
    let fds = pipe_new_raw()?;
    // SAFETY: `new_raw` initialised the `fds` above.
    let r = unsafe { Receiver::from_raw_fd(fds[0]) };
    let w = unsafe { Sender::from_raw_fd(fds[1]) };
    Ok((w, r))
}

/// Sending end of an Unix pipe.
///
/// See [`new`] for documentation, including examples.
#[derive(Debug)]
struct Sender {
    inner: File,
}

impl Sender {
    /// Set the `Sender` into or out of non-blocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_nonblocking(self.inner.as_raw_fd(), nonblocking)
    }
}

impl FromRawFd for Sender {
    unsafe fn from_raw_fd(fd: RawFd) -> Sender {
        Sender {
            inner: File::from_raw_fd(fd),
        }
    }
}

impl AsRawFd for Sender {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for Sender {
    fn into_raw_fd(self) -> RawFd {
        self.inner.into_raw_fd()
    }
}

/// Receiving end of an Unix pipe.
///
/// See [`new`] for documentation, including examples.
#[derive(Debug)]
struct Receiver {
    inner: File,
}

/// # Notes
///
/// The underlying pipe is **not** set to non-blocking.
impl From<ChildStdout> for Receiver {
    fn from(stdout: ChildStdout) -> Receiver {
        // Safety: `ChildStdout` is guaranteed to be a valid file descriptor.
        unsafe { Receiver::from_raw_fd(stdout.into_raw_fd()) }
    }
}

/// # Notes
///
/// The underlying pipe is **not** set to non-blocking.
impl From<ChildStderr> for Receiver {
    fn from(stderr: ChildStderr) -> Receiver {
        // Safety: `ChildStderr` is guaranteed to be a valid file descriptor.
        unsafe { Receiver::from_raw_fd(stderr.into_raw_fd()) }
    }
}

impl FromRawFd for Receiver {
    unsafe fn from_raw_fd(fd: RawFd) -> Receiver {
        Receiver {
            inner: File::from_raw_fd(fd),
        }
    }
}

impl AsRawFd for Receiver {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for Receiver {
    fn into_raw_fd(self) -> RawFd {
        self.inner.into_raw_fd()
    }
}

#[cfg(not(target_os = "illumos"))]
fn set_nonblocking(fd: RawFd, nonblocking: bool) -> io::Result<()> {
    let value = nonblocking as libc::c_int;
    if unsafe { libc::ioctl(fd, libc::FIONBIO, &value) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
