use std::sync::OnceLock;

use crate::guards::{Collector, Handle};

fn collector() -> &'static Collector {
    /// The global data for the default garbage collector.
    static COLLECTOR: OnceLock<Collector> = OnceLock::new();
    COLLECTOR.get_or_init(Collector::new)
}

thread_local! {
    /// The per-thread participant for the default garbage collector.
    static HANDLE: Handle = collector().register();
}

/// Acquire a local handle for the garbage collector.
pub fn handle() -> Handle {
    HANDLE.with(|primary| primary.clone())
}
