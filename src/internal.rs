use crate::collector::{Collector, LocalHandle};
use crate::epoch::{AtomicEpoch, Epoch};
use crate::guard::Guard;
use crate::sync::{Entry, IsElement, List, fence};
use crossbeam::epoch::{Guard as EbrGuard, Owned, Shared as EbrShared, unprotected};
use crossbeam::utils::CachePadded;
use std::cell::{Cell, UnsafeCell};
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::Ordering;

/// The global data for a garbage collector.
pub(crate) struct Global {
    /// The intrusive linked list of `Local`s.
    locals: List<Local>,

    /// The global epoch.
    pub(crate) epoch: CachePadded<AtomicEpoch>,
}

impl Global {
    /// Creates a new global data for garbage collection.
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            locals: List::new(),
            epoch: CachePadded::new(AtomicEpoch::new(Epoch::starting())),
        }
    }
}

/// Participant for garbage collection.
#[repr(C)] // Note: `entry` must be the first field
pub(crate) struct Local {
    /// A node in the intrusive linked list of `Local`s.
    entry: Entry,

    /// A reference to the global data.
    ///
    /// When all guards and handles get dropped, this reference is destroyed.
    collector: UnsafeCell<ManuallyDrop<Collector>>,

    /// The number of guards keeping this participant pinned.
    guard_count: Cell<usize>,

    /// The number of active handles.
    handle_count: Cell<usize>,

    /// The local epoch.
    epoch: CachePadded<AtomicEpoch>,
}

impl Local {
    /// Registers a new `Local` in the provided `Global`.
    pub(crate) fn register(collector: &Collector) -> LocalHandle {
        unsafe {
            // Since we dereference no pointers in this block, it is safe to use `unprotected`.

            let local = Owned::new(Self {
                entry: Entry::default(),
                collector: UnsafeCell::new(ManuallyDrop::new(collector.clone())),
                guard_count: Cell::new(0),
                handle_count: Cell::new(1),
                epoch: CachePadded::new(AtomicEpoch::new(Epoch::starting())),
            })
            .into_shared(unprotected());
            collector.global.locals.insert(local, unprotected());
            LocalHandle {
                local: local.as_raw(),
            }
        }
    }

    /// Returns a reference to the `Global` in which this `Local` resides.
    #[inline]
    pub(crate) fn global(&self) -> &Global {
        &self.collector().global
    }

    /// Returns a reference to the `Collector` in which this `Local` resides.
    #[inline]
    pub(crate) fn collector(&self) -> &Collector {
        unsafe { &**self.collector.get() }
    }

    /// Returns `true` if the current participant is pinned.
    #[inline]
    pub(crate) fn is_pinned(&self) -> bool {
        self.guard_count.get() > 0
    }

    /// Pins the `Local`.
    #[inline]
    pub(crate) fn pin(&self) -> Guard {
        let guard = Guard { local: self };

        let guard_count = self.guard_count.get();
        self.guard_count.set(guard_count.checked_add(1).unwrap());

        if guard_count == 0 {
            let mut curr_epoch = self.global().epoch.load(Ordering::Acquire);
            loop {
                // Now we must store `new_epoch` into `self.epoch` and execute a light fence.
                self.epoch.store(curr_epoch.pinned(), Ordering::Release);
                fence::light();

                let new_epoch = self.global().epoch.load(Ordering::Acquire);
                if curr_epoch != new_epoch {
                    curr_epoch = new_epoch;
                    continue;
                }
                break;
            }
        }

        guard
    }

    /// Unpins the `Local`.
    #[inline]
    pub(crate) fn unpin(&self) {
        let guard_count = self.guard_count.get();
        self.guard_count.set(guard_count - 1);

        if guard_count == 1 {
            self.epoch.store(Epoch::starting(), Ordering::Release);

            if self.handle_count.get() == 0 {
                self.finalize();
            }
        }
    }

    /// Unpins and then pins the `Local`.
    #[inline]
    pub(crate) fn repin(&self) {
        let guard_count = self.guard_count.get();

        // Update the local epoch only if there's only one guard.
        if guard_count == 1 {
            let epoch = self.epoch.load(Ordering::Relaxed);
            let global_epoch = self.global().epoch.load(Ordering::Relaxed).pinned();

            // Update the local epoch only if the global epoch is greater than the local epoch.
            if epoch != global_epoch {
                // We store the new epoch with `Release` because we need to ensure any memory
                // accesses from the previous epoch do not leak into the new one.
                self.epoch.store(global_epoch, Ordering::Release);

                // However, we don't need a following `SeqCst` fence, because it is safe for memory
                // accesses from the new epoch to be executed before updating the local epoch. At
                // worse, other threads will see the new epoch late and delay GC slightly.
            }
        }
    }

    /// Increments the handle count.
    #[inline]
    pub(crate) fn acquire_handle(&self) {
        let handle_count = self.handle_count.get();
        debug_assert!(handle_count >= 1);
        self.handle_count.set(handle_count + 1);
    }

    /// Decrements the handle count.
    #[inline]
    pub(crate) fn release_handle(&self) {
        let guard_count = self.guard_count.get();
        let handle_count = self.handle_count.get();
        debug_assert!(handle_count >= 1);
        self.handle_count.set(handle_count - 1);

        if guard_count == 0 && handle_count == 1 {
            self.finalize();
        }
    }

    /// Removes the `Local` from the global linked list.
    #[cold]
    fn finalize(&self) {
        debug_assert_eq!(self.guard_count.get(), 0);
        debug_assert_eq!(self.handle_count.get(), 0);

        unsafe {
            // Take the reference to the `Global` out of this `Local`. Since we're not protected
            // by a guard at this time, it's crucial that the reference is read before marking the
            // `Local` as deleted.
            let collector: Collector = ptr::read(&*(*self.collector.get()));

            // Mark this node in the linked list as deleted.
            self.entry.delete(unprotected());

            // Finally, drop the reference to the global. Note that this might be the last reference
            // to the `Global`. If so, the global data will be destroyed and all deferred functions
            // in its queue will be executed.
            drop(collector);
        }
    }
}

impl IsElement<Self> for Local {
    fn entry_of(local: &Self) -> &Entry {
        // SAFETY: `Local` is `repr(C)` and `entry` is the first field of it.
        unsafe {
            let entry_ptr = (local as *const Self).cast::<Entry>();
            &*entry_ptr
        }
    }

    unsafe fn element_of(entry: &Entry) -> &Self {
        // SAFETY: `Local` is `repr(C)` and `entry` is the first field of it.
        unsafe {
            let local_ptr = (entry as *const Entry).cast::<Self>();
            &*local_ptr
        }
    }

    unsafe fn finalize(entry: &Entry, guard: &EbrGuard) {
        unsafe { guard.defer_destroy(EbrShared::from(Self::element_of(entry) as *const _)) }
    }
}
