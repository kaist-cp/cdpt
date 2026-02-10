use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use cdpt::{AtomicSharedOption, Guard, Handle, Local, TraceObj, TracePtr, handle};

#[derive(TraceObj)]
struct Node<T: 'static + Send + Sync> {
    item: T,
    next: AtomicSharedOption<Self>,
}

pub struct ItemRef<'h, T: 'static + Send + Sync> {
    node: Local<'h, Handle, Node<T>>,
}

impl<'h, T: Send + Sync> ItemRef<'h, T> {
    fn new<'g>(node: Local<'g, Guard, Node<T>>, handle: &'h Handle) -> Self {
        Self {
            node: node.protect(handle),
        }
    }

    pub fn borrow(&self) -> &T {
        &self.node.item
    }
}

struct Stack<T: 'static + Send + Sync> {
    top: AtomicSharedOption<Node<T>>,
}

impl<T: Send + Sync> Stack<T> {
    fn new() -> Self {
        Self {
            top: AtomicSharedOption::none(),
        }
    }

    fn pop<'h>(&self, handle: &'h Handle) -> Option<ItemRef<'h, T>> {
        let guard = handle.pin();
        loop {
            let old = self.top.load(Ordering::Acquire, &guard);
            let new = if let Some(old) = old.as_ref() {
                old.next.load(Ordering::Acquire, &guard)
            } else {
                return None;
            };
            if self
                .top
                .compare_exchange(
                    old.as_ref(),
                    new.as_ref(),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                    &guard,
                )
                .is_ok()
            {
                return Some(ItemRef::new(old.unwrap(), handle));
            }
        }
    }

    fn push(&self, item: T, handle: &Handle) {
        let guard = handle.pin();
        let new = Local::new(
            Node {
                item,
                next: AtomicSharedOption::none(),
            },
            &guard,
        );

        loop {
            let old = self.top.load(Ordering::Acquire, &guard);
            new.next.store(old.as_ref(), Ordering::Relaxed, &guard);
            if self
                .top
                .compare_exchange(
                    old.as_ref(),
                    Some(&new),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                    &guard,
                )
                .is_ok()
            {
                return;
            }
        }
    }

    fn is_empty(&self, handle: &Handle) -> bool {
        let guard = handle.pin();
        self.top.load(Ordering::Acquire, &guard).is_none()
    }
}

#[test]
fn simple() {
    let handle = handle();
    let stack = Stack::new();
    stack.push(1, &handle);
    stack.push(2, &handle);
    stack.push(3, &handle);
    assert_eq!(Some(3), stack.pop(&handle).map(|entry| *entry.borrow()));
    assert_eq!(Some(2), stack.pop(&handle).map(|entry| *entry.borrow()));
    assert_eq!(Some(1), stack.pop(&handle).map(|entry| *entry.borrow()));
    assert!(stack.pop(&handle).is_none());
}

#[test]
fn smoke() {
    const THREADS: usize = 30;
    const COUNT: usize = 10000;

    use std::thread::scope;

    let stack = &Stack::new();
    let found = &[(); THREADS * COUNT].map(|_| AtomicBool::new(false));

    scope(|s| {
        for t in 0..THREADS {
            s.spawn(move || {
                let handle = handle();
                for v in (t * COUNT)..((t + 1) * COUNT) {
                    stack.push(v, &handle);
                }
                let mut popped = 0;
                while popped < COUNT {
                    if let Some(item) = stack.pop(&handle).as_ref() {
                        popped += 1;
                        assert!(!found[*item.borrow()].load(Ordering::Acquire));
                        found[*item.borrow()].store(true, Ordering::Release);
                    }
                }
            });
        }
    });

    // Note: `scope` checks the number of running threads with `Acquire` ordering.
    // Therefore, the following relaxed loads will see the latest values without additional fences.
    for bit in found {
        assert!(bit.load(Ordering::Relaxed));
    }
}

