//! Basic managed pointer types.

use crate::{
    epoch::{Color, Phase},
    guards::{Guard, Handle},
    internal::HazardPointer,
    tls::pin,
};
use std::{
    hint::unlikely,
    marker::PhantomData,
    mem::forget,
    ptr::null_mut,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
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
    const ROOT_COUNT_BITS: usize = ((1 << (usize::BITS - 1)) - 1);

    pub fn new(marked: Color, root_count: usize) -> Self {
        debug_assert!(root_count < (1 << (usize::BITS - 1)));
        let bits = ((marked as usize) << (1 << (usize::BITS - 1))) | root_count;
        Self(bits)
    }

    pub fn marked(self) -> Color {
        (self.0 & !Self::ROOT_COUNT_BITS).into()
    }

    pub fn root_count(self) -> usize {
        self.0 & Self::ROOT_COUNT_BITS
    }
}

pub(crate) struct AtomicObjMeta(AtomicUsize);

impl Default for AtomicObjMeta {
    fn default() -> Self {
        Self::from(ObjMeta::default())
    }
}

impl AtomicObjMeta {
    pub fn new(marked: Color, root_count: usize) -> Self {
        Self::from(ObjMeta::new(marked, root_count))
    }

    pub fn load(&self, order: Ordering) -> ObjMeta {
        ObjMeta::from(self.0.load(order))
    }

    pub fn increment_root_count(&self, order: Ordering) -> usize {
        let prev = ObjMeta::from(self.0.fetch_add(1, order)).root_count();
        debug_assert!(prev < ObjMeta::ROOT_COUNT_BITS);
        prev
    }

    pub fn decrement_root_count(&self, order: Ordering) -> usize {
        let prev = ObjMeta::from(self.0.fetch_sub(1, order)).root_count();
        debug_assert!(prev > 0);
        prev
    }
}

impl From<ObjMeta> for AtomicObjMeta {
    fn from(value: ObjMeta) -> Self {
        Self(AtomicUsize::new(value.0))
    }
}

pub(crate) struct ManObj<T: TraceObj> {
    header: AtomicObjMeta,
    item: T,
}

impl<T: TraceObj> ManObj<T> {
    pub fn new(item: T, color: Color, root_count: usize) -> Self {
        Self {
            header: AtomicObjMeta::new(color, root_count),
            item,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PtrMeta {
    Rooted,
    Unrooted(Color),
}

pub(crate) struct ManPtr<T: TraceObj> {
    // Note: Intentionally not used `*mut ManObj<T>` here, to prevent
    // mistakenly dereferencing this pointer. It must be untagged properly
    // before dereferencing (e.g., `as_ptr`).
    data: *mut (),
    _marker: PhantomData<*mut ManObj<T>>,
}

impl<T: TraceObj> Clone for ManPtr<T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data,
            _marker: PhantomData,
        }
    }
}

impl<T: TraceObj> Copy for ManPtr<T> {}

impl<T: TraceObj> PartialEq for ManPtr<T> {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl<T: TraceObj> Eq for ManPtr<T> {}

impl<T: TraceObj> From<*mut ()> for ManPtr<T> {
    fn from(value: *mut ()) -> Self {
        Self {
            data: value,
            _marker: PhantomData,
        }
    }
}

impl<T: TraceObj> ManPtr<T> {
    const META_WIDTH: u32 = 2;
    const META_BITS: usize = ((1 << Self::META_WIDTH) - 1) << (usize::BITS - Self::META_WIDTH);
    const LOW_BITS: usize = (1 << align_of::<T>().trailing_zeros()) - 1;
    const ADDR_BITS: usize = usize::MAX & !Self::META_BITS & !Self::LOW_BITS;

    pub fn alloc_rooted(item: T, color: Color, root_count: usize) -> Self {
        debug_assert!(root_count > 0);
        let obj = ManObj::new(item, color, root_count);
        let addr = Box::into_raw(Box::new(obj));
        let ptr = Self {
            data: addr.cast(),
            _marker: PhantomData,
        };
        ptr.with_meta(PtrMeta::Rooted)
    }

