//! A lock-free, insert-only hash set with fixed capacity.
//!
//! [`FixedHashSet`] is a concurrent hash set backed by a flat array of atomic
//! pointers. It supports only insertion and lookup; entries are never removed
//! or reclaimed, which eliminates the need for a memory reclamation scheme.
//!
//! # Design
//!
//! - **Open addressing with linear probing.**  Each slot is an [`AtomicPtr`]
//!   that transitions from null to a leaked `Box<T>` exactly once.
//! - **Lock-free insertion.**  A thread claims a slot by CAS-ing it from null.
//!   On CAS failure it re-reads the slot and either finds a duplicate or probes
//!   the next slot.
//! - **Allocation-deferred.**  A heap allocation is performed only when the
//!   probe reaches a null slot (i.e., the key is not already present).  Lookups
//!   of existing keys are allocation-free.
//!
//! # Capacity
//!
//! The capacity `N` is fixed at construction time (as a const generic).  It
//! **must** be a power of two so that index masking works correctly.  The set
//! panics on insertion if the table is full.

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    hint::cold_path,
    ptr::null_mut,
    sync::atomic::{AtomicPtr, Ordering},
};

/// A lock-free, insert-only hash set with a fixed capacity of `N` slots.
///
/// `N` must be a power of two.  Entries are heap-allocated on insertion and
/// never freed until the set itself is dropped, which makes the set safe to
/// use from concurrent threads without a memory reclamation scheme.
pub struct FixedHashSet<T, const N: usize> {
    slots: [AtomicPtr<T>; N],
}

// SAFETY: The only shared mutable state is the array of `AtomicPtr`s, which
// are themselves `Send + Sync`.  The pointees are immutable after insertion
// (unless `T` has interior mutability, which is the caller's responsibility).
unsafe impl<T: Send + Sync, const N: usize> Send for FixedHashSet<T, N> {}
unsafe impl<T: Send + Sync, const N: usize> Sync for FixedHashSet<T, N> {}

impl<T, const N: usize> FixedHashSet<T, N> {
    /// Creates an empty set.
    ///
    /// All slots are initialized to null.  This is a `const fn`, so the set
    /// can be placed in a `static`.
    pub const fn new() -> Self {
        assert!(N > 0, "FixedHashSet capacity must be positive");
        assert!(
            N & (N - 1) == 0,
            "FixedHashSet capacity must be a power of two"
        );
        Self {
            slots: [const { AtomicPtr::new(null_mut()) }; N],
        }
    }
}

impl<T, const N: usize> Default for FixedHashSet<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Hash + Eq, const N: usize> FixedHashSet<T, N> {
    /// Inserts `key` into the set if it is not already present.
    ///
    /// Returns a reference to the entry in the set (either the newly inserted
    /// one or the existing duplicate) and a `bool` that is `true` when the key
    /// was newly inserted.
    ///
    /// The returned reference is valid for the lifetime of `&self`.  Since
    /// entries are never removed, callers holding a `&'static Self` effectively
    /// get `&'static T`.
    ///
    /// # Panics
    ///
    /// Panics if the table is full (all `N` slots are occupied by distinct
    /// keys).
    pub fn get_or_insert(&self, key: T) -> (&T, bool) {
        debug_assert!(N.is_power_of_two(), "N must be a power of two");
        let mask = N - 1;
        let hash = Self::hash_key(&key);

        // Hot path: probe for an existing entry.  At steady state almost every
        // call finds its key here and returns without allocating.
        let mut null_idx = None;
        for i in 0..N {
            let idx = (hash + i) & mask;
            let ptr = self.slots[idx].load(Ordering::Acquire);
            if ptr.is_null() {
                null_idx = Some(idx);
                break;
            }
            let existing = unsafe { &*ptr };
            if existing == &key {
                return (existing, false);
            }
        }

        // Cold path: key not found — allocate and try to claim a slot.
        // This runs at most once per distinct key over the lifetime of the set.
        cold_path();
        let allocated = Box::into_raw(Box::new(key));
        let mut idx = null_idx.expect("FixedHashSet is full");

        loop {
            let slot = &self.slots[idx];
            let ptr = slot.load(Ordering::Acquire);

            if ptr.is_null() {
                match slot.compare_exchange(
                    null_mut(),
                    allocated,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        return (unsafe { &*allocated }, true);
                    }
                    Err(winner) => {
                        // Another thread claimed this slot — check its key.
                        let winner_ref = unsafe { &*winner };
                        if winner_ref == unsafe { &*allocated } {
                            unsafe { drop(Box::from_raw(allocated)) };
                            return (winner_ref, false);
                        }
                        // Different key — probe forward.
                        idx = (idx + 1) & mask;
                        continue;
                    }
                }
            }

            // Slot occupied — check for duplicate.
            let existing = unsafe { &*ptr };
            if existing == unsafe { &*allocated } {
                unsafe { drop(Box::from_raw(allocated)) };
                return (existing, false);
            }
            idx = (idx + 1) & mask;
        }
    }

    #[cfg(test)]
    fn insert(&self, key: T) -> bool {
        self.get_or_insert(key).1
    }

    #[cfg(test)]
    fn contains(&self, key: &T) -> bool {
        debug_assert!(N.is_power_of_two(), "N must be a power of two");
        let mask = N - 1;
        let hash = Self::hash_key(key);

        for i in 0..N {
            let idx = (hash + i) & mask;
            let ptr = self.slots[idx].load(Ordering::Acquire);
            if ptr.is_null() {
                return false;
            }
            if unsafe { &*ptr } == key {
                return true;
            }
        }
        false
    }

    fn hash_key(key: &T) -> usize {
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        h.finish() as usize
    }
}

