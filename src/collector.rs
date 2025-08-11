use crossbeam::{
    epoch::{Guard as EbrGuard, pin as ebr_pin},
    utils::Backoff,
};
use rustc_hash::FxHashSet;
use std::{iter::repeat_with, sync::atomic::Ordering, thread::sleep, time::Duration};

use crate::{
    Handle,
    epoch::{Color, Phase},
    global,
    internal::{Local, OBJ_BATCH_SIZE, ObjBatch},
    sync::fence,
    task::Task,
    tls::handle,
};

use log::Logger;

/// A function body for the primary collector thread.
pub(crate) fn collector_loop() {
    let handle = handle();
    let logger = Logger::new();

    loop {
        while !is_collection_necessary() {
            sleep(Duration::from_millis(1));
        }
        let objs = logger.measure("RT", || root_tracing(&handle, &logger));
        while logger.measure("CT", || !completion_tracing(&handle, &logger)) {}
        logger.measure("N", || next_normal(objs, &handle, &logger));
    }
}

#[must_use]
fn root_tracing(handle: &Handle, logger: &Logger) -> Vec<Box<ObjBatch>> {
    debug_assert!(global().load_epoch().phase() == Phase::N);
    logger.measure("transition", || phase_trans(Phase::RT));

    // Before scanning:
    // * All mutators are unpinned from the normal phase.
    // * Some of them may have already observed this tracing phase, scanning their own local
    //   hazard pointers (phase barrier).
    let obj_batches = {
        let guard = handle.pin();

        // It would be good to create an iterator for allocated objects as soon as
        // the completion of phase transition, to scan less objects.
        let obj_taken = logger.measure("take objects", || {
            global()
                .objs
                .iter()
                .flat_map(|list| list.take())
                .chain(iter_locals(&ebr_pin()).flat_map(|local| unsafe {
                    local.take_obj_batch(guard.white_color() as usize)
                }))
                .collect::<Vec<_>>()
        });

        // Scan HPs first. And they will be marked during the RC scan.
        let hazards = logger.measure("scan HP", || {
            iter_locals(&ebr_pin())
                .flat_map(|local| local.hazards.iter().map(|hp| hp.load(Ordering::Relaxed)))
                .collect::<FxHashSet<_>>()
        });

        // TODO: parallelize if it helps (profiling should be necessary).
        logger.measure("mark", || {
            for batch in &obj_taken {
                for obj in &batch.0 {
                    if obj.root_count() > 0 || hazards.contains(&obj.address()) {
                        obj.mark(&guard);
                    }
                }
            }
        });
        obj_taken
    };
    // After scanning:
    // * Speaking with weak tricolor invariant, both mutators’ stacks (HP) and
    //   the global region (RC) are black (rescanning isn’t needed).
    // * For HPs, it is guaranteed that all local HPs at the beginning of this phase are
    //   recognized (i.e., greyed).
    //   1. For cooperative mutators, they marked their HPs by themselves.
    //   2. For uncooperative mutators, the collector has just marked.
    // => All mutators are grey (not black due to insertion barrier).
    // * For RCs, deletion barriers by mutators and scanning by the
    //   collectors guarantee that no live objects are missed.

    logger.measure("drain", || drain_mark_tasks(handle));
    obj_batches
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
    return false;
}

fn try_confirm_completion() -> bool {
    // The main objective of this function is to check whether there were a non-empty mark queue
    // when each mutator thread closed its last critical section.
    let curr_ts = global().load_epoch().timestamp();

    let ebr_guard = ebr_pin();
    // The 1st iteration of reading `mt_modified_ts` flags.
    // If any of them are the latest timestamp, we assume that the tracing is not done.
    if iter_locals(&ebr_guard).any(|local| local.mt_modified_ts.load(Ordering::Relaxed) == curr_ts)
    {
        return false;
    }

    // Check whether there's a non-empty mark queue.
    if !global().mark_tasks.is_empty()
        || iter_locals(&ebr_guard).any(|local| !local.mark_tasks_stealer.is_empty())
    {
        return false;
    }

    // The 2nd iteration of reading `mt_modified_ts` flags.
    // All mutators first record the timestamp before modifying their mark queues,
    // so even if the collector misses a non-empty mark queue due to races,
    // the collector can recognize that the `mt_modified_ts` flag changed.
    // Note: we do not need `fence(SeqCst)` here, as `Stealer::is_empty` above already uses it.
    if iter_locals(&ebr_guard).any(|local| local.mt_modified_ts.load(Ordering::Relaxed) == curr_ts)
    {
        return false;
    }

    true
}

