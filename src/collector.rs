use crossbeam::{deque::Steal, epoch::pin as ebr_pin, utils::Backoff};
use std::{
    iter::repeat_with,
    mem::take,
    sync::{
        LazyLock, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering, fence},
    },
};

use crate::{
    Handle, HeapHeadroom,
    epoch::{Color, Phase},
    global,
    internal::{OBJ_BATCHES_SHARD, ObjBatch},
    platform::{Instant, parallel_shard_work},
    task::Task,
};

use rustc_hash::FxHashSet;

#[derive(Clone)]
pub(crate) struct HeartbeatStats {
    measured_time: Instant,
    heap_usage: usize,
    total_alloc: usize,
    alloc_per_ms: usize,
    alloc_per_ms_smooth: usize,
}

struct CollectionStats {
    alloc_per_ms_smooth: AtomicUsize,
    reclm_per_ms_smooth: AtomicUsize,
    coll_time_ms_smooth: AtomicUsize,
    desired_heap_limit: AtomicUsize,
}

static HBSTATS: LazyLock<RwLock<HeartbeatStats>> = LazyLock::new(|| {
    RwLock::new(HeartbeatStats {
        measured_time: Instant::now(),
        heap_usage: 0,
        total_alloc: 0,
        alloc_per_ms: 0,
        alloc_per_ms_smooth: 0,
    })
});

static CSTATS: CollectionStats = CollectionStats {
    alloc_per_ms_smooth: AtomicUsize::new(0),
    reclm_per_ms_smooth: AtomicUsize::new(0),
    coll_time_ms_smooth: AtomicUsize::new(0),
    desired_heap_limit: AtomicUsize::new(0),
};

const ALLOC_PER_MS_SMOOTH_FACTOR: f64 = 0.5;
const RECLM_PER_MS_SMOOTH_FACTOR: f64 = 0.5;
const COLL_TIME_MS_SMOOTH_FACTOR: f64 = 0.5;
const EXTRA_TUNING_FACTOR: usize = 10;
const LOCAL_MARK_TASKS_BATCH_LIMIT: usize = 2048;

/// Seeds the heartbeat stats baseline before the background sampler starts.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn init_heartbeat_stats() {
    let mut stats = HBSTATS.write().unwrap();
    stats.total_alloc = global().estimate_total_alloc();
}

/// Serializes cycle-driving: whichever caller wins becomes the collector for
/// one cycle. In threaded mode the background thread is the only caller; in
/// cooperative mode mutators race here from safepoints and requests, and
/// losers simply skip — a cycle is already in progress. The flag also keeps a
/// driving thread from re-entering through the pin/unpin its own cycle does.
static DRIVING: AtomicBool = AtomicBool::new(false);

struct DrivingGuard;

impl Drop for DrivingGuard {
    fn drop(&mut self) {
        DRIVING.store(false, Ordering::Release);
    }
}

/// Runs one collection cycle on the calling thread when the heuristic (or an
/// explicit request) calls for one and no other thread is already driving.
/// Returns whether a cycle ran.
pub(crate) fn drive_collection_if_necessary(handle: &Handle) -> bool {
    if !is_collection_necessary() {
        return false;
    }
    if DRIVING
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    let _driving = DrivingGuard;

    // Re-check now that we are the driver: the cycle we raced with may have
    // already resolved the pressure.
    let collected = is_collection_necessary();
    if collected {
        // This cycle serves any pending explicit request. A request arriving
        // mid-cycle leaves the flag set for the next check.
        global().collection_requested.swap(false, Ordering::SeqCst);
        collect_once(handle);
    }
    collected
}

fn collect_once(handle: &Handle) {
    let start = Instant::now();
    let recl_at_start = global().estimate_total_reclm();

    root_tracing(handle);
    while !completion_tracing(handle) {}
    next_normal(handle);

    record_collection_stats(start, recl_at_start);
}

