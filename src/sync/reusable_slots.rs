use std::{
    ops::Deref,
    ptr::null_mut,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

pub(crate) struct Node<T> {
    using: AtomicBool,
    next: AtomicPtr<Self>,
    item: T,
}

impl<T: Default> Node<T> {
    pub(crate) fn new_using() -> Self {
        Self {
            using: AtomicBool::new(true),
            next: AtomicPtr::new(null_mut()),
            item: T::default(),
        }
    }
}

pub(crate) struct Entry<T: 'static> {
    node: &'static Node<T>,
}

impl<T: 'static> Drop for Entry<T> {
    fn drop(&mut self) {
        self.node.using.store(false, Ordering::Release);
    }
}

impl<T: 'static> Deref for Entry<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.node.item
    }
}

pub(crate) struct ReusableSlots<T> {
    head: AtomicPtr<Node<T>>,
}

impl<T> Default for ReusableSlots<T> {
    fn default() -> Self {
        Self {
            head: AtomicPtr::new(null_mut()),
        }
    }
}

impl<T: 'static + Default> ReusableSlots<T> {
    pub(crate) fn acquire_or_default(&self) -> Entry<T> {
        let mut prev = &self.head;
        loop {
            let curr = prev.load(Ordering::Acquire);
            if curr.is_null() {
                // We are at the end of the list. Let's try inserting a new slot.
                let new_node = Box::into_raw(Box::new(Node::new_using()));
                match prev.compare_exchange(
                    null_mut(),
                    new_node,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        return Entry {
                            node: unsafe { &*new_node },
                        };
                    }
                    Err(_) => {
                        drop(unsafe { Box::from_raw(new_node) });
                        continue;
                    }
                }
            }

            let curr_node = unsafe { &*curr };
            if !curr_node.using.load(Ordering::Relaxed) {
                // If the current node is not using by other threads, let's try to take it.
                if curr_node
                    .using
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return Entry { node: curr_node };
                }
            }

            // The current node is being used by another thread.
            // Let's move to the next node.
            prev = &curr_node.next;
        }
    }

    pub(crate) fn iter_all(&self) -> impl Iterator<Item = &'static T> {
        SlotIter {
            curr: self.head.load(Ordering::Acquire),
            cond: |_| true,
        }
    }

    pub(crate) fn iter_using(&self) -> impl Iterator<Item = &'static T> {
        SlotIter {
            curr: self.head.load(Ordering::Acquire),
            cond: |node| node.using.load(Ordering::Acquire),
        }
    }
}

pub(crate) struct SlotIter<T, F>
where
    T: 'static,
    F: Fn(&Node<T>) -> bool,
{
    curr: *mut Node<T>,
    cond: F,
}

impl<T, F> Iterator for SlotIter<T, F>
where
    T: 'static,
    F: Fn(&Node<T>) -> bool,
{
    type Item = &'static T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.curr.is_null() {
                return None;
            }
            let node = unsafe { &*self.curr };
            self.curr = node.next.load(Ordering::Acquire);

            if (self.cond)(node) {
                return Some(&node.item);
            }
        }
    }
}
