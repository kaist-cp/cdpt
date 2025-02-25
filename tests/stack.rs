use std::sync::atomic::AtomicBool;

use gc_design::{Atomic, Local};

struct Node<T: Send + Sync> {
    item: Atomic<T>,
    next: Atomic<Self>,
}

struct Stack<T: Send + Sync> {
    top: Atomic<Node<T>>,
}

impl<T: Send + Sync> Stack<T> {
    fn new() -> Self {
        Self {
            top: Atomic::null(),
        }
    }

    fn pop(&self) -> Option<Local<T>> {
        loop {
            let old = self.top.load();
            let new = if let Some(old) = old.as_ref() {
                old.borrow().next.load()
            } else {
                return None;
            };
            if self.top.compare_exchange(old.as_ref(), new.as_ref()) {
                return old.as_ref().map(|node| node.borrow().item.load()).unwrap();
            }
        }
    }

    fn push(&self, item: T) {
        let new = Local::new(Node {
            item: Atomic::new(item),
            next: Atomic::null(),
        });

        loop {
            let old = self.top.load();
            new.borrow().next.store(old.as_ref());
            if self.top.compare_exchange(old.as_ref(), Some(&new)) {
                return;
            }
        }
    }
}

#[test]
fn simple() {
    let stack = Stack::new();
    stack.push(1);
    stack.push(2);
    stack.push(3);
    assert_eq!(Some(3), stack.pop().map(|entry| *entry.borrow()));
    assert_eq!(Some(2), stack.pop().map(|entry| *entry.borrow()));
    assert_eq!(Some(1), stack.pop().map(|entry| *entry.borrow()));
    assert!(stack.pop().is_none());
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
                for v in (t * COUNT)..((t + 1) * COUNT) {
                    stack.push(v);
                }
                let mut popped = 0;
                while popped < COUNT {
                    if let Some(item) = stack.pop().as_ref() {
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
