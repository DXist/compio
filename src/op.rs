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
    Accept, Connect, Read, ReadAt, Recv, RecvFrom, RecvMsgImpl, RecvVectoredImpl, Send, SendTo, SendVectoredImpl, SendMsgImpl, Sync,
    Write, WriteAt,
};
use crate::{
    buf::{AsIoSlicesMut, BufWrapperMut, IoBufMut, VectoredBufWrapper},
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
    for BufResult<'arena, usize, (T, SockAddr)>
{
    type RecvFromResult = BufResult<'arena, (usize, SockAddr), T>;

    fn map_addr(self) -> Self::RecvFromResult {
        let (res, (buffer, addr)) = self;
        (res.map(|received| (received, addr)), buffer)
    }
}

/// Receive a single piece of data with vectored buffer.
pub type RecvVectored<'arena, T> = RecvVectoredImpl<'arena, VectoredBufWrapper<'arena, T>>;
/// Send a single piece of data with vectored buffer.
pub type SendVectored<'arena, T> = SendVectoredImpl<'arena, VectoredBufWrapper<'arena, T>>;

/// Receive a single piece of data and address with vectored buffer.
pub type RecvFromVectored<'arena, T> = RecvMsgImpl<'arena, VectoredBufWrapper<'arena, T>>;
/// Send a single piece of data to address with vectored buffer.
pub type SendToVectored<'arena, T> = SendMsgImpl<'arena, VectoredBufWrapper<'arena, T>>;
