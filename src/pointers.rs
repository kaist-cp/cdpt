//! Managed pointer types for building GC-integrated data structures.
//!
//! This module provides four pointer types, each suited to a different role:
//!
//! - [`Local`] — the only dereferenceable pointer; scoped to a [`Guard`] or
//!   [`Handle`](crate::Handle).
//! - [`Shared`] — an immutable, `Send + Sync`, root-counted reference (like `Arc<T>`).
//! - [`AtomicShared`] — a non-nullable atomic pointer for concurrently mutable edges.
//! - [`AtomicSharedOption`] — like `AtomicShared`, but nullable.
//!
//! See the [crate-level docs](crate) for a quick-start guide and comparison table.

use crate::{
    epoch::{Color, Phase},
    guards::{Guard, Handle},
    internal::HazardPointer,
    tls::pin,
};
use std::{
    any::TypeId,
    borrow::Borrow,
    collections::HashMap,
    hash::Hash,
    hint::{cold_path, unlikely},
    marker::PhantomData,
    mem::forget,
    ops::Deref,
    ptr::{NonNull, null_mut},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicPtr, AtomicUsize, Ordering},
    },
};

// ── Type registry for type-erased shade functions ───────────────────────────
//
// The type registry exists so that managed pointer types (`AtomicSharedOption`,
// `AtomicShared`, `Shared`, `Local`) can be defined *without* a `T: TraceObj`
// bound on the struct itself.  The `TraceObj` bound is only required on the
// methods that actually interact with the GC (allocation, tracing, barriers).
//
// This matters because `TraceObj` implies `Send + Sync + 'static`.  If the
// struct carried that bound, every containing type would transitively inherit
// it, forcing `TraceObj` (and its supertraits) onto hundreds of struct blocks
// throughout downstream crates — a cascading, tedious, and destructive change.
//
// The trade-off is a small runtime lookup table that maps a compact type id,
// packed into each object's header, to the type-erased shade function for
// that type.  This is consulted only during `Drop` of a rooted
// `AtomicSharedOption` whose root count drops to zero while the collector is
// active — a rare path that is already cold.
//
// The default capacity is 2^TYPE_ID_BITS = 256 distinct managed types.  This
// is generous for the intended use case: this GC manages only *concurrently
// shared* objects, not every allocation in an application.  Most programs have
// a small, fixed set of such types (hash-table nodes, queue entries, tree
// nodes, wrapper structs around them, etc.).  If 256 is ever insufficient,
// increase `TYPE_ID_BITS` — the root-count field shrinks accordingly, but a
// compile-time assertion ensures it never drops below 32 bits.

/// Number of bits reserved for the type id in the object header.
/// Determines the maximum number of distinct managed types (2^TYPE_ID_BITS).
/// Increasing this value shrinks the root-count field by the same number of bits.
pub(crate) const TYPE_ID_BITS: u32 = 8;

/// Maximum number of distinct managed types: 2^TYPE_ID_BITS.
const MAX_TYPE_ID: usize = 1 << TYPE_ID_BITS;

// Ensure the root-count field (63 - 1 color bit - TYPE_ID_BITS) has at least 32 bits.
const_assert!(63 - TYPE_ID_BITS >= 32);

/// Type-erased shade function: given a ManObj pointer, shade its outgoing edges.
type ShadePointeeFn = unsafe fn(mobj_ptr: *mut (), guard: &Guard);

struct TypeRegistry {
    by_type: HashMap<TypeId, u8>,
    fns: Vec<ShadePointeeFn>,
}

static TYPE_REGISTRY: OnceLock<Mutex<TypeRegistry>> = OnceLock::new();

/// Fast lookup table for shade functions (lock-free reads).
static SHADE_FN_TABLE: [AtomicPtr<()>; MAX_TYPE_ID] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; MAX_TYPE_ID];

fn register_type<T: TraceObj>() -> u8 {
    let tid = TypeId::of::<T>();

    let registry = TYPE_REGISTRY.get_or_init(|| {
        Mutex::new(TypeRegistry {
            by_type: HashMap::new(),
            fns: Vec::new(),
        })
    });
    let mut reg = registry.lock().unwrap();

    if let Some(&id) = reg.by_type.get(&tid) {
        return id;
    }

    /// Type-erased shade function.
    unsafe fn shade_erased<U: TraceObj>(mobj_ptr: *mut (), guard: &Guard) {
        let mobj = unsafe { &*(mobj_ptr as *const ManObj<U>) };
        if mobj.is_marked(guard) {
            return;
        }
        guard.schedule_mark(mobj);
    }

    let id = reg.fns.len();
    assert!(
        id < MAX_TYPE_ID,
        "type registry overflow: more than 2^TYPE_ID_BITS distinct managed types \
        (consider increasing TYPE_ID_BITS)"
    );
    let id = id as u8;
    let f: ShadePointeeFn = shade_erased::<T>;
    reg.fns.push(f);
    reg.by_type.insert(tid, id);

    SHADE_FN_TABLE[id as usize].store(f as *mut (), Ordering::Release);
    id
}

