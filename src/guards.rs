//! RAII-style guards for users.

use crate::{
    TraceObj,
    epoch::{Color, Epoch, Phase},
    internal::{CollectionMode, Local, OBJ_BATCH_SIZE},
    pointers::ManObj,
    sync::Entry,
    task::Task,
    tls::global,
};
use crossbeam::epoch::pin as ebr_pin;
use std::{
    cell::{Cell, LazyCell},
    rc::Rc,
    sync::atomic::{Ordering, compiler_fence, fence},
};

const HELP_NORMAL_MAX_TRIAL: usize = 2;
const HELP_TRACING_MAX_TRIAL: usize = 4;

/// A thread-local handle to the garbage collector.
///
/// Obtain one with [`handle()`](crate::handle). A `Handle` is cheap to clone
/// (it is reference-counted) and serves two purposes:
///
/// 1. **Accessing the managed heap.** Call [`pin()`](Handle::pin) to get a
///    [`Guard`] that lets you allocate, load, and dereference managed pointers.
/// 2. **Extending pointer lifetimes.** Pass it to
///    [`Local::protect`](crate::Local::protect) to keep a reference alive past
///    its [`Guard`] with hazard-pointer protection.
///
/// # Examples
///
/// ```
/// # use cdpt::*;
/// # #[derive(TraceObj)]
/// # struct Node { value: i32 }
/// let handle = handle();
/// let guard = handle.pin();
/// let node = Local::new(Node { value: 1 }, &guard);
/// let kept = node.protect(&handle); // outlives the guard
/// drop(guard);
/// assert_eq!(kept.value, 1);
/// ```
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
    ///
    /// # Examples
    ///
    /// ```
    /// # use cdpt::*;
    /// let handle = handle();
    /// let guard = handle.pin();
    /// // ... access the managed heap through `guard` ...
    /// drop(guard);
    /// ```
    #[inline]
    pub fn pin(&self) -> Guard {
        let guard = Guard::new(self.local.clone());
        self.local().pin_inner();
        guard
    }

    /// Returns `true` if this handle's thread is currently pinned, that is, if a
    /// [`Guard`] obtained from this handle is alive.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cdpt::*;
    /// let handle = handle();
    /// assert!(!handle.is_pinned());
    /// let guard = handle.pin();
    /// assert!(handle.is_pinned());
    /// drop(guard);
    /// assert!(!handle.is_pinned());
    /// ```
    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.local().is_pinned()
    }

    /// Voluntarily assists the collector with pending work.
    ///
    /// Useful in long-running threads that rarely allocate: calling it
    /// periodically helps the collector make progress. It pins the thread
    /// briefly for the duration of the call.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cdpt::*;
    /// let handle = handle();
    /// handle.help_collect();
    /// ```
    #[inline]
    pub fn help_collect(&self) {
        self.pin().help_collect();
    }
}

/// Grants the current thread access to the managed heap.
///
/// Obtain one with [`pin()`](crate::pin) or [`Handle::pin`]. While the guard is
/// alive you can allocate managed objects, load atomic pointers, and
/// dereference the resulting [`Local`](crate::Local) references. A `Guard` is
/// **not a lock**: any number of threads may hold their own guards at once. It
/// only marks the thread as actively using the heap so the collector does not
/// reclaim anything the thread might still reach, much like the read side of an
/// RCU critical section.
///
/// # Keep guards short-lived
///
/// A long-lived guard prevents the collector from reclaiming garbage, so drop
/// it as soon as you stop loading new pointers. To keep a reference past the
/// guard, promote it to a [`Local<Handle, T>`](crate::Local) with
/// [`Local::protect`](crate::Local::protect) (hazard-pointer protected) or to a
/// [`Shared<T>`](crate::Shared) with [`Local::as_shared`](crate::Local::as_shared)
/// (root-count protected). The `Local<Handle, T>` form is generally cheaper to
/// create.
///
/// # Examples
///
/// ```
/// # use cdpt::*;
/// # use std::sync::atomic::Ordering;
/// # #[derive(TraceObj)]
/// # struct Node { value: i32 }
/// let guard = pin();                                // enter a critical section
/// let cell = AtomicShared::new(Node { value: 1 }, &guard);
/// let node = cell.load(Ordering::Acquire, &guard);  // load a pointer
/// assert_eq!(node.value, 1);                         // dereference it
/// drop(guard);                                       // let the collector proceed
/// ```
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
    ///
    /// # Examples
    ///
    /// ```
    /// # use cdpt::*;
    /// let mut guard = pin();
    /// for _ in 0..3 {
    ///     // ... a chunk of work using `&guard` ...
    ///     guard.repin(); // let the collector make progress between chunks
    /// }
    /// ```
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

    /// Loads the current global epoch with a preceding fence. This function
    /// must be called after a successful atomic RMW operation, which on
    /// x86/x86-64 is lock-prefixed and therefore already provides full
    /// ordering.
    ///
    /// On non-x86 architectures, this issues a `SeqCst` fence. On x86/x86-64,
    /// it issues only a compiler fence, which suffices to prevent local
    /// reordering by the compiler.
    ///
    /// This optimization is motivated by
    /// [crossbeam_epoch's approach](https://github.com/crossbeam-rs/crossbeam/blob/master/crossbeam-epoch/src/internal.rs#L413-L448).
    /// One difference worth noting: the original comment there assumes that
    /// only RMW operations with `SeqCst` ordering produce a full fence, but
    /// we believe the ordering does not matter on x86/x86-64. Even a CAS
    /// with `(Relaxed, Relaxed)` ordering is compiled to a `lock cmpxchg`,
    /// which carries full hardware ordering regardless of the requested
    /// ordering ([godbolt](https://godbolt.org/z/517o9nKcj)). The only
    /// remaining risk is local reordering by the compiler, and that is
    /// addressed by the compiler fence, as crossbeam_epoch already does.
    #[inline]
    pub(crate) fn global_phase_with_fence(&self) -> Phase {
        if cfg!(any(target_arch = "x86_64", target_arch = "x86")) {
            compiler_fence(Ordering::SeqCst);
        } else {
            fence(Ordering::SeqCst);
        }
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
    /// explicitly in long-running operations to help the collector make
    /// progress, for example sweeping dead objects or tracing live ones.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cdpt::*;
    /// let guard = pin();
    /// // In a long loop that rarely allocates, lend the collector a hand.
    /// guard.help_collect();
    /// ```
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

        // Leaving the last critical section is a safepoint: in cooperative
        // mode, where no background thread watches heap pressure, this is
        // where a mutator picks up the collector's job.
        if !self.local().is_pinned() && global().collection_mode() == CollectionMode::Cooperative {
            let handle = Handle {
                local: self.local.clone(),
            };
            crate::collector::drive_collection_if_necessary(&handle);
        }
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