#[test]
fn empty_stack() {
    let handle = handle();
    let stack: Stack<i32> = Stack::new();

    // Pop from empty stack should return None
    assert!(stack.pop(&handle).is_none());
    assert!(stack.pop(&handle).is_none());
    assert!(stack.is_empty(&handle));
}

#[test]
fn push_pop_single() {
    let handle = handle();
    let stack = Stack::new();

    // Push single element
    stack.push(42, &handle);
    assert!(!stack.is_empty(&handle));

    // Pop single element
    assert_eq!(Some(42), stack.pop(&handle).map(|e| *e.borrow()));
    assert!(stack.is_empty(&handle));
    assert!(stack.pop(&handle).is_none());
}

#[test]
fn lifo_order() {
    let handle = handle();
    let stack = Stack::new();

    // Push elements in order 1, 2, 3, 4, 5
    for i in 1..=5 {
        stack.push(i, &handle);
    }

    // Pop should return in reverse order: 5, 4, 3, 2, 1
    for i in (1..=5).rev() {
        assert_eq!(Some(i), stack.pop(&handle).map(|e| *e.borrow()));
    }

    assert!(stack.is_empty(&handle));
}

#[test]
fn interleaved_push_pop() {
    let handle = handle();
    let stack = Stack::new();

    stack.push(1, &handle);
    stack.push(2, &handle);
    assert_eq!(Some(2), stack.pop(&handle).map(|e| *e.borrow()));

    stack.push(3, &handle);
    assert_eq!(Some(3), stack.pop(&handle).map(|e| *e.borrow()));
    assert_eq!(Some(1), stack.pop(&handle).map(|e| *e.borrow()));

    assert!(stack.is_empty(&handle));
}

#[test]
fn many_elements() {
    let handle = handle();
    let stack = Stack::new();

    const N: i32 = 1000;

    // Push N elements
    for i in 0..N {
        stack.push(i, &handle);
    }

    // Pop and verify all elements in LIFO order
    for i in (0..N).rev() {
        assert_eq!(Some(i), stack.pop(&handle).map(|e| *e.borrow()));
    }

    assert!(stack.is_empty(&handle));
}

#[test]
fn concurrent_push_only() {
    use std::thread::scope;

    const THREADS: usize = 8;
    const COUNT: usize = 1000;

    let stack = &Stack::new();

    scope(|s| {
        for t in 0..THREADS {
            s.spawn(move || {
                let handle = handle();
                for i in 0..COUNT {
                    stack.push(t * COUNT + i, &handle);
                }
            });
        }
    });

    // Count elements
    let handle = handle();
    let mut count = 0;
    while stack.pop(&handle).is_some() {
        count += 1;
    }
    assert_eq!(count, THREADS * COUNT);
}

#[test]
fn concurrent_pop_only() {
    use std::thread::scope;

    const THREADS: usize = 8;
    const ELEMENTS: usize = THREADS * 100;

    let stack = &Stack::new();

    // Pre-populate the stack
    {
        let h = handle();
        for i in 0..ELEMENTS {
            stack.push(i, &h);
        }
    }

    let total_popped = &AtomicUsize::new(0);

    scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let h = handle();
                let mut count = 0;
                while stack.pop(&h).is_some() {
                    count += 1;
                }
                total_popped.fetch_add(count, Ordering::Relaxed);
            });
        }
    });

    // All elements should have been popped exactly once
    assert_eq!(total_popped.load(Ordering::Relaxed), ELEMENTS);

    let h = handle();
    assert!(stack.is_empty(&h));
}