    pub fn alloc_unrooted(item: T, color: Color) -> Self {
        let obj = ManObj::new(item, color, 0);
        let addr = Box::into_raw(Box::new(obj));
        let ptr = Self {
            data: addr.cast(),
            _marker: PhantomData,
        };
        ptr.with_meta(PtrMeta::Unrooted(color))
    }

    pub fn null_base() -> Self {
        Self {
            data: null_mut(),
            _marker: PhantomData,
        }
    }

    pub fn null_rooted() -> Self {
        Self::null_base().with_meta(PtrMeta::Rooted)
    }

    pub fn meta(self) -> PtrMeta {
        let bits = self.data.addr();
        if bits & (1 << (usize::BITS - 1)) > 0 {
            PtrMeta::Rooted
        } else {
            PtrMeta::Unrooted(Color::from(bits & (1 << (usize::BITS - 2))))
        }
    }

    pub fn with_meta(self, meta: PtrMeta) -> Self {
        let new_ptr = self.data.map_addr(|addr| {
            let wo_meta = addr & !Self::META_BITS;
            let meta = match meta {
                PtrMeta::Rooted => 0b10,
                PtrMeta::Unrooted(Color::C0) => 0b00,
                PtrMeta::Unrooted(Color::C1) => 0b01,
            };
            (meta << (usize::BITS - Self::META_WIDTH)) | wo_meta
        });
        Self {
            data: new_ptr,
            _marker: PhantomData,
        }
    }

    pub fn without_meta(self) -> Self {
        Self {
            data: self.data.map_addr(|addr| addr & !Self::META_BITS),
            _marker: PhantomData,
        }
    }

    pub(crate) fn as_ptr(self) -> *mut ManObj<T> {
        self.data.map_addr(|addr| addr & Self::ADDR_BITS).cast()
    }

    pub fn tag(self) -> usize {
        self.data.addr() & Self::LOW_BITS
    }

    pub fn with_tag(self, tag: usize) -> Self {
        Self {
            data: self
                .data
                .map_addr(|addr| (addr & !Self::LOW_BITS) | (tag & Self::LOW_BITS)),
            _marker: PhantomData,
        }
    }

    pub fn is_null(self) -> bool {
        self.as_ptr().is_null()
    }

    pub unsafe fn deref<'l>(self) -> &'l ManObj<T> {
        unsafe { &*self.as_ptr() }
    }

    pub unsafe fn as_ref<'l>(self) -> Option<&'l ManObj<T>> {
        unsafe { self.as_ptr().as_ref() }
    }

    pub(crate) fn shade_pointee(self, guard: &Guard) {
        let Some(mobj) = (unsafe { self.as_ref() }) else {
            // The pointer is null.
            return;
        };
        if mobj.header.load(Ordering::Acquire).marked() == guard.black_color() {
            // It is already marked and traced.
            return;
        }
        todo!("shade it by push to mark stack")
    }
}

pub trait TracePtr {
    /// Hint: Colors after unrooting should be the allocation color.
    fn unroot(&self, guard: &Guard);
}

pub unsafe trait TraceObj {
    fn scan_outgoings(&self) -> &[&dyn TracePtr];

    fn unroot_outgoings(&self, guard: &Guard) {
        for src in self.scan_outgoings() {
            src.unroot(guard);
        }
    }
}

/// A root-count-protected atomic reference to the managed object.
/// It can be sent and atomic with other threads. Before dereferencing,
/// you must create a `Local` reference to the same object by calling `load`.
///
/// It is interiorly-mutable, meaning that you can atomically update
/// the underlying reference.
pub struct AtomicShared<T: Send + Sync + TraceObj> {
    link: AtomicPtr<()>,
    _marker: PhantomData<ManPtr<T>>,
}

unsafe impl<T: Send + Sync + TraceObj> Sync for AtomicShared<T> {}
unsafe impl<T: Send + Sync + TraceObj> Send for AtomicShared<T> {}

impl<T: Send + Sync + TraceObj> AtomicShared<T> {
    pub fn new<'g>(item: T, guard: &'g Guard) -> Self {
        let ptr = ManPtr::alloc_rooted(item, guard.alloc_color(), 1);
        // Safety: `ptr` is freshly allocated.
        unsafe { &ptr.deref().item }.unroot_outgoings(guard);
        Self::from_raw(ptr)
    }

