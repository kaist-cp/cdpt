//! Cooperative collection mode: no background thread, cycles driven on request.
//!
//! Runs as its own test binary so the process-global collection mode is isolated
//! from the other integration tests, which rely on the default threaded mode.

use std::sync::Mutex;

use cdpt::{AtomicSharedOption, CollectionMode, Shared, TraceObj, TracePtr, global, pin};

/// Serializes the tests in this binary: they share the process-global
/// collector and drive cycles from their own threads, so running them
/// concurrently would make each other's reclamation counters racy.
static SERIAL: Mutex<()> = Mutex::new(());

#[derive(TraceObj)]
struct Node {
    #[allow(dead_code)]
    value: usize,
    next: AtomicSharedOption<Self>,
}

/// In cooperative mode there is no background collector thread: a cycle only
/// makes progress when `request_collection` drives it synchronously on the
/// calling thread.
#[test]
fn cooperative_collects_on_request() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // Select the mode before the collector is ever deployed (the first `pin`
    // in this process latches it), so no background thread is spawned.
    assert!(global().set_collection_mode(CollectionMode::Cooperative));
    assert_eq!(global().collection_mode(), CollectionMode::Cooperative);

    // Allocate a batch of unreachable nodes, then drop every root.
    {
        let guard = pin();
        let mut nodes = Vec::new();
        for value in 0..2000 {
            nodes.push(Shared::new(
                Node {
                    value,
                    next: AtomicSharedOption::none(),
                },
                &guard,
            ));
        }
        drop(guard);
        drop(nodes);
    }

    assert!(
        global().estimate_total_alloc() > 0,
        "allocations should be recorded"
    );

    // With no background thread, nothing reclaims until we ask. Deferred tracing
    // reclaims a previous cycle's garbage, so drive a few cycles; each one runs
    // inline on this thread.
    let baseline = global().estimate_total_reclm();
    let mut reclaimed = baseline;
    for _ in 0..8 {
        global().request_collection();
        reclaimed = global().estimate_total_reclm();
        if reclaimed > baseline {
            break;
        }
    }

    assert!(
        reclaimed > baseline,
        "cooperative mode must reclaim garbage synchronously on request"
    );
}

/// Heap pressure alone must trigger a cycle at a safepoint (dropping the last
/// guard), without any explicit `request_collection`.
#[test]
fn cooperative_collects_at_safepoints() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    assert!(global().set_collection_mode(CollectionMode::Cooperative));

    // Churn unreachable nodes until the trigger heuristic fires and a
    // safepoint-driven cycle reclaims some of them. Deferred tracing reclaims
    // a previous cycle's garbage, and the early cycles seed the heuristic's
    // rate estimates, so allow generous headroom.
    let baseline = global().estimate_total_reclm();
    let mut reclaimed = baseline;
    for _ in 0..512 {
        {
            let guard = pin();
            for value in 0..1000 {
                let _ = Shared::new(
                    Node {
                        value,
                        next: AtomicSharedOption::none(),
                    },
                    &guard,
                );
            }
        } // the guard drops here: a safepoint

        reclaimed = global().estimate_total_reclm();
        if reclaimed > baseline {
            break;
        }
    }

    assert!(
        reclaimed > baseline,
        "safepoint-driven collection must reclaim without an explicit request"
    );
}