#[inline(always)]
fn get_shade_fn(type_id: u8) -> ShadePointeeFn {
    let ptr = SHADE_FN_TABLE[type_id as usize].load(Ordering::Acquire);
    debug_assert!(!ptr.is_null());
    unsafe { std::mem::transmute(ptr) }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Toggles a method's visibility between `pub` (when `feature = "tag"`) and
/// `pub(crate)` (otherwise). Used for `*_with_tag` / `fetch_tag_*` APIs.
macro_rules! tag_fn {
    ($(#[$meta:meta])* fn $name:ident $($rest:tt)*) => {
        $(#[$meta])*
        #[cfg(feature = "tag")]
        pub fn $name $($rest)*

        $(#[$meta])*
        #[cfg(not(feature = "tag"))]
        #[allow(dead_code)]
        pub(crate) fn $name $($rest)*
    };
    ($(#[$meta:meta])* const fn $name:ident $($rest:tt)*) => {
        $(#[$meta])*
        #[cfg(feature = "tag")]
        pub const fn $name $($rest)*

        $(#[$meta])*
        #[cfg(not(feature = "tag"))]
        #[allow(dead_code)]
        pub(crate) const fn $name $($rest)*
    };
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ObjMeta(usize);

impl From<usize> for ObjMeta {
    #[inline(always)]
    fn from(value: usize) -> Self {
        Self(value)
    }
}

impl ObjMeta {
    // Bit layout (64-bit):
    //   Bit 63:              color (1 bit)
    //   Bits RC..RC+TID-1:   type_id (TYPE_ID_BITS bits)
    //   Bits 0..RC-1:        root_count (remaining bits)
    // ... which is by default (64-bit):
    //   Bit 63:     color
    //   Bits 48-55: type_id (8 bits, 256 types)
    //   Bits 0-47:  root_count (48 bits)
    const COLOR_SHIFT: u32 = 63;
    const TYPE_ID_SHIFT: u32 = Self::COLOR_SHIFT - TYPE_ID_BITS;
    const TYPE_ID_MASK: usize = ((1usize << TYPE_ID_BITS) - 1) << Self::TYPE_ID_SHIFT;
    const ROOT_COUNT_BITS: u32 = Self::TYPE_ID_SHIFT;
    const ROOT_COUNT_MASK: usize = (1usize << Self::ROOT_COUNT_BITS) - 1;

    #[cfg(test)]
    #[inline(always)]
    pub fn new(marked: Color, root_count: usize) -> Self {
        debug_assert!(root_count <= Self::ROOT_COUNT_MASK);
        let bits = ((marked as usize) << Self::COLOR_SHIFT) | (root_count & Self::ROOT_COUNT_MASK);
        Self(bits)
    }

    #[inline(always)]
    pub fn new_with_type_id(marked: Color, root_count: usize, type_id: u8) -> Self {
        debug_assert!(root_count <= Self::ROOT_COUNT_MASK);
        let bits = ((marked as usize) << Self::COLOR_SHIFT)
            | ((type_id as usize) << Self::TYPE_ID_SHIFT)
            | (root_count & Self::ROOT_COUNT_MASK);
        Self(bits)
    }

    #[inline(always)]
    pub fn type_id(self) -> u8 {
        ((self.0 & Self::TYPE_ID_MASK) >> Self::TYPE_ID_SHIFT) as u8
    }

    #[inline(always)]
    pub fn marked(self) -> Color {
        (self.0 >> Self::COLOR_SHIFT).into()
    }

    #[inline(always)]
    pub fn with_marked(self, color: Color) -> Self {
        let cleared = self.0 & !(1usize << Self::COLOR_SHIFT);
        Self(cleared | ((color as usize) << Self::COLOR_SHIFT))
    }

    #[inline(always)]
    pub fn root_count(self) -> usize {
        self.0 & Self::ROOT_COUNT_MASK
    }
}

pub(crate) struct AtomicObjMeta(AtomicUsize);

impl Default for AtomicObjMeta {
    #[inline(always)]
    fn default() -> Self {
        Self::from(ObjMeta::default())
    }
}

impl AtomicObjMeta {
    #[cfg(test)]
    #[inline(always)]
    pub fn new(marked: Color, root_count: usize) -> Self {
        Self::from(ObjMeta::new(marked, root_count))
    }

    #[inline(always)]
    pub fn load(&self, order: Ordering) -> ObjMeta {
        ObjMeta::from(self.0.load(order))
    }

    #[inline(always)]
    pub fn increment_root_count(&self, order: Ordering) -> usize {
        let prev = ObjMeta::from(self.0.fetch_add(1, order)).root_count();
        debug_assert!(prev < ObjMeta::ROOT_COUNT_MASK);
        prev
    }

    #[inline(always)]
    pub fn decrement_root_count(&self, order: Ordering) -> usize {
        let prev = ObjMeta::from(self.0.fetch_sub(1, order)).root_count();
        debug_assert!(prev > 0);
        prev
    }

    #[inline(always)]
    pub fn mark(&self, guard: &Guard) {
        let mut meta = ObjMeta::from(self.0.load(Ordering::Relaxed));
        while meta.marked() != guard.black_color() {
            match self.0.compare_exchange(
                meta.0,
                meta.with_marked(guard.black_color()).0,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => meta = ObjMeta::from(current),
            }
        }
    }
}

impl From<ObjMeta> for AtomicObjMeta {
    #[inline(always)]
    fn from(value: ObjMeta) -> Self {
        Self(AtomicUsize::new(value.0))
    }
}

#[repr(C)]
pub(crate) struct ManObj<T> {
    pub(crate) header: AtomicObjMeta,
    item: T,
}

impl<T: TraceObj> ManObj<T> {
    #[inline(always)]
    pub fn new(item: T, color: Color, root_count: usize) -> Self {
        let type_id = register_type::<T>();
        Self {
            header: AtomicObjMeta::from(ObjMeta::new_with_type_id(color, root_count, type_id)),
            item,
        }
    }

    /// Note that marking the object immediately (not scheduling) may be dangerous
    /// if the current thread is in RT, and there are threads in N.
    /// If those threads in N are helping the sweeping, the marked object here
    /// can be misidentified as a dead object.
    #[inline(always)]
    pub fn mark(&self, guard: &Guard) {
        if !self.is_marked(guard) {
            // Safety: Called by the collector during tracing.
            unsafe { self.shade_outgoings(guard) };
            self.header.mark(guard);
        }
    }

    #[inline(always)]
    pub fn is_marked(&self, guard: &Guard) -> bool {
        self.header.load(Ordering::Acquire).marked() == guard.black_color()
    }
}

impl<T: TraceObj> TraceObj for ManObj<T> {
    #[inline(always)]
    unsafe fn unroot_outgoings(&self, guard: &Guard) {
        unsafe { self.item.unroot_outgoings(guard) };
    }

    #[inline(always)]
    unsafe fn shade_outgoings(&self, guard: &Guard) {
        unsafe { self.item.shade_outgoings(guard) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PtrMeta {
    Rooted,
    Unrooted(Color),
}

pub struct ManPtr<T> {
    // Note: Intentionally not used `*mut ManObj<T>` here, to prevent
    // mistakenly dereferencing this pointer. It must be untagged properly
    // before dereferencing (e.g., `as_ptr`).
    data: *mut (),
    _marker: PhantomData<*mut ManObj<T>>,
}

impl<T> Clone for ManPtr<T> {
    #[inline(always)]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ManPtr<T> {}

impl<T> PartialEq for ManPtr<T> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl<T> Eq for ManPtr<T> {}

impl<T> From<*mut ()> for ManPtr<T> {
    #[inline(always)]
    fn from(value: *mut ()) -> Self {
        Self {
            data: value,
            _marker: PhantomData,
        }
    }
}

impl<T> From<NonNull<ManObj<T>>> for ManPtr<T> {
    #[inline(always)]
    fn from(value: NonNull<ManObj<T>>) -> Self {
        Self {
            data: value.cast().as_ptr(),
            _marker: PhantomData,
        }
    }
}

impl<'g, G, T> From<Option<&Local<'g, G, T>>> for ManPtr<T>
where
    G: Protector,
{
    fn from(value: Option<&Local<'g, G, T>>) -> Self {
        value.map(|l| l.as_man_ptr()).unwrap_or(Self::null_base())
    }
}

impl<'g, G, T> From<(Option<&Local<'g, G, T>>, usize)> for ManPtr<T>
where
    G: Protector,
{
    fn from(value: (Option<&Local<'g, G, T>>, usize)) -> Self {
        Self::from(value.0).with_tag(value.1)
    }
}

impl<T> ManPtr<T> {
    const META_WIDTH: u32 = 2;
    const META_BITS: usize = ((1 << Self::META_WIDTH) - 1) << (usize::BITS - Self::META_WIDTH);
    const LOW_BITS: usize = (1 << align_of::<ManObj<T>>().trailing_zeros()) - 1;
    const ADDR_BITS: usize = usize::MAX & !Self::META_BITS & !Self::LOW_BITS;

    #[inline(always)]
    pub(crate) const fn null_base() -> Self {
        Self {
            data: null_mut(),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub(crate) const fn null_rooted() -> Self {
        Self::null_rooted_with_tag(0)
    }

    #[inline(always)]
    pub(crate) const fn null_rooted_with_tag(tag: usize) -> Self {
        // Rooted meta = 0b00 in the top two bits, so only the tag bits remain.
        Self {
            data: (tag & Self::LOW_BITS) as *const () as *mut (),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub(crate) fn meta(self) -> PtrMeta {
        let bits = self.data.addr();
        if bits & (1 << (usize::BITS - 1)) == 0 {
            PtrMeta::Rooted
        } else {
            PtrMeta::Unrooted(Color::from(bits & (1 << (usize::BITS - 2))))
        }
    }

    #[inline(always)]
    pub(crate) fn with_meta(self, meta: PtrMeta) -> Self {
        let new_ptr = self.data.map_addr(|addr| {
            let wo_meta = addr & !Self::META_BITS;
            let meta = match meta {
                PtrMeta::Rooted => 0b00,
                PtrMeta::Unrooted(Color::C0) => 0b10,
                PtrMeta::Unrooted(Color::C1) => 0b11,
            };
            (meta << (usize::BITS - Self::META_WIDTH)) | wo_meta
        });
        Self {
            data: new_ptr,
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub(crate) fn without_meta(self) -> Self {
        Self {
            data: self.data.map_addr(|addr| addr & !Self::META_BITS),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub(crate) fn as_ptr(self) -> *mut ManObj<T> {
        self.data.map_addr(|addr| addr & Self::ADDR_BITS).cast()
    }

    #[inline(always)]
    pub(crate) fn tag(self) -> usize {
        self.data.addr() & Self::LOW_BITS
    }

    #[inline(always)]
    pub(crate) fn with_tag(self, tag: usize) -> Self {
        Self {
            data: self
                .data
                .map_addr(|addr| (addr & !Self::LOW_BITS) | (tag & Self::LOW_BITS)),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub(crate) fn is_null(self) -> bool {
        self.as_ptr().is_null()
    }

    /// # Safety
    ///
    /// The pointer must be non-null and point to a valid `ManObj<T>`.
    #[inline(always)]
    pub(crate) unsafe fn deref<'l>(self) -> &'l ManObj<T> {
        unsafe { &*self.as_ptr() }
    }

    #[inline(always)]
    pub(crate) unsafe fn as_ref<'l>(self) -> Option<&'l ManObj<T>> {
        unsafe { self.as_ptr().as_ref() }
    }
}

impl<T: TraceObj> ManPtr<T> {
    #[inline(always)]
    pub(crate) fn alloc_rooted(item: T, color: Color, root_count: usize, guard: &Guard) -> Self {
        debug_assert!(root_count > 0);
        let obj = ManObj::new(item, color, root_count);
        let addr = guard.alloc(obj);
        let ptr = Self {
            data: addr.cast(),
            _marker: PhantomData,
        };
        ptr.with_meta(PtrMeta::Rooted)
    }

    #[inline(always)]
    pub(crate) fn alloc_unrooted(item: T, color: Color, guard: &Guard) -> Self {
        let obj = ManObj::new(item, color, 0);
        let addr = guard.alloc(obj);
        let ptr = Self {
            data: addr.cast(),
            _marker: PhantomData,
        };
        ptr.with_meta(PtrMeta::Unrooted(color))
    }

    /// Returns `true` if it scheduled a task.
    #[inline(always)]
    pub(crate) fn shade_pointee(self, guard: &Guard) -> bool {
        let Some(mobj) = (unsafe { self.as_ref() }) else {
            // The pointer is null.
            return false;
        };
        if mobj.is_marked(guard) {
            // It is already marked and traced.
            return false;
        }
        // Note that marking the object immediately (not scheduling) may be dangerous
        // if the current thread is in RT, and there are threads in N.
        // If those threads in N are helping the sweeping, the marked object here
        // can be misidentified as a dead object.
        guard.schedule_mark(mobj);
        true
    }
}

impl<T: TraceObj> ManPtr<T> {
    /// # Safety
    ///
    /// Its address bits must be null, or point to a valid memory location.
    #[inline(always)]
    pub(crate) unsafe fn as_local_with_tag<'g>(
        self,
        guard: &'g Guard,
    ) -> (Option<Local<'g, Guard, T>>, usize) {
        let tag = self.tag();
        let opt_local = if self.is_null() {
            None
        } else {
            Some(unsafe { Local::from_raw(self.without_meta().with_tag(0), guard) })
        };
        (opt_local, tag)
    }
}

/// Per-field tracing interface for GC-managed pointer fields.
///
/// Implemented by [`Shared`], [`AtomicShared`], [`AtomicSharedOption`].
/// You typically don't call these methods directly —
/// they are invoked by the `#[derive(TraceObj)]` macro's generated code.
pub trait TracePtr {
    /// Decrements the root count when the parent object is *boxed* (becomes a
    /// heap edge rather than a root).
    fn unroot(&self, guard: &Guard);

    /// Shades the pointee and flips this pointer's color to black during tracing.
    fn shade(&self, guard: &Guard);
}

/// Any type stored in the managed heap must implement this trait.
///
/// Use `#[derive(TraceObj)]` — manual implementations are rarely needed. The
/// derive macro automatically enumerates all [`Shared`], [`AtomicShared`], and
/// [`AtomicSharedOption`] fields (including those inside `Option`, `Vec`, `Box`,
/// tuples, etc.) and generates the required tracing methods.
///
/// # Safety (for method implementors)
///
/// Implementations **must not** scan through interiorly mutable wrappers
/// (e.g., `Mutex<Shared<T>>`). If a field is behind interior mutability, a
/// mutator could move the pointer out concurrently, causing the collector to
/// miss a root. Use [`AtomicShared`] or [`AtomicSharedOption`] for concurrently
/// mutable edges instead.
pub trait TraceObj: 'static + Sync + Send {
    /// Calls [`TracePtr::unroot`] on every outgoing managed pointer field.
    ///
    /// Invoked when the parent object is boxed onto the heap (its outgoing
    /// edges transition from roots to internal heap edges).
    ///
    /// # Safety
    ///
    /// Must only be called by the collector or during object initialization.
    /// Must not scan through interiorly mutable wrappers.
    unsafe fn unroot_outgoings(&self, guard: &Guard);

    /// Calls [`TracePtr::shade`] on every outgoing managed pointer field.
    ///
    /// Invoked during tracing to mark all objects reachable from this one.
    ///
    /// # Safety
    ///
    /// Must only be called by the collector during the tracing phase.
    /// Must not scan through interiorly mutable wrappers.
    unsafe fn shade_outgoings(&self, guard: &Guard);
}

/// Implements `TraceObj` with empty implementations.
#[macro_export]
macro_rules! empty_trace_impl {
    ($($T:ty),*) => {
        $(
            impl TraceObj for $T {
                #[inline]
                unsafe fn unroot_outgoings(&self, _: &Guard) {}
                #[inline]
                unsafe fn shade_outgoings(&self, _: &Guard) {}
            }
        )*
    }
}

empty_trace_impl![
    (),
    bool,
    isize,
    usize,
    i8,
    u8,
    i16,
    u16,
    i32,
    u32,
    i64,
    u64,
    i128,
    u128,
    f32,
    f64,
    char,
    String,
    str,
    std::path::Path,
    std::path::PathBuf,
    std::num::NonZeroIsize,
    std::num::NonZeroUsize,
    std::num::NonZeroI8,
    std::num::NonZeroU8,
    std::num::NonZeroI16,
    std::num::NonZeroU16,
    std::num::NonZeroI32,
    std::num::NonZeroU32,
    std::num::NonZeroI64,
    std::num::NonZeroU64,
    std::num::NonZeroI128,
    std::num::NonZeroU128,
    std::sync::atomic::AtomicBool,
    std::sync::atomic::AtomicIsize,
    std::sync::atomic::AtomicUsize,
    std::sync::atomic::AtomicI8,
    std::sync::atomic::AtomicU8,
    std::sync::atomic::AtomicI16,
    std::sync::atomic::AtomicU16,
    std::sync::atomic::AtomicI32,
    std::sync::atomic::AtomicU32,
    std::sync::atomic::AtomicI64,
    std::sync::atomic::AtomicU64,
    std::collections::hash_map::DefaultHasher,
    std::hash::RandomState
];

/// An internal trait for marking and tracing the object.
pub(crate) trait MarkObj: TraceObj {
    fn mark(&self, guard: &Guard);
    fn color(&self) -> Color;
    fn root_count(&self) -> usize;
    fn address(&self) -> *mut ();
}

impl<T: TraceObj> MarkObj for ManObj<T> {
    #[inline(always)]
    fn mark(&self, guard: &Guard) {
        // Safety: Called by the collector during tracing.
        unsafe { self.shade_outgoings(guard) };
        self.header.mark(guard);
    }

    #[inline(always)]
    fn color(&self) -> Color {
        self.header.load(Ordering::Relaxed).marked()
    }

    #[inline(always)]
    fn root_count(&self) -> usize {
        self.header.load(Ordering::Relaxed).root_count()
    }

    #[inline(always)]
    fn address(&self) -> *mut () {
        // HACK: `header` is always the first field of `ManObj`, so its address must be
        // equal to the address of the `ManObj`.
        ((&self.header) as *const _ as *const ()).cast_mut()
    }
}

/// A nullable, atomic, root-count-protected pointer to a managed object.
///
/// This is the primary *mutable edge* type for building concurrent data
/// structures. It behaves like `AtomicPtr` but with integrated GC barriers:
///
/// - **Nullable** — can hold `None` (use [`AtomicShared`] for the non-nullable
///   variant).
/// - **Atomic** — supports `load`, `store`, `swap`, and `compare_exchange`,
///   each requiring a [`Guard`] to maintain the tricolor invariant.
/// - **Root-counted** — when first created (e.g., via [`some`](Self::some)),
///   the target has root count = 1. Once stored as a heap edge (inside another
///   managed object), the root count is decremented by `unroot_outgoings`.
///
/// # Example
///
/// ```ignore
/// #[derive(TraceObj)]
/// struct Node {
///     next: AtomicSharedOption<Node>,  // nullable, concurrently mutable
/// }
/// ```
/// The `T` parameter is unconstrained on the struct, allowing types that
/// contain `AtomicSharedOption<T>` to avoid requiring `T: TraceObj` on their
/// own struct definitions. The `TraceObj` bound is instead required on all
/// impl blocks that allocate or dereference managed objects.
pub struct AtomicSharedOption<T> {
    link: AtomicPtr<()>,
    _marker: PhantomData<*mut T>,
}

assert_eq_size!(AtomicSharedOption<()>, usize);
// `none()` is zero-initialized because `PtrMeta::Rooted` encodes as 0b00 in the
// top two bits, making `null_rooted()` an all-zero pointer.
const_assert!(ManPtr::<()>::null_rooted().data.is_null());

// Safety: AtomicSharedOption only stores an `AtomicPtr<()>`. All access to the
// managed `T` goes through methods that require `T: TraceObj` (which implies
// `T: Send + Sync + 'static`), so there is no way to create an
// AtomicSharedOption<T> pointing to a non-Send/Sync T.
unsafe impl<T: Sync + Send> Sync for AtomicSharedOption<T> {}
unsafe impl<T: Sync + Send> Send for AtomicSharedOption<T> {}

impl<T> Default for AtomicSharedOption<T> {
    #[inline(always)]
    fn default() -> Self {
        Self::none()
    }
}

impl<T> AtomicSharedOption<T> {
    /// Creates a null (empty) atomic pointer. This is a `const` function, so
    /// it can be used in static or field initializers.
    ///
    /// The returned value is **zero-initialized** (all bytes are `0x00`),
    /// which means arrays of `AtomicSharedOption` can be safely created with
    /// `zeroed()` or equivalent zero-fill operations.
    ///
    /// This method does not require `T: TraceObj` because no managed object
    /// is allocated — the pointer is simply null.
    #[inline(always)]
    pub const fn none() -> Self {
        // Rooted meta = 0b00, null pointer = 0x0, so this is all-zero.
        Self {
            link: AtomicPtr::new(null_mut()),
            _marker: PhantomData,
        }
    }

    tag_fn! {
        /// Creates a null atomic pointer with the given tag bits set.
        #[inline(always)]
        const fn none_with_tag(tag: usize) -> Self {
            Self::from_raw(ManPtr::null_rooted_with_tag(tag))
        }
    }

    #[inline(always)]
    pub(crate) const fn from_raw(ptr: ManPtr<T>) -> Self {
        Self {
            link: AtomicPtr::new(ptr.data),
            _marker: PhantomData,
        }
    }
}

impl<T: TraceObj> AtomicSharedOption<T> {
    /// Allocates a new managed object and wraps it in a nullable atomic pointer.
    ///
    /// The returned pointer holds a root count of 1 on the new object.
    #[inline(always)]
    pub fn some(item: T, guard: &Guard) -> Self {
        let ptr = ManPtr::alloc_rooted(item, guard.alloc_color(), 1, guard);
        // Safety: `ptr` is freshly allocated; unrooting during initialization.
        unsafe { ptr.deref().item.unroot_outgoings(guard) };
        Self::from_raw(ptr)
    }

    tag_fn! {
        /// Like [`some`](Self::some), but stores extra bits in the pointer's low
        /// tag bits (available due to alignment).
        #[inline(always)]
        fn some_with_tag(item: T, tag: usize, guard: &Guard) -> Self {
            let ptr = ManPtr::alloc_rooted(item, guard.alloc_color(), 1, guard);
            // Safety: `ptr` is freshly allocated; unrooting during initialization.
            unsafe { ptr.deref().item.unroot_outgoings(guard) };
            Self::from_raw(ptr.with_tag(tag))
        }
    }

    /// Reads the current pointer value.
    ///
    /// Returns `None` if null, or a guard-scoped [`Local`] that you can
    /// dereference to access the managed object.
    #[inline(always)]
    pub fn load<'g>(&self, order: Ordering, guard: &'g Guard) -> Option<Local<'g, Guard, T>> {
        self.load_with_tag(order, guard).0
    }

    tag_fn! {
        /// Like [`load`](Self::load), but also returns the tag bits.
        #[inline(always)]
        fn load_with_tag<'g>(
            &self,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Option<Local<'g, Guard, T>>, usize) {
            let ptr = ManPtr::from(self.link.load(order));
            // Safety: If the pointer is not null, it points to a valid memory location.
            // The validity should be guaranteed by the correctness of this garbage collector.
            unsafe { ptr.as_local_with_tag(guard) }
        }
    }

    /// Replaces the pointer with `new` (or null if `None`).
    ///
    /// The previous target is released automatically (root count decremented,
    /// write barriers applied).
    pub fn store<'l, G: Protector>(
        &self,
        new: Option<&Local<'l, G, T>>,
        order: Ordering,
        guard: &Guard,
    ) {
        self.store_with_tag(new, 0, order, guard);
    }

    tag_fn! {
        /// Like [`store`](Self::store), but also sets the tag bits.
        fn store_with_tag<'l, G: Protector>(
            &self,
            new: Option<&Local<'l, G, T>>,
            tag: usize,
            order: Ordering,
            guard: &Guard,
        ) {
            self.swap_with_tag(new, tag, order, guard);
        }
    }

    /// Removes and returns the current value, leaving this pointer null.
    pub fn take<'g>(&self, order: Ordering, guard: &'g Guard) -> Option<Local<'g, Guard, T>> {
        self.take_with_tag(order, guard).0
    }

    tag_fn! {
        /// Like [`take`](Self::take), but also returns the tag bits.
        fn take_with_tag<'g>(
            &self,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Option<Local<'g, Guard, T>>, usize) {
            self.swap_with_tag(None::<&Local<'g, Guard, T>>, 0, order, guard)
        }
    }

    /// Replaces the pointer with `new` and returns the old value.
    pub fn swap<'l, 'g, G: Protector>(
        &self,
        new: Option<&Local<'l, G, T>>,
        order: Ordering,
        guard: &'g Guard,
    ) -> Option<Local<'g, Guard, T>> {
        self.swap_with_tag(new, 0, order, guard).0
    }

    tag_fn! {
        /// Like [`swap`](Self::swap), but also sets/returns tag bits.
        fn swap_with_tag<'l, 'g, G: Protector>(
            &self,
            new: Option<&Local<'l, G, T>>,
            tag: usize,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Option<Local<'g, Guard, T>>, usize) {
            let ptr = self.internal_swap(ManPtr::from(new).with_tag(tag), order, guard);
            // Safety: If the pointer is not null, it points to a valid memory location.
            // The validity should be guaranteed by the correctness of this garbage collector.
            unsafe { ptr.as_local_with_tag(guard) }
        }
    }

    fn internal_swap(&self, new: ManPtr<T>, order: Ordering, guard: &Guard) -> ManPtr<T> {
        let mut old = ManPtr::<T>::from(self.link.load(Ordering::Relaxed));

        // First loop to handle the `Rooted` case.
        if unlikely(old.meta() == PtrMeta::Rooted) {
            // If the source is rooted, we increment the root count before trying update.
            let new_shared = Shared::try_inc_raw(new, guard);
            let new_rooted = new.with_meta(PtrMeta::Rooted);

            while old.meta() == PtrMeta::Rooted {
                match self.internal_cmpxchg_rooted(old, new_rooted, order, Ordering::Relaxed, guard)
                {
                    Ok(current) => {
                        // The `new` pointer is successfully inserted.
                        // Skip decrementing the root count for the inserted one.
                        forget(new_shared);
                        return current;
                    }
                    Err(current) => old = current,
                }
            }
        }

        // If the source is unrooted, we focus on the `Unrooted` case only from now on.
        // We can guarantee that an unrooted pointer will never be re-rooted later.
        loop {
            match self.internal_cmpxchg_unrooted(old, new, order, Ordering::Relaxed, guard) {
                Ok(current) => return current,
                Err(current) => old = current,
            }
        }
    }

    /// Atomically compares-and-swaps the pointer.
    ///
    /// On success, returns `Ok` with the **previous** value that was in the
    /// atomic (i.e. the old value, which matches `current`). On failure,
    /// returns `Err` with the **current actual** value.
    ///
    /// This follows the same convention as [`std::sync::atomic::AtomicPtr::compare_exchange`]:
    /// both `Ok` and `Err` carry the value that was loaded from the atomic.
    ///
    /// Both `current` and `new` may be `None` (null). GC write barriers
    /// (insertion + deletion) are applied automatically.
    #[allow(clippy::type_complexity)]
    pub fn compare_exchange<'l1, 'l2, 'g, G1, G2>(
        &self,
        current: Option<&Local<'l1, G1, T>>,
        new: Option<&Local<'l2, G2, T>>,
        success: Ordering,
        failure: Ordering,
        guard: &'g Guard,
    ) -> Result<Option<Local<'g, Guard, T>>, Option<Local<'g, Guard, T>>>
    where
        G1: Protector,
        G2: Protector,
    {
        self.compare_exchange_with_tag((current, 0), (new, 0), success, failure, guard)
            .map(|pt| pt.0)
            .map_err(|pt| pt.0)
    }

    tag_fn! {
        /// Like [`compare_exchange`](Self::compare_exchange), but also compares/sets
        /// tag bits.
        ///
        /// On success, returns `Ok` with the **previous** (pointer, tag) pair.
        /// On failure, returns `Err` with the **current actual** (pointer, tag) pair.
        #[allow(clippy::type_complexity)]
        fn compare_exchange_with_tag<'l1, 'l2, 'g, G1, G2>(
            &self,
            current_tag: (Option<&Local<'l1, G1, T>>, usize),
            new_tag: (Option<&Local<'l2, G2, T>>, usize),
            success: Ordering,
            failure: Ordering,
            guard: &'g Guard,
        ) -> Result<(Option<Local<'g, Guard, T>>, usize), (Option<Local<'g, Guard, T>>, usize)>
        where
            G1: Protector,
            G2: Protector,
        {
            let mut old = ManPtr::<T>::from(self.link.load(Ordering::Relaxed));

            if old.without_meta() != ManPtr::from(current_tag) {
                // Trivial failure case of CAS.
                // Safety: If the pointer is not null, it points to a valid memory location.
                // The validity should be guaranteed by the correctness of this garbage collector.
                return Err(unsafe { old.as_local_with_tag(guard) });
            }

            // First block to handle the `Rooted` case.
            if unlikely(old.meta() == PtrMeta::Rooted) {
                // If the source is rooted, we increment the root count before trying update.
                let new_shared = Shared::try_inc_raw(ManPtr::from(new_tag.0), guard);
                let new_rooted = ManPtr::from(new_tag).with_meta(PtrMeta::Rooted);

                match self.internal_cmpxchg_rooted(old, new_rooted, success, failure, guard) {
                    Ok(current) => {
                        // The `new` pointer is successfully inserted.
                        // Skip decrementing the root count for the inserted one.
                        forget(new_shared);
                        return Ok(unsafe { current.as_local_with_tag(guard) });
                    }
                    Err(current) => match current.meta() {
                        PtrMeta::Rooted => return Err(unsafe { current.as_local_with_tag(guard) }),
                        PtrMeta::Unrooted(_) => old = current,
                    },
                }

                // We just want to re-check the trivial failure case.
                if old.without_meta() != ManPtr::from(current_tag) {
                    return Err(unsafe { old.as_local_with_tag(guard) });
                }
            }

            // If the source is unrooted, we focus on the `Unrooted` case only from now on.
            // We can guarantee that an unrooted pointer will never be re-rooted later.
            self.internal_cmpxchg_unrooted(old, ManPtr::from(new_tag), success, failure, guard)
                .map(|current| unsafe { current.as_local_with_tag(guard) })
                .map_err(|current| unsafe { current.as_local_with_tag(guard) })
        }
    }

    tag_fn! {
        /// Weak variant of [`compare_exchange_with_tag`](Self::compare_exchange_with_tag).
        ///
        /// Unlike the strong variant, this does NOT retry when the CAS fails due
        /// to a concurrent GC color-metadata change. Callers that already sit
        /// inside their own retry loop should prefer this to avoid redundant
        /// retries.
        ///
        /// On success, returns `Ok` with the **previous** (pointer, tag) pair.
        /// On failure, returns `Err` with the **current actual** (pointer, tag)
        /// pair. The failure may be spurious (only GC metadata changed).
        #[allow(clippy::type_complexity)]
        fn compare_exchange_weak_with_tag<'l1, 'l2, 'g, G1, G2>(
            &self,
            current_tag: (Option<&Local<'l1, G1, T>>, usize),
            new_tag: (Option<&Local<'l2, G2, T>>, usize),
            success: Ordering,
            failure: Ordering,
            guard: &'g Guard,
        ) -> Result<(Option<Local<'g, Guard, T>>, usize), (Option<Local<'g, Guard, T>>, usize)>
        where
            G1: Protector,
            G2: Protector,
        {
            let old = ManPtr::<T>::from(self.link.load(Ordering::Relaxed));

            if old.without_meta() != ManPtr::from(current_tag) {
                return Err(unsafe { old.as_local_with_tag(guard) });
            }

            if old.meta() == PtrMeta::Rooted {
                cold_path();
                let new_shared = Shared::try_inc_raw(ManPtr::from(new_tag.0), guard);
                let new_rooted = ManPtr::from(new_tag).with_meta(PtrMeta::Rooted);

                match self.internal_cmpxchg_rooted(old, new_rooted, success, failure, guard) {
                    Ok(current) => {
                        forget(new_shared);
                        return Ok(unsafe { current.as_local_with_tag(guard) });
                    }
                    Err(current) => {
                        return Err(unsafe { current.as_local_with_tag(guard) });
                    }
                }
            }

            self.internal_cmpxchg_unrooted_weak(old, ManPtr::from(new_tag), success, failure, guard)
                .map(|current| unsafe { current.as_local_with_tag(guard) })
                .map_err(|current| unsafe { current.as_local_with_tag(guard) })
        }
    }

    fn internal_cmpxchg_rooted(
        &self,
        old: ManPtr<T>,
        new_rooted: ManPtr<T>,
        success: Ordering,
        failure: Ordering,
        guard: &Guard,
    ) -> Result<ManPtr<T>, ManPtr<T>> {
        debug_assert!(old.meta() == PtrMeta::Rooted);
        debug_assert!(new_rooted.meta() == PtrMeta::Rooted);

        match self
            .link
            .compare_exchange(old.data, new_rooted.data, success, failure)
        {
            Ok(_) => {
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
            Err(current) => Err(ManPtr::from(current)),
        }
    }

    fn internal_cmpxchg_unrooted(
        &self,
        mut old: ManPtr<T>,
        new: ManPtr<T>,
        success: Ordering,
        failure: Ordering,
        guard: &Guard,
    ) -> Result<ManPtr<T>, ManPtr<T>> {
        debug_assert!(old.meta() != PtrMeta::Rooted);
        // Note: `new.meta()` may appear Rooted here because `ManPtr::from(Local)`
        // produces a bare pointer (meta bits = 0 = Rooted encoding). This is fine
        // because we overwrite the meta to `Unrooted(old_color)` below.

        loop {
            let PtrMeta::Unrooted(old_color) = old.meta() else {
                unreachable!("An unrooted pointer is never re-rooted.");
            };
            if old.as_ptr() != new.as_ptr() && old_color == guard.black_color() {
                // Dijkstra-style insertion barrier.
                new.shade_pointee(guard);
            }
            let new = new.with_meta(PtrMeta::Unrooted(old_color));

            let result = self
                .link
                .compare_exchange(old.data, new.data, success, failure)
                .map(ManPtr::from)
                .map_err(ManPtr::from);

            match result {
                Ok(_) => {
                    if old.as_ptr() != new.as_ptr()
                        && old_color == guard.white_color()
                        && guard.global_phase() != Phase::N
                    {
                        // Yuasa-style deletion barrier.
                        old.shade_pointee(guard);
                    }
                }
                Err(current) => {
                    if current.without_meta() == old.without_meta() {
                        // if the only metadata (i.e., color) is changed, let's retry.
                        old = current;
                        continue;
                    }
                }
            }

            return result;
        }
    }

    fn internal_cmpxchg_unrooted_weak(
        &self,
        old: ManPtr<T>,
        new: ManPtr<T>,
        success: Ordering,
        failure: Ordering,
        guard: &Guard,
    ) -> Result<ManPtr<T>, ManPtr<T>> {
        debug_assert!(old.meta() != PtrMeta::Rooted);

        let PtrMeta::Unrooted(old_color) = old.meta() else {
            unreachable!("An unrooted pointer is never re-rooted.");
        };
        if old.as_ptr() != new.as_ptr() && old_color == guard.black_color() {
            new.shade_pointee(guard);
        }
        let new = new.with_meta(PtrMeta::Unrooted(old_color));

        let result = self
            .link
            .compare_exchange(old.data, new.data, success, failure)
            .map(ManPtr::from)
            .map_err(ManPtr::from);

        if let Ok(_) = &result {
            if old.as_ptr() != new.as_ptr()
                && old_color == guard.white_color()
                && guard.global_phase() != Phase::N
            {
                old.shade_pointee(guard);
            }
        }

        result
    }

    tag_fn! {
        /// Atomically ANDs the tag bits, returning the previous (pointer, tag) pair.
        ///
        /// Tag bits are low bits available due to pointer alignment, commonly used
        /// to mark nodes (e.g., logical deletion in lock-free data structures).
        /// Only the tag bits are affected; the pointed-to address is unchanged.
        #[inline(always)]
        fn fetch_tag_and<'g>(
            &self,
            tag: usize,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Option<Local<'g, Guard, T>>, usize) {
            // Safety: If the pointer is not null, it points to a valid memory location.
            // The validity should be guaranteed by the correctness of this garbage collector.
            unsafe {
                ManPtr::from(self.link.fetch_and(tag & ManPtr::<T>::LOW_BITS, order))
                    .as_local_with_tag(guard)
            }
        }
    }

    tag_fn! {
        /// Atomically ORs the tag bits, returning the previous (pointer, tag) pair.
        #[inline(always)]
        fn fetch_tag_or<'g>(
            &self,
            tag: usize,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Option<Local<'g, Guard, T>>, usize) {
            // Safety: If the pointer is not null, it points to a valid memory location.
            // The validity should be guaranteed by the correctness of this garbage collector.
            unsafe {
                ManPtr::from(self.link.fetch_or(tag & ManPtr::<T>::LOW_BITS, order))
                    .as_local_with_tag(guard)
            }
        }
    }

    tag_fn! {
        /// Atomically XORs the tag bits, returning the previous (pointer, tag) pair.
        #[inline(always)]
        fn fetch_tag_xor<'g>(
            &self,
            tag: usize,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Option<Local<'g, Guard, T>>, usize) {
            // Safety: If the pointer is not null, it points to a valid memory location.
            // The validity should be guaranteed by the correctness of this garbage collector.
            unsafe {
                ManPtr::from(self.link.fetch_xor(tag & ManPtr::<T>::LOW_BITS, order))
                    .as_local_with_tag(guard)
            }
        }
    }
}

impl<T> Drop for AtomicSharedOption<T> {
    #[inline(always)]
    fn drop(&mut self) {
        let ptr = ManPtr::<T>::from(self.link.load(Ordering::Relaxed));

        // Unrooted → no drop action needed.
        if ptr.meta() != PtrMeta::Rooted {
            return;
        }

        if ptr.is_null() {
            return;
        }

        // If it is rooted, we need to pin the thread before decrementing,
        // to safely execute deletion barrier if necessary.
        let guard = pin();

        // Safety: `ptr` is non-null and rooted. `ManObj` is `#[repr(C)]`
        // with `header: AtomicObjMeta` at offset 0, so `as_ptr()` gives us
        // a valid header pointer.
        let header = unsafe { &*(ptr.as_ptr() as *const AtomicObjMeta) };
        if header.decrement_root_count(Ordering::Relaxed) == 1 && guard.global_phase() != Phase::N {
            // Look up the shade function via the type_id packed in the header.
            let meta = header.load(Ordering::Relaxed);
            let shade_fn = get_shade_fn(meta.type_id());
            unsafe { shade_fn(ptr.as_ptr() as *mut (), &guard) };
        }
    }
}

impl<T: TraceObj> TracePtr for AtomicSharedOption<T> {
    #[inline(always)]
    fn unroot(&self, guard: &Guard) {
        let ptr = ManPtr::<T>::from(self.link.load(Ordering::Relaxed));
        debug_assert!(ptr.meta() == PtrMeta::Rooted);
        if let Some(obj) = unsafe { ptr.as_ref() } {
            cold_path(); // When being allocated, most objects' outgoing edges are `null`. 
            // Note: We believe that the deletion barrier must be considered here.
            // Imagine a scenario that an object is allocated in a normal phase,
            // and it becomes a child of another object that is allocated in a next tracing phase.
            // If its link has the allocation color (i.e., black) without marking the child,
            // it is possible that the collector misses the child object.
            let count = obj.header.decrement_root_count(Ordering::Relaxed);
            if count == 1 && guard.global_phase() != Phase::N {
                ptr.shade_pointee(guard);
            }
        }
        self.link.store(
            ptr.with_meta(PtrMeta::Unrooted(guard.alloc_color())).data,
            Ordering::Relaxed,
        );
    }

    #[inline(always)]
    fn shade(&self, guard: &Guard) {
        let mut ptr = ManPtr::<T>::from(self.link.load(Ordering::Relaxed));
        loop {
            debug_assert!(matches!(ptr.meta(), PtrMeta::Unrooted(_)));

            if ptr.meta() == PtrMeta::Unrooted(guard.black_color()) {
                // Already shaded by others. Let's skip.
                break;
            }
            ptr.shade_pointee(guard);
            let black = ptr.with_meta(PtrMeta::Unrooted(guard.black_color()));
            match self.link.compare_exchange(
                ptr.data,
                black.data,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => ptr = ManPtr::<T>::from(current),
            }
        }
    }
}

// TODO: Add a complete set of conversion functions (both `From` traits and type's methods).

impl<T> From<AtomicShared<T>> for AtomicSharedOption<T> {
    fn from(value: AtomicShared<T>) -> Self {
        value.inner
    }
}

impl<T> From<Shared<T>> for AtomicSharedOption<T> {
    fn from(value: Shared<T>) -> Self {
        value.inner.inner
    }
}

impl<T> From<Option<Shared<T>>> for AtomicSharedOption<T> {
    fn from(value: Option<Shared<T>>) -> Self {
        if let Some(shared) = value {
            Self::from(shared)
        } else {
            AtomicSharedOption::none()
        }
    }
}

// Instead of having `From<Option<&Shared<T>>>` and `From<Option<&Local<'g, G, T>>>`,
// one might try to generalize this to something like:
//
//     impl<A> From<Option<A>> where A: AsRef<Shared<T>>
//
// (and similarly for `Local`).
//
// However, this does not work due to overlapping impls:
// a type may implement both `AsRef<Shared<T>>` and `AsRef<Local<'g, G, T>>`,
// which violates Rust’s coherence rules.

impl<T: TraceObj> From<Option<&Shared<T>>> for AtomicSharedOption<T> {
    fn from(value: Option<&Shared<T>>) -> Self {
        if let Some(shared) = value {
            Self::from(shared.clone())
        } else {
            AtomicSharedOption::none()
        }
    }
}

impl<'g, G, T> From<Local<'g, G, T>> for AtomicSharedOption<T>
where
    G: Protector,
{
    fn from(value: Local<'g, G, T>) -> Self {
        Self::from(value.as_atomic_shared())
    }
}

impl<'g, G, T> From<Option<Local<'g, G, T>>> for AtomicSharedOption<T>
where
    G: Protector,
{
    fn from(value: Option<Local<'g, G, T>>) -> Self {
        if let Some(local) = value {
            Self::from(local)
        } else {
            AtomicSharedOption::none()
        }
    }
}

impl<'g, G, T> From<Option<&Local<'g, G, T>>> for AtomicSharedOption<T>
where
    G: Protector,
{
    fn from(value: Option<&Local<'g, G, T>>) -> Self {
        if let Some(local) = value {
            Self::from(local.as_atomic_shared())
        } else {
            AtomicSharedOption::none()
        }
    }
}

/// A non-nullable, atomic, root-count-protected pointer to a managed object.
///
/// Identical to [`AtomicSharedOption`] except it is guaranteed to always point
/// to a valid object (never null). Use this for edges that must always be
/// populated, such as the sentinel head of a linked list.
///
/// All `load`/`store`/`swap`/`compare_exchange` methods mirror those of
/// `AtomicSharedOption` but return `Local` directly instead of `Option<Local>`.
///
/// # Example
///
/// ```ignore
/// struct List {
///     head: AtomicShared<Node>,  // always points to a sentinel node
/// }
/// ```
pub struct AtomicShared<T> {
    inner: AtomicSharedOption<T>,
}

unsafe impl<T: Sync + Send> Sync for AtomicShared<T> {}
unsafe impl<T: Sync + Send> Send for AtomicShared<T> {}

impl<T> AtomicShared<T> {
    #[inline(always)]
    pub(crate) const fn from_raw(ptr: ManPtr<T>) -> Self {
        Self {
            inner: AtomicSharedOption::from_raw(ptr),
        }
    }
}

impl<T: TraceObj> AtomicShared<T> {
    /// Allocates a managed object and returns a non-nullable atomic pointer.
    #[inline(always)]
    pub fn new(item: T, guard: &Guard) -> Self {
        Self {
            inner: AtomicSharedOption::some(item, guard),
        }
    }

    tag_fn! {
        /// Like [`new`](Self::new), but with initial tag bits.
        #[inline(always)]
        fn new_with_tag(item: T, tag: usize, guard: &Guard) -> Self {
            Self {
                inner: AtomicSharedOption::some_with_tag(item, tag, guard),
            }
        }
    }

    /// Reads the current pointer value as a guard-scoped [`Local`].
    ///
    /// Unlike [`AtomicSharedOption::load`], this never returns `None`.
    #[inline(always)]
    pub fn load<'g>(&self, order: Ordering, guard: &'g Guard) -> Local<'g, Guard, T> {
        let r = self.inner.load(order, guard);
        debug_assert!(r.is_some());
        // Safety: There must be no possible path to write a `None` to the inner atomic.
        unsafe { r.unwrap_unchecked() }
    }

    tag_fn! {
        /// Like [`load`](Self::load), but also returns the tag bits.
        #[inline(always)]
        fn load_with_tag<'g>(
            &self,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Local<'g, Guard, T>, usize) {
            let r = self.inner.load_with_tag(order, guard);
            debug_assert!(r.0.is_some());
            // Safety: There must be no possible path to write a `None` to the inner atomic.
            unsafe { (r.0.unwrap_unchecked(), r.1) }
        }
    }

    /// Replaces the pointer with `new`, applying write barriers as needed.
    pub fn store<'l, G: Protector>(&self, new: &Local<'l, G, T>, order: Ordering, guard: &Guard) {
        self.inner.store(Some(new), order, guard);
    }

    tag_fn! {
        /// Like [`store`](Self::store), but also sets the tag bits.
        fn store_with_tag<'l, G: Protector>(
            &self,
            new: &Local<'l, G, T>,
            tag: usize,
            order: Ordering,
            guard: &Guard,
        ) {
            self.inner.store_with_tag(Some(new), tag, order, guard);
        }
    }

    /// Replaces the pointer with `new` and returns the old value.
    pub fn take<'g>(&self, order: Ordering, guard: &'g Guard) -> Local<'g, Guard, T> {
        let r = self.inner.take(order, guard);
        debug_assert!(r.is_some());
        // Safety: There must be no possible path to write a `None` to the inner atomic.
        unsafe { r.unwrap_unchecked() }
    }

    tag_fn! {
        /// Like [`take`](Self::take), but also returns the tag bits.
        fn take_with_tag<'g>(
            &self,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Local<'g, Guard, T>, usize) {
            let r = self.inner.take_with_tag(order, guard);
            debug_assert!(r.0.is_some());
            // Safety: There must be no possible path to write a `None` to the inner atomic.
            unsafe { (r.0.unwrap_unchecked(), r.1) }
        }
    }

    /// Replaces the pointer with `new` and returns the old value.
    pub fn swap<'l, 'g, G: Protector>(
        &self,
        new: &Local<'l, G, T>,
        order: Ordering,
        guard: &'g Guard,
    ) -> Local<'g, Guard, T> {
        let r = self.inner.swap(Some(new), order, guard);
        debug_assert!(r.is_some());
        // Safety: There must be no possible path to write a `None` to the inner atomic.
        unsafe { r.unwrap_unchecked() }
    }

    tag_fn! {
        /// Like [`swap`](Self::swap), but also sets/returns tag bits.
        fn swap_with_tag<'l, 'g, G: Protector>(
            &self,
            new: &Local<'l, G, T>,
            tag: usize,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Local<'g, Guard, T>, usize) {
            let r = self.inner.swap_with_tag(Some(new), tag, order, guard);
            debug_assert!(r.0.is_some());
            // Safety: There must be no possible path to write a `None` to the inner atomic.
            unsafe { (r.0.unwrap_unchecked(), r.1) }
        }
    }

    /// Atomically compares-and-swaps the pointer with GC write barriers.
    ///
    /// On success, returns `Ok` with the **previous** value that was in the
    /// atomic (i.e. the old value, which matches `current`). On failure,
    /// returns `Err` with the **current actual** value.
    ///
    /// This follows the same convention as [`std::sync::atomic::AtomicPtr::compare_exchange`].
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
        let r = self
            .inner
            .compare_exchange(Some(current), Some(new), success, failure, guard);
        match r {
            Ok(l) | Err(l) => debug_assert!(l.is_some()),
        }
        // Safety: There must be no possible path to write a `None` to the inner atomic.
        unsafe {
            r.map(|l| l.unwrap_unchecked())
                .map_err(|l| l.unwrap_unchecked())
        }
    }

    tag_fn! {
        /// Like [`compare_exchange`](Self::compare_exchange), but also compares/sets
        /// tag bits.
        ///
        /// On success, returns `Ok` with the **previous** (pointer, tag) pair.
        /// On failure, returns `Err` with the **current actual** (pointer, tag) pair.
        #[allow(clippy::type_complexity)]
        fn compare_exchange_with_tag<'l1, 'l2, 'g, G1, G2>(
            &self,
            current_tag: (&Local<'l1, G1, T>, usize),
            new_tag: (&Local<'l2, G2, T>, usize),
            success: Ordering,
            failure: Ordering,
            guard: &'g Guard,
        ) -> Result<(Local<'g, Guard, T>, usize), (Local<'g, Guard, T>, usize)>
        where
            G1: Protector,
            G2: Protector,
        {
            let r = self.inner.compare_exchange_with_tag(
                (Some(current_tag.0), current_tag.1),
                (Some(new_tag.0), new_tag.1),
                success,
                failure,
                guard,
            );
            match r {
                Ok((l, _)) | Err((l, _)) => debug_assert!(l.is_some()),
            }
            // Safety: There must be no possible path to write a `None` to the inner atomic.
            unsafe {
                r.map(|(l, t)| (l.unwrap_unchecked(), t))
                    .map_err(|(l, t)| (l.unwrap_unchecked(), t))
            }
        }
    }

    tag_fn! {
        /// Atomically ANDs the tag bits, returning the previous (pointer, tag) pair.
        #[inline(always)]
        fn fetch_tag_and<'g>(
            &self,
            tag: usize,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Local<'g, Guard, T>, usize) {
            let r = self.inner.fetch_tag_and(tag, order, guard);
            debug_assert!(r.0.is_some());
            // Safety: There must be no possible path to write a `None` to the inner atomic.
            unsafe { (r.0.unwrap_unchecked(), r.1) }
        }
    }

    tag_fn! {
        /// Atomically ORs the tag bits, returning the previous (pointer, tag) pair.
        #[inline(always)]
        fn fetch_tag_or<'g>(
            &self,
            tag: usize,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Local<'g, Guard, T>, usize) {
            let r = self.inner.fetch_tag_or(tag, order, guard);
            debug_assert!(r.0.is_some());
            // Safety: There must be no possible path to write a `None` to the inner atomic.
            unsafe { (r.0.unwrap_unchecked(), r.1) }
        }
    }

    tag_fn! {
        /// Atomically XORs the tag bits, returning the previous (pointer, tag) pair.
        #[inline(always)]
        fn fetch_tag_xor<'g>(
            &self,
            tag: usize,
            order: Ordering,
            guard: &'g Guard,
        ) -> (Local<'g, Guard, T>, usize) {
            let r = self.inner.fetch_tag_xor(tag, order, guard);
            debug_assert!(r.0.is_some());
            // Safety: There must be no possible path to write a `None` to the inner atomic.
            unsafe { (r.0.unwrap_unchecked(), r.1) }
        }
    }
}

