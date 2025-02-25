use std::sync::atomic::AtomicBool;

use gc_design::{Local, Shared};

struct Node<T: Send + Sync> {
    item: Shared<T>,
    next: Shared<Self>,
}

struct Stack<T: Send + Sync> {
    top: Shared<Node<T>>,
}

impl<T: Send + Sync> Stack<T> {
    fn new() -> Self {
        Self {
            top: Shared::null(),
        }
    }

    fn pop(&self) -> Local<T> {
        loop {
            let old = self.top.load();
            let new = if let Some(old) = old.as_ref() {
                old.next.load()
            } else {
                return Local::null();
            };
            if self.top.compare_exchange(&old, &new) {
                return old.as_ref().map(|node| node.item.load()).unwrap();
            }
        }
    }

    fn push(&self, item: T) {
        let new = Local::new(Node {
            item: Shared::new(item),
            next: Shared::null(),
        });

        loop {
            let old = self.top.load();
            new.as_ref().unwrap().next.store(&old);
            if self.top.compare_exchange(&old, &new) {
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
    assert_eq!(3, *stack.pop().as_ref().unwrap());
    assert_eq!(2, *stack.pop().as_ref().unwrap());
    assert_eq!(1, *stack.pop().as_ref().unwrap());
    assert!(stack.pop().is_null());
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
                        assert!(!found[*item].load(Ordering::Acquire));
                        found[*item].store(true, Ordering::Release);
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
