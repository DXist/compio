use std::{
    future::Future,
    io,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use slab::Slab;

use crate::{
    driver::{Entry, OpCode},
    key::Key,
};

// pub(super) trait OpCode: OpCode + Any + 'static {}

// impl<T: OpCode + 'static> Any for T {}

pub(super) struct RegisteredOp {
    pub op: Option<&'static mut dyn OpCode>,
    pub waker: Option<Waker>,
    pub result: Option<io::Result<usize>>,
    pub cancelled: bool,
}

impl RegisteredOp {
    fn new(op: Option<&'static mut dyn OpCode>) -> Self {
        Self {
            op,
            waker: None,
            result: None,
            cancelled: false,
        }
    }
}

#[derive(Default)]
pub(super) struct OpRuntime {
    ops: Slab<RegisteredOp>,
}

impl OpRuntime {
    pub fn insert<T: OpCode + 'static>(&mut self, op: T) -> (Key<T>, &'static mut dyn OpCode) {
        let op: &'static mut dyn OpCode = Box::leak(Box::new(op));
        let op_ptr = op as *mut dyn OpCode;
        let user_data = self.ops.insert(RegisteredOp::new(Some(op)));
        // SAFETY: we leaked box and remove the allocation only during remove
        unsafe { (Key::new(user_data), &mut *op_ptr) }
    }

    pub fn insert_dummy(&mut self) -> Key<()> {
        Key::new_dummy(self.ops.insert(RegisteredOp::new(None)))
    }

    pub fn update_waker<T>(&mut self, key: Key<T>, waker: Waker) {
        if let Some(op) = self.ops.get_mut(*key) {
            op.waker = Some(waker);
        }
    }

    pub fn update_result<T>(&mut self, key: Key<T>, result: io::Result<usize>) {
        if let Some(op) = self.ops.get_mut(*key) {
            if let Some(waker) = op.waker.take() {
                waker.wake();
            }
            op.result = Some(result);
            if op.cancelled {
                self.remove(key);
            }
        }
    }

    pub fn has_result<T>(&mut self, key: Key<T>) -> bool {
        self.ops
            .get_mut(*key)
            .map(|op| op.result.is_some())
            .unwrap_or_default()
    }

    pub fn cancel<T>(&mut self, key: Key<T>) {
        if let Some(ops) = self.ops.get_mut(*key) {
            ops.cancelled = true;
        }
    }

    pub fn remove<T>(&mut self, key: Key<T>) -> (Option<io::Result<usize>>, Option<T>) {
        let registered_op = self.ops.remove(*key);
        let maybe_op = registered_op.op.map(|op| {
            let mut_ptr = op as *mut dyn OpCode;
            let ptr = mut_ptr.cast::<T>();
            // moves from the previous data allocation and frees it
            let operation: T = *unsafe { Box::from_raw(ptr) };
            operation
        });
        (registered_op.result, maybe_op)
    }

    pub fn completer(&mut self) -> &mut Self {
        self
    }
}

impl Extend<Entry> for OpRuntime {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = Entry>,
    {
        for entry in iter.into_iter() {
            self.update_result(Key::new_dummy(entry.user_data()), entry.into_result());
        }
    }
}

#[derive(Debug)]
pub struct OpFuture<T: 'static> {
    user_data: Key<T>,
    completed: bool,
    _p: PhantomData<&'static T>,
}

impl<T> OpFuture<T> {
    pub fn new(user_data: Key<T>) -> Self {
        Self {
            user_data,
            completed: false,
            _p: PhantomData,
        }
    }
}

impl<T: OpCode + 'static> Future for OpFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = crate::task::RUNTIME.with(|runtime| runtime.poll_task(cx, self.user_data));
        if res.is_ready() {
            self.get_mut().completed = true;
        }
        res
    }
}

impl Future for OpFuture<()> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = crate::task::RUNTIME.with(|runtime| runtime.poll_dummy(cx, self.user_data));
        if res.is_ready() {
            self.get_mut().completed = true;
        }
        res
    }
}

impl<T> Drop for OpFuture<T> {
    fn drop(&mut self) {
        if !self.completed {
            crate::task::RUNTIME.with(|runtime| runtime.cancel_op(self.user_data))
        }
    }
}
