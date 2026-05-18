use crate::collector::collector_loop;
use crate::epoch::{AtomicEpoch, Color, Epoch, Phase};
use crate::guards::{Guard, Handle};
use crate::pointers::{ManObj, ManPtr, MarkObj, TraceObj};
use crate::sync::{Entry, Queue, ReusableSlots};
use crate::task::Task;
use crate::{global, pin};
use crossbeam::deque::{Stealer, Worker};
use crossbeam::epoch::{
    Atomic as EbrAtomic, Guard as EbrGuard, Owned as EbrOwned, pin as ebr_pin, unprotected,
};
use crossbeam::utils::CachePadded;
use fastrand::Rng;
use rustc_hash::FxHashSet;
use std::array::from_fn;
use std::cell::{Cell, UnsafeCell};
use std::mem::{MaybeUninit, take};
use std::ops::DerefMut;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{self, AtomicBool, AtomicPtr, AtomicUsize, Ordering, fence};
use std::thread::spawn;

pub(crate) const OBJ_BATCHES_SHARD: usize = 8;
pub(crate) const OBJ_BATCH_SIZE: usize = 64;
const HAZARDS_INIT_COUNT: usize = 8;
const ALLOC_HELPING_PERIOD: usize = 64;
const SCHED_HELPING_PERIOD: usize = 32;

/// Global state of the garbage collector, providing configuration and
/// profiling.
///
/// Obtain the singleton instance via [`global()`](crate::global). Most
/// users only need [`pin()`](crate::pin) and [`handle()`](crate::handle);
/// use `Global` when you need to toggle collection or monitor heap usage.
///
/// # Heap size estimation
///
/// The `estimate_*` methods track bytes allocated and reclaimed based on
/// [`std::mem::size_of`] of each managed object. This means heap allocations
/// owned *inside* a managed type (e.g., a `String`'s buffer) are **not**
/// accounted for — the reported sizes reflect only the shallow size of
/// each object.
pub struct Global {
    /// The intrusive linked list of `Local`s.
    pub(crate) locals: CachePadded<ReusableSlots<Local>>,

    /// The global epoch.
    pub(crate) epoch: CachePadded<AtomicEpoch>,

    /// The global statistics data.
    pub(crate) stats: GlobalStats,

    /// `fresh_objs` and `marked_objs`: the global sharded object lists.
    ///
    /// The first index represents the allocation color when the object is allocated.
    pub(crate) fresh_objs: [[Queue<ObjBatch>; OBJ_BATCHES_SHARD]; 2],
    pub(crate) marked_objs: [[Queue<ObjBatch>; OBJ_BATCHES_SHARD]; 2],

    /// The global flag indicating whether the collector is online.
    collector_init: CachePadded<AtomicBool>,

    /// The global flag indicating whether the collection is enabled.
    pub(crate) collection_enabled: CachePadded<AtomicBool>,

    /// Flag to request an immediate collection cycle, bypassing the heuristic.
    pub(crate) collection_requested: CachePadded<AtomicBool>,

    /// Packed heap-headroom setting (single atomic for tear-free reads).
    /// MSB clear = fixed mode, lower bits = bytes.
    /// MSB set   = proportional mode, lower bits = divisor.
    pub(crate) headroom: AtomicUsize,

    /// Number of threads used for parallel collection (1..=OBJ_BATCHES_SHARD).
    pub(crate) collector_threads: AtomicUsize,

    #[cfg(feature = "profiling")]
    pub(crate) rc_updates: AtomicUsize,
}

/// Controls the minimum heap headroom that must be exceeded before the
/// collector triggers the next cycle.
///
/// After each collection the collector computes a minimum extra headroom and
/// will not start a new cycle until `heap_usage` grows beyond
/// `post_collection_usage + headroom`.  This enum selects how that minimum is
/// determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeapHeadroom {
    /// Fixed minimum headroom in mebibytes.
    ///
    /// A larger value reduces collection frequency (lower CPU) but allows peak
    /// memory to grow.  A smaller value triggers collection sooner (lower peak
    /// memory) at the cost of more CPU.
    ///
    /// The value is clamped to `1..=1024` MiB.
    FixedMiB(usize),

    /// Proportional minimum headroom: `heap_usage / divisor`.
    ///
    /// A **higher** divisor means less headroom, so the collector fires sooner
    /// and peak memory is lower.  A **lower** divisor gives more headroom.
    ///
    /// The value is clamped to `1..=1024`.
    Proportional(usize),
}

