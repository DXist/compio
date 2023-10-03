//! This mod doesn't actually contain any driver, but meant to provide some
//! common op type and utilities for unix platform (for iour and kqueue).

pub(crate) mod op;

// Sealed trait - Unix mod is private
pub trait IntoFdOrFixed: Sized {
    type Target;

    fn into(self) -> Self::Target;
}
