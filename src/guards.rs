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
/// 1. **Accessing the managed heap** — call [`pin()`](Handle::pin) to get a
///    [`Guard`] that lets you allocate, load, and dereference managed pointers.
/// 2. **Extending pointer lifetimes** — pass a `Handle` to
///    [`Local::protect`](crate::Local::protect) to keep a reference alive after
///    its [`Guard`] is dropped, using hazard-pointer protection.
#[derive(Clone)]
pub struct Handle {
    pub(crate) local: Rc<Entry<Local>>,
}

impl Handle {
    #[inline]
    pub(crate) fn local(&self) -> &Local {
        &self.local
    }

    /// Returns a [`Guard`] that grants access to the managed heap.
    ///
    /// While the guard is alive, you may allocate managed objects, load atomic
    /// pointers, and freely dereference any [`Local`](crate::Local) obtained
    /// from those loads. Internally, this pins the current thread to the global
    /// epoch (entering a *phase-critical section*).
    #[inline]
    pub fn pin(&self) -> Guard {
        let guard = Guard::new(self.local.clone());
        self.local().pin_inner();
        guard
    }

    /// Returns `true` if this handle's thread is currently pinned (i.e., a
    /// [`Guard`] obtained from this handle is alive).
    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.local().is_pinned()
    }

    /// Voluntarily assists the collector with pending work.
    ///
    /// Useful in long-running threads that rarely allocate — calling this
    /// periodically helps the GC make progress. Internally pins the thread
    /// briefly.
    #[inline]
    pub fn help_collect(&self) {
        self.pin().help_collect();
    }
}

/// Grants access to the managed heap for the current thread.
///
/// Create one via [`pin()`](crate::pin) or [`Handle::pin`]. While a `Guard` is
/// alive, you can allocate managed objects, load atomic pointers, and dereference
/// the resulting [`Local`](crate::Local) references.
///
/// # Typical usage
///
/// ```ignore
/// let guard = cdpt::pin();            // start accessing the managed heap
/// let local = some_atomic.load(Ordering::Acquire, &guard);
/// println!("{}", *local);             // safe dereference
/// // guard dropped here → collector can make progress
/// ```
///
/// # Keep guards short-lived
///
/// A long-lived `Guard` prevents the collector from reclaiming garbage.
/// Drop the guard as soon as you no longer need to load new pointers.
/// If you need a reference that outlives the guard, promote it to either:
/// - [`Local<Handle, T>`](crate::Local) via [`Local::protect`](crate::Local::protect)
///   (hazard-pointer–protected), or
/// - [`Shared<T>`](crate::Shared) via [`Local::as_shared`](crate::Local::as_shared)
///   (root-count–protected).
///
/// `Local<Handle, T>` is generally cheaper to create than `Shared<T>`,
/// because it is based on thread-local hazard-pointer protection.
///
/// # How it works
///
/// A `Guard` pins the current thread to the global epoch, entering a
/// *phase-critical section*. This is **not a lock** — any number of threads
/// may hold their own `Guard`s concurrently. The section only tells the
/// collector that this thread is actively accessing the managed heap, so
/// that phase transitions can be coordinated safely (similar to RCU's
/// read-side critical section).
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

    /// Lets the collector make progress during a long-running loop.
    ///
    /// Briefly unpins and re-pins the thread, refreshing the local epoch so
    /// the collector is not blocked indefinitely. Takes `&mut self` to
    /// statically ensure no [`Local<Guard, T>`](crate::Local) references are
    /// held across the call. To keep those references alive across this call,
    /// consider promoting them
    /// (see the [struct-level docs](Guard#keep-guards-short-lived)).
    ///
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

    pub(crate) fn global_phase_with_fence(&self) -> Phase {
        fence(Ordering::SeqCst);
        global().load_epoch().phase()
    }

    /// Like [`global_phase`](Self::global_phase) but without a preceding SeqCst
    /// fence. Safe to call immediately after a successful atomic RMW with at
    /// least AcqRel success ordering — on x86/x86-64, lock-prefixed
    /// instructions already provide full ordering, making the fence redundant.
    #[inline(always)]
    pub(crate) fn global_phase_no_fence(&self) -> Phase {
        #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
        fence(Ordering::SeqCst);
        global().load_epoch().phase()
    }

    pub(crate) fn alloc<T: TraceObj>(&self, obj: ManObj<T>) -> *mut ManObj<T> {
        self.local().alloc(obj, self)
    }

    pub(crate) fn schedule_mark<T: TraceObj>(&self, obj: &ManObj<T>) {
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

    /// Voluntarily assists the collector with pending work.
    ///
    /// Called automatically during intensive allocation, but you can invoke it
    /// explicitly in long-running operations to help the GC make progress
    /// (e.g., sweeping dead objects or tracing live ones).
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
    pub(crate) fn help_draining_mark_tasks_inner(&self) {
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