/// MSB used to distinguish proportional (set) from fixed (clear).
const HEADROOM_PROPORTIONAL_BIT: usize = 1 << (usize::BITS - 1);

impl HeapHeadroom {
    fn pack(self) -> usize {
        match self {
            Self::FixedMiB(mib) => mib.clamp(1, 1024) * 1024 * 1024,
            Self::Proportional(div) => div.clamp(1, 1024) | HEADROOM_PROPORTIONAL_BIT,
        }
    }

    fn unpack(bits: usize) -> Self {
        if bits & HEADROOM_PROPORTIONAL_BIT != 0 {
            Self::Proportional(bits & !HEADROOM_PROPORTIONAL_BIT)
        } else {
            Self::FixedMiB(bits / (1024 * 1024))
        }
    }
}

/// FIXME: This may be very inaccurate for some types that internally allocate unmanaged memory.
/// E.g., `String`'s size will be measured as 24 bytes,
/// but its actual memory usage depends on the size of buffer.
pub(crate) struct GlobalStats {
    /// Total allocated memory (bytes) since the beginning of the program.
    pub(crate) total_allocated: CachePadded<AtomicUsize>,
    /// Total reclaimed memory (bytes) since the beginning of the program.
    pub(crate) total_reclaimed: CachePadded<AtomicUsize>,
}

impl Default for GlobalStats {
    fn default() -> Self {
        Self {
            total_allocated: CachePadded::new(AtomicUsize::new(0)),
            total_reclaimed: CachePadded::new(AtomicUsize::new(0)),
        }
    }
}

unsafe impl Sync for Global {}
unsafe impl Send for Global {}

pub(crate) struct ObjBatch(Vec<Box<dyn MarkObj>>);

impl Default for ObjBatch {
    fn default() -> Self {
        Self::with_capacity(OBJ_BATCH_SIZE)
    }
}