pub(crate) fn heartbeat() -> HeartbeatStats {
    let mut stats = HBSTATS.write().unwrap();
    let now = Instant::now();
    let dur = now - stats.measured_time;

    if dur.as_millis() == 0 {
        // Very low chance, but possible...
        return stats.clone();
    }

    let new_total_alloc = global().estimate_total_alloc();
    let new_total_reclm = global().estimate_total_reclm();
    let alloc_diff = new_total_alloc - stats.total_alloc;
    let new_alloc_per_ms = alloc_diff / (dur.as_millis() as usize);

    let prev_alloc_per_ms_smooth = stats.alloc_per_ms_smooth;
    let new_alloc_per_ms_smooth = smooth(
        ALLOC_PER_MS_SMOOTH_FACTOR,
        prev_alloc_per_ms_smooth,
        new_alloc_per_ms,
    );

    stats.measured_time = now;
    stats.total_alloc = new_total_alloc;
    stats.heap_usage = new_total_alloc - new_total_reclm;
    stats.alloc_per_ms = new_alloc_per_ms;
    stats.alloc_per_ms_smooth = new_alloc_per_ms_smooth;
    CSTATS
        .alloc_per_ms_smooth
        .store(new_alloc_per_ms_smooth, Ordering::Relaxed);

    stats.clone()
}

fn record_collection_stats(start: Instant, recl_at_start: usize) {
    let end = Instant::now();
    let alloc_at_end = global().estimate_total_alloc();
    let recl_at_end = global().estimate_total_reclm();

    let prev_coll_time_ms = CSTATS.coll_time_ms_smooth.load(Ordering::Relaxed);
    // For some workloads with small heaps, `(end - start).as_millis()` may be 0,
    // generating a nonsense reclamation rate. So, we want it to be at least 1.
    let curr_coll_time_ms = (end - start).as_millis().max(1) as usize;
    let new_coll_time_ms = smooth(
        COLL_TIME_MS_SMOOTH_FACTOR,
        prev_coll_time_ms,
        curr_coll_time_ms,
    );

    let prev_reclm_rate = CSTATS.reclm_per_ms_smooth.load(Ordering::Relaxed);
    let curr_reclm_rate =
        (((recl_at_end - recl_at_start) as f64) / (curr_coll_time_ms as f64)) as usize;
    let new_reclm_rate = smooth(RECLM_PER_MS_SMOOTH_FACTOR, prev_reclm_rate, curr_reclm_rate);

    let hbstats = heartbeat();

    // Calculate the desired heap limit (Membalancer).
    let heap_usage = alloc_at_end - recl_at_end;
    #[allow(clippy::manual_checked_ops)]
    let extra = if new_reclm_rate == 0 {
        global().locals.active_count() * 1024 * 1024 // 1MB per thread
    } else {
        let headroom_min = match global().heap_headroom() {
            HeapHeadroom::FixedMiB(mib) => mib * 1024 * 1024,
            HeapHeadroom::Proportional(divisor) => (heap_usage / divisor).max(16 * 1024),
        };
        ((heap_usage * hbstats.alloc_per_ms_smooth / new_reclm_rate / EXTRA_TUNING_FACTOR) as f64)
            .sqrt()
            .max(headroom_min as f64) as usize
    };
    let desired_heap_limit = heap_usage + extra;

    CSTATS
        .reclm_per_ms_smooth
        .store(new_reclm_rate, Ordering::Relaxed);
    CSTATS
        .coll_time_ms_smooth
        .store(new_coll_time_ms, Ordering::Relaxed);
    CSTATS
        .desired_heap_limit
        .store(desired_heap_limit, Ordering::Relaxed);
}

fn smooth(factor: f64, prev: usize, curr: usize) -> usize {
    if prev == 0 {
        // If the previos value was 0, we do not smooth.
        curr
    } else {
        (factor * (prev as f64) + (1.0 - factor) * (curr as f64)) as usize
    }
}

fn root_tracing(handle: &Handle) {
    debug_assert!(global().load_epoch().phase() == Phase::N);
    phase_trans(Phase::RT);

    // Before scanning:
    // * All mutators are unpinned from the normal phase.
    // * Some of them may have already observed this tracing phase, scanning their own local
    //   hazard pointers (phase barrier).
    scan_allocated_objs(handle);
    // After scanning:
    // * Speaking with weak tricolor invariant, both mutators' stacks (HP) and
    //   the global region (RC) are black (rescanning isn't needed).
    // * For HPs, it is guaranteed that all local HPs at the beginning of this phase are
    //   recognized (i.e., greyed).
    //   1. For cooperative mutators, they marked their HPs by themselves.
    //   2. For uncooperative mutators, the collector has just marked.
    // => All mutators are black thanks to Yuasa's deletion barrier.
    // * For RCs, deletion barriers by mutators and scanning by the
    //   collectors guarantee that no live objects are missed.

    drain_mark_tasks(handle);
}

