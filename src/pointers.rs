//! Basic managed pointer types.

use crate::epoch::Color;
use std::{marker::PhantomData, sync::atomic::AtomicUsize};

#[derive(Clone, Copy)]
pub(crate) struct ObjMeta(usize);

impl Default for ObjMeta {
    fn default() -> Self {
        Self(0)
    }
}

impl From<usize> for ObjMeta {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

impl ObjMeta {
    pub fn marked(self) -> Color {
        if self.0 & (1 << (usize::BITS - 1)) > 0 {
            Color::C1
        } else {
            Color::C0
        }
    }

    pub fn root_count(self) -> usize {
        self.0 & ((1 << (usize::BITS - 1)) - 1)
    }
}

pub(crate) struct AtomicObjMeta(AtomicUsize);

impl Default for AtomicObjMeta {
    fn default() -> Self {
        Self(AtomicUsize::new(0))
    }
}

impl AtomicObjMeta {}

pub(crate) struct ManObj<T> {
    header: AtomicObjMeta,
    item: T,
}

impl<T> ManObj<T> {}

#[derive(Clone, Copy)]
pub(crate) enum PtrMeta {
    Rooted,
    Unrooted(Color),
}

#[derive(Clone, Copy)]
pub(crate) struct ManPtr<T> {
    bits: usize,
    _marker: PhantomData<*mut ManObj<T>>,
}

impl<T> ManPtr<T> {
    const META_WIDTH: u32 = 2;
    const META_BITS: usize = ((1 << Self::META_WIDTH) - 1) << (usize::BITS - Self::META_WIDTH);

    pub fn meta(self) -> PtrMeta {
        if self.bits & (1 << (usize::BITS - 1)) > 0 {
            PtrMeta::Rooted
        } else {
            PtrMeta::Unrooted(Color::from(self.bits & (1 << (usize::BITS - 2))))
        }
    }

    pub fn addr(self) -> *mut ManObj<T> {
        (self.bits & (!Self::META_BITS)) as _
    }

    pub unsafe fn deref<'l>(self) -> &'l T {
        unsafe { &(*self.addr()).item }
    }
}