impl<T: TraceObj> TracePtr for AtomicShared<T> {
    #[inline(always)]
    fn unroot(&self, guard: &Guard) {
        self.inner.unroot(guard);
    }

    #[inline(always)]
    fn shade(&self, guard: &Guard) {
        self.inner.shade(guard);
    }
}

impl<T> From<Shared<T>> for AtomicShared<T> {
    fn from(value: Shared<T>) -> Self {
        value.inner
    }
}

impl<'g, G, T> From<Local<'g, G, T>> for AtomicShared<T>
where
    G: Protector,
{
    fn from(value: Local<'g, G, T>) -> Self {
        value.as_atomic_shared()
    }
}

/// An immutable, root-count-protected reference to a managed object.
///
/// `Shared<T>` is the GC equivalent of `Arc<T>`: it keeps the target alive via
/// a root count in the object header and can be freely sent between threads or
/// stored as a field inside other managed objects.
///
/// - **Immutable** — the target pointer cannot be changed after construction.
///   Use [`AtomicShared`] or [`AtomicSharedOption`] for mutable atomic edges.
/// - **Cloneable** — cloning increments the root count.
/// - **Deref** — dereferences directly to `&T`.
/// - As a field of a `TraceObj` struct, it represents an internal heap edge
///   (root count is decremented when the parent is boxed via `unroot_outgoings`).
///
/// # Example
///
/// ```
/// use cdpt::{TraceObj, TracePtr, Shared};
///
/// #[derive(TraceObj)]
/// struct Node {
///     value: usize,
///     left: Option<Shared<Node>>,   // immutable edge to a child
///     right: Option<Shared<Node>>,
/// }
/// ```
pub struct Shared<T> {
    // Note: We just use `AtomicShared` to implement `Shared`, even though `Shared` is immutable
    // from the user's perspective. The reason is that `Shared` is also a target of
    // tracing and marking by collectors, so we must still use atomically mutable link.
    //
    // Therefore, we should still use atomic operations to access this pointer.
    // But some optimizations using relaxed operations might be possible because
    // the user is the only one who mutates the address.
    inner: AtomicShared<T>,
}

