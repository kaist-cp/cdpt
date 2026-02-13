//! RAII-style guards for users.

use crate::{
    TraceObj,
    epoch::{Color, Epoch, Phase},
    internal::{Local, OBJ_BATCH_SIZE},
    pointers::ManObj,
    sync::Entry,
    task::Task,
    tls::global,
};
use crossbeam::epoch::pin as ebr_pin;
use std::{
    cell::{Cell, LazyCell},
    rc::Rc,
    sync::atomic::{Ordering, fence},
};

const HELP_NORMAL_MAX_TRIAL: usize = 2;
const HELP_TRACING_MAX_TRIAL: usize = 4;

/// A thread-local handle to the garbage collector.
///
/// Obtain one via [`handle()`](crate::handle). A `Handle` is cheap to clone
/// (reference-counted) and is used for two purposes:
///
/// 1. **Pinning** — call [`pin()`](Handle::pin) to enter a phase-critical section
///    and get a [`Guard`].
/// 2. **HP protection** — pass a `Handle` to [`Local::protect`](crate::Local::protect)
///    to create a [`Local<Handle, T>`](crate::Local) whose lifetime extends beyond
///    a single [`Guard`], protected by a hazard pointer instead.
#[derive(Clone)]
pub struct Handle {
    pub(crate) local: Rc<Entry<Local>>,
}

impl Handle {
    #[inline]
    pub(crate) fn local(&self) -> &Local {
        &self.local
    }

    /// Pins the current thread, entering a *phase-critical section*.
    ///
    /// Returns a [`Guard`] whose lifetime defines the critical section. Within
    /// it you may allocate managed objects, load atomic pointers, and freely
    /// dereference any [`Local`](crate::Local) obtained from those loads.
    #[inline]
    pub fn pin(&self) -> Guard {
        let guard = Guard::new(self.local.clone());
        self.local().pin_inner();
        guard
    }

    /// Returns `true` if the handle is pinned.
    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.local().is_pinned()
    }

    /// Helps the ongoing collection works if possible.
    #[inline]
    pub fn help_collect(&self) {
        self.pin().help_collect();
    }
}

/// An RAII guard representing an active *phase-critical section*.
///
/// While a `Guard` is alive, the current thread is *pinned* to the global epoch.
/// This guarantees that all managed pointers reachable at the time of pinning
/// remain valid for the guard's lifetime, enabling safe, lock-free traversals
/// of the managed heap.
///
/// **Not a lock.** Despite the name "critical section", a `Guard` does *not*
/// block other threads. Any number of mutators may hold their own `Guard`s
/// concurrently. The section only serves as a synchronization point between
/// mutators and the collector: it tells the collector that this thread is
/// actively accessing the managed heap during a particular epoch, so that
/// phase transitions can be coordinated safely (similar to RCU's read-side
/// critical section).
///
/// # Typical usage
///
/// ```ignore
/// let guard = cdpt::pin();            // enter critical section
/// let local = some_atomic.load(Ordering::Acquire, &guard);
/// println!("{}", *local);             // safe dereference
/// // guard dropped here → critical section ends
/// ```
///
/// **Keep critical sections short.** A long-lived `Guard` delays epoch
/// advancement and prevents garbage collection from making progress.
/// If you need a reference that outlives the guard, promote it to a
/// hazard-pointer–protected [`Local<Handle, T>`](crate::Local) via
/// [`Local::protect`](crate::Local::protect).
pub struct Guard {
    local: Rc<Entry<Local>>,
    should_help: Cell<bool>,
}

impl Guard {
    fn new(local: Rc<Entry<Local>>) -> Self {
        Self {
            local,
            should_help: Cell::new(false),
        }
    }

    /// Unpins and immediately re-pins the thread, refreshing the local epoch.
    ///
    /// Call this in long-running loops to avoid blocking global epoch advancement.
    /// Takes `&mut self` to statically prevent any `Local<Guard, T>` from being
    /// held across the call — all such references must be re-acquired afterwards.
    /// Only effective when this is the sole active guard for the current thread.
    pub fn repin(&mut self) {
        self.help_collect_if_scheduled();
        self.local().repin();
    }

    pub(crate) fn local(&self) -> &Local {
        &self.local
    }

    pub(crate) fn local_epoch(&self) -> Epoch {
        unsafe { self.local().pinned_epoch() }
    }

    pub(crate) fn phase(&self) -> Phase {
        self.local_epoch().phase()
    }

    pub(crate) fn white_color(&self) -> Color {
        self.local_epoch().color()
    }

    pub(crate) fn black_color(&self) -> Color {
        self.white_color().flip()
    }

    pub(crate) fn alloc_color(&self) -> Color {
        unsafe { self.local().pinned_alloc_color() }
    }

    pub(crate) fn global_phase(&self) -> Phase {
        fence(Ordering::SeqCst);
        global().load_epoch().phase()
    }

