# CompleteIo

[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/DXist/completeio/blob/master/LICENSE)
[![crates.io](https://img.shields.io/crates/v/completeio)](https://crates.io/crates/completeio)
[![docs.rs](https://img.shields.io/badge/docs.rs-completeio-latest)](https://docs.rs/completeio)

A thread-per-core Rust IO drivers and async runtime backed by IOCP/io_uring/kqueue.
The name comes from "completion-based IO".

This repository is a fork of [compio](https://github.com/compio-rs/compio).

## Why not Compio?

The project has different goals:

* provide low overhead IO drivers that preallocate memory on initialization
* drivers API accepts non-'static IO buffers

    * drivers don't own buffers
    * buffers are required to be Unpin

* bias towards `io_uring` API to achieve zero-cost abstraction on Linux:

    * fixed size submission queue for operations
    * external runtime could submit an external queue of not yet queued operations as a single batch
    * timers are exposed as `Timeout` operation and use suspend-aware CLOCK_BOOTTIME clock source when it's available
    * file descriptors could be registered to remove refcounting overhead in `io_uring`

* Async runtime is an example runtime to test implementation of drivers

## Quick start

With `runtime` feature enabled, we can use the high level APIs to perform fs & net IO.

```rust,no_run
use completeio::{fs::File, task::block_on};

let buffer = block_on(async {
    let file = File::open("Cargo.toml").unwrap();
    let (read, buffer) = file.read_to_end_at(Vec::with_capacity(1024), 0).await;
    let read = read.unwrap();
    assert_eq!(read, buffer.len());
    String::from_utf8(buffer).unwrap()
});
println!("{}", buffer);
```

While you can also control the low-level driver manually:

```rust,no_run
use arrayvec::ArrayVec;
use std::collections::VecDeque;
use completeio::{
    buf::IntoInner,
    driver::{AsRawFd, Driver, Entry, CompleteIo},
    fs::File,
    op::ReadAt,
};

let mut driver = Driver::new().unwrap();
let file = File::open("Cargo.toml").unwrap();
// Attach the `RawFd` to driver first.
//
// The result is `Fd` attached to the driver.
// It's possible to register file descriptor instead of attaching.
let fd = driver.attach(file.as_raw_fd()).unwrap();

// Create IO operation and push it to the driver's submission queue.
let mut op = ReadAt::new(fd, 0, Vec::with_capacity(4096));
// We don't pass operation ownership to the driver and have to keep operations
// until they will be finished
let mut ops = VecDeque::from([(&mut op, 0).into()]);
driver.push_queue(&mut ops);

// Submit the queued operations and wait for IO completed.
let mut entries = ArrayVec::<Entry, 1>::new();
unsafe {
    driver
        .submit(None, &mut entries)
        .unwrap();
}
let entry = entries.drain(..).next().unwrap();
assert_eq!(entry.user_data(), 0);

// Resize the buffer by return value.
let n = entry.into_result().unwrap();
let mut buffer = op.into_inner();
unsafe {
    buffer.set_len(n);
}

println!("{}", String::from_utf8(buffer).unwrap());
```
