//! RAII-style guards for users.

use crate::{
    TraceObj,
    epoch::{Color, Phase},
    internal::Local,
    pointers::ManObj,
    sync::fence,
    tls::global,
};
use std::ptr::NonNull;

/// A handle to a garbage collector.
pub struct Handle {
    pub(crate) local: NonNull<Local>,
}

impl Handle {
    #[inline]
    pub(crate) fn local(&self) -> &Local {
        unsafe { self.local.as_ref() }
    }

    /// Pins the handle.
    #[inline]
    pub fn pin(&self) -> Guard {
        self.local().pin()
    }

    /// Returns `true` if the handle is pinned.
    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.local().is_pinned()
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

    pub(crate) fn phase(&self) -> Phase {
        unsafe { self.local.as_ref().pinned_epoch() }.phase()
    }

    pub(crate) fn white_color(&self) -> Color {
        unsafe { self.local.as_ref().pinned_epoch() }.color()
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
        global().load_epoch().phase()
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
