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

/// Acquire a local handle for the garbage collector.
pub fn handle() -> Handle {
    HANDLE.with(|primary| primary.clone())
}

pub fn pin() -> Guard {
    HANDLE.with(|primary| primary.pin())
}
