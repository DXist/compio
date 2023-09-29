#[cfg(feature = "once_cell_try")]
use std::sync::OnceLock;
use std::io;

#[cfg(not(feature = "once_cell_try"))]
use once_cell::sync::OnceCell as OnceLock;

use crate::{driver::{AsRawFd, Fd}, task::RUNTIME};

/// Attach a handle to the driver of current thread.
///
/// A handle can and only can attach once to one driver. However, the handle
/// itself is Send & Sync. We mark it !Send & !Sync to warn users, making them
/// ensure that they are using it in the correct thread.
#[derive(Debug, Clone)]
pub struct Attacher {
    // Make it thread safe and !Send & !Sync.
    once: OnceLock<Fd>,
}

impl Attacher {
    pub const fn new() -> Self {
        Self {
            once: OnceLock::new(),
        }
    }

    pub fn attach(&self, source: &impl AsRawFd) -> io::Result<Fd> {
        self.once
            .get_or_try_init(|| RUNTIME.with(|runtime| runtime.attach(source.as_raw_fd()))).map(|r| *r)
    }
}