impl ObjBatch {
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    pub fn push_within_capacity(&mut self, item: Box<dyn MarkObj>) -> Result<(), Box<dyn MarkObj>> {
        self.0.push_within_capacity(item).map(|_| ())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Box<dyn MarkObj>> {
        self.0.iter()
    }

    pub fn into_iter(self) -> impl Iterator<Item = Box<dyn MarkObj>> {
        self.0.into_iter()
    }
}

impl Global {
    /// Creates a new global data for garbage collection.
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            locals: CachePadded::new(ReusableSlots::default()),
            epoch: CachePadded::new(AtomicEpoch::new(Epoch::starting())),
            stats: GlobalStats::default(),
            fresh_objs: from_fn(|_| from_fn(|_| Queue::new())),
            marked_objs: from_fn(|_| from_fn(|_| Queue::new())),
            collector_init: CachePadded::new(AtomicBool::new(false)),
            collection_enabled: CachePadded::new(AtomicBool::new(true)),
            collection_requested: CachePadded::new(AtomicBool::new(false)),
            headroom: AtomicUsize::new(HeapHeadroom::FixedMiB(1).pack()),
            collector_threads: AtomicUsize::new(default_collector_threads()),
            #[cfg(feature = "profiling")]
            rc_updates: AtomicUsize::new(0),
        }
    }

    /// Freshly loads the global epoch value. It does not execute any fences.
    #[inline]
    pub(crate) fn load_epoch(&self) -> Epoch {
        self.epoch.load(Ordering::Acquire)
    }

    #[inline]
    fn initialize_if_necessary(&self) {
        if !self.collector_init.load(Ordering::Relaxed) {
            self.try_deploy_collector();
        }
    }

    #[cold]
    fn try_deploy_collector(&self) {
        if self
            .collector_init
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            spawn(collector_loop);
        }
    }

    pub(crate) fn collect_hps(&self, ebr_guard: &EbrGuard) -> FxHashSet<*mut ()> {
        self.locals
            .iter_using()
            .flat_map(|local| {
                let hazards = local.hazards.load(Ordering::Acquire, ebr_guard);
                unsafe { hazards.deref() }
                    .iter()
                    .map(|hp| unsafe { hp.assume_init_ref() }.load(Ordering::Relaxed))
            })
            .collect::<_>()
    }

    pub(crate) fn push_fresh_objs(
        &self,
        batch: ObjBatch,
        size_bytes: usize,
        alloc_color: Color,
        shard_index: usize,
        ebr_guard: &EbrGuard,
    ) {
        unsafe {
            self.fresh_objs
                .get_unchecked(alloc_color as usize)
                .get_unchecked(shard_index)
                .push(batch, ebr_guard);
            self.stats
                .total_allocated
                .fetch_add(size_bytes, Ordering::Release);
        }
    }

    /// Enables or disables garbage collection.
    ///
    /// When disabled, the collector thread will not reclaim any objects.
    /// Collection is enabled by default.
    pub fn enable_collection(&self, set: bool) {
        self.collection_enabled.store(set, Ordering::SeqCst);
    }

    /// Requests an immediate collection cycle, bypassing the normal heuristic.
    ///
    /// The collector thread will run one cycle as soon as possible, regardless
    /// of current heap pressure. Useful in tests to force garbage collection
    /// of recently dropped objects.
    pub fn request_collection(&self) {
        self.collection_requested.store(true, Ordering::SeqCst);
    }

    #[cfg(feature = "profiling")]
    pub fn rc_updates(&self) -> usize {
        self.rc_updates.load(Ordering::Relaxed)
    }

    #[cfg(feature = "profiling")]
    pub fn reset_rc_updates(&self) {
        self.rc_updates.store(0, Ordering::Relaxed);
    }

    /// Returns the total bytes allocated on the managed heap since program
    /// start. See [struct-level docs](Global#heap-size-estimation) for
    /// accuracy caveats.
    pub fn estimate_total_alloc(&self) -> usize {
        self.stats.total_allocated.load(Ordering::Acquire)
    }

    /// Returns the total bytes reclaimed by the collector since program
    /// start. See [struct-level docs](Global#heap-size-estimation) for
    /// accuracy caveats.
    pub fn estimate_total_reclm(&self) -> usize {
        self.stats.total_reclaimed.load(Ordering::Acquire)
    }

    /// Returns the estimated current managed-heap size in bytes
    /// (allocated - reclaimed, saturating at zero).
    pub fn estimate_heap_usage(&self) -> usize {
        let allocated = self.estimate_total_alloc();
        let reclaimed = self.estimate_total_reclm();
        allocated.saturating_sub(reclaimed)
    }

    /// Sets the heap-headroom strategy.
    ///
    /// See [`HeapHeadroom`] for details on each variant.
    ///
    /// Default: `HeapHeadroom::FixedMiB(1)`.
    pub fn set_heap_headroom(&self, headroom: HeapHeadroom) {
        self.headroom.store(headroom.pack(), Ordering::Relaxed);
    }

    /// Returns the current heap-headroom strategy.
    pub fn heap_headroom(&self) -> HeapHeadroom {
        HeapHeadroom::unpack(self.headroom.load(Ordering::Relaxed))
    }

    /// Sets the number of threads used for parallel collection.
    ///
    /// The value is clamped to `1..=8`. Higher values speed up
    /// collection but consume more CPU. Setting this to `1` disables
    /// parallel collection entirely.
    ///
    /// Default: one-eighth of available parallelism, clamped to `1..=8`
    /// (e.g. `1` on an 8-core machine, `8` on a 64-core machine).
    pub fn set_collector_threads(&self, count: usize) {
        let clamped = count.clamp(1, OBJ_BATCHES_SHARD);
        self.collector_threads.store(clamped, Ordering::Relaxed);
    }

    /// Returns the current number of collector threads.
    pub fn collector_threads(&self) -> usize {
        self.collector_threads.load(Ordering::Relaxed)
    }
}

/// Picks the default collector thread count: one-eighth of the available
/// parallelism, clamped to `1..=OBJ_BATCHES_SHARD`. Falls back to `1` when
/// the platform cannot report parallelism.
fn default_collector_threads() -> usize {
    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    (parallelism / 8).clamp(1, OBJ_BATCHES_SHARD)
}

/// Participant for garbage collection.
pub(crate) struct Local {
    /// The number of guards keeping this participant pinned.
    guard_count: Cell<usize>,

    #[cfg(feature = "profiling")]
    unpin_count: Cell<usize>,

    /// The epoch that this local thread observed most recently.
    last_observed: Cell<Epoch>,

    /// A counter of allocations to periodically trigger helping collection.
    alloc_count: Cell<usize>,

    /// A counter of scheduling to periodically trigger helping collection.
    sched_count: Cell<usize>,

