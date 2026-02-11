//! Common utilities for concurrent map tests.
//!
//! This module provides shared traits, types, and test functions for testing
//! concurrent map data structures.

// Each integration test file includes this module via `mod map_common;`, compiling
// its own copy. Not every test file uses every item, so suppress per-crate warnings.
#![allow(dead_code)]

use cdpt::Handle;
use fastrand::shuffle;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{available_parallelism, scope};

/// A trait for value references returned by concurrent maps.
pub trait ValueRef<V> {
    fn borrow(&self) -> &V;
}

/// Trait for concurrent map data structures.
pub trait ConcurrentMap<K, V>: Send + Sync
where
    K: Send + Sync,
    V: Send + Sync,
{
    /// The type of value reference returned by get/remove operations.
    type ValueRef<'h>: ValueRef<V>
    where
        Self: 'h;

    /// Creates a new empty map.
    fn new() -> Self;

    /// Returns a reference to the value associated with the key.
    fn get<'h>(&self, key: &K, handle: &'h Handle) -> Option<Self::ValueRef<'h>>;

    /// Inserts a key-value pair into the map.
    /// Returns `true` if the insertion was successful (key was not present),
    /// or `false` if the key already existed.
    fn insert(&self, key: K, value: V, handle: &Handle) -> bool;

    /// Removes a key from the map and returns the associated value if present.
    fn remove<'h>(&self, key: &K, handle: &'h Handle) -> Option<Self::ValueRef<'h>>;
}

/// Configuration for smoke tests.
pub const SMOKE_THREADS: i32 = 30;
pub const SMOKE_ELEMENTS_PER_THREAD: i32 = 1000;

/// Basic smoke test for concurrent maps.
///
/// This test:
/// 1. Inserts elements from multiple threads (each thread inserts its own disjoint set)
/// 2. Removes elements from half the threads
/// 3. Verifies remaining elements can be read
pub fn smoke<M>()
where
    M: ConcurrentMap<i32, String>,
    for<'h> M::ValueRef<'h>: ValueRef<String>,
{
    use cdpt::handle;

    let map = &M::new();

    // Phase 1: Concurrent insertions
    scope(|s| {
        for t in 0..SMOKE_THREADS {
            s.spawn(move || {
                let handle = handle();
                let mut keys: Vec<i32> = (0..SMOKE_ELEMENTS_PER_THREAD)
                    .map(|k| k * SMOKE_THREADS + t)
                    .collect();
                shuffle(&mut keys);
                for i in keys {
                    assert!(map.insert(i, i.to_string(), &handle));
                }
            });
        }
    });

    // Phase 2: Concurrent removals (first half of threads' keys)
    scope(|s| {
        for t in 0..(SMOKE_THREADS / 2) {
            s.spawn(move || {
                let handle = handle();
                let mut keys: Vec<i32> = (0..SMOKE_ELEMENTS_PER_THREAD)
                    .map(|k| k * SMOKE_THREADS + t)
                    .collect();
                shuffle(&mut keys);
                for i in keys {
                    assert_eq!(
                        Some(&i.to_string()),
                        map.remove(&i, &handle).as_ref().map(|v| v.borrow())
                    );
                }
            });
        }
    });

    // Phase 3: Verify remaining elements (second half of threads' keys)
    scope(|s| {
        for t in (SMOKE_THREADS / 2)..SMOKE_THREADS {
            s.spawn(move || {
                let handle = handle();
                let mut keys: Vec<i32> = (0..SMOKE_ELEMENTS_PER_THREAD)
                    .map(|k| k * SMOKE_THREADS + t)
                    .collect();
                shuffle(&mut keys);
                for i in keys {
                    assert_eq!(
                        Some(&i.to_string()),
                        map.get(&i, &handle).as_ref().map(|v| v.borrow())
                    );
                }
            });
        }
    });
}

