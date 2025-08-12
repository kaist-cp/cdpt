use crate::collector::collector_loop;
use crate::epoch::{AtomicEpoch, Color, Epoch, Phase};
use crate::guards::{Guard, Handle};
use crate::pointers::{ManObj, ManPtr, MarkObj, TraceObj};
use crate::sync::{Entry, IsElement, List, Queue, fence};
use crate::task::Task;
use crate::{global, pin};
use crossbeam::deque::{Injector, Stealer, Worker};
use crossbeam::epoch::{
    Guard as EbrGuard, Owned, Shared as EbrShared, pin as ebr_pin, unprotected,
};
use crossbeam::utils::CachePadded;
use fastrand::Rng;
use rustc_hash::FxHashSet;
use std::array::from_fn;
use std::cell::{Cell, UnsafeCell};
use std::mem::{ManuallyDrop, forget};
use std::ptr::{self, NonNull};
use std::sync::atomic::{self, AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::thread::spawn;

const OBJ_BATCHES_SHARD: usize = 8;
pub(crate) const OBJ_BATCH_SIZE: usize = 64;
/// TODO: Make it resizable. We might need a dedicated SMR (e.g., EBR, HP).
const HAZARDS_COUNT: usize = 8;
const ALLOC_HELPING_PERIOD: usize = 64;

/// The global data for a garbage collector.
pub struct Global {
    /// The intrusive linked list of `Local`s.
    pub(crate) locals: CachePadded<List<Local>>,

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
            locals: CachePadded::new(List::new()),
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

    pub(crate) fn iter_locals<'g>(
        &'g self,
        guard: &'g EbrGuard,
    ) -> impl Iterator<Item = &'g Local> {
        self.locals.iter(guard).map(|r| r.unwrap())
    }

    pub(crate) fn collect_hps(&self, guard: &EbrGuard) -> FxHashSet<*mut ()> {
        self.iter_locals(guard)
            .flat_map(|local| local.hazards.iter().map(|hp| hp.load(Ordering::Relaxed)))
            .collect::<_>()
    }
}

/// Participant for garbage collection.
#[repr(C)] // Note: `entry` must be the first field
pub(crate) struct Local {
    /// A node in the intrusive linked list of `Local`s.
    entry: Entry,

    /// The number of guards keeping this participant pinned.
    guard_count: Cell<usize>,

    /// The number of active handles.
    handle_count: Cell<usize>,

    /// The epoch that this local thread observed most recently.
    last_observed: Cell<Epoch>,

    /// An allocation counter to periodically trigger helping collection.
    alloc_count: Cell<usize>,

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
    available_hids: UnsafeCell<ManuallyDrop<Vec<usize>>>,

    /// A local random number generater to select a shard.
    rng: UnsafeCell<ManuallyDrop<Rng>>,

    /// A local batch of newly allocated objects.
    ///
    /// It is sharded by the allocation color. That is, the thread push the allocated object
    /// reference to the index 0 if `guard.alloc_color() == C0`. In this way, the collector
    /// can take the object list of the previous normal phase without breaking lock-freedom of
    /// mutators, because the mutator can still modify its black object list during RT & CT phase
    /// while the collector accesses the white object list, which is from the previous N phase.
    pub(crate) objs: [UnsafeCell<ManuallyDrop<ObjBatch>>; 2],

    /// A local mark queue.
    pub(crate) mark_tasks: UnsafeCell<ManuallyDrop<Worker<Task>>>,

    /// A previously collected hazards that may be reused for later helpings.
    pub(crate) cached_hazards: UnsafeCell<ManuallyDrop<Option<(FxHashSet<*mut ()>, Epoch)>>>,
}

