use crate::epoch::{AtomicEpoch, Epoch, Phase};
use crate::guards::{Collector, Guard, Handle};
use crate::pin;
use crate::pointers::{ManObj, ManPtr, TraceObj};
use crate::sync::{Entry, IsElement, List, fence};
use crate::task::Task;
use arrayvec::ArrayVec;
use crossbeam::deque::{Stealer, Worker};
use crossbeam::epoch::{Guard as EbrGuard, Owned, Shared as EbrShared, unprotected};
use crossbeam::utils::CachePadded;
use fastrand::Rng;
use std::cell::{Cell, UnsafeCell};
use std::mem::ManuallyDrop;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicPtr, Ordering};

const OBJ_BATCHES_SHARD: usize = 8;
const OBJ_BATCH_SIZE: usize = 64;
/// TODO: Make it resizable. We might need a dedicated SMR (e.g., EBR, HP).
const HAZARDS_COUNT: usize = 8;

/// The global data for a garbage collector.
pub(crate) struct Global {
    /// The intrusive linked list of `Local`s.
    locals: List<Local>,

    /// The global epoch.
    pub(crate) epoch: CachePadded<AtomicEpoch>,

    objs: [List<ObjBatch>; OBJ_BATCHES_SHARD],
}

#[repr(C)] // Note: `entry` must be the first field
struct ObjBatch {
    entry: Entry,
    objs: ArrayVec<Box<dyn TraceObj>, OBJ_BATCH_SIZE>,
}

impl Default for ObjBatch {
    fn default() -> Self {
        Self {
            entry: Entry::default(),
            objs: ArrayVec::default(),
        }
    }
}

impl Global {
    /// Creates a new global data for garbage collection.
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            locals: List::new(),
            epoch: CachePadded::new(AtomicEpoch::new(Epoch::starting())),
            objs: [const { List::new() }; OBJ_BATCHES_SHARD],
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

    /// The epoch that this local thread observed most recently.
    last_observed: Cell<Epoch>,

    /// A single-writer multiple-reader list of protected pointers.
    hazards: [AtomicPtr<()>; HAZARDS_COUNT],

    /// The function pointers to mark each HP-protected object.
    hazards_marker: [Cell<Option<unsafe fn(*mut ())>>; HAZARDS_COUNT],

    /// A vector of available hazard indices.
    ///
    /// When all guards and handles get dropped, this vector is destroyed.
    available_hids: UnsafeCell<ManuallyDrop<Vec<usize>>>,

    /// A local random number generater to select a shard.
    rng: UnsafeCell<ManuallyDrop<Rng>>,

    /// A local batch of newly allocated objects.
    objs: UnsafeCell<ManuallyDrop<ObjBatch>>,

    /// A local mark queue.
    mark_jobs: UnsafeCell<ManuallyDrop<Worker<Task>>>,

    /// A stealer handle for `mark_jobs`.
    mark_jobs_stealer: Stealer<Task>,

    /// The local epoch.
    epoch: CachePadded<AtomicEpoch>,
}

