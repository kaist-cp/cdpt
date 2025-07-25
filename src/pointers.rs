//! Basic managed pointer types.

use crate::{
    epoch::Color,
    guards::{Guard, Handle},
    internal::HazardPointer,
};
use std::{
    marker::PhantomData,
    sync::atomic::{AtomicPtr, AtomicUsize},
};

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
        (self.0 & (1 << (usize::BITS - 1))).into()
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
    ptr: *mut (),
    _marker: PhantomData<*mut ManObj<T>>,
}

impl<T> ManPtr<T> {
    const META_WIDTH: u32 = 2;
    const META_BITS: usize = ((1 << Self::META_WIDTH) - 1) << (usize::BITS - Self::META_WIDTH);

    pub fn meta(self) -> PtrMeta {
        let bits = self.ptr.addr();
        if bits & (1 << (usize::BITS - 1)) > 0 {
            PtrMeta::Rooted
        } else {
            PtrMeta::Unrooted(Color::from(bits & (1 << (usize::BITS - 2))))
        }
    }

    pub fn as_ptr(self) -> *mut T {
        let mo_ptr = decompose_tag::<ManObj<T>>(self.ptr).0.cast::<ManObj<T>>();
        ((unsafe { &(*mo_ptr).item }) as *const T).cast_mut()
    }

    pub fn tag(self) -> usize {
        decompose_tag::<ManObj<T>>(self.ptr).1
    }

    pub fn with_tag(self, tag: usize) -> Self {
        Self {
            ptr: compose_tag::<ManObj<T>>(self.as_ptr().cast(), tag),
            _marker: PhantomData,
        }
    }

    pub fn is_null(self) -> bool {
        self.as_ptr().is_null()
    }

    pub unsafe fn deref<'l>(self) -> &'l T {
        unsafe { &*self.as_ptr() }
    }
}

pub trait TracePtr {}

pub trait TraceObj {
    fn scan_outgoings(&self);
}

pub struct EdgeScanner {}

/// A root-count-protected atomic reference to the managed object.
/// It can be sent and atomic with other threads. Before dereferencing,
/// you must create a `Local` reference to the same object by calling `load`.
///
/// It is interiorly-mutable, meaning that you can atomically update
/// the underlying reference.
pub struct AtomicShared<T: Send + Sync> {
    link: AtomicPtr<()>,
    _marker: PhantomData<*mut ManObj<T>>,
}

unsafe impl<T: Send + Sync> Sync for AtomicShared<T> {}
unsafe impl<T: Send + Sync> Send for AtomicShared<T> {}

impl<T: Send + Sync> AtomicShared<T> {
    pub fn new(item: T) -> Self {
        todo!()
    }

    pub fn null() -> Self {
        todo!()
    }

    pub fn load<'g>(&self, guard: &'g Guard) -> Local<'g, Guard, T> {
        todo!()
    }
}

/// A root-count-protected reference to the managed object.
/// It can be sent and atomic with other threads.
///
/// It is immutable, meaning that you cannot update the underlying reference.
pub struct Shared<T: Send + Sync> {
    // Note: We just use `AtomicShared` to implement `Shared`, even though `Shared` is immutable
    // from the user's perspective. The reason is that `Shared` is also a target of
    // tracing and marking by collectors, so we must still use atomically mutable link.
    //
    // Therefore, we should still use atomic operations to access this pointer.
    // But some optimizations using relaxed operations might be possible because
    // the user is the only one who mutates the address.
    inner: AtomicShared<T>,
}

pub(crate) trait Protector {
    type Shield;

    fn protect(&self, ptr: *mut ()) -> Self::Shield;
}

impl Protector for Handle {
    type Shield = HazardPointer;

    fn protect(&self, ptr: *mut ()) -> Self::Shield {
        let hp = HazardPointer::new(unsafe { self.local.as_ref() });
        hp.protect_addr(ptr);
        hp
    }
}

impl Protector for Guard {
    type Shield = ();

    fn protect(&self, _: *mut ()) -> Self::Shield {
        ()
    }
}

/// A thread-local reference to the managed object, protected by either a hazard pointer,
/// To dereference, you must call `borrow` which creates an immuatable reference.
pub struct Local<'g, G: Protector, T: Send + Sync> {
    ptr: *mut (),
    sh: G::Shield,
    _marker: PhantomData<(&'g (), *mut ManObj<T>)>,
}

impl<'g, G: Protector, T: Send + Sync> Clone for Local<'g, G, T>
where
    G::Shield: Clone,
{
    fn clone(&self) -> Self {
        Self {
            ptr: self.ptr,
            sh: self.sh.clone(),
            _marker: PhantomData,
        }
    }
}

impl<'g, G: Protector, T: Send + Sync> Local<'g, G, T> {
    pub fn protect_by<'h, H: Protector>(&self, prot: &'h H) -> Local<'h, H, T> {
        Local {
            ptr: self.ptr,
            sh: prot.protect(self.ptr),
            _marker: PhantomData,
        }
    }
}

impl<'g, G: Protector, T: Send + Sync> Copy for Local<'g, G, T> where G::Shield: Copy {}

/// Returns a bitmask containing the unused least significant bits of an aligned pointer to `T`.
#[inline]
fn low_bits<T>() -> usize {
    (1 << align_of::<T>().trailing_zeros()) - 1
}

/// Panics if the pointer is not properly unaligned.
#[inline]
fn ensure_aligned<T>(raw: *mut ()) {
    assert_eq!(raw as usize & low_bits::<T>(), 0, "unaligned pointer");
}

/// Given a tagged pointer `data`, returns the same pointer, but tagged with `tag`.
///
/// `tag` is truncated to fit into the unused bits of the pointer to `T`.
#[inline]
fn compose_tag<T>(ptr: *mut (), tag: usize) -> *mut () {
    ptr.map_addr(|a| (a & !low_bits::<T>()) | (tag & low_bits::<T>()))
}

/// Decomposes a tagged pointer `data` into the pointer and the tag.
#[inline]
fn decompose_tag<T>(ptr: *mut ()) -> (*mut (), usize) {
    (
        ptr.map_addr(|a| a & !low_bits::<T>()),
        ptr as usize & low_bits::<T>(),
    )
}