/// Tests basic single-threaded operations.
pub fn test_basic_operations<M>()
where
    M: ConcurrentMap<i32, String>,
    for<'h> M::ValueRef<'h>: ValueRef<String>,
{
    use cdpt::handle;

    let handle = handle();
    let map = M::new();

    // Test empty map
    assert!(map.get(&1, &handle).is_none());
    assert!(map.remove(&1, &handle).is_none());

    // Test insert and get
    assert!(map.insert(1, "one".to_string(), &handle));
    assert_eq!(
        Some(&"one".to_string()),
        map.get(&1, &handle).as_ref().map(|v| v.borrow())
    );

    // Test duplicate insert
    assert!(!map.insert(1, "one again".to_string(), &handle));
    assert_eq!(
        Some(&"one".to_string()),
        map.get(&1, &handle).as_ref().map(|v| v.borrow())
    );

    // Test remove
    assert_eq!(
        Some(&"one".to_string()),
        map.remove(&1, &handle).as_ref().map(|v| v.borrow())
    );
    assert!(map.get(&1, &handle).is_none());

    // Test double remove
    assert!(map.remove(&1, &handle).is_none());
}

/// Tests insert and remove with multiple elements.
pub fn test_multiple_elements<M>()
where
    M: ConcurrentMap<i32, String>,
    for<'h> M::ValueRef<'h>: ValueRef<String>,
{
    use cdpt::handle;

    let handle = handle();
    let map = M::new();

    // Insert elements in order
    for i in 0..100 {
        assert!(map.insert(i, i.to_string(), &handle));
    }

    // Verify all elements
    for i in 0..100 {
        assert_eq!(
            Some(&i.to_string()),
            map.get(&i, &handle).as_ref().map(|v| v.borrow())
        );
    }

    // Remove odd elements
    for i in (1..100).step_by(2) {
        assert_eq!(
            Some(&i.to_string()),
            map.remove(&i, &handle).as_ref().map(|v| v.borrow())
        );
    }

    // Verify odd elements are gone, even elements remain
    for i in 0..100 {
        if i % 2 == 0 {
            assert_eq!(
                Some(&i.to_string()),
                map.get(&i, &handle).as_ref().map(|v| v.borrow())
            );
        } else {
            assert!(map.get(&i, &handle).is_none());
        }
    }
}

/// Tests inserting elements in reverse order (stress test for tree balancing).
pub fn test_reverse_order_insert<M>()
where
    M: ConcurrentMap<i32, String>,
    for<'h> M::ValueRef<'h>: ValueRef<String>,
{
    use cdpt::handle;

    let handle = handle();
    let map = M::new();

    // Insert elements in reverse order
    for i in (0..100).rev() {
        assert!(map.insert(i, i.to_string(), &handle));
    }

    // Verify all elements
    for i in 0..100 {
        assert_eq!(
            Some(&i.to_string()),
            map.get(&i, &handle).as_ref().map(|v| v.borrow())
        );
    }
}