    /// An indicator that the thread is helping sweeping works for the current Normal phase.
    pub(crate) is_helping_normal: Cell<bool>,

    /// An indicator that the thread is helping root marking for the current RT phase.
    pub(crate) is_helping_root_tracing: Cell<bool>,

    /// An indicator that the thread is helping tracing works for the current CT phase.
    pub(crate) is_helping_draining_mark_tasks: Cell<bool>,

    /// A single-writer multiple-reader list of protected pointers.
    ///
    /// The owning thread may resize and defer destruction of the old hazards.
    /// Therefore, a collector must access this array with an EBR guard.
    pub(crate) hazards: EbrAtomic<[MaybeUninit<AtomicPtr<()>>]>,

    /// The function pointers to mark each HP-protected object.
    #[allow(clippy::type_complexity)]
    hazards_marker: UnsafeCell<Vec<Option<unsafe fn(*mut ())>>>,

    /// A stealer handle for `mark_tasks`.
    pub(crate) mark_tasks_stealer: Stealer<Task>,

    /// The local epoch.
    pub(crate) epoch: CachePadded<AtomicEpoch>,

    /// The last timestamp when this thread modified the local mark queue.
    pub(crate) mt_modified_ts: CachePadded<AtomicUsize>,

    // All resources below with `UnsafeCell`s will be destroyed
    // when all guards and handles get dropped.
    /// A vector of available hazard indices.
    available_hids: UnsafeCell<Vec<usize>>,

    /// A local random number generater to select a shard.
    rng: UnsafeCell<Rng>,

    /// A local batch of newly allocated objects.
    ///
    /// It is sharded by the allocation color. That is, the thread push the allocated object
    /// reference to the index 0 if `guard.alloc_color() == C0`. In this way, the collector
    /// can take the object list of the previous normal phase without breaking lock-freedom of
    /// mutators, because the mutator can still modify its black object list during RT & CT phase
    /// while the collector accesses the white object list, which is from the previous N phase.
    pub(crate) objs: [UnsafeCell<(ObjBatch, usize)>; 2],

    /// A local mark queue.
    pub(crate) mark_tasks: UnsafeCell<Worker<Task>>,

    /// A previously collected hazards that may be reused for later helpings.
    pub(crate) cached_hazards: UnsafeCell<Option<(FxHashSet<*mut ()>, Epoch)>>,
}

impl Drop for Local {
    fn drop(&mut self) {
        // This is called when an insertion fails in `reusable_slot.rs`.
        unsafe { take(&mut self.hazards).into_owned() };
    }
}

impl Default for Local {
    fn default() -> Self {
        let mark_tasks = Worker::new_fifo();
        let stealer = mark_tasks.stealer();

        let mut hazards: EbrOwned<[MaybeUninit<AtomicPtr<()>>]> =
            EbrOwned::init(HAZARDS_INIT_COUNT);
        let slots = hazards.deref_mut();
        unsafe {
            ptr::write_bytes(slots.as_mut_ptr(), 0, slots.len());
        }

        Self {
            guard_count: Cell::new(0),
            #[cfg(feature = "profiling")]
            unpin_count: Cell::new(0),
            last_observed: Cell::new(Epoch::starting()),
            alloc_count: Cell::new(0),
            sched_count: Cell::new(0),
            is_helping_normal: Cell::new(false),
            is_helping_root_tracing: Cell::new(false),
            is_helping_draining_mark_tasks: Cell::new(false),
            hazards: EbrAtomic::from(hazards),
            hazards_marker: UnsafeCell::new(vec![None; HAZARDS_INIT_COUNT]),
            mark_tasks_stealer: stealer,
            epoch: CachePadded::new(AtomicEpoch::new(Epoch::starting())),
            mt_modified_ts: CachePadded::new(AtomicUsize::new(0)),
            available_hids: UnsafeCell::new((0..HAZARDS_INIT_COUNT).collect()),
            rng: UnsafeCell::new(Rng::new()),
            objs: [UnsafeCell::default(), UnsafeCell::default()],
            mark_tasks: UnsafeCell::new(mark_tasks),
            cached_hazards: UnsafeCell::default(),
        }
    }
}

impl Local {
    /// Registers a new `Local` in the provided `Global`.
    pub(crate) fn register() -> Handle {
        global().initialize_if_necessary();
        let local = Rc::new(global().locals.acquire_or_default());
        Handle { local }
    }

