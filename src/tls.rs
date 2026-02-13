use std::sync::OnceLock;

use crate::guards::{Guard, Handle};
use crate::internal::{Global, Local};

/// Returns the singleton [`Global`] instance for the garbage collector.
///
/// Use this to configure the collector or query heap statistics:
///
/// ```ignore
/// let g = cdpt::global();
/// println!("heap usage: {} bytes", g.estimate_heap_usage());
/// g.enable_collection(false);  // pause GC
/// ```
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
/// Each thread gets its own `Handle` automatically. Use it to:
/// - Call [`pin()`](Handle::pin) to access the managed heap.
/// - Pass to [`Local::protect`](crate::Local::protect) to keep a reference
///   alive after its [`Guard`] is dropped.
///
/// # Example
///
/// ```ignore
/// let handle = cdpt::handle();
/// let guard = handle.pin();
/// // ... allocate, load, and dereference managed pointers ...
/// ```
pub fn handle() -> Handle {
    HANDLE.with(|primary| primary.clone())
}

/// Shorthand for [`handle().pin()`](Handle::pin).
///
/// Returns a [`Guard`] that lets you allocate, load, and dereference managed
/// pointers. Drop the guard when you are done to let the collector make
/// progress.
pub fn pin() -> Guard {
    HANDLE.with(|primary| primary.pin())
}