// Assert that the size of `Shared`is the same with the pointer size.
assert_eq_size!(Shared<()>, usize);

// The following is not guaranteed; and trying to guarantee it by exploiting
// Rust's memory layout optimization (e.g., using `UnsafeCell<NonNull>`) seems to be unsafe.
//
// assert_eq_size!(Option<Shared<()>>, usize);
//
// The main reason is that, although the address itself is immutable, the collector may atomically
// mutate the metadata bits during tracing, e.g., flipping `None` to `Some(garbage value)`.

unsafe impl<T: Sync + Send> Sync for Shared<T> {}
unsafe impl<T: Sync + Send> Send for Shared<T> {}

impl<T> Shared<T> {
    #[inline(always)]
    pub(crate) fn as_man_ptr(&self) -> ManPtr<T> {
        ManPtr::from(self.inner.inner.link.load(Ordering::Relaxed))
    }

    /// Returns `true` if the two `Shared`s point to the same allocation.
    #[inline(always)]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.as_man_ptr().without_meta() == other.as_man_ptr().without_meta()
    }

    /// Returns `true` if both options point to the same allocation (or are
    /// both `None`).
    #[inline(always)]
    pub fn opt_ptr_eq(this: Option<&Self>, other: Option<&Self>) -> bool {
        match (this, other) {
            (None, None) => true,
            (Some(l1), Some(l2)) => Self::ptr_eq(l1, l2),
            _ => false,
        }
    }
}

