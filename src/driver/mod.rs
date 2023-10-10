//! The platform-specified driver.
//! Some types differ by compilation target.

#[cfg(feature = "allocator_api")]
use std::alloc::Allocator;
use std::{io, time::Duration};

use crate::vec_deque_alloc;

#[cfg(unix)]
mod unix;

cfg_if::cfg_if! {
    if #[cfg(target_os = "windows")] {
        mod iocp;
        #[cfg(feature="time")]
        mod time;
        pub use iocp::*;
    } else if #[cfg(target_os = "linux")] {
        mod iour;
        pub use iour::*;
    } else if #[cfg(any(target_vendor= "apple", target_os = "freebsd", target_os = "dragonfly", target_os = "openbsd", target_os = "netbsd"))] {
        mod kqueue;
        #[cfg(feature="time")]
        mod time;
        pub use self::kqueue::*;
    } else {
        compile_error!("unsupported build target");
    }

}

/// An abstract of [`Driver`].
/// It contains some low-level actions of completion-based IO.
///
/// You don't need them unless you are controlling a [`Driver`] yourself.
///
/// The driver could hold references into IO buffers. Their lifetime is 'arena.
///
/// # Examples
///
/// ```
/// use std::{collections::VecDeque, net::SocketAddr};
///
/// use arrayvec::ArrayVec;
/// use completeio::{
///     buf::IntoInner,
///     driver::{AsRawFd, CompleteIo, Driver, Entry},
///     net::UdpSocket,
///     op,
/// };
///
/// let first_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
/// let second_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
///
/// // bind sockets
/// let socket = UdpSocket::bind(first_addr).unwrap();
/// let first_addr = socket.local_addr().unwrap();
/// let other_socket = UdpSocket::bind(second_addr).unwrap();
/// let second_addr = other_socket.local_addr().unwrap();
///
/// // connect sockets
/// socket.connect(second_addr).unwrap();
/// other_socket.connect(first_addr).unwrap();
///
/// let mut driver = Driver::new().unwrap();
/// let fd = driver.attach(socket.as_raw_fd()).unwrap();
/// let other_fd = driver.attach(other_socket.as_raw_fd()).unwrap();
///
/// // write data
/// let mut op_send = op::Send::new(fd, "hello world");
///
/// // read data
/// let buf = Vec::with_capacity(32);
/// let mut op_recv = op::Recv::new(other_fd, buf);
///
/// let mut ops = VecDeque::from([(&mut op_send, 1).into(), (&mut op_recv, 2).into()]);
/// driver.push_queue(&mut ops);
/// let mut entries = ArrayVec::<Entry, 2>::new();
/// unsafe { driver.submit(None, &mut entries).unwrap() };
/// while entries.len() < 2 {
///     unsafe { driver.submit(None, &mut entries).unwrap() };
/// }
///
/// let mut n_bytes = 0;
/// for entry in entries {
///     match entry.user_data() {
///         1 => {
///             entry.into_result().unwrap();
///         }
///         2 => {
///             n_bytes = entry.into_result().unwrap();
///         }
///         _ => unreachable!(),
///     }
/// }
///
/// let mut buf = op_recv.into_inner();
/// unsafe { buf.set_len(n_bytes) };
/// assert_eq!(buf, b"hello world");
/// ```
pub trait CompleteIo<'arena> {
    /// Attach an fd to the driver.
    ///
    /// ## Platform specific
    /// * IOCP: it will be attached to the completion port. An fd could only be attached to one
    ///   driver, and could only be attached once, even if you `try_clone` it. It will cause
    ///   unexpected result to attach the handle with one driver and push an op to another driver.
    /// * io-uring/kqueue: it will do nothing and return `Ok(Fd)`
    ///
    /// To close fd issue `Close` operation using Fd as OpCode value.
    fn attach(&mut self, fd: RawFd) -> io::Result<Fd>;

    /// Attach fd to the driver and register it as fixed file descriptor with the provided fixed id.
    ///
    /// ## Platform specific
    /// * IOCP: it will be attached to the completion port. Thus the same fd cannot be registered
    ///   twice.
    /// * io-uring: it will be registered either
    ///     * in async way during `submit` call when submission queue is not full and no other async
    ///       registration is in progress
    ///     * or in sync style using a syscall in other cases
    /// Async operation uses reserved `u64::MAX` user_data key. Driver handles completion.
    /// Provided fd will override previously registered fd.
    /// * kqueue: it will do nothing.
    ///
    /// To unregister fd `unregister_fd` method.
    fn register_fd(&mut self, fd: RawFd, id: u32) -> io::Result<FixedFd>;

    /// Unregister fixed fd.
    ///
    /// io_uring: will unregister the provided fixed fd on kernel side
    /// IOCP/kqueue: will do nothing
    fn unregister_fd(&mut self, fixed_fd: FixedFd) -> io::Result<()>;

    /// Try to cancel an operation with the pushed user-defined data.
    ///
    /// If submission queue is full the error is returned. The caller should
    /// queue the cancelation request or submit queued entries first.
    ///
    /// If the cancellation is not possible the operation will run till
    /// completed.
    ///
    /// When an operation is cancelled or completed successfully
    /// `submit` will output it in `completed` iterator.
    fn try_cancel(&mut self, user_data: usize) -> Result<(), ()>;

    /// Try to push operation into submission queue
    ///
    /// If the queue is full the submitted operation is returned as an error.
    /// Caller could use an external queue like VecDeque<OpObject<'a>> to keep
    /// unqueued operations.
    fn try_push<O: OpCode>(&mut self, op: Operation<'arena, O>)
        -> Result<(), Operation<'arena, O>>;

    /// Try to push operation object into submission queue
    fn try_push_dyn(&mut self, op: OpObject<'arena>) -> Result<(), OpObject<'arena>>;

    /// Push multiple operations into submission queue from an external VecDeque
    ///
    /// After push the external queue could contain operations that didn't fit
    /// into the submission queue
    fn push_queue<#[cfg(feature = "allocator_api")] A: Allocator + Unpin + 'arena>(
        &mut self,
        ops_queue: &mut vec_deque_alloc!(OpObject<'arena>, A),
    );

    /// Returns submission queue capacity left for pushing.
    fn capacity_left(&self) -> usize;

    /// Submit queued operations and wait for completed entries with an optional
    /// timeout.
    ///
    /// If there are no operations completed and `timeout` > 0  this call will
    /// block and wait. If no timeout specified, it will block forever.
    /// If timeout is `Duration::ZERO` no waiting is performed.
    ///
    /// To interrupt the blocking, see [`Event`].
    ///
    /// [`Event`]: crate::event::Event
    ///
    /// # Safety
    ///
    /// * Operations should be alive until [`CompleteIo::poll`] returns its result.
    /// * User defined data should be unique.
    unsafe fn submit(
        &mut self,
        timeout: Option<Duration>,
        completed: &mut impl Extend<Entry>,
    ) -> io::Result<()>;
}

/// An operation with a unique user defined data.
pub struct Operation<'a, O: OpCode> {
    op: &'a mut O,
    user_data: usize,
}

