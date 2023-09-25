use std::{
    cell::RefCell,
    collections::VecDeque,
    future::Future,
    io,
    task::{Context, Poll},
    time::Duration,
};

use async_task::{Runnable, Task};

#[cfg(feature = "time")]
use crate::task::time::{TimerFuture, TimerRuntime};
use crate::{
    driver::{AsRawFd, CompleteIo, Driver, OpCode, OpObject, RawFd},
    task::op::{OpFuture, OpRuntime},
    Key,
};

pub(crate) struct Runtime {
    driver: RefCell<Driver<'static>>,
    runnables: RefCell<VecDeque<Runnable>>,
    unqueued_operations: RefCell<VecDeque<OpObject<'static>>>,
    unqueued_cancels: RefCell<VecDeque<usize>>,
    op_runtime: RefCell<OpRuntime>,
    #[cfg(feature = "time")]
    timer_runtime: RefCell<TimerRuntime>,
}

impl Runtime {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            driver: RefCell::new(Driver::new()?),
            runnables: RefCell::default(),
            unqueued_operations: RefCell::default(),
            unqueued_cancels: RefCell::default(),
            op_runtime: RefCell::default(),
            #[cfg(feature = "time")]
            timer_runtime: RefCell::new(TimerRuntime::new()),
        })
    }

    #[allow(dead_code)]
    pub fn raw_driver(&self) -> RawFd {
        self.driver.borrow().as_raw_fd()
    }

    // Safety: the return runnable should be scheduled.
    unsafe fn spawn_unchecked<F: Future>(&self, future: F) -> Task<F::Output> {
        let schedule = move |runnable| self.runnables.borrow_mut().push_back(runnable);
        let (runnable, task) = async_task::spawn_unchecked(future, schedule);
        runnable.schedule();
        task
    }

    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        let mut result = None;
        unsafe { self.spawn_unchecked(async { result = Some(future.await) }) }.detach();
        loop {
            loop {
                let next_task = self.runnables.borrow_mut().pop_front();
                if let Some(task) = next_task {
                    task.run();
                } else {
                    break;
                }
            }
            if let Some(result) = result.take() {
                return result;
            }
            self.poll();
        }
    }

    pub fn spawn<F: Future + 'static>(&self, future: F) -> Task<F::Output> {
        unsafe { self.spawn_unchecked(future) }
    }

    pub fn attach(&self, fd: RawFd) -> io::Result<()> {
        self.driver.borrow_mut().attach(fd)
    }

    pub fn submit<T: OpCode + 'static>(
        &self,
        op: T,
    ) -> impl Future<Output = (io::Result<usize>, T)> {
        let mut op_runtime = self.op_runtime.borrow_mut();
        let (user_data, op_mut) = op_runtime.insert(op);
        let op_object = OpObject::new(op_mut, *user_data);
        if let Err(op_object) = self.driver.borrow_mut().try_push_dyn(op_object) {
            self.unqueued_operations.borrow_mut().push_back(op_object);
        };
        self.spawn(OpFuture::new(user_data))
    }

    #[allow(dead_code)]
    pub fn submit_dummy(&self) -> Key<()> {
        self.op_runtime.borrow_mut().insert_dummy()
    }

    #[cfg(feature = "time")]
    pub fn create_timer(&self, delay: std::time::Duration) -> impl Future<Output = ()> {
        use futures_util::future::Either;

        let mut timer_runtime = self.timer_runtime.borrow_mut();
        if let Some(key) = timer_runtime.insert(delay) {
            Either::Left(TimerFuture::new(key))
        } else {
            Either::Right(std::future::ready(()))
        }
    }

    pub fn cancel_op<T>(&self, user_data: Key<T>) {
        if let Err(_) = self.driver.borrow_mut().try_cancel(*user_data) {
            _ = self.unqueued_cancels.borrow_mut().push_back(*user_data)
        } else {
            self.op_runtime.borrow_mut().cancel(user_data);
        }
    }

    #[cfg(feature = "time")]
    pub fn cancel_timer(&self, key: usize) {
        self.timer_runtime.borrow_mut().cancel(key);
    }

    pub fn poll_task<T: OpCode + 'static>(
        &self,
        cx: &mut Context,
        user_data: Key<T>,
    ) -> Poll<(io::Result<usize>, T)> {
        let mut op_runtime = self.op_runtime.borrow_mut();
        if op_runtime.has_result(user_data) {
            let (maybe_result, maybe_op) = op_runtime.remove(user_data);
            let result = maybe_result.unwrap();
            let operation = maybe_op.expect("`poll_task` is not called on dummy Op");
            Poll::Ready((result, operation))
        } else {
            op_runtime.update_waker(user_data, cx.waker().clone());
            Poll::Pending
        }
    }

    #[allow(dead_code)]
    pub fn poll_dummy(&self, cx: &mut Context, user_data: Key<()>) -> Poll<io::Result<usize>> {
        let mut op_runtime = self.op_runtime.borrow_mut();
        if op_runtime.has_result(user_data) {
            let (maybe_result, _) = op_runtime.remove(user_data);
            Poll::Ready(maybe_result.unwrap())
        } else {
            op_runtime.update_waker(user_data, cx.waker().clone());
            Poll::Pending
        }
    }

    #[cfg(feature = "time")]
    pub fn poll_timer(&self, cx: &mut Context, key: usize) -> Poll<()> {
        let mut timer_runtime = self.timer_runtime.borrow_mut();
        if timer_runtime.contains(key) {
            timer_runtime.update_waker(key, cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    fn poll(&self) {
        let mut unqueued_cancels = self.unqueued_cancels.borrow_mut();
        let mut driver = self.driver.borrow_mut();
        while let Some(user_data) = unqueued_cancels.pop_front() {
            if let Err(_) = driver.try_cancel(user_data) {
                unqueued_cancels.push_front(user_data);
                break;
            }
        }

        let mut unqueued_operations = self.unqueued_operations.borrow_mut();
        driver.push_queue(&mut unqueued_operations);

        let timeout = if unqueued_operations.len() > 0 {
            // busy loop to push outstanding work
            Some(Duration::ZERO)
        } else {
            #[cfg(not(feature = "time"))]
            let timeout = None;
            #[cfg(feature = "time")]
            let timeout = self.timer_runtime.borrow().min_timeout();

            timeout
        };
        let mut runtime_ref = self.op_runtime.borrow_mut();
        let completer = runtime_ref.completer();

        if let Err(e) = unsafe { driver.submit_and_wait_completed(timeout, completer) } {
            if e.kind() == io::ErrorKind::TimedOut {
                println!("Timeout: {}", e);
            } else {
                panic!("{:?}", e);
            }
        }
        #[cfg(feature = "time")]
        self.timer_runtime.borrow_mut().wake();
    }
}