impl<T: TraceObj> Shared<T> {
    /// Allocates a new managed object and returns an immutable root reference.
    ///
    /// Requires a [`Guard`] for allocation. The returned `Shared` keeps the
    /// object alive independently of any guard.
    #[inline(always)]
    pub fn new(item: T, guard: &Guard) -> Self {
        Self {
            inner: AtomicShared::new(item, guard),
        }
    }

    pub(crate) fn try_inc_raw(ptr: ManPtr<T>, _: &Guard) -> Option<Self> {
        let obj = (unsafe { ptr.as_ref() })?;
        obj.header.increment_root_count(Ordering::Release);
        Some(Self {
            // As we are returning an owned `Shared`, it must have `PtrMeta::Rooted`.
            inner: AtomicShared::from_raw(ptr.with_meta(PtrMeta::Rooted)),
        })
    }

    /// Creates a guard-scoped [`Local`] view of this `Shared`.
    ///
    /// Useful for passing to methods that expect a `Local`, such as
    /// [`AtomicSharedOption::store`].
    #[inline(always)]
    pub fn as_local<'g>(&self, guard: &'g Guard) -> Local<'g, Guard, T> {
        // Safety: `Shared` (and its inner `AtomicShared`) always points to
        // a valid memory location by design.
        unsafe { Local::from_raw(self.as_man_ptr(), guard) }
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        // Note: If we have a valid reference to `Shared<T>`, then incrementing the root count
        // will not have to be protected by a phase-critical section (i.e., `Guard`).
        //
        // - If the previous RC was greater than 0, the increment has virtually no effect.
        // - Otherwise, if the RC was 0, it is guaranteed that
        //   (1) this `Shared<T>` is transitively reachable (via a path of `Local`s and `Shared`s)
        //       from a protected node (by HP or RC), or
        //   (2) this thread is within a phase-critical section (i.e., there's a valid `Guard`).
        let ptr = ManPtr::from(self.inner.inner.link.load(Ordering::Relaxed));
        debug_assert!(!ptr.is_null());
        let obj = unsafe { ptr.deref() };
        obj.header.increment_root_count(Ordering::Release);
        Self {
            // As we are returning an owned `Shared`, it must have `PtrMeta::Rooted`.
            inner: AtomicShared::from_raw(ptr.with_meta(PtrMeta::Rooted)),
        }
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // Safety: By design, `Shared` MUST always point to a valid memory location.
        unsafe {
            let r = self.as_man_ptr().as_ref().map(|obj| &obj.item);
            debug_assert!(r.is_some());
            r.unwrap_unchecked()
        }
    }
}

