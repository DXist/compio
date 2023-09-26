//! The runtime of completeio.
//! We don't expose the runtime struct because there could be only one runtime
//! in each thread.
//!
//! ```
//! let ans = completeio::task::block_on(async {
//!     println!("Hello world!");
//!     42
//! });
//! assert_eq!(ans, 42);
//! ```

use std::future::Future;

use async_task::Task;

pub(crate) mod runtime;
use runtime::Runtime;

pub(crate) mod op;

thread_local! {
    pub(crate) static RUNTIME: Runtime = Runtime::new().expect("cannot create completeio runtime");
}

/// Start a completeio runtime and block on the future till it completes.
///
/// ```
/// completeio::task::block_on(async {
///     // Open a file
///     let file = completeio::fs::File::open("Cargo.toml").unwrap();
///
///     let buf = Vec::with_capacity(4096);
///     // Read some data, the buffer is passed by ownership and
///     // submitted to the kernel. When the operation completes,
///     // we get the buffer back.
///     let (res, buf) = file.read_at(buf, 0).await;
///     let n = res.unwrap();
///     assert_eq!(n, buf.len());
///
///     // Display the contents
///     println!("{:?}", &buf);
/// })
/// ```
pub fn block_on<F: Future>(future: F) -> F::Output {
    RUNTIME.with(|runtime| runtime.block_on(future))
}

/// Spawns a new asynchronous task, returning a [`Task`] for it.
///
/// Spawning a task enables the task to execute concurrently to other tasks.
/// There is no guarantee that a spawned task will execute to completion.
///
/// ```
/// completeio::task::block_on(async {
///     let task = completeio::task::spawn(async {
///         println!("Hello from a spawned task!");
///         42
///     });
///
///     assert_eq!(task.await, 42);
/// })
/// ```
pub fn spawn<F: Future + 'static>(future: F) -> Task<F::Output> {
    RUNTIME.with(|runtime| runtime.spawn(future))
}