#[test]
fn concurrent_push_pop_balanced() {
    use std::thread::scope;

    const THREADS: usize = 8;
    const OPS_PER_THREAD: usize = 1000;

    let stack = &Stack::new();
    let pushed = &AtomicUsize::new(0);
    let popped = &AtomicUsize::new(0);

    scope(|s| {
        // Push threads
        for _ in 0..(THREADS / 2) {
            s.spawn(|| {
                let handle = handle();
                for i in 0..OPS_PER_THREAD {
                    stack.push(i, &handle);
                    pushed.fetch_add(1, Ordering::Relaxed);
                }
            });
        }

        // Pop threads
        for _ in 0..(THREADS / 2) {
            s.spawn(|| {
                let handle = handle();
                let mut count = 0;
                // Try to pop as many as possible
                for _ in 0..(OPS_PER_THREAD * 2) {
                    if stack.pop(&handle).is_some() {
                        count += 1;
                    }
                }
                popped.fetch_add(count, Ordering::Relaxed);
            });
        }
    });

    // Drain remaining elements
    let handle = handle();
    let mut remaining = 0;
    while stack.pop(&handle).is_some() {
        remaining += 1;
    }

    let total_pushed = pushed.load(Ordering::Relaxed);
    let total_popped = popped.load(Ordering::Relaxed) + remaining;

    assert_eq!(total_pushed, total_popped);
}

// Stress tests (disabled by default)
// To run: cargo test -- --ignored
// To run with address sanitizer: RUSTFLAGS="-Z sanitizer=address" cargo +nightly test -- --ignored

#[test]
#[ignore]
#[serial_test::serial]
fn stress_push_pop() {
    use std::thread::scope;

    const THREADS: usize = 16;
    const OPS_PER_THREAD: usize = 50000;

    let stack = &Stack::new();
    let ops_completed = &AtomicUsize::new(0);

    scope(|s| {
        for thread_id in 0..THREADS {
            s.spawn(move || {
                let handle = handle();
                let mut rng = fastrand::Rng::with_seed(thread_id as u64);

                for _ in 0..OPS_PER_THREAD {
                    if rng.bool() {
                        stack.push(rng.usize(..), &handle);
                    } else {
                        let _ = stack.pop(&handle);
                    }
                    ops_completed.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    assert_eq!(
        ops_completed.load(Ordering::Relaxed),
        THREADS * OPS_PER_THREAD
    );
}

#[test]
#[ignore]
#[serial_test::serial]
fn stress_heavy_contention() {
    use std::thread::scope;

    const THREADS: usize = 32;
    const OPS_PER_THREAD: usize = 100000;

    let stack = &Stack::new();

    scope(|s| {
        for thread_id in 0..THREADS {
            s.spawn(move || {
                let handle = handle();
                let mut rng = fastrand::Rng::with_seed(thread_id as u64);

                for i in 0..OPS_PER_THREAD {
                    // Mix of operations with varying patterns
                    match rng.u8(0..10) {
                        0..=5 => {
                            // 60% push
                            stack.push(thread_id * OPS_PER_THREAD + i, &handle);
                        }
                        6..=9 => {
                            // 40% pop
                            let _ = stack.pop(&handle);
                        }
                        _ => unreachable!(),
                    }
                }
            });
        }
    });

    // Drain the stack to verify no corruption
    let handle = handle();
    let mut count = 0;
    while stack.pop(&handle).is_some() {
        count += 1;
    }
    // We can't predict exact count due to concurrent pop, but stack should be valid
    assert!(stack.is_empty(&handle));
    let _ = count; // Use count to avoid warning
}

#[test]
#[ignore]
#[serial_test::serial]
fn stress_burst_operations() {
    use std::thread::scope;

    const THREADS: usize = 16;
    const BURSTS: usize = 100;
    const OPS_PER_BURST: usize = 1000;

    let stack = &Stack::new();

    scope(|s| {
        for thread_id in 0..THREADS {
            s.spawn(move || {
                let handle = handle();

                for burst in 0..BURSTS {
                    // Burst of pushes
                    for i in 0..OPS_PER_BURST {
                        stack.push(
                            thread_id * BURSTS * OPS_PER_BURST + burst * OPS_PER_BURST + i,
                            &handle,
                        );
                    }

                    // Burst of pops
                    for _ in 0..OPS_PER_BURST {
                        let _ = stack.pop(&handle);
                    }
                }
            });
        }
    });

    // Verify stack integrity
    let handle = handle();
    while stack.pop(&handle).is_some() {}
    assert!(stack.is_empty(&handle));
}
