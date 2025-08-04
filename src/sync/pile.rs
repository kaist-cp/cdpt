use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::null_mut;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};

struct Node<T> {
    next: AtomicPtr<Self>,
    item: MaybeUninit<T>,
}

impl<T> Node<T> {
    fn new(item: T) -> Self {
        Node {
            next: AtomicPtr::new(null_mut()),
            item: MaybeUninit::new(item),
        }
    }

    fn alloc(item: T) -> *mut Self {
        Box::into_raw(Box::new(Self::new(item)))
    }
}

pub(crate) struct Iter<T> {
    curr: *mut Node<T>,
}

/// A lock-free linked list that can be taken (deleted) as a whole at once.
#[derive(Debug)]
pub(crate) struct Pile<T> {
    /// The head of the linked list.
    head: AtomicPtr<Node<T>>,
    _marker: PhantomData<T>,
}

impl<T> Pile<T> {
    /// Returns a new, empty pile.
    pub(crate) const fn new() -> Self {
        Self {
            head: AtomicPtr::new(null_mut()),
            _marker: PhantomData,
        }
    }

    /// Pushes the batch of elements into the head of the pile.
    pub(crate) fn push_batch<I>(&self, mut batch: I)
    where
        I: Iterator<Item = T>,
    {
        let Some(first) = batch.next().map(Node::alloc) else {
            return;
        };

        let mut curr = first;
        while let Some(next) = batch.next().map(Node::alloc) {
            unsafe { &*curr }.next.store(next, Relaxed);
            curr = next;
        }
        self.push_internal(first, curr);
    }

    /// Pushes the element into the head of the pile.
    pub(crate) fn push(&self, item: T) {
        let node = Node::alloc(item);
        self.push_internal(node, node);
    }

    fn push_internal(&self, first: *mut Node<T>, last: *mut Node<T>) {
        let mut curr_head = self.head.load(Acquire);

        loop {
            unsafe { &*last }.next.store(curr_head, Relaxed);
            match self
                .head
                .compare_exchange_weak(curr_head, first, Release, Relaxed)
            {
                Ok(_) => break,
                Err(current) => curr_head = current,
            }
        }
    }

    pub(crate) fn take(&self) -> Iter<T> {
        let curr = self.head.swap(null_mut(), AcqRel);
        Iter { curr }
    }
}

impl<T> Iterator for Iter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.curr.is_null() {
            return None;
        }
        let curr = unsafe { *Box::from_raw(self.curr) };
        self.curr = curr.next.load(Relaxed);
        let item = unsafe { MaybeUninit::assume_init(curr.item) };
        Some(item)
    }
}

impl<T> Drop for Iter<T> {
    fn drop(&mut self) {
        while let Some(item) = self.next() {
            drop(item)
        }
    }
}
