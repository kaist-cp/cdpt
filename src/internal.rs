use crate::collector::collector_loop;
use crate::epoch::{AtomicEpoch, Color, Epoch, Phase};
use crate::guards::{Guard, Handle};
use crate::pointers::{ManObj, ManPtr, MarkObj, TraceObj};
use crate::sync::{Entry, Queue, ReusableSlots, fence};
use crate::task::Task;
use crate::{global, pin};
use crossbeam::deque::{Injector, Stealer, Worker};
use crossbeam::epoch::pin as ebr_pin;
use crossbeam::utils::CachePadded;
use fastrand::Rng;
use rustc_hash::FxHashSet;
use std::array::from_fn;
use std::cell::{Cell, UnsafeCell};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{self, AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::thread::spawn;

const OBJ_BATCHES_SHARD: usize = 8;
pub(crate) const OBJ_BATCH_SIZE: usize = 64;
/// TODO: Make it resizable. We might need a dedicated SMR (e.g., EBR, HP).
const HAZARDS_COUNT: usize = 8;
const ALLOC_HELPING_PERIOD: usize = 64;
const SCHED_HELPING_PERIOD: usize = OBJ_BATCH_SIZE / 2;

/// The global data for a garbage collector.
pub struct Global {
    /// The intrusive linked list of `Local`s.
    pub(crate) locals: CachePadded<ReusableSlots<Local>>,

    /// The global epoch.
    pub(crate) epoch: CachePadded<AtomicEpoch>,

    /// `fresh_objs` and `marked_objs`: the global sharded object lists.
    ///
    /// The first index represents the allocation color when the object is allocated.
    pub(crate) fresh_objs: [[Queue<ObjBatch>; OBJ_BATCHES_SHARD]; 2],
    pub(crate) marked_objs: [[Queue<ObjBatch>; OBJ_BATCHES_SHARD]; 2],

    /// The global marking tasks.
    pub(crate) mark_tasks: Injector<Task>,

    /// The global flag indicating whether the collector is online.
    collector_init: CachePadded<AtomicBool>,
}

unsafe impl Sync for Global {}
unsafe impl Send for Global {}

pub(crate) struct ObjBatch(pub Vec<Box<dyn MarkObj>>);

impl Default for ObjBatch {
    fn default() -> Self {
        Self::with_capacity(OBJ_BATCH_SIZE)
    }
}

impl ObjBatch {
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }
}

impl Global {
    /// Creates a new global data for garbage collection.
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            locals: CachePadded::new(ReusableSlots::default()),
            epoch: CachePadded::new(AtomicEpoch::new(Epoch::starting())),
            fresh_objs: from_fn(|_| from_fn(|_| Queue::new())),
            marked_objs: from_fn(|_| from_fn(|_| Queue::new())),
            mark_tasks: Injector::new(),
            collector_init: CachePadded::new(AtomicBool::new(false)),
        }
    }

    /// Freshly loads the global epoch value. It does not execute any fences.
    #[inline]
    pub(crate) fn load_epoch(&self) -> Epoch {
        self.epoch.load(Ordering::Relaxed)
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

    pub(crate) fn collect_hps(&self) -> FxHashSet<*mut ()> {
        self.locals
            .iter_using()
            .flat_map(|local| local.hazards.iter().map(|hp| hp.load(Ordering::Relaxed)))
            .collect::<_>()
    }
}

// TODO: add finalization (all handles are dead) trait. Call self.finalize().
/// Participant for garbage collection.
pub(crate) struct Local {
    /// The number of guards keeping this participant pinned.
    guard_count: Cell<usize>,

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
    pub(crate) hazards: [AtomicPtr<()>; HAZARDS_COUNT],

    /// The function pointers to mark each HP-protected object.
    hazards_marker: [Cell<Option<unsafe fn(*mut ())>>; HAZARDS_COUNT],

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
    pub(crate) objs: [UnsafeCell<ObjBatch>; 2],

    /// A local mark queue.
    pub(crate) mark_tasks: UnsafeCell<Worker<Task>>,

    /// A previously collected hazards that may be reused for later helpings.
    pub(crate) cached_hazards: UnsafeCell<Option<(FxHashSet<*mut ()>, Epoch)>>,
}