impl<T> AsRef<T> for Shared<T> {
    fn as_ref(&self) -> &T {
        self.deref()
    }
}

impl<T> Borrow<T> for Shared<T> {
    fn borrow(&self) -> &T {
        self.deref()
    }
}

impl<T: PartialEq> PartialEq for Shared<T> {
    fn eq(&self, other: &Self) -> bool {
        self.deref() == other.deref()
    }
}

impl<T: Eq> Eq for Shared<T> {}

impl<T: PartialOrd> PartialOrd for Shared<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.deref().partial_cmp(other.deref())
    }
}

impl<T: Ord> Ord for Shared<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deref().cmp(other.deref())
    }
}

impl<T: Hash> Hash for Shared<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.deref().hash(state);
    }
}

impl<T: TraceObj> TracePtr for Shared<T> {
    #[inline(always)]
    fn unroot(&self, guard: &Guard) {
        self.inner.unroot(guard);
    }

    #[inline(always)]
    fn shade(&self, guard: &Guard) {
        self.inner.shade(guard);
    }
}

pub trait Protector {
    type Shield;

    fn protect<T: TraceObj>(&self, ptr: ManPtr<T>) -> Self::Shield;
}

impl Protector for Handle {
    type Shield = HazardPointer;

    #[inline(always)]
    fn protect<T: TraceObj>(&self, ptr: ManPtr<T>) -> Self::Shield {
        let hp = HazardPointer::new(self.local.clone());
        hp.protect(ptr);
        hp
    }
}

