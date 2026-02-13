//! # CDPT — Concurrent Deferred Partial Tracing
//!
//! A safe, efficient, and easy-to-integrate concurrent garbage collector for Rust.
//!
//! ## Quick start
//!
//! ```
//! use cdpt::*;
//! use std::sync::atomic::Ordering;
//!
//! // 1. Define your data type and derive `TraceObj`.
//! #[derive(TraceObj)]
//! struct Node {
//!     value: usize,
//!     next: AtomicSharedOption<Node>,
//! }
//!
//! // 2. Acquire a handle (one per thread) and pin to access the GC-managed heap.
//! let handle = handle();
//! let guard = handle.pin();
//!
//! // 3. Allocate and traverse managed objects within the critical section.
//! let node = Local::new(Node { value: 42, next: AtomicSharedOption::none() }, &guard);
//! assert_eq!(node.value, 42);  // Local<T> implements Deref
//!
//! // 4. Promote to a hazard-pointer-protected reference that outlives the guard.
//! let hp_ref: Local<Handle, Node> = node.protect(&handle);
//!
//! // 5. Drop the guard — the collector is free to make progress, but
//! //    hp_ref (and any Shared<T>) remain valid and dereferenceable.
//! drop(guard);
//! assert_eq!(hp_ref.value, 42);  // still safe
//! ```
//!
//! ## Pointer types at a glance
//!
//! | Type | Nullable | Protection | Deref | Traits |
//! |------|----------|------------|-------|--------|
//! | [`Local<Guard, T>`] | No | Phase-critical section | Yes | `Sync + !Send` |
//! | [`Local<Handle, T>`] | No | Hazard pointer | Yes | `Sync + !Send` |
//! | [`Shared<T>`] | No | Root count | Yes | `Sync + Send` |
//! | [`AtomicShared<T>`] | No | Root count | No (load first) | `Sync + Send` |
//! | [`AtomicSharedOption<T>`] | Yes | Root count | No (load first) | `Sync + Send` |
//!
//! **Keeping critical sections short matters.** While a [`Guard`] is alive, the
//! collector cannot advance the epoch past the pinned thread. Prefer promoting
//! pointers you need long-term to [`Local<Handle, T>`] (via [`Local::protect`])
//! or [`Shared<T>`] (via [`Local::as_shared`]), then dropping the guard.
//! This lets the collector reclaim garbage concurrently while your code continues
//! to safely use the promoted references.

#![feature(cold_path)]
#![feature(likely_unlikely)]
#![feature(vec_push_within_capacity)]

#[macro_use]
extern crate static_assertions;

mod collector;
mod epoch;
mod guards;
mod internal;
mod pointers;
mod sync;
mod task;
mod tls;

pub use cdpt_derive::TraceObj;
pub use guards::{Guard, Handle};
pub use pointers::{AtomicShared, AtomicSharedOption, Local, Shared, TraceObj, TracePtr};
pub use tls::*;

#[doc(hidden)]
pub mod export {
    pub use std::result::Result;
}
