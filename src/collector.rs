use crossbeam::{epoch::pin as ebr_pin, utils::Backoff};
use std::{
    iter::repeat_with,
    mem::take,
    sync::{
        LazyLock, RwLock,
        atomic::{AtomicUsize, Ordering, fence},
    },
    thread::{sleep, spawn},
    time::{Duration, Instant},
};

use crate::{
    Handle, HeapHeadroom,
    epoch::{Color, Phase},
    global,
    internal::ObjBatch,
    task::Task,
    tls::handle,
};

use log::Logger;

#[derive(Clone)]
struct HeartbeatStats {
    measured_time: Instant,
    heap_usage: usize,
    total_alloc: usize,
    alloc_per_ms: usize,
    alloc_per_ms_smooth: usize,
}

struct CollectionStats {
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
    reclm_per_ms_smooth: AtomicUsize::new(0),
    coll_time_ms_smooth: AtomicUsize::new(0),
    desired_heap_limit: AtomicUsize::new(0),
};

const HEARTBEAT_PERIOD_MS: u64 = 500;
const ALLOC_PER_MS_SMOOTH_FACTOR: f64 = 0.5;
const RECLM_PER_MS_SMOOTH_FACTOR: f64 = 0.5;
const COLL_TIME_MS_SMOOTH_FACTOR: f64 = 0.5;
const EXTRA_TUNING_FACTOR: usize = 10;

/// A function body for the primary collector thread.
pub(crate) fn collector_loop() {
    let handle = handle();
    let logger = Logger::new();

    // Initialize stats data and spawn the heartbeat thread that periodically samples heap stats.
    {
        let mut stats = HBSTATS.write().unwrap();
        stats.total_alloc = global().estimate_total_alloc();
    }
    spawn(heartbeat_loop);

    loop {
        let backoff = Backoff::new();
        while !is_collection_necessary() {
            backoff.snooze();
        }

        let start = Instant::now();
        let recl_at_start = global().estimate_total_reclm();

        // Do actual collection works.
        logger.measure("RT", || root_tracing(&handle, &logger));
        while logger.measure("CT", || !completion_tracing(&handle, &logger)) {}
        logger.measure("N", || next_normal(&handle, &logger));

        record_collection_stats(start, recl_at_start);
    }
}

fn heartbeat_loop() {
    loop {
        sleep(Duration::from_millis(HEARTBEAT_PERIOD_MS));
        heartbeat();
    }
}