    /// Returns `true` if the current participant is pinned.
    #[inline]
    pub(crate) fn is_pinned(&self) -> bool {
        self.guard_count.get() > 0
    }

    #[inline]
    pub(crate) fn pin_inner(&self) {
        let guard_count = self.guard_count.get();
        self.guard_count.set(guard_count.checked_add(1).unwrap());

        if guard_count == 0 {
            self.pin_freshly();
        }
    }

    #[inline]
    fn pin_freshly(&self) {
        let mut curr_epoch = global().load_epoch();
        loop {
            // Now we must store `new_epoch` into `self.epoch` and execute a light fence.
            self.epoch.store(curr_epoch.pinned(), Ordering::Release);
            fence(Ordering::SeqCst);

            let new_epoch = global().load_epoch();
            if curr_epoch == new_epoch {
                break;
            }
            curr_epoch = new_epoch;
        }

        if self.last_observed.get().timestamp() != curr_epoch.timestamp()
            && curr_epoch.phase() == Phase::RT
        {
            // If we are in a root tracing phase and it’s the first time
            // observing this phase, scan and shade (i.e., push to mark stack)
            // objects that are protected by thread-local HPs.
            self.phase_barrier();
        }
        self.last_observed.set(curr_epoch);
    }

    /// Unpins the `Local`.
    #[inline]
    pub(crate) fn unpin_inner(&self) {
        let guard_count = self.guard_count.get();
        self.guard_count.set(guard_count - 1);

        if guard_count == 1 {
            // This is the last guard. This thread will be unpinned.
            self.epoch.store(Epoch::starting(), Ordering::Release);

            #[cfg(feature = "profiling")]
            {
                self.unpin_count.set(self.unpin_count.get() + 1);
                if self.unpin_count.get() % 128 == 0 {
                    let count = crate::pointers::RC_UPDATE_COUNTER.get();
                    if count > 0 {
                        global().rc_updates.fetch_add(count, Ordering::Relaxed);
                        crate::pointers::RC_UPDATE_COUNTER.set(0);
                    }
                }
            }
        }
    }

    /// Unpins and then pins the `Local`.
    #[inline]
    pub(crate) fn repin(&self) {
        self.unpin_inner();
        self.pin_inner();
    }

    /// Execute the phase barrier for this local thread.
    #[inline]
    pub(crate) fn phase_barrier(&self) {
        // Safety of `unprotected`: It is always safe to access my own hazard vector.
        // This is because other threads never attempt to change this hazard vector.
        let hazards = self
            .hazards
            .load(Ordering::Acquire, unsafe { unprotected() });
        let hazards_marker = unsafe { &*self.hazards_marker.get() };
        for (hp, mark) in unsafe { hazards.deref() }.iter().zip(hazards_marker.iter()) {
            let ptr = unsafe { hp.assume_init_ref() }.load(Ordering::Relaxed);
            if ptr.is_null() {
                continue;
            }
            let mark = mark.unwrap();
            unsafe { mark(ptr) };
        }
    }

    #[inline]
    pub(crate) fn acquire_hp(&self) -> usize {
        let Some(hid) = (unsafe { (*self.available_hids.get()).pop() }) else {
            self.grow_hazards();
            return unsafe { (*self.available_hids.get()).pop() }.unwrap();
        };
        hid
    }

    #[cold]
    pub(crate) fn grow_hazards(&self) {
        let ebr_guard = &ebr_pin();
        let old_sh = self.hazards.load(Ordering::Relaxed, ebr_guard);
        let old_ref = unsafe { old_sh.deref() };
        let mut new: EbrOwned<[MaybeUninit<AtomicPtr<()>>]> =
            EbrOwned::init(old_ref.len().max(1) * 2);
        let half = old_ref.len();

        unsafe {
            ptr::copy(old_ref.as_ptr(), new.deref_mut().as_mut_ptr(), half);
            ptr::write_bytes(new.deref_mut().as_mut_ptr().add(half), 0, half);
        }
        self.hazards.store(new, Ordering::Release);

        unsafe {
            // FIXME: crossbeam-epoch's `defer_destroy` does not allow unsized types,
            // but defering its destruction is actually safe. Replace the following line
            // with `defer_destroy` once the following patch is accepted:
            // https://github.com/crossbeam-rs/crossbeam/pull/1201
            ebr_guard.defer_unchecked(move || old_sh.into_owned());
            (*self.hazards_marker.get()).resize(old_ref.len() * 2, None);
            (*self.available_hids.get()).extend(half..(half * 2));
        }
    }