    pub fn null() -> Self {
        Self::from_raw(ManPtr::null_rooted())
    }

    pub(crate) fn from_raw(ptr: ManPtr<T>) -> Self {
        Self {
            link: AtomicPtr::new(ptr.data),
            _marker: PhantomData,
        }
    }

    pub fn load<'g>(&self, order: Ordering, guard: &'g Guard) -> Local<'g, Guard, T> {
        let ptr = self.link.load(order);
        Local::from_raw(ManPtr::from(ptr), guard)
    }

    pub fn store<'l, G: Protector>(&self, new: &Local<'l, G, T>, order: Ordering, guard: &Guard) {
        self.swap(new, order, guard);
    }

    pub fn take<'g, G: Protector>(&self, order: Ordering, guard: &'g Guard) -> Local<'g, Guard, T> {
        self.swap(&Local::null(guard), order, guard)
    }

    pub fn swap<'l, 'g, G: Protector>(
        &self,
        new: &Local<'l, G, T>,
        order: Ordering,
        guard: &'g Guard,
    ) -> Local<'l, Guard, T> {
        let mut old = ManPtr::<T>::from(self.link.load(Ordering::Relaxed));

        // First loop to handle the `Rooted` case.
        if unlikely(old.meta() == PtrMeta::Rooted) {
            // If the source is rooted, we increment the root count before trying update.
            let mut new_rooted = new.as_shared();

            while old.meta() == PtrMeta::Rooted {
                match self.internal_cmpxchg_rooted(old, new_rooted, order, Ordering::Relaxed, guard)
                {
                    Ok(current) => return Local::from_raw(current, guard),
                    Err((current, new)) => {
                        old = current;
                        new_rooted = new;
                    }
                }
            }
        }

        // If the source is unrooted, we focus on the `Unrooted` case only from now on.
        // We can guarantee that an unrooted pointer will never be re-rooted later.
        loop {
            match self.internal_cmpxchg_unrooted(
                old,
                new.as_man_ptr(),
                order,
                Ordering::Relaxed,
                guard,
            ) {
                Ok(current) => return Local::from_raw(current, guard),
                Err(current) => old = current,
            }
        }
    }

    pub fn compare_exchange<'l1, 'l2, 'g, G1, G2>(
        &self,
        current: &Local<'l1, G1, T>,
        new: &Local<'l2, G2, T>,
        success: Ordering,
        failure: Ordering,
        guard: &'g Guard,
    ) -> Result<Local<'g, Guard, T>, Local<'g, Guard, T>>
    where
        G1: Protector,
        G2: Protector,
    {
        let mut old = ManPtr::<T>::from(self.link.load(Ordering::Relaxed));

        if old.without_meta() != current.as_man_ptr().without_meta() {
            // Trivial failure case of CAS.
            return Err(Local::from_raw(old, guard));
        }

        // First loop to handle the `Rooted` case.
        if unlikely(old.meta() == PtrMeta::Rooted) {
            // If the source is rooted, we increment the root count before trying update.
            let new_rooted = new.as_shared();

            match self.internal_cmpxchg_rooted(old, new_rooted, success, failure, guard) {
                Ok(current) => return Ok(Local::from_raw(current, guard)),
                Err((current, _)) => match current.meta() {
                    PtrMeta::Rooted => return Err(Local::from_raw(current, guard)),
                    PtrMeta::Unrooted(_) => old = current,
                },
            }
        }

        // We just want to re-check the trivial failure case.
        if old.without_meta() != current.as_man_ptr() {
            return Err(Local::from_raw(old, guard));
        }

        // If the source is unrooted, we focus on the `Unrooted` case only from now on.
        // We can guarantee that an unrooted pointer will never be re-rooted later.
        self.internal_cmpxchg_unrooted(old, new.as_man_ptr(), success, failure, guard)
            .map(|current| Local::from_raw(ManPtr::from(current), guard))
            .map_err(|current| Local::from_raw(ManPtr::from(current), guard))
    }

    fn internal_cmpxchg_rooted(
        &self,
        old: ManPtr<T>,
        new_rooted: Shared<T>,
        success: Ordering,
        failure: Ordering,
        guard: &Guard,
    ) -> Result<ManPtr<T>, (ManPtr<T>, Shared<T>)> {
        match self
            .link
            .compare_exchange(old.data, new_rooted.as_man_ptr().data, success, failure)
        {
            Ok(_) => {
                // The `new` pointer is successfully inserted.
                // Skip decrementing the root count for the inserted one.
                forget(new_rooted);
                // Decrement the root count of the overwritten one,
                // and execute a deletion barrier if necessary.
                let Some(old_ref) = (unsafe { old.as_ref() }) else {
                    return Ok(old);
                };
                if old_ref.header.decrement_root_count(Ordering::Relaxed) == 1
                    && guard.global_phase() != Phase::N
                {
                    // Root-count deletion barrier.
                    old.shade_pointee(guard);
                }
                Ok(old)
            }
            Err(current) => Err((ManPtr::from(current), new_rooted)),
        }
    }

    fn internal_cmpxchg_unrooted(
        &self,
        old: ManPtr<T>,
        new: ManPtr<T>,
        success: Ordering,
        failure: Ordering,
        guard: &Guard,
    ) -> Result<ManPtr<T>, ManPtr<T>> {
        let PtrMeta::Unrooted(old_color) = old.meta() else {
            unreachable!("An unrooted pointer is never re-rooted.");
        };
        if old_color == guard.black_color() && guard.phase() != Phase::N {
            // Dijkstra-style insertion barrier.
            new.shade_pointee(guard);
        }
        let new = new.with_meta(PtrMeta::Unrooted(old_color));

        let result = self
            .link
            .compare_exchange(old.data, new.data, success, failure)
            .map(|current| ManPtr::from(current))
            .map_err(|current| ManPtr::from(current));

        if result.is_ok() && old_color == guard.white_color() {
            // Yuasa-style deletion barrier.
            old.shade_pointee(guard);
        }
        result
    }

    pub fn fetch_or_tag<'g>(
        &self,
        tag: usize,
        order: Ordering,
        guard: &'g Guard,
    ) -> Local<'g, Guard, T> {
        Local::from_raw(
            ManPtr::from(self.link.fetch_or(tag & ManPtr::<T>::LOW_BITS, order)),
            guard,
        )
    }
}