fn heartbeat() -> HeartbeatStats {
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

    // TODO: Small profiling code. Remove later...
    // fn readable_bytes(num: usize) -> String {
    //     const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    //     for (i, unit) in UNITS.iter().enumerate() {
    //         if num / 2usize.pow(i as u32 * 10) < 1000 {
    //             return format!("{:.3} {}", num as f64 / 2f64.powf(i as f64 * 10.0), unit);
    //         }
    //     }
    //     format!(
    //         "{:.3} {}",
    //         num as f64 / 2f64.powf((UNITS.len() - 1) as f64 * 10.0),
    //         UNITS.last().unwrap()
    //     )
    // }
    // println!(
    //     "heap_usage: {}, total_alloc: {}, alloc_per_ms: {}, alloc_per_ms (smooth): {} / reclm_per_ms (smooth): {} / heap_limit: {}",
    //     readable_bytes(new_total_alloc - new_total_reclm),
    //     readable_bytes(new_total_alloc),
    //     readable_bytes(new_alloc_per_ms),
    //     readable_bytes(new_alloc_per_ms_smooth),
    //     readable_bytes(CSTATS.reclm_per_ms_smooth.load(Ordering::Relaxed)),
    //     readable_bytes(CSTATS.desired_heap_limit.load(Ordering::Relaxed))
    // );

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

fn root_tracing(handle: &Handle, logger: &Logger) {
    debug_assert!(global().load_epoch().phase() == Phase::N);
    logger.measure("transition", || phase_trans(Phase::RT));

    // Before scanning:
    // * All mutators are unpinned from the normal phase.
    // * Some of them may have already observed this tracing phase, scanning their own local
    //   hazard pointers (phase barrier).
    logger.measure("scan", || scan_allocated_objs(handle));
    // After scanning:
    // * Speaking with weak tricolor invariant, both mutators’ stacks (HP) and
    //   the global region (RC) are black (rescanning isn’t needed).
    // * For HPs, it is guaranteed that all local HPs at the beginning of this phase are
    //   recognized (i.e., greyed).
    //   1. For cooperative mutators, they marked their HPs by themselves.
    //   2. For uncooperative mutators, the collector has just marked.
    // => All mutators are black thanks to Yuasa's deletion barrier.
    // * For RCs, deletion barriers by mutators and scanning by the
    //   collectors guarantee that no live objects are missed.

    logger.measure("drain", || drain_mark_tasks(handle));
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
    let hazards = global().collect_hps(ebr_guard);

    // Scan all freshly allocated objects (in `fresh_objs`)
    // and mark protected ones (moving to `marked_objs`).
    for q_idx in handle.local().generate_shard_permut() {
        let fresh_q = &global().fresh_objs[guard.white_color() as usize][q_idx];
        while let Some(batch) = fresh_q.try_pop(ebr_guard) {
            for obj in batch.iter() {
                if obj.root_count() > 0 || hazards.contains(&obj.address()) {
                    obj.mark(&guard);
                }
            }
            let marked_q_idx = handle.local().select_obj_shard();
            let marked_q = &global().marked_objs[guard.white_color() as usize][marked_q_idx];
            marked_q.push(batch, ebr_guard);
        }
    }
}

fn completion_tracing(handle: &Handle, logger: &Logger) -> bool {
    debug_assert!({
        let curr = global().epoch.load(Ordering::Acquire).phase();
        curr == Phase::RT || curr == Phase::CT
    });
    logger.measure("transition", || phase_trans(Phase::CT));

    if logger.measure("confirm", try_confirm_completion) {
        return true;
    }
    logger.measure("drain", || drain_mark_tasks(handle));
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
    if !global().mark_tasks.is_empty()
        || global()
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
    let global_inj = &global().mark_tasks;

    local_w.pop().or_else(|| {
        repeat_with(|| {
            global_inj.steal_batch_and_pop(local_w).or_else(|| {
                global()
                    .locals
                    .iter_all()
                    .map(|local| local.mark_tasks_stealer.steal())
                    .collect()
            })
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

fn next_normal(handle: &Handle, logger: &Logger) {
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
    logger.measure("sweep", || sweep(prev_epoch.color(), handle, logger));
    // For the case of very fast execution of `sweep`, we need to check and wait
    // the unpinning of all mutators.
    logger.measure("unpin", || wait_all_mutators_unpin(new_epoch.timestamp()));
}

fn sweep(prev_white: Color, handle: &Handle, _logger: &Logger) {
    let guard = &ebr_pin();

    let mut survived_batch = ObjBatch::default();
    for q_idx in handle.local().generate_shard_permut() {
        let marked_q = &global().marked_objs[prev_white as usize][q_idx];
        while let Some(batch) = marked_q.try_pop(guard) {
            let mut reclaimed_bytes = 0;
            for obj in batch.into_iter() {
                if prev_white == obj.color() {
                    reclaimed_bytes += size_of_val(&*obj);
                    drop(obj);
                    continue;
                }
                if let Err(e) = survived_batch.push_within_capacity(obj) {
                    let full = take(&mut survived_batch);
                    let next_white = prev_white.flip() as usize;
                    let shard = handle.local().select_obj_shard();
                    global().fresh_objs[next_white][shard].push(full, guard);
                    assert!(survived_batch.push_within_capacity(e).is_ok());
                }
            }
            global()
                .stats
                .total_reclaimed
                .fetch_add(reclaimed_bytes, Ordering::Release);
        }
    }

    if !survived_batch.is_empty() {
        let next_white = prev_white.flip() as usize;
        let shard = handle.local().select_obj_shard();
        global().fresh_objs[next_white][shard].push(survived_batch, guard);
    }
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

fn is_collection_necessary() -> bool {
    if !global().collection_enabled.load(Ordering::SeqCst) {
        // The user manually turned the collection off.
        CSTATS.desired_heap_limit.store(0, Ordering::Relaxed);
        return false;
    }

    // Check if an explicit collection was requested.
    if global().collection_requested.load(Ordering::SeqCst) {
        global().collection_requested.store(false, Ordering::SeqCst);
        return true;
    }

    let hbstats = HBSTATS.read().unwrap();

    let reclm_per_ms_smooth = CSTATS.reclm_per_ms_smooth.load(Ordering::Relaxed);
    let heap_usage = global().estimate_heap_usage();
    let heap_limit = CSTATS.desired_heap_limit.load(Ordering::Relaxed);

    if heap_limit < heap_usage {
        return true;
    }

    let pure_alloc_rate = hbstats.alloc_per_ms_smooth as isize - reclm_per_ms_smooth as isize;
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

#[cfg(feature = "logging")]
mod log {
    use std::cell::Cell;
    use std::time::Instant;

    pub struct Logger {
        birth: Instant,
        depth: Cell<usize>,
        at_new_line: Cell<bool>,
    }

    impl Logger {
        pub fn new() -> Self {
            Self {
                birth: Instant::now(),
                depth: Cell::new(0),
                at_new_line: Cell::new(true),
            }
        }

        fn log(&self, text: &str) {
            eprint!("[{:05}ms] {text}", self.birth.elapsed().as_millis());
        }

        pub fn measure<R>(&self, name: &str, f: impl FnOnce() -> R) -> R {
            let depth = self.depth.get();
            self.depth.set(depth + 1);
            if !self.at_new_line.get() {
                eprintln!();
            }
            self.log(&format!("{}{}", "  ".repeat(depth), name));
            self.at_new_line.set(false);

            let start = Instant::now();
            let result = f();
            let end = Instant::now();

            let time_str = format!("- {}ms", (end - start).as_millis());
            if self.at_new_line.get() {
                self.log(&format!("{}{}\n", "  ".repeat(depth), time_str));
            } else {
                eprintln!("{time_str}");
            }
            self.at_new_line.set(true);
            self.depth.set(depth);
            result
        }
    }
}

#[cfg(not(feature = "logging"))]
mod log {
    pub struct Logger;

    impl Logger {
        pub fn new() -> Self {
            Logger
        }

        pub fn measure<R>(&self, _: &str, f: impl FnOnce() -> R) -> R {
            f()
        }
    }
}
