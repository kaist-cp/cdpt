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
//! // 2. Pin the current thread to access the managed heap.
//! let handle = handle();
//! let guard = handle.pin();
//!
//! // 3. Allocate and dereference managed objects while the guard is alive.
//! let node = Local::new(Node { value: 42, next: AtomicSharedOption::none() }, &guard);
//! assert_eq!(node.value, 42);  // Local<T> implements Deref
//!
//! // 4. Promote to a long-lived reference that outlives the guard.
//! let hp_ref: Local<Handle, Node> = node.protect(&handle);
//!
//! // 5. Drop the guard — the collector can now make progress,
//! //    but hp_ref (and any Shared<T>) remain valid.
//! drop(guard);
//! assert_eq!(hp_ref.value, 42);  // still safe
//! ```
//!
//! ## Pointer types at a glance
//!
//! | Type | Nullable | Lifetime | Deref | Send |
//! |------|----------|----------|-------|------|
//! | [`Local<Guard, T>`] | No | Scoped to [`Guard`] | Yes | No |
//! | [`Local<Handle, T>`] | No | Until dropped | Yes | No |
//! | [`Shared<T>`] | No | Until dropped | Yes | Yes |
//! | [`AtomicShared<T>`] | No | Until dropped | No (load first) | Yes |
//! | [`AtomicSharedOption<T>`] | Yes | Until dropped | No (load first) | Yes |
//!
//! ## Keep guards short-lived
//!
//! A long-lived [`Guard`] prevents the collector from reclaiming garbage.
//! Drop it as soon as you no longer need to load new pointers. If you need a
//! reference that outlives the guard, promote it to [`Local<Handle, T>`] (via
//! [`Local::protect`]) or [`Shared<T>`] (via [`Local::as_shared`]), then drop
//! the guard. The collector will work concurrently while your code continues to
//! safely use the promoted references.

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
pub use internal::Global;
pub use pointers::{AtomicShared, AtomicSharedOption, Local, Shared, TraceObj, TracePtr};
pub use tls::*;

#[doc(hidden)]
pub mod export {
    pub use std::result::Result;
}