impl<'a, O: OpCode> Operation<'a, O> {
    /// Create [`Operation`].
    pub fn new(op: &'a mut O, user_data: usize) -> Self {
        Self { op, user_data }
    }

    /// Get the opcode.
    pub fn opcode(&mut self) -> &mut O {
        self.op
    }

    /// Get the user defined data.
    pub fn user_data(&self) -> usize {
        self.user_data
    }
}

impl<'a, O: OpCode> From<(&'a mut O, usize)> for Operation<'a, O> {
    fn from((op, user_data): (&'a mut O, usize)) -> Self {
        Self::new(op, user_data)
    }
}

impl<'a, O: OpCode> From<Operation<'a, O>> for (&'a mut O, usize) {
    fn from(other: Operation<'a, O>) -> Self {
        (other.op, other.user_data)
    }
}
/// An operation object with a unique user defined data.
pub struct OpObject<'a> {
    op: &'a mut dyn OpCode,
    user_data: usize,
}

impl<'a> OpObject<'a> {
    /// Create [`Operation`].
    pub fn new(op: &'a mut dyn OpCode, user_data: usize) -> Self {
        Self { op, user_data }
    }

    /// Get the mut opcode.
    pub fn opcode(&mut self) -> &mut dyn OpCode {
        self.op
    }

    /// Get the opcode ref.
    pub fn opcode_ref(&self) -> &dyn OpCode {
        self.op
    }

    /// Get the user defined data.
    pub fn user_data(&self) -> usize {
        self.user_data
    }
}

impl<'a, O: OpCode> From<(&'a mut O, usize)> for OpObject<'a> {
    fn from((op, user_data): (&'a mut O, usize)) -> Self {
        Self::new(op, user_data)
    }
}

impl<'a> From<(&'a mut dyn OpCode, usize)> for OpObject<'a> {
    fn from((op, user_data): (&'a mut dyn OpCode, usize)) -> Self {
        Self::new(op, user_data)
    }
}

impl<'a, O: OpCode> From<Operation<'a, O>> for OpObject<'a> {
    fn from(other: Operation<'a, O>) -> Self {
        Self::new(other.op, other.user_data)
    }
}

impl<'a> From<OpObject<'a>> for (&'a mut dyn OpCode, usize) {
    fn from(other: OpObject<'a>) -> Self {
        (other.op, other.user_data)
    }
}

/// An completed entry returned from kernel.
#[derive(Debug)]
pub struct Entry {
    user_data: usize,
    result: io::Result<usize>,
}

impl Entry {
    pub(crate) fn new(user_data: usize, result: io::Result<usize>) -> Self {
        Self { user_data, result }
    }

    /// The user-defined data passed to [`Operation`].
    pub fn user_data(&self) -> usize {
        self.user_data
    }

    /// The result of the operation.
    pub fn into_result(self) -> io::Result<usize> {
        self.result
    }
}