impl<T: Send + Sync + TraceObj> Drop for AtomicShared<T> {
    fn drop(&mut self) {
        let ptr = ManPtr::<T>::from(self.link.load(Ordering::Relaxed));
        if let PtrMeta::Unrooted(_) = ptr.meta() {
            return;
        }
        if ptr.is_null() {
            return;
        }

        // If it is rooted, we need to pin the thread before decrementing,
        // to safely execute deletion barrier if necessary.
        let guard = pin();
        if unsafe { &*ptr.as_ptr() }
            .header
            .decrement_root_count(Ordering::Relaxed)
            == 1
            && guard.global_phase() != Phase::N
        {
            // Root-count deletion barrier.
            ptr.shade_pointee(&guard);
        }
    }
}

impl<T: Send + Sync + TraceObj> TracePtr for AtomicShared<T> {
    fn unroot(&self, guard: &Guard) {
        let ptr = ManPtr::<T>::from(self.link.load(Ordering::Relaxed));
        debug_assert!(ptr.meta() == PtrMeta::Rooted);
        if let Some(obj) = unsafe { ptr.as_ref() } {
            obj.header.decrement_root_count(Ordering::Relaxed);
        }
        self.link.store(
            ptr.with_meta(PtrMeta::Unrooted(guard.alloc_color())).data,
            Ordering::Relaxed,
        );
    }
}

/// A root-count-protected reference to the managed object.
/// It can be sent and atomic with other threads.
///
/// It is immutable, meaning that you cannot update the underlying reference.
pub struct Shared<T: Send + Sync + TraceObj> {
    // Note: We just use `AtomicShared` to implement `Shared`, even though `Shared` is immutable
    // from the user's perspective. The reason is that `Shared` is also a target of
    // tracing and marking by collectors, so we must still use atomically mutable link.
    //
    // Therefore, we should still use atomic operations to access this pointer.
    // But some optimizations using relaxed operations might be possible because
    // the user is the only one who mutates the address.
    inner: AtomicShared<T>,
}