    #[inline]
    pub(crate) fn release_hp(&self, hid: usize) {
        unsafe { (*self.available_hids.get()).push(hid) };
    }

    /// # Safety
    ///
    /// The thread must be properly pinned.
    #[inline]
    pub(crate) unsafe fn pinned_alloc_color(&self) -> Color {
        let epoch = unsafe { self.pinned_epoch() };
        match epoch.phase() {
            Phase::N => epoch.color(),
            _ => epoch.color().flip(),
        }
    }

    #[inline]
    pub(crate) fn alloc<T: TraceObj>(&self, obj: ManObj<T>, guard: &Guard) -> *mut ManObj<T> {
        let b = Box::new(obj);
        let ptr = ((&*b) as *const ManObj<T>).cast_mut();
        let b_dyn: Box<dyn MarkObj> = b;
        unsafe { self.push_fresh_obj(b_dyn, true) };

        let alloc_count = self.alloc_count.get() + 1;
        self.alloc_count.set(alloc_count);
        if alloc_count.is_multiple_of(ALLOC_HELPING_PERIOD) {
            guard.schedule_helping_collect();
        }

        ptr
    }

    /// # Safety
    ///
    /// The thread must be properly pinned.
    #[inline]
    pub(crate) unsafe fn push_fresh_obj(&self, mut obj: Box<dyn MarkObj>, newly_allocated: bool) {
        let obj_size = size_of_val(&*obj);
        let objs_index = unsafe { self.pinned_alloc_color() } as usize;
        loop {
            let slot = unsafe { &mut *self.objs[objs_index].get() };
            match slot.0.push_within_capacity(obj) {
                Ok(_) => {
                    if newly_allocated {
                        slot.1 += obj_size;
                    }
                    break;
                }
                Err(e) => {
                    obj = e;
                    unsafe { self.flush_objs() };
                }
            }
        }
    }

    #[inline]
    pub(crate) fn select_obj_shard(&self) -> usize {
        unsafe { &mut *self.rng.get() }.usize(0..OBJ_BATCHES_SHARD)
    }

    #[inline]
    pub(crate) fn generate_shard_permut(&self) -> [usize; OBJ_BATCHES_SHARD] {
        let mut result = [0, 1, 2, 3, 4, 5, 6, 7];
        unsafe { &mut *self.rng.get() }.shuffle(&mut result);
        result
    }

    /// # Safety
    ///
    /// The caller must have exclusive write permission for the object batch of the given index.
    ///
    /// For example,
    /// 1. A pinned thread has an exclusive write permission for the current `alloc_color` index.
    /// 2. The collector during RT phase has exclusive write permissions for the current
    ///    `white_color` index, for every mutator's allocation list.
    #[inline]
    pub(crate) unsafe fn take_obj_batch(&self, index: usize) -> Option<(ObjBatch, usize)> {
        let batch_and_size = unsafe {
            if (&*self.objs[index].get()).0.is_empty() {
                return None;
            }
            ptr::replace(self.objs[index].get(), Default::default())
        };
        Some(batch_and_size)
    }

    /// # Safety
    ///
    /// The thread must be properly pinned.
    #[inline]
    pub(crate) unsafe fn flush_objs(&self) {
        let alloc_color = unsafe { self.pinned_alloc_color() };
        let index = alloc_color as usize;
        let Some((batch, size_bytes)) = (unsafe { self.take_obj_batch(index) }) else {
            return;
        };
        global().push_fresh_objs(
            batch,
            size_bytes,
            alloc_color,
            self.select_obj_shard(),
            &ebr_pin(),
        );
    }

    /// # Safety
    ///
    /// * The thread must be properly pinned.
    /// * There must not be any interleaving writes.
    ///   (i.e., this local thread must have a write permission for this `Local` record.)
    #[inline]
    unsafe fn record_mt_modification(&self) {
        let epoch = unsafe { self.pinned_epoch() };
        // Safety: there will be no interleaving writes.
        let last_modified = unsafe { *(*self.mt_modified_ts).as_ptr() };
        if last_modified == epoch.timestamp() {
            return;
        }
        self.mt_modified_ts
            .store(epoch.timestamp(), Ordering::Relaxed);
        if unsafe { self.pinned_epoch() }.phase() == Phase::CT {
            atomic::fence(Ordering::SeqCst); // Sync with the collector in CT phase.
        }
    }