fn scan_allocated_objs(handle: &Handle) {
    let guard = handle.pin();
    let ebr_guard = &ebr_pin();

    // Make sure that there's no pending freshly allocated objects in locals.
    let pending = global()
        .locals
        .iter_all()
        .flat_map(|local| unsafe { local.take_obj_batch(guard.white_color() as usize) });

    for (batch, size_bytes) in pending {
        global().push_fresh_objs(
            batch,
            size_bytes,
            guard.white_color(),
            handle.local().select_obj_shard(),
            ebr_guard,
        );
    }

    // Scan HPs first. And they will be marked during the RC scan.
    // Re-key the set as `usize` so it implements `Sync` and can be shared
    // with worker threads (raw pointers are `!Sync`; we only need the bit
    // pattern for equality comparison here).
    let hazards: FxHashSet<usize> = global()
        .collect_hps(ebr_guard)
        .into_iter()
        .map(|p| p as usize)
        .collect();

    let white_color = guard.white_color();
    let num_threads = global().collector_threads();

    // Scan all freshly allocated objects (in `fresh_objs`)
    // and mark protected ones (moving to `marked_objs`).
    parallel_shard_work(num_threads, |range| {
        let guard = crate::pin();
        let ebr_guard = &ebr_pin();
        let rng = &mut fastrand::Rng::new();

        for q_idx in range {
            let fresh_q = &global().fresh_objs[white_color as usize][q_idx];
            while let Some(batch) = fresh_q.try_pop(ebr_guard) {
                for obj in batch.iter() {
                    if obj.root_count() > 0 || hazards.contains(&(obj.address() as usize)) {
                        obj.mark(&guard);
                    }
                }
                let marked_q_idx = rng.usize(0..OBJ_BATCHES_SHARD);
                let marked_q = &global().marked_objs[white_color as usize][marked_q_idx];
                marked_q.push(batch, ebr_guard);
            }
        }
    });
}

fn completion_tracing(handle: &Handle) -> bool {
    debug_assert!({
        let curr = global().epoch.load(Ordering::Acquire).phase();
        curr == Phase::RT || curr == Phase::CT
    });
    phase_trans(Phase::CT);

    if try_confirm_completion() {
        return true;
    }
    drain_mark_tasks(handle);
    false
}

fn try_confirm_completion() -> bool {
    // The main objective of this function is to check whether there were a non-empty mark queue
    // when each mutator thread closed its last critical section.
    let curr_ts = global().load_epoch().timestamp();

    // The 1st iteration of reading `mt_modified_ts` flags.
    // If any of them are the latest timestamp, we assume that the tracing is not done.
    if global()
        .locals
        .iter_all()
        .any(|local| local.mt_modified_ts.load(Ordering::Relaxed) == curr_ts)
    {
        return false;
    }

    // Check whether there's a non-empty mark queue.
    if global()
        .locals
        .iter_all()
        .any(|local| !local.mark_tasks_stealer.is_empty())
    {
        return false;
    }

    // The 2nd iteration of reading `mt_modified_ts` flags.
    // All mutators first record the timestamp before modifying their mark queues,
    // so even if the collector misses a non-empty mark queue due to races,
    // the collector can recognize that the `mt_modified_ts` flag changed.
    // Note: we do not need `fence(SeqCst)` here, as `Stealer::is_empty` above already uses it.
    if global()
        .locals
        .iter_all()
        .any(|local| local.mt_modified_ts.load(Ordering::Relaxed) == curr_ts)
    {
        return false;
    }

    true
}

/// Returns `true` if it executed something.
fn drain_mark_tasks(handle: &Handle) -> bool {
    let guard = &handle.pin();
    let mut executed = false;
    while let Some(task) = find_task(handle) {
        executed = true;
        task.call(guard);
    }
    executed
}

fn find_task(handle: &Handle) -> Option<Task> {
    let local_w = unsafe { &*handle.local().mark_tasks.get() };

    local_w.pop().or_else(|| {
        repeat_with(|| {
            global()
                .locals
                .iter_all()
                .map(|local| {
                    local
                        .mark_tasks_stealer
                        .steal_batch_with_limit_and_pop(local_w, LOCAL_MARK_TASKS_BATCH_LIMIT)
                })
                .collect::<Steal<Task>>()
        })
        .find(|s| !s.is_retry())
        .and_then(|s| s.success())
    })
}

