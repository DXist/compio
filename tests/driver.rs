use std::{collections::VecDeque, time::Duration};

use arrayvec::ArrayVec;
use completeio::{
    driver::{AsRawFd, CompleteIo, Driver, Entry, Operation},
    fs::File,
    op::ReadAt,
};

#[test]
fn cancel_before_poll() {
    let mut driver = Driver::new().unwrap();

    let file = File::open("Cargo.toml").unwrap();
    let fd = driver.attach(file.as_raw_fd()).unwrap();

    driver.try_cancel(0).unwrap();

    let mut op = ReadAt::new(fd, 0, Vec::with_capacity(8));
    let mut ops = VecDeque::from([(&mut op, 0).into()]);
    driver.push_queue(&mut ops);
    let mut entries = ArrayVec::<Entry, 1>::new();

    let res =
        unsafe { driver.submit_and_wait_completed(Some(Duration::from_secs(1)), &mut entries) };
    res.unwrap();
}

#[test]
fn timeout() {
    let mut driver = Driver::new().unwrap();

    let mut entries = ArrayVec::<Entry, 1>::new();
    let res =
        unsafe { driver.submit_and_wait_completed(Some(Duration::from_millis(1)), &mut entries) };
    res.unwrap();
}

#[test]
fn attach_read_multiple() {
    const TASK_LEN: usize = 3;

    let mut driver = Driver::new().unwrap();

    let file = File::open("Cargo.toml").unwrap();
    let fd = driver.attach(file.as_raw_fd()).unwrap();

    let mut ops = [
        ReadAt::new(fd, 0, Vec::with_capacity(1024)),
        ReadAt::new(fd, 0, Vec::with_capacity(1024)),
        ReadAt::new(fd, 0, Vec::with_capacity(1024)),
    ];

    for (i, read) in ops.iter_mut().enumerate() {
        let op = Operation::new(read, i);
        driver.try_push(op).unwrap_or_else(|_| panic!("queue is full"));

    }

    let mut entries = ArrayVec::<Entry, TASK_LEN>::new();
    while entries.len() < TASK_LEN {
        unsafe { driver.submit_and_wait_completed(Some(Duration::from_millis(10)), &mut entries) }
            .unwrap();
    }
}

#[test]
fn register_read_multiple() {
    const TASK_LEN: usize = 3;
    const ENTRIES: u32 = 1024;
    const FILES_TO_REGISTER: u32 = 64;

    let mut driver = Driver::with(ENTRIES, FILES_TO_REGISTER).unwrap();

    let file = File::open("Cargo.toml").unwrap();
    let fixed_fd = driver.register_fd(file.as_raw_fd(), 1).unwrap();

    let mut ops = [
        ReadAt::new(fixed_fd, 0, Vec::with_capacity(1024)),
        ReadAt::new(fixed_fd, 0, Vec::with_capacity(1024)),
        ReadAt::new(fixed_fd, 0, Vec::with_capacity(1024)),
    ];

    for (i, read) in ops.iter_mut().enumerate() {
        let op = Operation::new(read, i);
        driver.try_push(op).unwrap_or_else(|_| panic!("queue is full"));

    }

    let mut entries = ArrayVec::<Entry, TASK_LEN>::new();
    while entries.len() < TASK_LEN {
        unsafe { driver.submit_and_wait_completed(Some(Duration::from_millis(10)), &mut entries) }
            .unwrap();
    }
}