/// Returns `true` if it executed something.
fn drain_mark_tasks(handle: &Handle) -> bool {
    let mut executed = false;
    while let Some(task) = find_task(handle) {
        executed = true;
        task.call();
    }
    executed
}

fn find_task(handle: &Handle) -> Option<Task> {
    let local_w = unsafe { &**handle.local().mark_tasks.get() };
    let global_inj = &global().mark_tasks;

    local_w.pop().or_else(|| {
        repeat_with(|| {
            global_inj.steal_batch_and_pop(local_w).or_else(|| {
                iter_locals(&ebr_pin())
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
    fence::heavy();
    wait_all_mutators_unpin(new_epoch.timestamp());
}

fn next_normal(obj_batches: Vec<Box<ObjBatch>>, handle: &Handle, logger: &Logger) {
    let prev_epoch = global().load_epoch();
    debug_assert!(prev_epoch.phase() == Phase::CT);
    debug_assert!(find_task(handle).is_none());

    let new_epoch = prev_epoch
        .with_timestamp(prev_epoch.timestamp() + 1)
        .with_phase(Phase::N)
        .with_color(prev_epoch.color().flip());
    global().epoch.store(new_epoch, Ordering::Release);
    fence::heavy();

    // Reclaim unmarked objects from the previous cycle.
    logger.measure("sweep", || {
        sweep(obj_batches, prev_epoch.color(), &handle, logger)
    });
    // For the case of very fast execution of `sweep`, we need to check and wait
    // the unpinning of all mutators.
    logger.measure("unpin", || wait_all_mutators_unpin(new_epoch.timestamp()));
}

fn sweep(obj_batches: Vec<Box<ObjBatch>>, prev_white: Color, handle: &Handle, logger: &Logger) {
    let survived = logger.measure("sweep and free", || {
        obj_batches
            .into_iter()
            .flat_map(|batch| batch.0.into_iter())
            .filter_map(|obj| {
                if prev_white == obj.color() {
                    None // `drop(obj)` is called implicitly.
                } else {
                    Some(obj)
                }
            })
            .collect::<Vec<_>>()
    });

    let batch_count = survived.len().div_ceil(OBJ_BATCH_SIZE);
    let mut batches: Vec<Box<ObjBatch>> = Vec::with_capacity(batch_count);
    batches.push(Box::default());
    logger.measure("pack", || {
        for obj in survived {
            if let Err(e) = batches.last_mut().unwrap().0.push_within_capacity(obj) {
                batches.push(Box::default());
                batches.last_mut().unwrap().0.push(e);
            }
        }
    });
    logger.measure("push batch", || {
        global().objs[handle.local().select_obj_shard()].push_batch(batches.into_iter())
    });
}

fn wait_all_mutators_unpin(new_ts: usize) {
    // Loop until all mutators unpin from the previous phase.
    for local in iter_locals(&ebr_pin()) {
        let backoff = Backoff::new();
        let mut local_epoch;
        loop {
            local_epoch = local.epoch.load(Ordering::Relaxed);
            if !local_epoch.is_pinned() || new_ts <= local_epoch.timestamp() {
                break;
            }
            backoff.spin();
        }
    }
}

fn is_collection_necessary() -> bool {
    // TODO: How long should it sleep? Heuristic. E.g., MemBalancer
    true
}

fn iter_locals(guard: &EbrGuard) -> impl Iterator<Item = &'_ Local> {
    global().locals.iter(guard).map(|r| r.unwrap())
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