fn phase_trans(new: Phase) {
    let epoch = global().load_epoch();
    let new_epoch = epoch.with_timestamp(epoch.timestamp() + 1).with_phase(new);
    global().epoch.store(new_epoch, Ordering::Release);
    fence(Ordering::SeqCst);
    wait_all_mutators_unpin(new_epoch.timestamp());
}

fn next_normal(handle: &Handle) {
    let prev_epoch = global().load_epoch();
    debug_assert!(prev_epoch.phase() == Phase::CT);
    debug_assert!(find_task(handle).is_none());

    let new_epoch = prev_epoch
        .with_timestamp(prev_epoch.timestamp() + 1)
        .with_phase(Phase::N)
        .with_color(prev_epoch.color().flip());
    global().epoch.store(new_epoch, Ordering::Release);
    fence(Ordering::SeqCst);

    // Reclaim unmarked objects from the previous cycle.
    sweep(prev_epoch.color(), handle);
    // For the case of very fast execution of `sweep`, we need to check and wait
    // the unpinning of all mutators.
    wait_all_mutators_unpin(new_epoch.timestamp());
}

fn sweep(prev_white: Color, _handle: &Handle) {
    let next_white = prev_white.flip() as usize;
    let num_threads = global().collector_threads();

    parallel_shard_work(num_threads, |range| {
        let ebr_guard = &ebr_pin();
        let mut survived_batch = ObjBatch::default();
        let mut reclaimed_bytes = 0usize;
        let rng = &mut fastrand::Rng::new();

        for q_idx in range {
            let marked_q = &global().marked_objs[prev_white as usize][q_idx];
            while let Some(batch) = marked_q.try_pop(ebr_guard) {
                for obj in batch.into_iter() {
                    if prev_white == obj.color() {
                        reclaimed_bytes += size_of_val(&*obj);
                        drop(obj);
                        continue;
                    }
                    if let Err(e) = survived_batch.push_within_capacity(obj) {
                        let full = take(&mut survived_batch);
                        let shard = rng.usize(0..OBJ_BATCHES_SHARD);
                        global().fresh_objs[next_white][shard].push(full, ebr_guard);
                        assert!(survived_batch.push_within_capacity(e).is_ok());
                    }
                }
            }
        }

        if !survived_batch.is_empty() {
            let shard = rng.usize(0..OBJ_BATCHES_SHARD);
            global().fresh_objs[next_white][shard].push(survived_batch, ebr_guard);
        }

        global()
            .stats
            .total_reclaimed
            .fetch_add(reclaimed_bytes, Ordering::Release);
    });
}

fn wait_all_mutators_unpin(new_ts: usize) {
    // Loop until all mutators unpin from the previous phase.
    for local in global().locals.iter_using() {
        let backoff = Backoff::new();
        let mut local_epoch;
        loop {
            local_epoch = local.epoch.load(Ordering::Acquire);
            if !local_epoch.is_pinned() || new_ts <= local_epoch.timestamp() {
                break;
            }
            backoff.snooze();
        }
    }
}

/// Read-only trigger check; the pending-request flag is consumed by
/// `drive_collection_if_necessary` when a cycle actually starts.
fn is_collection_necessary() -> bool {
    if !global().collection_enabled.load(Ordering::SeqCst) {
        // The user manually turned the collection off.
        CSTATS.desired_heap_limit.store(0, Ordering::Relaxed);
        return false;
    }

    // Check if an explicit collection was requested.
    if global().collection_requested.load(Ordering::SeqCst) {
        return true;
    }

    let heap_usage = global().estimate_heap_usage();
    let heap_limit = CSTATS.desired_heap_limit.load(Ordering::Relaxed);

    if heap_limit < heap_usage {
        return true;
    }

    let alloc_per_ms_smooth = CSTATS.alloc_per_ms_smooth.load(Ordering::Relaxed);
    let reclm_per_ms_smooth = CSTATS.reclm_per_ms_smooth.load(Ordering::Relaxed);

    let pure_alloc_rate = alloc_per_ms_smooth as isize - reclm_per_ms_smooth as isize;
    if pure_alloc_rate < 0 {
        return heap_usage >= heap_limit;
    }

    // Calculate how much time we have before (conceptual) OOM.
    let pure_alloc_rate = pure_alloc_rate as usize;
    if pure_alloc_rate == 0 {
        return false;
    }
    let oom_time_ms = (heap_limit - heap_usage) / pure_alloc_rate;

    // We need to start collection if we expect OOM before it ends.
    oom_time_ms <= CSTATS.coll_time_ms_smooth.load(Ordering::Relaxed)
}
