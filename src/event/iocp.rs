use std::io;

use crate::{
    driver::{RawFd, post_driver_raw, Overlapped},
    key::Key,
    task::{op::OpFuture, RUNTIME},
};

/// An event that won't wake until [`EventHandle::notify`] is called
/// successfully.
#[derive(Debug)]
pub struct Event {
    user_data: Key<()>,
}

impl Event {
    /// Create [`Event`].
    pub fn new() -> io::Result<Self> {
        let user_data = RUNTIME.with(|runtime| runtime.submit_dummy());
        Ok(Self { user_data })
    }

    /// Get a notify handle.
    pub fn handle(&self) -> io::Result<EventHandle> {
        Ok(EventHandle::new(&self.user_data))
    }

    /// Wait for [`EventHandle::notify`] called.
    pub async fn wait(&self) -> io::Result<()> {
        let future = OpFuture::new(self.user_data);
        future.await?;
        Ok(())
    }
}

/// A handle to [`Event`].
pub struct EventHandle {
    handle: RawFd,
    overlapped: Overlapped
}

// Safety: IOCP handle is thread safe.
unsafe impl Send for EventHandle {}
unsafe impl Sync for EventHandle {}

impl EventHandle {
    pub(crate) fn new(user_data: &Key<()>) -> Self {
        let handle = RUNTIME.with(|runtime| runtime.raw_driver());
        let overlapped = Overlapped::new(**user_data);
        Self {
            handle,
            overlapped
        }
    }

    /// Notify the event.
    pub fn notify(&mut self) -> io::Result<()> {
        unsafe {post_driver_raw(self.handle, Ok(0), &mut self.overlapped.base) }
    }
}