/// Tests concurrent insert and remove on overlapping keys.
pub fn test_concurrent_insert_remove<M>()
where
    M: ConcurrentMap<i32, String>,
    for<'h> M::ValueRef<'h>: ValueRef<String>,
{
    use cdpt::handle;

    const THREADS: usize = 8;
    const OPS_PER_THREAD: usize = 1000;

    let map = &M::new();
    let successful_inserts = &AtomicUsize::new(0);
    let successful_removes = &AtomicUsize::new(0);

    scope(|s| {
        // Half threads insert
        for _ in 0..(THREADS / 2) {
            s.spawn(|| {
                let handle = handle();
                for i in 0..OPS_PER_THREAD as i32 {
                    if map.insert(i, i.to_string(), &handle) {
                        successful_inserts.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }

        // Half threads remove
        for _ in 0..(THREADS / 2) {
            s.spawn(|| {
                let handle = handle();
                for i in 0..OPS_PER_THREAD as i32 {
                    if map.remove(&i, &handle).is_some() {
                        successful_removes.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });

    // The total successful inserts minus successful removes should equal
    // the number of elements remaining in the map
    let remaining_inserts = successful_inserts.load(Ordering::Relaxed);
    let total_removes = successful_removes.load(Ordering::Relaxed);

    // Count remaining elements
    let handle = handle();
    let mut remaining = 0;
    for i in 0..OPS_PER_THREAD as i32 {
        if map.get(&i, &handle).is_some() {
            remaining += 1;
        }
    }

    assert_eq!(remaining_inserts - total_removes, remaining);
}

/// Stress test configuration.
pub struct StressConfig {
    /// Number of threads to use.
    pub threads: usize,
    /// Number of operations per thread.
    pub ops_per_thread: usize,
    /// Range of keys to use (0..key_range).
    pub key_range: i32,
}

impl Default for StressConfig {
    fn default() -> Self {
        Self {
            threads: 16,
            ops_per_thread: 10000,
            key_range: 1000,
        }
    }
}

impl StressConfig {
    /// Configuration for list-based data structures (smaller key range for better performance).
    pub fn for_list() -> Self {
        Self {
            threads: available_parallelism().map(|t| t.get()).unwrap_or(16),
            ops_per_thread: 10 * 1000 * 1000, // 10M
            key_range: 50,                    // Smaller range for O(n) traversal
        }
    }

    /// Configuration for tree-based data structures.
    pub fn for_tree() -> Self {
        Self {
            threads: available_parallelism().map(|t| t.get()).unwrap_or(16),
            ops_per_thread: 10 * 1000 * 1000, // 10M
            key_range: 1000,
        }
    }
}

/// Stress test that performs random operations from multiple threads.
/// This test is designed to be run with address sanitizer to detect memory issues.
///
/// **Recommended:** Run stress tests with release profile for reasonable performance:
/// ```sh
/// cargo test --release --all-targets -- --ignored
/// ```
///
/// To run with address sanitizer (requires nightly):
/// ```sh
/// RUSTFLAGS="-Z sanitizer=address" cargo +nightly test --release --all-targets -- --ignored
/// ```
/// (Set `--target` for your machine: https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html)
pub fn stress_test_with_config<M>(config: StressConfig)
where
    M: ConcurrentMap<i32, String>,
    for<'h> M::ValueRef<'h>: ValueRef<String>,
{
    use cdpt::handle;

    let map = &M::new();

    scope(|s| {
        for thread_id in 0..config.threads {
            s.spawn(move || {
                let handle = handle();
                let mut rng = fastrand::Rng::with_seed(thread_id as u64);

                for _ in 0..config.ops_per_thread {
                    let key = rng.i32(0..config.key_range);
                    let op = rng.u8(0..3);

                    match op {
                        0 => {
                            // Insert
                            let _ = map.insert(key, key.to_string(), &handle);
                        }
                        1 => {
                            // Get
                            if let Some(v) = map.get(&key, &handle) {
                                // Verify the value matches the key
                                assert_eq!(v.borrow(), &key.to_string());
                            }
                        }
                        2 => {
                            // Remove
                            if let Some(v) = map.remove(&key, &handle) {
                                // Verify the removed value matches the key
                                assert_eq!(v.borrow(), &key.to_string());
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            });
        }
    });
}

/// Stress test with default configuration for trees.
pub fn stress_test<M>()
where
    M: ConcurrentMap<i32, String>,
    for<'h> M::ValueRef<'h>: ValueRef<String>,
{
    stress_test_with_config::<M>(StressConfig::for_tree());
}

/// Stress test with configuration for lists.
pub fn stress_test_list<M>()
where
    M: ConcurrentMap<i32, String>,
    for<'h> M::ValueRef<'h>: ValueRef<String>,
{
    stress_test_with_config::<M>(StressConfig::for_list());
}