    #[inline]
    pub(crate) fn schedule_mark<T: TraceObj>(&self, obj: &ManObj<T>, guard: &Guard) {
        let task = Task::new(|guard| obj.mark(guard));
        let mark_task = unsafe {
            self.record_mt_modification();
            &*self.mark_tasks.get()
        };
        mark_task.push(task);

        let sched_count = self.sched_count.get() + 1;
        self.sched_count.set(sched_count);
        if sched_count.is_multiple_of(SCHED_HELPING_PERIOD) {
            guard.schedule_helping_collect();
        }
    }

    /// # Safety
    ///
    /// There must not be any interleaving writes.
    /// (i.e., this local thread must have a write permission for this `Local` record.)
    #[inline]
    pub(crate) unsafe fn pinned_epoch(&self) -> Epoch {
        unsafe { self.epoch.load_non_atomic() }
    }

    /// # Safety
    ///
    /// * The thread must be properly pinned.
    /// * There must not be any interleaving writes.
    ///   (i.e., this local thread must have a write permission for this `Local` record.)
    #[inline]
    pub(crate) fn try_pop_mark_task(&self) -> Option<Task> {
        unsafe {
            let tasks = &mut *self.mark_tasks.get();
            if !tasks.is_empty() {
                // Optimistically assume that we are going to successfully pop the task.
                // Note that recording after successfully popping the task will be too late.
                // E.g., In CT phase, a mutator pops the last mark task, and it is stalled
                // right before recording. In this case, the collector's validation succeeds,
                // prematurly transitioning to the next normal.
                self.record_mt_modification();
                if let Some(task) = tasks.pop() {
                    return Some(task);
                }
            }
        }
        None
    }

    #[inline]
    pub(crate) fn scan_or_reuse_hazards<'g>(
        &self,
        guard: &'g Guard,
        ebr_guard: &EbrGuard,
    ) -> &'g FxHashSet<*mut ()> {
        unsafe {
            let hazards = &mut *self.cached_hazards.get();
            if let Some((hazards, prev_epoch)) = hazards
                && *prev_epoch == guard.local_epoch()
            {
                return hazards;
            }
        }
        let new_hazards = global().collect_hps(ebr_guard);
        unsafe {
            let hazards = &mut *self.cached_hazards.get();
            *hazards = Some((new_hazards, guard.local_epoch()));
            &hazards.as_ref().unwrap_unchecked().0
        }
    }
}

pub struct HazardPointer {
    hid: usize,
    local: Rc<Entry<Local>>,
}

impl HazardPointer {
    pub(crate) fn new(local: Rc<Entry<Local>>) -> Self {
        let hid = local.acquire_hp();
        Self { hid, local }
    }

    pub(crate) fn protect<T: TraceObj>(&self, addr: ManPtr<T>) {
        unsafe fn mark<T: TraceObj>(ptr: *mut ()) {
            let ptr = ManPtr::<T>::from(ptr);
            unsafe { ptr.deref().mark(&pin()) };
        }

        self.hazard_slot()
            .store(addr.as_ptr().cast(), Ordering::Release);

        unsafe {
            let local = self.local.as_ref();
            let marker_ref = (&mut *local.hazards_marker.get()).get_unchecked_mut(self.hid);
            *marker_ref = if addr.is_null() {
                None
            } else {
                Some(mark::<T>)
            };
        }
    }

    pub(crate) fn clear(&self) {
        self.hazard_slot().store(ptr::null_mut(), Ordering::Release);
        unsafe {
            let local = self.local.as_ref();
            *(&mut *local.hazards_marker.get()).get_unchecked_mut(self.hid) = None;
        }
    }

    fn hazard_slot(&self) -> &AtomicPtr<()> {
        // Safety of `unprotected`: It is always safe to access my own hazard vector.
        // This is because other threads never attempt to destroy this hazard vector.
        unsafe {
            self.local
                .as_ref()
                .hazards
                .load(Ordering::Relaxed, unprotected())
                .deref()
                .get_unchecked(self.hid)
                .assume_init_ref()
        }
    }
}

impl Drop for HazardPointer {
    fn drop(&mut self) {
        self.clear();
        self.local.as_ref().release_hp(self.hid);
    }
}
