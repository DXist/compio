use std::{io, time::Duration};
use std::collections::VecDeque;

use arrayvec::ArrayVec;
use compio::{
    driver::{AsRawFd, Driver, Entry, CompleteIo},
    fs::File,
    op::ReadAt,
};

#[test]
fn cancel_before_poll() {
    let mut driver = Driver::new().unwrap();

    let file = File::open("Cargo.toml").unwrap();
    driver.attach(file.as_raw_fd()).unwrap();

    driver.try_cancel(0).unwrap();

    let mut op = ReadAt::new(file.as_raw_fd(), 0, Vec::with_capacity(8));
    let mut ops = VecDeque::from([(&mut op, 0).into()]);
    driver.push_queue(&mut ops);
    let mut entries = ArrayVec::<Entry, 1>::new();

    let res = unsafe {
        driver.submit_and_wait_completed(
            Some(Duration::from_secs(1)),
            &mut entries,
        )
    };
    assert!(res.is_ok() || res.unwrap_err().kind() == io::ErrorKind::TimedOut);
}
