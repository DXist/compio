//! Unix-specific types for signal handling.

use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    os::fd::{AsRawFd, RawFd},
};

use crate::event::{Event, EventHandle};

thread_local! {
    static HANDLER: RefCell<HashMap<i32, HashMap<RawFd, EventHandle>>> =
        RefCell::new(HashMap::new());
}

unsafe extern "C" fn signal_handler(sig: i32) {
    HANDLER.with(|handler| {
        let mut handler = handler.borrow_mut();
        if let Some(fds) = handler.get_mut(&sig) {
            if !fds.is_empty() {
                let fds = std::mem::take(fds);
                for (_, mut fd) in fds {
                    fd.notify().ok();
                }
            }
        }
    });
}

unsafe fn init(sig: i32) {
    libc::signal(sig, signal_handler as *const () as usize);
}

unsafe fn uninit(sig: i32) {
    libc::signal(sig, libc::SIG_DFL);
}

fn register(sig: i32, fd: &Event) -> io::Result<()> {
    unsafe { init(sig) };
    let raw_fd = fd.as_raw_fd();
    let handle = fd.handle()?;
    HANDLER.with(|handler| {
        handler
            .borrow_mut()
            .entry(sig)
            .or_default()
            .insert(raw_fd, handle)
    });
    Ok(())
}

fn unregister(sig: i32, fd: RawFd) {
    let need_uninit = HANDLER.with(|handler| {
        let mut handler = handler.borrow_mut();
        if let Some(fds) = handler.get_mut(&sig) {
            fds.remove(&fd);
            if !fds.is_empty() {
                return false;
            }
        }
        true
    });
    if need_uninit {
        unsafe { uninit(sig) };
    }
}

/// Represents a listener to unix signal event.
#[derive(Debug)]
struct SignalFd {
    sig: i32,
    fd: Event,
}

impl SignalFd {
    fn new(sig: i32) -> io::Result<Self> {
        let fd = Event::new()?;
        register(sig, &fd)?;
        Ok(Self { sig, fd })
    }

    async fn wait(&self) -> io::Result<()> {
        self.fd.wait().await
    }
}

impl Drop for SignalFd {
    fn drop(&mut self) {
        unregister(self.sig, self.fd.as_raw_fd());
    }
}

/// Creates a new listener which will receive notifications when the current
/// process receives the specified signal.
pub async fn signal(sig: i32) -> io::Result<()> {
    let fd = SignalFd::new(sig)?;
    fd.wait().await
}
