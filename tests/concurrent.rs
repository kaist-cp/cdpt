use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use cdpt::{
    AtomicSharedOption, CollectionMode, Local, Shared, TraceObj, TracePtr, global, handle, pin,
};

#[derive(TraceObj)]
struct Node {
    value: usize,
    next: AtomicSharedOption<Self>,
}

// ---- Multi-threaded Shared tests ----

#[test]
fn shared_across_threads() {
    let guard = pin();
    let shared = Shared::new(
        Node {
            value: 42,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    drop(guard);

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let s = shared.clone();
            thread::spawn(move || {
                assert_eq!(s.value, 42);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn shared_clone_across_threads() {
    let guard = pin();
    let shared = Shared::new(
        Node {
            value: 99,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    drop(guard);

    thread::scope(|s| {
        for _ in 0..8 {
            let c = shared.clone();
            s.spawn(move || {
                // Each thread clones again and reads.
                let c2 = c.clone();
                assert_eq!(c2.value, 99);
            });
        }
    });
}

// ---- Multi-threaded AtomicSharedOption tests ----

#[test]
fn concurrent_store_load() {
    let guard = pin();
    let atomic = AtomicSharedOption::some(
        Node {
            value: 0,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    drop(guard);

    // Wrap in a struct to share across threads.
    let atomic = &atomic;

    thread::scope(|s| {
        // Writer threads.
        for i in 1..=4 {
            s.spawn(move || {
                let h = handle();
                let guard = h.pin();
                let node = Local::new(
                    Node {
                        value: i,
                        next: AtomicSharedOption::none(),
                    },
                    &guard,
                );
                atomic.store(Some(&node), Ordering::Release, &guard);
            });
        }

        // Reader threads.
        for _ in 0..4 {
            s.spawn(move || {
                let h = handle();
                let guard = h.pin();
                // Just verify we can load without crashing.
                if let Some(loaded) = atomic.load(Ordering::Acquire, &guard) {
                    assert!(loaded.value <= 4);
                }
            });
        }
    });
}

#[test]
fn concurrent_compare_exchange() {
    let guard = pin();
    let counter_node = AtomicSharedOption::some(
        Node {
            value: 0,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    drop(guard);

    let counter_node = &counter_node;
    let success_count = &AtomicUsize::new(0);

    thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                let h = handle();
                let guard = h.pin();
                let current = counter_node.load(Ordering::Acquire, &guard);
                let new_val = current.as_ref().map(|c| c.value + 1).unwrap_or(1);
                let new_node = Local::new(
                    Node {
                        value: new_val,
                        next: AtomicSharedOption::none(),
                    },
                    &guard,
                );

                if counter_node
                    .compare_exchange(
                        current.as_ref(),
                        Some(&new_node),
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                        &guard,
                    )
                    .is_ok()
                {
                    success_count.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    // At least one CAS should have succeeded.
    assert!(success_count.load(Ordering::Relaxed) >= 1);
}

#[test]
fn concurrent_swap() {
    let guard = pin();
    let slot = AtomicSharedOption::some(
        Node {
            value: 0,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    drop(guard);

    let slot = &slot;

    thread::scope(|s| {
        for i in 1..=8 {
            s.spawn(move || {
                let h = handle();
                let guard = h.pin();
                let new_node = Local::new(
                    Node {
                        value: i,
                        next: AtomicSharedOption::none(),
                    },
                    &guard,
                );
                let _old = slot.swap(Some(&new_node), Ordering::AcqRel, &guard);
            });
        }
    });

    // After all swaps, the slot should contain one of the values.
    let guard = pin();
    let final_val = slot.load(Ordering::Acquire, &guard).unwrap();
    assert!(final_val.value >= 1 && final_val.value <= 8);
}

// ---- Multi-threaded allocation tests ----

#[test]
fn concurrent_allocations() {
    const THREADS: usize = 8;
    const ALLOCS_PER_THREAD: usize = 200;

    thread::scope(|s| {
        for t in 0..THREADS {
            s.spawn(move || {
                let h = handle();
                let guard = h.pin();
                for i in 0..ALLOCS_PER_THREAD {
                    let node = Local::new(
                        Node {
                            value: t * ALLOCS_PER_THREAD + i,
                            next: AtomicSharedOption::none(),
                        },
                        &guard,
                    );
                    assert_eq!(node.value, t * ALLOCS_PER_THREAD + i);
                }
            });
        }
    });
}

#[test]
fn concurrent_shared_allocations() {
    const THREADS: usize = 8;
    const ALLOCS_PER_THREAD: usize = 100;

    thread::scope(|s| {
        for t in 0..THREADS {
            s.spawn(move || {
                let guard = pin();
                let mut shareds = Vec::new();
                for i in 0..ALLOCS_PER_THREAD {
                    shareds.push(Shared::new(
                        Node {
                            value: t * ALLOCS_PER_THREAD + i,
                            next: AtomicSharedOption::none(),
                        },
                        &guard,
                    ));
                }
                for (i, s) in shareds.iter().enumerate() {
                    assert_eq!(s.value, t * ALLOCS_PER_THREAD + i);
                }
            });
        }
    });
}

// ---- Pinning from multiple threads ----

#[test]
fn multi_thread_pinning() {
    thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                let h = handle();
                for _ in 0..100 {
                    let guard = h.pin();
                    let _ = Local::new(
                        Node {
                            value: 1,
                            next: AtomicSharedOption::none(),
                        },
                        &guard,
                    );
                    drop(guard);
                }
            });
        }
    });
}

// ---- Guard repin under contention ----

#[test]
fn concurrent_repin() {
    thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                let h = handle();
                let mut guard = h.pin();
                for i in 0..50 {
                    let _ = Local::new(
                        Node {
                            value: i,
                            next: AtomicSharedOption::none(),
                        },
                        &guard,
                    );
                    guard.repin();
                }
            });
        }
    });
}

// ---- HP-protected reference across guard ----

#[test]
fn hp_protected_ref_survives_guard_drop() {
    let h = handle();

    let hp_ref;
    {
        let guard = h.pin();
        let local = Local::new(
            Node {
                value: 123,
                next: AtomicSharedOption::none(),
            },
            &guard,
        );
        // Protect with Handle -> creates hazard pointer.
        hp_ref = local.protect(&h);
    }
    // Guard is dropped, but HP-protected reference is still valid.
    assert_eq!(hp_ref.value, 123);
}

// ---- Concurrent linked list building ----

#[test]
fn concurrent_list_build() {
    let head: AtomicSharedOption<Node> = AtomicSharedOption::none();
    let head = &head;

    thread::scope(|s| {
        for t in 0..4 {
            s.spawn(move || {
                let h = handle();
                let guard = h.pin();
                for i in 0..50 {
                    let node = Local::new(
                        Node {
                            value: t * 50 + i,
                            next: AtomicSharedOption::none(),
                        },
                        &guard,
                    );
                    loop {
                        let old = head.load(Ordering::Acquire, &guard);
                        node.next.store(old.as_ref(), Ordering::Relaxed, &guard);
                        if head
                            .compare_exchange(
                                old.as_ref(),
                                Some(&node),
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                                &guard,
                            )
                            .is_ok()
                        {
                            break;
                        }
                    }
                }
            });
        }
    });

    // Count all nodes in the list.
    let guard = pin();
    let mut count = 0;
    let mut curr = head.load(Ordering::Acquire, &guard);
    while let Some(node) = curr {
        count += 1;
        curr = node.next.load(Ordering::Acquire, &guard);
    }
    assert_eq!(count, 4 * 50);
}

// ---- Help collect ----

#[test]
fn help_collect_does_not_crash() {
    let guard = pin();
    // Allocate some objects to create work.
    for i in 0..100 {
        let _ = Local::new(
            Node {
                value: i,
                next: AtomicSharedOption::none(),
            },
            &guard,
        );
    }
    // Calling help_collect should not panic.
    guard.help_collect();
}

#[test]
fn handle_help_collect() {
    let h = handle();
    let guard = h.pin();
    for i in 0..100 {
        let _ = Local::new(
            Node {
                value: i,
                next: AtomicSharedOption::none(),
            },
            &guard,
        );
    }
    drop(guard);
    // Calling help_collect on handle should not panic.
    h.help_collect();
}

/// The collection mode is latched by the collector's deployment: once a pin
/// has happened in this process (threaded default here), switching to
/// cooperative must be refused, so a background collector and mutator drivers
/// can never coexist. Re-requesting the latched mode still reports success.
#[test]
fn collection_mode_latched_after_deploy() {
    drop(pin());
    assert_eq!(global().collection_mode(), CollectionMode::Threaded);
    assert!(!global().set_collection_mode(CollectionMode::Cooperative));
    assert!(global().set_collection_mode(CollectionMode::Threaded));
    assert_eq!(global().collection_mode(), CollectionMode::Threaded);
}
