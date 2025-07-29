use std::sync::atomic::{AtomicBool, Ordering};

use gc_design::{AtomicShared, Guard, Handle, Local, TraceObj, TracePtr, handle};

struct Node<T: 'static + Send + Sync> {
    item: T,
    next: AtomicShared<Self>,
}

unsafe impl<T: Send + Sync> TraceObj for Node<T> {
    fn unroot_outgoings(&self, guard: &Guard) {
        self.next.unroot(guard);
    }

    fn shade_outgoings(&self, guard: &Guard) {
        self.next.shade(guard);
    }
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
        self.node.as_ref().map(|node| &node.item).unwrap()
    }
}

struct Stack<T: 'static + Send + Sync> {
    top: AtomicShared<Node<T>>,
}

impl<T: Send + Sync> Stack<T> {
    fn new() -> Self {
        Self {
            top: AtomicShared::null(),
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
                .compare_exchange(&old, &new, Ordering::AcqRel, Ordering::Relaxed, &guard)
                .is_ok()
            {
                return Some(ItemRef::new(old, handle));
            }
        }
    }

    fn push(&self, item: T, handle: &Handle) {
        let guard = handle.pin();
        let new = Local::new(
            Node {
                item,
                next: AtomicShared::null(),
            },
            &guard,
        );

        loop {
            let old = self.top.load(Ordering::Acquire, &guard);
            unsafe { new.deref() }
                .next
                .store(&old, Ordering::Relaxed, &guard);
            if self
                .top
                .compare_exchange(&old, &new, Ordering::AcqRel, Ordering::Relaxed, &guard)
                .is_ok()
            {
                return;
            }
        }
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

    use std::sync::atomic::{Ordering, fence};
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
                fence(Ordering::SeqCst);
            });
        }
    });

    fence(Ordering::SeqCst);
    for bit in found {
        assert!(bit.load(Ordering::Relaxed));
    }
}