impl Local {
    /// Registers a new `Local` in the provided `Global`.
    pub(crate) fn register(collector: &Collector) -> Handle {
        unsafe {
            let mark_jobs = Worker::new_fifo();
            let stealer = mark_jobs.stealer();
            // Since we dereference no pointers in this block, it is safe to use `unprotected`.
            let local = Owned::new(Self {
                entry: Entry::default(),
                collector: UnsafeCell::new(ManuallyDrop::new(collector.clone())),
                guard_count: Cell::new(0),
                handle_count: Cell::new(1),
                last_observed: Cell::new(Epoch::starting()),
                hazards: [const { AtomicPtr::new(ptr::null_mut()) }; HAZARDS_COUNT],
                hazards_marker: [const { Cell::new(None) }; HAZARDS_COUNT],
                available_hids: UnsafeCell::new(ManuallyDrop::new((0..HAZARDS_COUNT).collect())),
                rng: UnsafeCell::new(ManuallyDrop::new(Rng::new())),
                objs: UnsafeCell::new(ManuallyDrop::new(ObjBatch::default())),
                mark_jobs: UnsafeCell::new(ManuallyDrop::new(mark_jobs)),
                mark_jobs_stealer: stealer,
                epoch: CachePadded::new(AtomicEpoch::new(Epoch::starting())),
            })
            .into_shared(unprotected());
            // SAFETY: `Local` is being inserted at the head of the list, so we will not
            // dereference any dangerous (e.g., retired) shared memory.
            collector.global.locals.insert(local, unprotected());
            Handle {
                local: NonNull::new_unchecked(local.as_raw().cast_mut()),
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
        let mut curr_epoch = self.global().epoch.load(Ordering::Acquire);
        loop {
            // Now we must store `new_epoch` into `self.epoch` and execute a light fence.
            self.epoch.store(curr_epoch.pinned(), Ordering::Relaxed);
            fence::light();

            let new_epoch = self.global().epoch.load(Ordering::Acquire);
            if curr_epoch == new_epoch {
                break;
            }
            curr_epoch = new_epoch;
        }

        if self.last_observed.get().timestamp() != curr_epoch.timestamp()
            && curr_epoch.phase() == Phase::RT
        {
            // Phase barrier: if we are in a root tracing phase and it’s the first time
            // observing this phase, scan and mark (i.e., push to mark stack)
            // objects that are protected by thread-local HPs.
            for (hp, mark) in self.hazards.iter().zip(self.hazards_marker.iter()) {
                let ptr = hp.load(Ordering::Relaxed);
                if ptr.is_null() {
                    continue;
                }
                let mark = mark.get().unwrap();
                unsafe { mark(ptr) };
            }
        }
        self.last_observed.set(curr_epoch);
    }

    /// Unpins the `Local`.
    #[inline]
    pub(crate) fn unpin_inner(&self) {
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
        self.unpin_inner();
        self.pin_inner();
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

    #[inline]
    pub(crate) fn alloc<T: 'static + TraceObj>(&self, obj: ManObj<T>) -> *mut ManObj<T> {
        let b = Box::new(obj);
        let ptr = ((&*b) as *const ManObj<T>).cast_mut();
        let mut b_dyn: Box<dyn TraceObj> = b;

        loop {
            match unsafe { &mut *self.objs.get() }.objs.try_push(b_dyn) {
                Ok(_) => break,
                Err(e) => {
                    b_dyn = e.element();
                    self.flush_objs();
                }
            }
        }
        ptr
    }

    #[inline]
    pub(crate) fn select_obj_shard(&self) -> usize {
        unsafe { &mut *self.rng.get() }.usize(0..OBJ_BATCHES_SHARD)
    }

    #[inline]
    pub(crate) fn flush_objs(&self) {
        unsafe {
            let batch =
                ManuallyDrop::into_inner(ptr::replace(self.objs.get(), ManuallyDrop::default()));
            let guard = unprotected();
            self.global()
                .objs
                .get_unchecked(self.select_obj_shard())
                .insert(Owned::new(batch).into_shared(guard), guard);
        }
    }

    #[inline]
    pub(crate) fn schedule_mark<T: 'static + TraceObj>(&self, obj: &ManObj<T>) {
        let task = Task::new(|| obj.mark(&pin()));
        unsafe { &mut *self.mark_jobs.get() }.push(task);
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

            // TODO: flush mark jobs
            self.flush_objs();
            ManuallyDrop::drop(&mut *self.available_hids.get());
            ManuallyDrop::drop(&mut *self.rng.get());
            ManuallyDrop::drop(&mut *self.objs.get());

            // Mark this node in the linked list as deleted.
            self.entry.delete(unprotected());

            // Finally, drop the reference to the global. Note that this might be the last reference
            // to the `Global`.
            drop(collector);
        }
    }

    #[inline]
    pub(crate) fn pinned_epoch(&self) -> Epoch {
        // Safety: there cannot be any interleaving writes because this local thread has
        // an exclusive write permission for this `Local` record.
        unsafe { self.epoch.load_non_atomic() }
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
            unsafe {
                ptr.deref().mark(&pin());
            }
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