impl Local {
    /// Registers a new `Local` in the provided `Global`.
    pub(crate) fn register() -> Handle {
        global().initialize_if_necessary();
        unsafe {
            let mark_tasks = Worker::new_fifo();
            let stealer = mark_tasks.stealer();
            // Since we dereference no pointers in this block, it is safe to use `unprotected`.
            let local = Owned::new(Self {
                entry: Entry::default(),
                guard_count: Cell::new(0),
                handle_count: Cell::new(1),
                last_observed: Cell::new(Epoch::starting()),
                alloc_count: Cell::new(0),
                hazards: [const { AtomicPtr::new(ptr::null_mut()) }; HAZARDS_COUNT],
                hazards_marker: [const { Cell::new(None) }; HAZARDS_COUNT],
                mark_tasks_stealer: stealer,
                epoch: CachePadded::new(AtomicEpoch::new(Epoch::starting())),
                mt_modified_ts: CachePadded::new(AtomicUsize::new(0)),
                available_hids: UnsafeCell::new(ManuallyDrop::new((0..HAZARDS_COUNT).collect())),
                rng: UnsafeCell::new(ManuallyDrop::new(Rng::new())),
                objs: [UnsafeCell::default(), UnsafeCell::default()],
                mark_tasks: UnsafeCell::new(ManuallyDrop::new(mark_tasks)),
                cached_hazards: UnsafeCell::default(),
            })
            .into_shared(unprotected());
            // SAFETY: `Local` is being inserted at the head of the list, so we will not
            // dereference any dangerous (e.g., retired) shared memory.
            global().locals.insert(local, unprotected());
            Handle {
                local: NonNull::new_unchecked(local.as_raw().cast_mut()),
            }
        }
    }

    /// Returns `true` if the current participant is pinned.
    #[inline]
    pub(crate) fn is_pinned(&self) -> bool {
        self.guard_count.get() > 0
    }

