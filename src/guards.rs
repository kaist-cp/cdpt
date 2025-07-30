//! RAII-style guards for users.

use crate::{
    TraceObj,
    epoch::{Color, Phase},
    internal::{Global, Local},
    pointers::ManObj,
    sync::fence,
};
use std::{
    ptr::NonNull,
    sync::{Arc, atomic::Ordering},
};

/// A global garbage collector.
pub struct Collector {
    pub(crate) global: Arc<Global>,
}

unsafe impl Send for Collector {}
unsafe impl Sync for Collector {}

impl Default for Collector {
    #[allow(clippy::arc_with_non_send_sync)] // https://github.com/rust-lang/rust-clippy/issues/11382
    fn default() -> Self {
        Self {
            global: Arc::new(Global::new()),
        }
    }
}

impl Collector {
    /// Creates a new collector.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Registers a new handle for the collector.
    pub(crate) fn register(&self) -> Handle {
        Local::register(self)
    }
}

impl Clone for Collector {
    /// Creates another reference to the same garbage collector.
    fn clone(&self) -> Self {
        Self {
            global: self.global.clone(),
        }
    }
}

impl PartialEq for Collector {
    /// Checks if both handles point to the same collector.
    fn eq(&self, rhs: &Self) -> bool {
        Arc::ptr_eq(&self.global, &rhs.global)
    }
}
impl Eq for Collector {}

/// A handle to a garbage collector.
pub struct Handle {
    pub(crate) local: NonNull<Local>,
}

impl Handle {
    /// Pins the handle.
    #[inline]
    pub fn pin(&self) -> Guard {
        unsafe { self.local.as_ref().pin() }
    }

    /// Returns `true` if the handle is pinned.
    #[inline]
    pub fn is_pinned(&self) -> bool {
        unsafe { self.local.as_ref().is_pinned() }
    }

    /// Returns the `Collector` associated with this handle.
    #[inline]
    pub fn collector(&self) -> &Collector {
        unsafe { self.local.as_ref().collector() }
    }
}

impl Clone for Handle {
    fn clone(&self) -> Self {
        unsafe { Local::acquire_handle(self.local.as_ref()) };
        Self { local: self.local }
    }
}

impl Drop for Handle {
    #[inline]
    fn drop(&mut self) {
        unsafe { Local::release_handle(self.local.as_ref()) };
    }
}

pub struct Guard {
    pub(crate) local: NonNull<Local>,
}

impl Guard {
    /// Unpins and then immediately re-pins the thread.
    ///
    /// This method is useful when you don't want delay the advancement of the global epoch by
    /// holding an old epoch. For safety, you should not maintain any guard-based reference across
    /// the call (the latter is enforced by `&mut self`). The thread will only be repinned if this
    /// is the only active guard for the current thread.
    pub fn repin(&mut self) {
        unsafe { self.local.as_ref() }.repin();
    }

    /// Returns the `Collector` associated with this guard.
    ///
    /// This method is useful when you need to ensure that all guards used with
    /// a data structure come from the same collector.
    pub fn collector(&self) -> &Collector {
        unsafe { self.local.as_ref().collector() }
    }

    pub(crate) fn phase(&self) -> Phase {
        unsafe { self.local.as_ref() }.pinned_epoch().phase()
    }

    pub(crate) fn white_color(&self) -> Color {
        unsafe { self.local.as_ref() }.pinned_epoch().color()
    }

    pub(crate) fn black_color(&self) -> Color {
        self.white_color().flip()
    }

    pub(crate) fn alloc_color(&self) -> Color {
        // TODO: We may want to relax this (e.g., white for `Local` even during tracing).
        match self.phase() {
            Phase::N => self.white_color(),
            _ => self.black_color(),
        }
    }

    pub(crate) fn global_phase(&self) -> Phase {
        fence::light();
        unsafe { self.local.as_ref() }
            .global()
            .epoch
            .load(Ordering::Relaxed)
            .phase()
    }

    pub(crate) fn alloc<T: 'static + TraceObj>(&self, obj: ManObj<T>) -> *mut ManObj<T> {
        unsafe { self.local.as_ref().alloc(obj) }
    }

    pub(crate) fn schedule_mark<T: 'static + TraceObj>(&self, obj: &ManObj<T>) {
        unsafe { self.local.as_ref().schedule_mark(obj) };
    }
}

impl Drop for Guard {
    #[inline]
    fn drop(&mut self) {
        unsafe { self.local.as_ref() }.unpin_inner();
    }
}
