use std::sync::OnceLock;

use crate::guards::{Guard, Handle};
use crate::internal::{Global, Local};

/// Returns the singleton [`Global`] instance for the garbage collector.
///
/// Use it to configure the collector or read heap statistics. Most programs
/// never need it, since collection runs automatically.
///
/// # Examples
///
/// ```
/// let g = cdpt::global();
/// println!("heap usage: {} bytes", g.estimate_heap_usage());
/// g.enable_collection(false); // pause collection
/// g.enable_collection(true);  // resume
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
/// Each thread gets its own `Handle` automatically. Use it to call
/// [`pin()`](Handle::pin) for access to the managed heap, or pass it to
/// [`Local::protect`](crate::Local::protect) to keep a reference alive after
/// its [`Guard`] is dropped.
///
/// # Examples
///
/// ```
/// let handle = cdpt::handle();
/// let guard = handle.pin();
/// // ... allocate, load, and dereference managed pointers ...
/// drop(guard);
/// ```
pub fn handle() -> Handle {
    HANDLE.with(|primary| primary.clone())
}

/// Shorthand for [`handle().pin()`](Handle::pin).
///
/// Returns a [`Guard`] that lets you allocate, load, and dereference managed
/// pointers. Drop the guard when you are done to let the collector make
/// progress.
///
/// # Examples
///
/// ```
/// let guard = cdpt::pin();
/// // ... access the managed heap ...
/// drop(guard);
/// ```
pub fn pin() -> Guard {
    HANDLE.with(|primary| primary.pin())
}
