//! # CDPT: Concurrent Deferred Partial Tracing
//!
//! An automatic, safe, and concurrent garbage collector for building lock-free
//! and concurrent data structures in Rust, with **no `unsafe`**, **no
//! stop-the-world pauses**, and **automatic reclamation of cyclic garbage**.
//!
//! CDPT is a *partial-tracing* collector: it reference-counts only the roots
//! and traces the rest of the heap. This makes it safe (the roots are precise,
//! and a type-safe API rules out use-after-free and leaks at compile time) and
//! easy to use (tracing reclaims cycles for you). A highly concurrent design
//! keeps it efficient: tracing runs alongside your threads without ever
//! suspending them, and deferred reference counting amortizes the root-count
//! updates, so traversals touch no counts at all. For the algorithm and its
//! correctness, see the [paper](https://doi.org/10.1145/3808310).
//!
//! ## Quick start
//!
//! ```
//! use cdpt::*;
//!
//! #[derive(TraceObj)]
//! struct Node {
//!     value: usize,
//!     next: AtomicSharedOption<Node>,
//! }
//!
//! // Pin the current thread to access the managed heap.
//! let handle = handle();
//! let guard = handle.pin();
//!
//! // Allocate and dereference while the guard is alive.
//! let node = Local::new(Node { value: 42, next: AtomicSharedOption::none() }, &guard);
//! assert_eq!(node.value, 42); // `Local` dereferences to `&T`
//!
//! // Promote to references that outlive the guard.
//! let kept = node.protect(&handle); // `Local<Handle, Node>`: cheap, this thread only
//! let shared = node.as_shared();    // `Shared<Node>`: root-counted, like `Arc`
//!
//! drop(guard); // the collector can now make progress
//!
//! assert_eq!(kept.value, 42);  // still valid
//! let alias = shared.clone();  // `Shared` is `Clone` + `Send + Sync`
//! assert_eq!(alias.value, 42); // still valid
//! ```
//!
//! ## Core concepts
//!
//! **The managed heap.** CDPT manages a dedicated heap of *managed objects*:
//! values you allocate through CDPT's pointer types instead of `Box` or `Arc`.
//! Only objects you allocate this way are collected; the rest of your program
//! is left untouched.
//!
//! **Guards.** To touch the managed heap (allocate a managed object or load an
//! atomic pointer), a thread must hold a [`Guard`], obtained with [`pin()`] or
//! [`Handle::pin`]. A guard is **not a lock**: any number of threads may hold
//! their own guards at once. It simply marks the thread as actively using the
//! heap, so the collector knows not to reclaim anything the thread might still
//! reach. While a guard is alive, every pointer you load is safe to
//! dereference.
//!
//! **Keep guards short-lived.** A live guard holds the collector back. Pin, do
//! your work, and drop the guard promptly, ideally one short critical section
//! per operation. Avoid holding a guard across blocking calls or long loops; if
//! you must, refresh it periodically with [`Guard::repin`].
//!
//! **Promotion.** A [`Local`] loaded under a guard is valid only until that
//! guard drops. To keep a reference past the guard, *promote* it: to a
//! [`Local<Handle, T>`](Local) via [`Local::protect`] (cheap, hazard-pointer
//! protection) or to a [`Shared<T>`](Shared) via [`Local::as_shared`] (sendable
//! across threads). You can then drop the guard and keep using the reference.
//!
//! **What keeps an object alive.** An object survives as long as it is
//! *reachable*: held by a live [`Shared`] or a promoted [`Local`], or pointed
//! to (transitively) from another live object. Once the last such reference is
//! gone, the collector reclaims it, including whole cycles of objects that
//! only reference each other.
//!
//! ## The pointer types
//!
//! Four types cover every role:
//!
//! | Type | Nullable | Lifetime | Deref | `Send` |
//! |------|----------|----------|-------|--------|
//! | [`Local<Guard, T>`](Local) | No | Scoped to its [`Guard`] | Yes | No |
//! | [`Local<Handle, T>`](Local) | No | Until dropped | Yes | No |
//! | [`Shared<T>`](Shared) | No | Until dropped | Yes | Yes |
//! | [`AtomicShared<T>`](AtomicShared) | No | Until dropped | No (load first) | Yes |
//! | [`AtomicSharedOption<T>`](AtomicSharedOption) | Yes | Until dropped | No (load first) | Yes |
//!
//! - **[`Local`]** is the uncounted reference you hold while traversing the heap;
//!   it dereferences to `&T`. It comes in two flavors: `Local<Guard, T>`
//!   (zero-cost, `Copy`, valid for the guard's scope) and `Local<Handle, T>`
//!   (hazard-pointer protected, outlives the guard).
//! - **[`Shared`]** is an immutable, sendable, long-lived reference: the GC's
//!   `Arc<T>`. Clone it to share; store it as a struct field for an immutable
//!   heap edge.
//! - **[`AtomicShared`] / [`AtomicSharedOption`]** are the *mutable* edges
//!   inside your data structure (non-nullable / nullable). Their `load`,
//!   `store`, `swap`, and `compare_exchange` apply the GC's write barriers for
//!   you.
//!
//! You move between them along a few simple paths:
//!
//! - `AtomicShared*::load(.., &guard)` → `Local<Guard, T>`
//! - [`Local::protect`]`(&handle)` → `Local<Handle, T>` (outlives the guard)
//! - [`Local::as_shared`]`()` → [`Shared<T>`](Shared) (sendable, root-counted)
//! - [`Shared::as_local`]`(&guard)` → `Local<Guard, T>`
//!
//! ## Building data structures
//!
//! 1. **Derive [`TraceObj`](trait@TraceObj)** on every managed type. The
//!    `#[derive(TraceObj)]` macro finds the managed-pointer fields (including
//!    those nested in `Option`, `Vec`, `Box`, tuples, …) and generates the
//!    tracing code automatically.
//! 2. **Use `Atomic*` for edges that mutate**, and [`Shared`] for immutable
//!    ones. The example below links nodes immutably with [`Shared`]:
//!
//! ```
//! use cdpt::*;
//!
//! #[derive(TraceObj)]
//! struct Tree {
//!     value: i32,
//!     children: Vec<Shared<Tree>>,
//! }
//!
//! let guard = pin();
//! let leaf = Shared::new(Tree { value: 1, children: vec![] }, &guard);
//! let root = Shared::new(Tree { value: 2, children: vec![leaf.clone()] }, &guard);
//! drop(guard);
//!
//! // `root` keeps itself alive; `leaf` stays alive as `root`'s child.
//! assert_eq!(root.value, 2);
//! assert_eq!(root.children[0].value, 1);
//! ```
//!
//! 3. **Read and update edges with the usual atomic operations.** `load`,
//!    `store`, `swap`, and `compare_exchange` mirror `AtomicPtr` and crossbeam's
//!    `Atomic`, so the lock-free traversal and update patterns you already know
//!    carry over unchanged. Two things are specific to CDPT:
//!    - Every operation takes a `&guard`, and `load` hands back a [`Local`]
//!      *without* touching a reference count, so traversal stays cheap.
//!    - A loaded [`Local`] is valid only while the guard lives. To use it after
//!      the guard drops (return it, store it, or send it to another thread),
//!      promote it with [`Local::protect`] or [`Local::as_shared`].
//!
//! **Things to keep in mind:**
//!
//! - Keep guards short-lived (see [Core concepts](#core-concepts)).
//! - Managed types are `Send + Sync` (required by [`TraceObj`](trait@TraceObj)).
//!
//! See the `examples/` directory for complete lock-free structures: a Treiber
//! stack, a Harris linked list, and the Natarajan–Mittal and Ellen et al.
//! (EFRB) trees.
//!
//! ## Configuration and tuning
//!
//! Collection runs automatically, so most programs need nothing here. For
//! control, [`global()`] returns the [`Global`] singleton:
//!
//! - [`Global::enable_collection`]: pause or resume collection.
//! - [`Global::request_collection`]: force a cycle now (useful in tests).
//! - [`Global::set_heap_headroom`] / [`HeapHeadroom`]: trade peak memory
//!   against collector CPU.
//! - [`Global::set_collector_threads`]: set the collector's parallelism.
//! - [`Global::estimate_heap_usage`] and friends: coarse heap statistics.
//!
//! Cargo features:
//!
//! - `tag`: low-bit pointer tagging (the `*_with_tag` and `fetch_tag_*` APIs),
//!   for lock-free algorithms that mark pointers.
//!
//! ## Limitations and requirements
//!
//! - **No finalizers.** When an object is reclaimed its fields are dropped
//!   normally, but you cannot run custom `Drop` logic on a managed type. The
//!   derive supplies the destructor, and reclamation timing is not observable.
//! - **No unboxing.** Once a managed object becomes a heap edge of another
//!   object, it cannot be moved back onto the stack. The API enforces this:
//!   [`Local`] and [`Shared`] dereference to `&T` only.
//! - **`Send + Sync` required.** Managed objects are shared with other threads
//!   and the collector, so any interior-mutable state must use a thread-safe
//!   primitive (`Mutex`, atomics, …).
//! - **Memory overhead.** Tracing keeps garbage until the next cycle confirms
//!   it dead, and every object carries a small header. Expect a higher
//!   footprint than hand-tuned manual reclamation, in exchange for safety and
//!   automatic cycle collection.
//!
//! ## Further reading
//!
//! - The [paper](https://doi.org/10.1145/3808310): design, correctness, and
//!   evaluation against `Arc`, BDWGC, CIRC, and manual schemes.
//! - `README.md`: project overview and a feature comparison.
//! - `BENCH.md`: running the benchmark suite.
//! - `examples/`: complete lock-free data structures.

#[macro_use]
extern crate static_assertions;

mod collector;
mod epoch;
mod guards;
mod internal;
mod platform;
mod pointers;
mod sync;
mod task;
mod tls;

pub use cdpt_derive::TraceObj;
pub use guards::{Guard, Handle};
pub use internal::{Global, HeapHeadroom};
pub use pointers::{
    AtomicShared, AtomicSharedOption, Local, ManPtr, Protector, Shared, TraceObj, TracePtr,
};
pub use tls::*;

#[doc(hidden)]
pub mod export {
    pub use std::result::Result;
}