unsafe impl<T: Send + Sync + TraceObj> Sync for Shared<T> {}
unsafe impl<T: Send + Sync + TraceObj> Send for Shared<T> {}

impl<T: Send + Sync + TraceObj> Shared<T> {
    pub fn new<'g>(item: T, guard: &'g Guard) -> Self {
        Self {
            inner: AtomicShared::new(item, guard),
        }
    }

    pub fn null() -> Self {
        Self {
            inner: AtomicShared::null(),
        }
    }

    pub(crate) fn as_man_ptr(&self) -> ManPtr<T> {
        ManPtr::from(self.inner.link.load(Ordering::Relaxed))
    }

    pub fn as_ref(&self) -> Option<&T> {
        unsafe { self.as_man_ptr().as_ref() }.map(|obj| &obj.item)
    }

    pub unsafe fn deref(&self) -> &T {
        unsafe { &self.as_man_ptr().deref().item }
    }

    pub fn as_local<'g>(&self, guard: &'g Guard) -> Local<'g, Guard, T> {
        Local::from_raw(self.as_man_ptr(), guard)
    }
}

impl<T: Send + Sync + TraceObj> TracePtr for Shared<T> {
    fn unroot(&self, guard: &Guard) {
        self.inner.unroot(guard);
    }
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
pub struct Local<'g, G: Protector, T: Send + Sync + TraceObj> {
    ptr: *mut (),
    sh: G::Shield,
    _marker: PhantomData<(&'g (), ManPtr<T>)>,
}

impl<'g, T: Send + Sync + TraceObj> Local<'g, Guard, T> {
    pub fn new(item: T, guard: &'g Guard) -> Self {
        let ptr = ManPtr::alloc_unrooted(item, guard.alloc_color());
        // Safety: `ptr` is freshly allocated.
        unsafe { &ptr.deref().item }.unroot_outgoings(guard);
        Self {
            ptr: ptr.data,
            sh: guard.protect(ptr.data),
            _marker: PhantomData,
        }
    }
}

impl<'g, G: Protector, T: Send + Sync + TraceObj> Local<'g, G, T> {
    pub fn null(prot: &G) -> Self {
        Self::from_raw(ManPtr::null_base(), prot)
    }

    pub(crate) fn from_raw(ptr: ManPtr<T>, prot: &G) -> Self {
        Self {
            ptr: ptr.without_meta().as_ptr().cast(),
            sh: prot.protect(ptr.as_ptr().cast()),
            _marker: PhantomData,
        }
    }

    pub fn protect<'h, H: Protector>(&self, prot: &'h H) -> Local<'h, H, T> {
        Local {
            ptr: self.ptr,
            sh: prot.protect(self.ptr),
            _marker: PhantomData,
        }
    }

    pub(crate) fn as_man_ptr(&self) -> ManPtr<T> {
        debug_assert!(ManPtr::<T>::from(self.ptr) == ManPtr::<T>::from(self.ptr).without_meta());
        ManPtr::from(self.ptr)
    }

    pub fn as_ref(&self) -> Option<&'g T> {
        unsafe { self.as_man_ptr().as_ref() }.map(|obj| &obj.item)
    }

    pub unsafe fn deref(&self) -> &'g T {
        unsafe { &self.as_man_ptr().deref().item }
    }

    pub fn as_atomic_shared(&self) -> AtomicShared<T> {
        let ptr = self.as_man_ptr();
        if ptr.is_null() {
            return AtomicShared::null();
        }
        unsafe { ptr.deref() }
            .header
            .increment_root_count(Ordering::Relaxed);
        AtomicShared::from_raw(ptr.with_meta(PtrMeta::Rooted))
    }

    pub fn as_shared(&self) -> Shared<T> {
        Shared {
            inner: self.as_atomic_shared(),
        }
    }
}

impl<'g, G: Protector, T: Send + Sync + TraceObj> Clone for Local<'g, G, T>
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

impl<'g, G: Protector, T: Send + Sync + TraceObj> Copy for Local<'g, G, T> where G::Shield: Copy {}