    /// Pins the `Local`.
    #[inline]
    pub(crate) fn pin(&self) -> Guard {
        let guard = Guard {
            local: NonNull::from_ref(self),
        };
        self.pin_inner();
        guard
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

            let handle_count = self.handle_count.get();
            if handle_count == 0 {
                // This local thread is about to be finalized.
                // Safety: The thread is not yet unpinned.
                unsafe { self.flush_before_finalization() };
            }

            self.epoch.store(Epoch::starting(), Ordering::Release);
            if handle_count == 0 {
                self.finalize();
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
        for (hp, mark) in self.hazards.iter().zip(self.hazards_marker.iter()) {
            let ptr = hp.load(Ordering::Relaxed);
            if ptr.is_null() {
                continue;
            }
            let mark = mark.get().unwrap();
            unsafe { mark(ptr) };
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

        if guard_count == 0 && handle_count == 1 {
            // This `Local` is about to be finalized.
            // Before the finalization, we need to temporarily pin the thread
            // and flush all remaining marking tasks and newly allocated objects.
            let guard = self.pin();
            unsafe { self.flush_before_finalization() };
            drop(guard);
        }

        self.handle_count.set(handle_count - 1);
        if guard_count == 0 && handle_count == 1 {
            self.finalize();
        }
    }

    #[inline]
    pub(crate) fn acquire_hp(&self) -> usize {
        let Some(hid) = (unsafe { (**self.available_hids.get()).pop() }) else {
            unimplemented!("implement growable HP");
        };
        hid
    }

    #[inline]
    pub(crate) fn release_hp(&self, hid: usize) {
        unsafe { (**self.available_hids.get()).push(hid) };
    }

    /// # Safety
    ///
    /// The thread must be properly pinned.
    #[inline]
    pub(crate) unsafe fn pinned_alloc_color(&self) -> Color {
        let temp_guard = Guard {
            local: NonNull::from_ref(self),
        };
        let color = temp_guard.alloc_color();
        forget(temp_guard);
        color
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
            ManuallyDrop::into_inner(ptr::replace(
                self.objs[index].get(),
                ManuallyDrop::default(),
            ))
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

    /// # Safety
    ///
    /// * The thread must be properly pinned.
    /// * There must not be any interleaving writes.
    ///   (i.e., this local thread must have a write permission for this `Local` record.)
    #[inline]
    pub(crate) unsafe fn flush_mark_tasks(&self) {
        unsafe {
            self.record_mt_modification();
            let tasks = &mut *self.mark_tasks.get();
            while let Some(task) = tasks.pop() {
                global().mark_tasks.push(task);
            }
        }
    }

    /// # Safety
    ///
    /// * The thread must be properly pinned.
    /// * There must not be any interleaving writes.
    ///   (i.e., this local thread must have a write permission for this `Local` record.)
    #[inline]
    pub(crate) unsafe fn schedule_mark<T: 'static + TraceObj>(&self, obj: &ManObj<T>) {
        let task = Task::new(|| obj.mark(&pin()));
        unsafe {
            self.record_mt_modification();
            (&mut *self.mark_tasks.get()).push(task);
        }
    }

    /// # Safety
    ///
    /// * The thread must be properly pinned.
    /// * There must not be any interleaving writes.
    ///   (i.e., this local thread must have a write permission for this `Local` record.)
    #[inline]
    pub(crate) unsafe fn flush_before_finalization(&self) {
        unsafe {
            // TODO: objs should be safe to dereference by the collector, even after finalization.
            // for i in 0..self.objs.len() {
            //     self.flush_objs(i);
            // }
            self.flush_mark_tasks();
        }
    }

    /// Removes the `Local` from the global linked list.
    #[cold]
    fn finalize(&self) {
        debug_assert_eq!(self.guard_count.get(), 0);
        debug_assert_eq!(self.handle_count.get(), 0);
        debug_assert!(unsafe { (&*self.mark_tasks.get()).is_empty() });
        // TODO: objs should be safe to dereference by the collector, even after finalization.
        // debug_assert!(unsafe { (0..self.objs.len()).all(|i| (&*self.objs[i].get()).objs.is_empty()) });

        unsafe {
            // The rest of `UnsafeCell`s are good to be dropped.
            ManuallyDrop::drop(&mut *self.available_hids.get());
            ManuallyDrop::drop(&mut *self.rng.get());
            ManuallyDrop::drop(&mut *self.mark_tasks.get());
            // TODO: objs should be safe to dereference by the collector, even after finalization.
            // for i in 0..self.objs.len() {
            //     ManuallyDrop::drop(&mut *self.objs[i].get());
            // }

            // TODO: use grow-only, recyclable registration list.
            // Mark this node in the linked list as deleted.
            // self.entry.delete(unprotected());
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
            if let Some(task) = tasks.pop() {
                self.record_mt_modification();
                return Some(task);
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
            let hazards = &mut **self.cached_hazards.get();
            if let Some((hazards, prev_epoch)) = hazards {
                if *prev_epoch == guard.local_epoch() {
                    return hazards;
                }
            }
        }
        let new_hazards = global().collect_hps(ebr_guard);
        unsafe {
            let hazards = &mut **self.cached_hazards.get();
            *hazards = Some((new_hazards, guard.local_epoch()));
            &hazards.as_ref().unwrap_unchecked().0
        }
    }
}

pub struct HazardPointer {
    hid: usize,
    local: NonNull<Local>,
}

impl HazardPointer {
    pub(crate) fn new(local: &Local) -> Self {
        let hid = local.acquire_hp();
        Self {
            hid,
            local: NonNull::from_ref(local),
        }
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
        unsafe {
            self.clear();
            self.local.as_ref().release_hp(self.hid);
        }
    }
}

impl IsElement<Self> for ObjBatch {
    fn entry_of(batch: &Self) -> &Entry {
        // SAFETY: `Local` is `repr(C)` and `entry` is the first field of it.
        unsafe {
            let entry_ptr = (batch as *const Self).cast::<Entry>();
            &*entry_ptr
        }
    }

    unsafe fn element_of(entry: &Entry) -> &Self {
        // SAFETY: `Local` is `repr(C)` and `entry` is the first field of it.
        unsafe {
            let batch_ptr = (entry as *const Entry).cast::<Self>();
            &*batch_ptr
        }
    }

    unsafe fn finalize(entry: &Entry, guard: &EbrGuard) {
        unsafe { guard.defer_destroy(EbrShared::from(Self::element_of(entry) as *const _)) }
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