impl Protector for Guard {
    type Shield = ();

    #[inline(always)]
    fn protect<T: TraceObj>(&self, _: ManPtr<T>) -> Self::Shield {}
}

/// A thread-local reference to a managed object, safe to dereference.
///
/// `Local` is the only pointer type that can be dereferenced (via `Deref`).
/// It is obtained by loading from an [`AtomicShared`] or [`AtomicSharedOption`],
/// or by allocating a new object with [`Local::new`].
///
/// # Protection modes
///
/// The generic parameter `G` determines how the reference is kept alive:
///
/// - **`Local<'g, Guard, T>`** — protected by a phase-critical section. This
///   is the common case: the reference is valid for the lifetime of the
///   [`Guard`] that created it. Cheap (zero-cost protection, `Copy`).
/// - **`Local<'h, Handle, T>`** — protected by a *hazard pointer*. Lives
///   independently of any `Guard`, useful for returning references from a
///   function. Create one via [`protect`](Local::protect).
///
/// # Example
///
/// ```ignore
/// let guard = cdpt::pin();
/// let local: Local<Guard, Node> = some_atomic.load(Ordering::Acquire, &guard);
/// println!("{}", local.value);  // deref to &Node
///
/// // To keep the reference after the guard is dropped:
/// let handle = cdpt::handle();
/// let hp_ref: Local<Handle, Node> = local.protect(&handle);
/// drop(guard);
/// println!("{}", hp_ref.value);  // still valid via hazard pointer
/// ```
pub struct Local<'g, G: Protector, T> {
    ptr: NonNull<ManObj<T>>,
    sh: G::Shield,
    _marker: PhantomData<(&'g (), ManPtr<T>)>,
}

// Assert that the sizes of `Local<Guard>` and `Option<Local<Guard>>`
// are the same with the pointer size.
assert_eq_size!(Local<'static, Guard, ()>, usize);
assert_eq_size!(Option<Local<'static, Guard, ()>>, usize);

unsafe impl<'g, G: Protector, T: Sync + Send> Sync for Local<'g, G, T> {}

impl<'g, T: TraceObj> Local<'g, Guard, T> {
    /// Allocates a new managed object and returns a guard-scoped local
    /// reference to it.
    ///
    /// The object is *unrooted* (no root count) — it is kept alive solely by
    /// the guard's critical section. To persist it in a data structure, store
    /// it into an [`AtomicSharedOption`] or convert it to a [`Shared`].
    #[inline(always)]
    pub fn new(item: T, guard: &'g Guard) -> Self {
        // `without_meta`: The object should have a right color, but the pointer should not,
        // because this is `Local` pointer.
        let ptr = ManPtr::alloc_unrooted(item, guard.alloc_color(), guard).without_meta();
        // Safety: `ptr` is freshly allocated.
        let (ptr, sh) = unsafe {
            ptr.deref().item.unroot_outgoings(guard);
            (NonNull::new_unchecked(ptr.as_ptr()), guard.protect(ptr))
        };
        Self {
            ptr,
            sh,
            _marker: PhantomData,
        }
    }
}

impl<'g, G: Protector, T> Local<'g, G, T> {
    #[inline(always)]
    pub(crate) fn as_man_ptr(&self) -> ManPtr<T> {
        debug_assert!(ManPtr::<T>::from(self.ptr) == ManPtr::<T>::from(self.ptr).without_meta());
        ManPtr::from(self.ptr)
    }

    /// Converts this reference into a sendable [`AtomicShared`] that can be
    /// stored in data structures or sent to other threads.
    ///
    /// Increments the target's root count so it stays alive independently of
    /// any guard or hazard pointer.
    #[inline(always)]
    pub fn as_atomic_shared(&self) -> AtomicShared<T> {
        let ptr = self.as_man_ptr();
        debug_assert!(!ptr.is_null());
        // Safety of `increment_root_count` without `Guard`:
        // See an explanation in `Shared::clone`.
        unsafe { ptr.deref() }
            .header
            .increment_root_count(Ordering::Release);
        // As we are returning an owned `Shared`, it must have `PtrMeta::Rooted`.
        AtomicShared::from_raw(ptr.with_meta(PtrMeta::Rooted))
    }

    /// Converts this reference into a [`Shared`] that can be stored as an
    /// immutable field in managed objects or kept indefinitely.
    #[inline(always)]
    pub fn as_shared(&self) -> Shared<T> {
        Shared {
            inner: self.as_atomic_shared(),
        }
    }

    /// Returns `true` if the two `Local`s point to the same allocation.
    /// This function ignores the underlying protection guards.
    #[inline(always)]
    pub fn ptr_eq<'h, H: Protector>(this: &Self, other: &Local<'h, H, T>) -> bool {
        this.ptr == other.ptr
    }

    /// Returns `true` if both options point to the same allocation (or are
    /// both `None`).
    #[inline(always)]
    pub fn opt_ptr_eq<'h, H: Protector>(
        this: Option<&Self>,
        other: Option<&Local<'h, H, T>>,
    ) -> bool {
        match (this, other) {
            (None, None) => true,
            (Some(l1), Some(l2)) => Self::ptr_eq(l1, l2),
            _ => false,
        }
    }
}

