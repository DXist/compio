//! The async operations.
//! Types in this mod represents the low-level operations passed to kernel.
//! The operation itself doesn't perform anything.
//! You need to pass them to [`crate::driver::Driver`], and poll the driver.

use socket2::SockAddr;

#[cfg(target_os = "windows")]
pub use crate::driver::op::ConnectNamedPipe;
#[cfg(feature = "time")]
pub use crate::driver::op::Timeout;
pub use crate::driver::op::{
    Accept, Connect, ReadAt, RecvMsgImpl, RecvImpl, SendImpl, SendToImpl, Sync, WriteAt,
};
use crate::{
    buf::{AsIoSlicesMut, BufWrapperMut, IoBufMut, VectoredBufWrapper},
    driver::{sockaddr_storage, socklen_t},
    BufResult,
};

/// Helper trait to update buffer length after kernel updated the buffer
pub trait UpdateBufferLen {
    /// Update length of wrapped buffer
    fn update_buffer_len(self) -> Self;
}

macro_rules! impl_update_buffer_len {
    ($t:ident) => {
        impl<'arena, T: IoBufMut<'arena>> UpdateBufferLen
            for BufResult<'arena, usize, $t<'arena, T>>
        {
            fn update_buffer_len(self) -> Self {
                let (res, mut buffer) = self;
                if let Ok(init) = &res {
                    buffer.set_init(*init);
                }
                (res, buffer)
            }
        }

        impl<'arena, T: IoBufMut<'arena>, O> UpdateBufferLen
            for BufResult<'arena, (usize, O), $t<'arena, T>>
        {
            fn update_buffer_len(self) -> Self {
                let (res, mut buffer) = self;
                if let Ok((init, _)) = &res {
                    buffer.set_init(*init);
                }
                (res, buffer)
            }
        }
    };
}

impl_update_buffer_len!(VectoredBufWrapper);
impl_update_buffer_len!(BufWrapperMut);

impl<'arena, T: IoBufMut<'arena>> UpdateBufferLen for BufResult<'arena, usize, T> {
    fn update_buffer_len(self) -> Self {
        let (res, mut buffer) = self;
        if let Ok(init) = &res {
            buffer.set_buf_init(*init);
        }
        (res, buffer)
    }
}

impl<'arena, T: IoBufMut<'arena>, O> UpdateBufferLen for BufResult<'arena, (usize, O), T> {
    fn update_buffer_len(self) -> Self {
        let (res, mut buffer) = self;
        if let Ok((init, _)) = &res {
            buffer.set_buf_init(*init);
        }
        (res, buffer)
    }
}

pub(crate) trait RecvResultExt {
    type RecvFromResult;

    fn map_addr(self) -> Self::RecvFromResult;
}

impl<'arena, T: 'arena> RecvResultExt
    for BufResult<'arena, usize, (T, sockaddr_storage, socklen_t)>
{
    type RecvFromResult = BufResult<'arena, (usize, SockAddr), T>;

    fn map_addr(self) -> Self::RecvFromResult {
        let (res, (buffer, addr_buffer, addr_size)) = self;
        let res = res.map(|res| {
            let addr = unsafe { SockAddr::new(addr_buffer, addr_size) };
            (res, addr)
        });
        (res, buffer)
    }
}

/// Receive data with one buffer.
pub type Recv<'arena, T> = RecvImpl<'arena, T>;
/// Receive data with vectored buffer.
pub type RecvVectored<'arena, T> = RecvImpl<'arena, VectoredBufWrapper<'arena, T>>;

/// Send data with one buffer.
pub type Send<'arena, T> = SendImpl<'arena, T>;
/// Send data with vectored buffer.
pub type SendVectored<'arena, T> = SendImpl<'arena, VectoredBufWrapper<'arena, T>>;

/// Receive data and address with one buffer.
pub type RecvFrom<'arena, T> = RecvMsgImpl<'arena, T>;
/// Receive data and address with vectored buffer.
pub type RecvFromVectored<'arena, T> = RecvMsgImpl<'arena, VectoredBufWrapper<'arena, T>>;

/// Send data to address with one buffer.
pub type SendTo<'arena, T> = SendToImpl<'arena, T>;
/// Send data to address with vectored buffer.
pub type SendToVectored<'arena, T> = SendToImpl<'arena, VectoredBufWrapper<'arena, T>>;