impl Default for Local {
    fn default() -> Self {
        let mark_tasks = Worker::new_fifo();
        let stealer = mark_tasks.stealer();
        Self {
            guard_count: Cell::new(0),
            last_observed: Cell::new(Epoch::starting()),
            alloc_count: Cell::new(0),
            sched_count: Cell::new(0),
            is_helping_normal: Cell::new(false),
            is_helping_root_tracing: Cell::new(false),
            is_helping_draining_mark_tasks: Cell::new(false),
            hazards: [const { AtomicPtr::new(ptr::null_mut()) }; HAZARDS_COUNT],
            hazards_marker: [const { Cell::new(None) }; HAZARDS_COUNT],
            mark_tasks_stealer: stealer,
            epoch: CachePadded::new(AtomicEpoch::new(Epoch::starting())),
            mt_modified_ts: CachePadded::new(AtomicUsize::new(0)),
            available_hids: UnsafeCell::new((0..HAZARDS_COUNT).collect()),
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
            self.epoch.store(curr_epoch.pinned(), Ordering::Relaxed);
            fence::light();

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
        for (hp, mark) in self.hazards.iter().zip(self.hazards_marker.iter()) {
            let ptr = hp.load(Ordering::Relaxed);
            if ptr.is_null() {
                continue;
            }
            let mark = mark.get().unwrap();
            unsafe { mark(ptr) };
        }
    }

    #[inline]
    pub(crate) fn acquire_hp(&self) -> usize {
        let Some(hid) = (unsafe { (*self.available_hids.get()).pop() }) else {
            unimplemented!("implement growable HP");
        };
        hid
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
    pub(crate) fn alloc<T: 'static + TraceObj>(
        &self,
        obj: ManObj<T>,
        guard: &Guard,
    ) -> *mut ManObj<T> {
        let b = Box::new(obj);
        let ptr = ((&*b) as *const ManObj<T>).cast_mut();
        let b_dyn: Box<dyn MarkObj> = b;
        unsafe { self.push_fresh_obj(b_dyn) };

        let alloc_count = self.alloc_count.get() + 1;
        self.alloc_count.set(alloc_count);
        if alloc_count % ALLOC_HELPING_PERIOD == 0 {
            guard.help_collect();
        }

        ptr
    }

    /// # Safety
    ///
    /// The thread must be properly pinned.
    #[inline]
    pub(crate) unsafe fn push_fresh_obj(&self, mut obj: Box<dyn MarkObj>) {
        let objs_index = unsafe { self.pinned_alloc_color() } as usize;
        loop {
            match unsafe { &mut *self.objs[objs_index].get() }
                .0
                .push_within_capacity(obj)
            {
                Ok(_) => break,
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
    pub(crate) unsafe fn take_obj_batch(&self, index: usize) -> Option<ObjBatch> {
        let batch = unsafe {
            if (&*self.objs[index].get()).0.is_empty() {
                return None;
            }
            ptr::replace(self.objs[index].get(), Default::default())
        };
        Some(batch)
    }

    /// # Safety
    ///
    /// The thread must be properly pinned.
    #[inline]
    pub(crate) unsafe fn flush_objs(&self) {
        let index = unsafe { self.pinned_alloc_color() } as usize;
        let Some(batch) = (unsafe { self.take_obj_batch(index) }) else {
            return;
        };
        unsafe {
            global()
                .fresh_objs
                .get_unchecked(index)
                .get_unchecked(self.select_obj_shard())
                .push(batch, &ebr_pin());
        }
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
    pub(crate) fn schedule_mark<T: 'static + TraceObj>(&self, obj: &ManObj<T>, guard: &Guard) {
        let task = Task::new(|| obj.mark(&pin()));
        unsafe {
            self.record_mt_modification();
            (&mut *self.mark_tasks.get()).push(task);
        }

        let sched_count = self.sched_count.get() + 1;
        self.sched_count.set(sched_count);
        if sched_count % SCHED_HELPING_PERIOD == 0 {
            guard.help_draining_mark_tasks();
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
                self.record_mt_modification();
            }
            if let Some(task) = tasks.pop() {
                return Some(task);
            }
        }
        None
    }

    #[inline]
    pub(crate) fn scan_or_reuse_hazards<'g>(&self, guard: &'g Guard) -> &'g FxHashSet<*mut ()> {
        unsafe {
            let hazards = &mut *self.cached_hazards.get();
            if let Some((hazards, prev_epoch)) = hazards {
                if *prev_epoch == guard.local_epoch() {
                    return hazards;
                }
            }
        }
        let new_hazards = global().collect_hps();
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

    pub(crate) fn protect<T: 'static + TraceObj>(&self, addr: ManPtr<T>) {
        unsafe fn mark<T: 'static + TraceObj>(ptr: *mut ()) {
            let ptr = ManPtr::<T>::from(ptr);
            unsafe { ptr.deref().mark(&pin()) };
        }

        unsafe {
            let local = self.local.as_ref();
            local
                .hazards
                .get_unchecked(self.hid)
                .store(addr.as_ptr().cast(), Ordering::Release);

            let marker_ref = local.hazards_marker.get_unchecked(self.hid);
            if addr.is_null() {
                marker_ref.set(None);
            } else {
                marker_ref.set(Some(mark::<T>));
            };
        }
    }

    pub(crate) fn clear(&self) {
        unsafe {
            let local = self.local.as_ref();
            local
                .hazards
                .get_unchecked(self.hid)
                .store(ptr::null_mut(), Ordering::Release);
            local.hazards_marker.get_unchecked(self.hid).set(None);
        }
    }
}

impl Drop for HazardPointer {
    fn drop(&mut self) {
        self.clear();
        self.local.as_ref().release_hp(self.hid);
    }
}
