use std::sync::OnceLock;

use crate::guards::{Guard, Handle};
use crate::internal::{Global, Local};

pub fn global() -> &'static Global {
    /// The global data for the default garbage collector.
    static GLOBAL: OnceLock<Global> = OnceLock::new();
    GLOBAL.get_or_init(Global::new)
}

thread_local! {
    /// The per-thread participant for the default garbage collector.
    static HANDLE: Handle = Local::register();
}

/// Returns a thread-local [`Handle`] for the garbage collector.
///
/// Each thread gets its own `Handle` automatically. Use it to [`pin()`](Handle::pin)
/// into a phase-critical section or to [`protect`](crate::Local::protect) a local
/// pointer beyond the lifetime of a [`Guard`].
///
/// # Example
///
/// ```ignore
/// let handle = cdpt::handle();
/// let guard = handle.pin();
/// // ... access managed pointers within this critical section ...
/// ```
pub fn handle() -> Handle {
    HANDLE.with(|primary| primary.clone())
}

/// Pins the current thread and returns a [`Guard`].
///
/// This is a shorthand for `handle().pin()`. Within the returned guard's
/// lifetime (a *phase-critical section*), all reachable managed pointers are
/// safe to dereference.
pub fn pin() -> Guard {
    HANDLE.with(|primary| primary.pin())
}