    pub(crate) fn alloc<T: 'static + TraceObj>(&self, obj: ManObj<T>) -> *mut ManObj<T> {
        self.local().alloc(obj, self)
    }

    pub(crate) fn schedule_mark<T: 'static + TraceObj>(&self, obj: &ManObj<T>) {
        self.local().schedule_mark(obj, self);
    }

    pub(crate) fn try_pop_mark_task(&self) -> Option<Task> {
        self.local().try_pop_mark_task()
    }

    pub(crate) fn schedule_helping_collect(&self) {
        self.should_help.set(true);
    }

    pub(crate) fn help_collect_if_scheduled(&self) {
        if self.should_help.get() {
            self.should_help.set(false);
            self.help_collect();
        }
    }

    /// Voluntarily assists the collector with pending work (sweeping or tracing).
    ///
    /// Called automatically on an intensive memory allocation, but can be invoked
    /// explicitly in long-running operations to help GC make progress.
    pub fn help_collect(&self) {
        match self.phase() {
            Phase::N => self.help_normal(),
            Phase::RT | Phase::CT => self.help_tracing(),
        }
    }

    /// Helps sweeping works for the current Normal phase.
    #[inline]
    pub(crate) fn help_normal(&self) {
        if self.phase() != Phase::N {
            return;
        }

        call_without_recursion(&self.local().is_helping_normal, || self.help_normal_inner());
    }

    #[inline]
    fn help_normal_inner(&self) {
        let ebr_guard = &ebr_pin();
        let mut trial_count = 0;

        for q_idx in self.local().generate_shard_permut() {
            let prev_white = self.black_color();
            let marked_q = &global().marked_objs[prev_white as usize][q_idx];

            while let Some(batch) = marked_q.try_pop(ebr_guard) {
                let mut reclaimed_bytes = 0;
                for obj in batch.into_iter() {
                    if prev_white == obj.color() {
                        reclaimed_bytes += size_of_val(&*obj);
                        drop(obj);
                        continue;
                    }
                    unsafe { self.local().push_fresh_obj(obj, false) };
                }
                global()
                    .stats
                    .total_reclaimed
                    .fetch_add(reclaimed_bytes, Ordering::Release);

                trial_count += 1;
                if trial_count >= HELP_NORMAL_MAX_TRIAL {
                    return;
                }
            }
        }
    }

    /// Helps root marking and tracing works for the current RT or CT phase.
    #[inline]
    pub(crate) fn help_tracing(&self) {
        if self.phase() != Phase::RT && self.phase() != Phase::CT {
            return;
        }

        if !self.is_tracing_synced() {
            return;
        }

        call_without_recursion(&self.local().is_helping_root_tracing, || {
            self.help_root_tracing()
        });
        call_without_recursion(&self.local().is_helping_draining_mark_tasks, || {
            self.help_draining_mark_tasks_inner()
        });
    }

    #[inline]
    fn is_tracing_synced(&self) -> bool {
        // Ensure that all previous Normal phases have ended.
        // Note: If some threads (T_m) are helping with marking tasks
        // while others (T_s) are helping with sweeping tasks,
        // T_s may misinterpret a black object (i.e., marked by T_m)
        // as an unreachable object (i.e., still having the previous allocation color),
        // which can lead to a use-after-free error.
        global().locals.iter_using().all(|local| {
            let other_epoch = local.epoch.load(Ordering::Relaxed);
            !other_epoch.is_pinned() || other_epoch.phase() != Phase::N
        })
    }

    #[inline]
    pub fn help_draining_mark_tasks_inner(&self) {
        let mt_len = unsafe { &*self.local().mark_tasks.get() }.len();
        for _ in 0..((mt_len / 2).max(OBJ_BATCH_SIZE / 2)) {
            if let Some(task) = self.try_pop_mark_task() {
                task.call(self);
                continue;
            }
            break;
        }
    }

    #[inline]
    fn help_root_tracing(&self) {
        if self.phase() != Phase::RT {
            return;
        }

        let ebr_guard = &ebr_pin();
        let hazards = LazyCell::new(|| self.local().scan_or_reuse_hazards(self, ebr_guard));
        let mut trial_count = 0;

        for q_idx in self.local().generate_shard_permut() {
            let fresh_q = &global().fresh_objs[self.white_color() as usize][q_idx];
            while let Some(batch) = fresh_q.try_pop(ebr_guard) {
                for obj in batch.iter() {
                    if obj.root_count() > 0 || hazards.contains(&obj.address()) {
                        obj.mark(self);
                    }
                }
                let marked_q_idx = self.local().select_obj_shard();
                let marked_q = &global().marked_objs[self.white_color() as usize][marked_q_idx];
                marked_q.push(batch, ebr_guard);

                trial_count += 1;
                if trial_count >= HELP_TRACING_MAX_TRIAL {
                    return;
                }
            }
        }
    }
}

impl Drop for Guard {
    #[inline]
    fn drop(&mut self) {
        self.help_collect_if_scheduled();
        self.local().unpin_inner();
    }
}

#[inline]
fn call_without_recursion(flag: &Cell<bool>, f: impl FnOnce()) {
    if flag.get() {
        return;
    }
    flag.set(true);
    f();
    flag.set(false);
}