impl<'g, G: Protector, T: TraceObj> Local<'g, G, T> {
    /// # Safety
    ///
    /// `ptr` must point to a valid memory location.
    #[inline(always)]
    pub(crate) unsafe fn from_raw(ptr: ManPtr<T>, prot: &G) -> Self {
        debug_assert!(!ptr.is_null());
        Self {
            ptr: unsafe { NonNull::new_unchecked(ptr.as_ptr()) },
            sh: prot.protect(ptr),
            _marker: PhantomData,
        }
    }

    /// Re-protects this reference under a different protector.
    ///
    /// Commonly used to convert a `Local<Guard, T>` into a `Local<Handle, T>`,
    /// upgrading from guard-scoped to hazard-pointer protection so the
    /// reference survives after the guard is dropped.
    #[inline(always)]
    pub fn protect<'h, H: Protector>(&self, prot: &'h H) -> Local<'h, H, T> {
        Local {
            ptr: self.ptr,
            sh: prot.protect(self.as_man_ptr()),
            _marker: PhantomData,
        }
    }
}

impl<'g, G: Protector, T> Deref for Local<'g, G, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // Safety: By design, `Local` MUST always point to a valid memory location.
        unsafe {
            let r = self.as_man_ptr().as_ref().map(|obj| &obj.item);
            debug_assert!(r.is_some());
            r.unwrap_unchecked()
        }
    }
}

impl<'g, G: Protector, T> AsRef<T> for Local<'g, G, T> {
    fn as_ref(&self) -> &T {
        self.deref()
    }
}

impl<'g, G: Protector, T> Borrow<T> for Local<'g, G, T> {
    fn borrow(&self) -> &T {
        self.deref()
    }
}

impl<'g, G, T> PartialEq for Local<'g, G, T>
where
    G: Protector,
    T: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<'g, G, T> Eq for Local<'g, G, T>
where
    G: Protector,
    T: Eq,
{
}

impl<'g, G, T> std::fmt::Debug for Local<'g, G, T>
where
    G: Protector,
    T: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl<'g, G, T> PartialOrd for Local<'g, G, T>
where
    G: Protector,
    T: PartialOrd,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        (**self).partial_cmp(&**other)
    }
}

impl<'g, G, T> Ord for Local<'g, G, T>
where
    G: Protector,
    T: Ord,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (**self).cmp(&**other)
    }
}

impl<'g, G: Protector, T> Clone for Local<'g, G, T>
where
    G::Shield: Clone,
{
    #[inline(always)]
    fn clone(&self) -> Self {
        Self {
            ptr: self.ptr,
            sh: self.sh.clone(),
            _marker: PhantomData,
        }
    }
}

// `Local<'g, Guard, T>`, which is protected by a coarse-grained phase-critical section,
// can be cloned (copied) without additional costs because `Guard::Shield` is just `()`.
impl<'g, G: Protector, T> Copy for Local<'g, G, T> where G::Shield: Copy {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epoch::Color;

    // ---- ObjMeta tests ----

    #[test]
    fn obj_meta_new_and_accessors() {
        let meta = ObjMeta::new(Color::C0, 0);
        assert_eq!(meta.marked(), Color::C0);
        assert_eq!(meta.root_count(), 0);

        let meta = ObjMeta::new(Color::C1, 5);
        assert_eq!(meta.marked(), Color::C1);
        assert_eq!(meta.root_count(), 5);
    }

    #[test]
    fn obj_meta_with_marked() {
        let meta = ObjMeta::new(Color::C0, 10);
        let flipped = meta.with_marked(Color::C1);
        assert_eq!(flipped.marked(), Color::C1);
        assert_eq!(flipped.root_count(), 10);
    }

    #[test]
    fn obj_meta_default() {
        let meta = ObjMeta::default();
        assert_eq!(meta.marked(), Color::C0);
        assert_eq!(meta.root_count(), 0);
    }

    // ---- AtomicObjMeta tests ----

    #[test]
    fn atomic_obj_meta_load() {
        let ameta = AtomicObjMeta::new(Color::C1, 3);
        let loaded = ameta.load(Ordering::SeqCst);
        assert_eq!(loaded.marked(), Color::C1);
        assert_eq!(loaded.root_count(), 3);
    }

    #[test]
    fn atomic_obj_meta_increment_root_count() {
        let ameta = AtomicObjMeta::new(Color::C0, 1);
        let prev = ameta.increment_root_count(Ordering::SeqCst);
        assert_eq!(prev, 1);
        assert_eq!(ameta.load(Ordering::SeqCst).root_count(), 2);
    }

    #[test]
    fn atomic_obj_meta_decrement_root_count() {
        let ameta = AtomicObjMeta::new(Color::C0, 3);
        let prev = ameta.decrement_root_count(Ordering::SeqCst);
        assert_eq!(prev, 3);
        assert_eq!(ameta.load(Ordering::SeqCst).root_count(), 2);
    }

    #[test]
    fn atomic_obj_meta_increment_preserves_color() {
        let ameta = AtomicObjMeta::new(Color::C1, 5);
        ameta.increment_root_count(Ordering::SeqCst);
        let loaded = ameta.load(Ordering::SeqCst);
        assert_eq!(loaded.marked(), Color::C1);
        assert_eq!(loaded.root_count(), 6);
    }

    // ---- PtrMeta tests ----

    #[test]
    fn ptr_meta_equality() {
        assert_eq!(PtrMeta::Rooted, PtrMeta::Rooted);
        assert_eq!(PtrMeta::Unrooted(Color::C0), PtrMeta::Unrooted(Color::C0));
        assert_eq!(PtrMeta::Unrooted(Color::C1), PtrMeta::Unrooted(Color::C1));
        assert_ne!(PtrMeta::Rooted, PtrMeta::Unrooted(Color::C0));
        assert_ne!(PtrMeta::Unrooted(Color::C0), PtrMeta::Unrooted(Color::C1));
    }

    // ---- ManPtr tests ----

    #[test]
    fn man_ptr_null_base_is_null() {
        let ptr = ManPtr::<()>::null_base();
        assert!(ptr.is_null());
    }

    #[test]
    fn man_ptr_null_rooted() {
        let ptr = ManPtr::<()>::null_rooted();
        assert!(ptr.is_null());
        assert_eq!(ptr.meta(), PtrMeta::Rooted);
    }

    #[test]
    fn man_ptr_null_rooted_with_tag() {
        let ptr = ManPtr::<()>::null_rooted_with_tag(1);
        assert!(ptr.is_null());
        assert_eq!(ptr.meta(), PtrMeta::Rooted);
        assert_eq!(ptr.tag(), 1);
    }

    #[test]
    fn man_ptr_meta_roundtrip() {
        let base = ManPtr::<()>::null_base();

        let rooted = base.with_meta(PtrMeta::Rooted);
        assert_eq!(rooted.meta(), PtrMeta::Rooted);

        let c0 = base.with_meta(PtrMeta::Unrooted(Color::C0));
        assert_eq!(c0.meta(), PtrMeta::Unrooted(Color::C0));

        let c1 = base.with_meta(PtrMeta::Unrooted(Color::C1));
        assert_eq!(c1.meta(), PtrMeta::Unrooted(Color::C1));
    }

    #[test]
    fn man_ptr_without_meta() {
        let base = ManPtr::<()>::null_base();
        let rooted = base.with_meta(PtrMeta::Rooted);
        let cleared = rooted.without_meta();
        // After clearing meta bits (setting top 2 bits to 0), the encoding is
        // 0b00 which is `PtrMeta::Rooted`.
        assert_eq!(cleared.meta(), PtrMeta::Rooted);
    }

    #[test]
    fn man_ptr_tag_roundtrip() {
        let ptr = ManPtr::<()>::null_rooted();
        assert_eq!(ptr.tag(), 0);

        // Tag bits are limited to the low bits of the alignment.
        // For ManObj<()>, the alignment determines available tag bits.
        let tagged = ptr.with_tag(1);
        assert_eq!(tagged.tag(), 1);
    }

    #[test]
    fn man_ptr_tag_preserves_meta() {
        let ptr = ManPtr::<()>::null_rooted();
        let tagged = ptr.with_tag(1);
        assert_eq!(tagged.meta(), PtrMeta::Rooted);
    }

    #[test]
    fn man_ptr_equality() {
        let a = ManPtr::<()>::null_base();
        let b = ManPtr::<()>::null_base();
        assert!(a == b);

        // Rooted meta is 0b00, same as null_base(), so they are equal.
        let c = a.with_meta(PtrMeta::Rooted);
        assert!(a == c);

        // Unrooted meta is nonzero, so they differ.
        let d = a.with_meta(PtrMeta::Unrooted(Color::C0));
        assert!(a != d);
    }

    #[test]
    fn man_ptr_clone_copy() {
        let ptr = ManPtr::<()>::null_rooted();
        let cloned = ptr.clone();
        assert!(ptr == cloned);

        let copied = ptr;
        assert!(ptr == copied);
    }
}
