use crate::{collector::Collector, internal::Local};

pub struct Guard {
    pub(crate) local: *const Local,
}

impl Guard {
    /// Unpins and then immediately re-pins the thread.
    ///
    /// This method is useful when you don't want delay the advancement of the global epoch by
    /// holding an old epoch. For safety, you should not maintain any guard-based reference across
    /// the call (the latter is enforced by `&mut self`). The thread will only be repinned if this
    /// is the only active guard for the current thread.
    ///
    /// If this method is called from an [`unprotected`] guard, then the call will be just no-op.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_epoch::{self as epoch, Atomic};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// let a = Atomic::new(777);
    /// let mut guard = epoch::pin();
    /// {
    ///     let p = a.load(SeqCst, &guard);
    ///     assert_eq!(unsafe { p.as_ref() }, Some(&777));
    /// }
    /// guard.repin();
    /// {
    ///     let p = a.load(SeqCst, &guard);
    ///     assert_eq!(unsafe { p.as_ref() }, Some(&777));
    /// }
    /// # unsafe { drop(a.into_owned()); } // avoid leak
    /// ```
    pub fn repin(&mut self) {
        if let Some(local) = unsafe { self.local.as_ref() } {
            local.repin();
        }
    }

    /// Returns the `Collector` associated with this guard.
    ///
    /// This method is useful when you need to ensure that all guards used with
    /// a data structure come from the same collector.
    ///
    /// If this method is called from an [`unprotected`] guard, then `None` is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_epoch as epoch;
    ///
    /// let guard1 = epoch::pin();
    /// let guard2 = epoch::pin();
    /// assert!(guard1.collector() == guard2.collector());
    /// ```
    pub fn collector(&self) -> Option<&Collector> {
        unsafe { self.local.as_ref().map(|local| local.collector()) }
    }
}

impl Drop for Guard {
    #[inline]
    fn drop(&mut self) {
        if let Some(local) = unsafe { self.local.as_ref() } {
            local.unpin();
        }
    }
}
