//! Platform abstraction for the collector's runtime.
//!
//! This splits along one axis, the target's thread capability. Where threads
//! exist, deployment and shard parallelism honor the selected
//! [`CollectionMode`](crate::CollectionMode):
//! [`Threaded`](crate::CollectionMode::Threaded) runs the collector on a
//! background thread, samples heap stats on a heartbeat thread, and
//! parallelizes mark and sweep across scoped workers;
//! [`Cooperative`](crate::CollectionMode::Cooperative) spawns nothing and runs
//! shard work inline. The wasm32 fallback has no threads at all: deployment is
//! a no-op, shard work always runs inline, and `Instant` is a no-op clock.
//!
//! Who *drives* a cycle in cooperative mode is platform-independent: mutators
//! call `collector::drive_collection_if_necessary` from safepoints and from
//! `Global::request_collection`.

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use std::ops::Range;
    use std::thread::{self, sleep, spawn};
    use std::time::Duration;

    use crossbeam::utils::Backoff;

    use crate::CollectionMode;
    use crate::collector::{drive_collection_if_necessary, heartbeat, init_heartbeat_stats};
    use crate::internal::OBJ_BATCHES_SHARD;

    pub(crate) use std::time::Instant;

    const HEARTBEAT_PERIOD_MS: u64 = 500;

    /// Deploys the background collector thread when the selected mode calls for
    /// one. In cooperative mode nothing is spawned; cycles run synchronously on
    /// the requesting thread instead.
    pub(crate) fn deploy_collector() {
        if crate::global().collection_mode() == CollectionMode::Threaded {
            spawn(collector_loop);
        }
    }

    /// A function body for the primary collector thread.
    fn collector_loop() {
        let handle = crate::handle();

        // Initialize stats data and spawn the heartbeat thread that periodically samples heap stats.
        init_heartbeat_stats();
        spawn(heartbeat_loop);

        loop {
            let backoff = Backoff::new();
            while !drive_collection_if_necessary(&handle) {
                backoff.snooze();
            }
        }
    }

    fn heartbeat_loop() {
        loop {
            sleep(Duration::from_millis(HEARTBEAT_PERIOD_MS));
            heartbeat();
        }
    }

    /// Picks the default collector thread count: one-eighth of the available
    /// parallelism, clamped to `1..=OBJ_BATCHES_SHARD`. Falls back to `1` when
    /// the platform cannot report parallelism.
    pub(crate) fn default_collector_threads() -> usize {
        let parallelism = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        (parallelism / 8).clamp(1, OBJ_BATCHES_SHARD)
    }

    /// Runs `work` over the `0..OBJ_BATCHES_SHARD` shard range. Cooperative mode
    /// and a single-thread request run it inline on the calling thread.
    /// Otherwise `num_threads` scoped workers each take a contiguous slice, and
    /// the last picks up any remainder when the range is not divisible.
    pub(crate) fn parallel_shard_work<F>(num_threads: usize, work: F)
    where
        F: Fn(Range<usize>) + Sync,
    {
        if num_threads <= 1 || crate::global().collection_mode() == CollectionMode::Cooperative {
            work(0..OBJ_BATCHES_SHARD);
            return;
        }
        thread::scope(|s| {
            for thread_idx in 0..num_threads {
                let work = &work;
                s.spawn(move || {
                    let base = OBJ_BATCHES_SHARD / num_threads;
                    let start = thread_idx * base;
                    let end = if thread_idx == num_threads - 1 {
                        OBJ_BATCHES_SHARD
                    } else {
                        start + base
                    };
                    work(start..end);
                });
            }
        });
    }
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use std::ops::Range;

    use crate::internal::OBJ_BATCHES_SHARD;

    /// A stand-in monotonic clock. `wasm32-unknown-unknown` has no
    /// `Instant::now()`, and the heartbeat heuristic it feeds is unused while
    /// collection is request-driven, so every instant compares equal (zero
    /// elapsed).
    #[derive(Clone, Copy)]
    pub(crate) struct Instant;

    impl Instant {
        pub(crate) fn now() -> Self {
            Self
        }
    }

    impl std::ops::Sub for Instant {
        type Output = std::time::Duration;

        fn sub(self, _rhs: Self) -> std::time::Duration {
            std::time::Duration::ZERO
        }
    }

    /// No background thread to deploy.
    pub(crate) fn deploy_collector() {}

    pub(crate) fn default_collector_threads() -> usize {
        1
    }

    pub(crate) fn parallel_shard_work<F>(_num_threads: usize, work: F)
    where
        F: Fn(Range<usize>),
    {
        work(0..OBJ_BATCHES_SHARD);
    }
}

pub(crate) use imp::*;