impl<T, const N: usize> Drop for FixedHashSet<T, N> {
    fn drop(&mut self) {
        for slot in &self.slots {
            let ptr = slot.load(Ordering::Relaxed);
            if !ptr.is_null() {
                unsafe { drop(Box::from_raw(ptr)) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn insert_and_contains() {
        let set = FixedHashSet::<u64, 16>::new();
        assert!(set.insert(1));
        assert!(set.insert(2));
        assert!(set.insert(3));
        assert!(set.contains(&1));
        assert!(set.contains(&2));
        assert!(set.contains(&3));
        assert!(!set.contains(&4));
    }

    #[test]
    fn duplicate_insert_returns_false() {
        let set = FixedHashSet::<u64, 16>::new();
        assert!(set.insert(42));
        assert!(!set.insert(42));
        assert!(!set.insert(42));
        assert!(set.contains(&42));
    }

    #[test]
    fn get_or_insert_returns_same_ref() {
        let set = FixedHashSet::<u64, 16>::new();
        let (r1, inserted1) = set.get_or_insert(42);
        assert!(inserted1);
        assert_eq!(*r1, 42);

        let (r2, inserted2) = set.get_or_insert(42);
        assert!(!inserted2);
        assert!(std::ptr::eq(r1, r2));
    }

    #[test]
    fn insert_up_to_capacity() {
        let set = FixedHashSet::<u64, 8>::new();
        for i in 0..8 {
            assert!(set.insert(i));
        }
        for i in 0..8 {
            assert!(set.contains(&i));
            assert!(!set.insert(i));
        }
    }

    #[test]
    fn static_set() {
        static SET: FixedHashSet<u64, 16> = FixedHashSet::new();
        assert!(SET.insert(100));
        assert!(!SET.insert(100));
        assert!(SET.contains(&100));
    }

    #[test]
    fn string_keys() {
        let set = FixedHashSet::<String, 16>::new();
        assert!(set.insert("hello".to_string()));
        assert!(!set.insert("hello".to_string()));
        assert!(set.insert("world".to_string()));
        assert!(set.contains(&"hello".to_string()));
        assert!(!set.contains(&"missing".to_string()));
    }

    #[test]
    fn concurrent_insert_no_duplicates() {
        static SET: FixedHashSet<u64, 256> = FixedHashSet::new();
        let barrier = Arc::new(Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    let mut inserted = 0u64;
                    // Each thread inserts the same 32 keys.
                    for i in 0..32 {
                        if SET.insert(i) {
                            inserted += 1;
                        }
                    }
                    inserted
                })
            })
            .collect();

        let total_inserted: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        // Exactly 32 distinct keys, so exactly 32 true insertions across all threads.
        assert_eq!(total_inserted, 32);
        for i in 0..32 {
            assert!(SET.contains(&i));
        }
    }

    #[test]
    fn concurrent_insert_disjoint_keys() {
        static SET: FixedHashSet<u64, 256> = FixedHashSet::new();
        let barrier = Arc::new(Barrier::new(4));

        let handles: Vec<_> = (0..4)
            .map(|t| {
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    for i in 0..32 {
                        let key = t * 1000 + i;
                        assert!(SET.insert(key));
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
        // 4 threads × 32 keys = 128 distinct keys.
        for t in 0..4u64 {
            for i in 0..32u64 {
                assert!(SET.contains(&(t * 1000 + i)));
            }
        }
    }

    #[test]
    fn concurrent_get_or_insert_returns_consistent_refs() {
        static SET: FixedHashSet<u64, 256> = FixedHashSet::new();
        let barrier = Arc::new(Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    let mut addrs = Vec::new();
                    for i in 0..16u64 {
                        let (r, _) = SET.get_or_insert(i);
                        addrs.push(r as *const u64 as usize);
                    }
                    addrs
                })
            })
            .collect();

        let all_addrs: Vec<Vec<usize>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // For each key, all threads must see the same pointer.
        for i in 0..16 {
            let expected = all_addrs[0][i];
            for thread_addrs in &all_addrs[1..] {
                assert_eq!(thread_addrs[i], expected, "key {i}: pointer mismatch");
            }
        }
    }
}
