use gc_design::{Atomic, Local};
use std::cmp::Ordering::*;

struct Node<K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
    next: Atomic<Self>,
    key: K,
    value: V,
}

struct List<K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
    head: Atomic<Node<K, V>>,
}

impl<K, V> Node<K, V>
where
    K: Send + Sync + Default,
    V: Send + Sync + Default,
{
    /// Creates a new node.
    #[inline]
    fn new(key: K, value: V) -> Self {
        Self {
            next: Atomic::null(),
            key,
            value,
        }
    }

    /// Creates a dummy head.
    /// We never deref key and value of this head node.
    fn head() -> Self {
        Self {
            next: Atomic::null(),
            key: K::default(),
            value: V::default(),
        }
    }
}

struct Cursor<K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
    prev: Local<Node<K, V>>,
    curr: Option<Local<Node<K, V>>>,
}

impl<K, V> Cursor<K, V>
where
    K: Ord + Send + Sync,
    V: Send + Sync,
{
    /// Creates the head cursor.
    #[inline]
    pub fn head(head: &Atomic<Node<K, V>>) -> Cursor<K, V> {
        let prev = head.load().0.unwrap();
        let (curr, _) = prev.borrow().next.load();
        Self { prev, curr }
    }
}

pub struct VHolder<K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
    node: Local<Node<K, V>>,
}

impl<K, V> VHolder<K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
    fn new(node: Local<Node<K, V>>) -> Self {
        Self { node }
    }

    pub fn borrow(&self) -> &V {
        &self.node.borrow().value
    }
}

enum HStepResult<K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
    SkipMarked(Option<Local<Node<K, V>>>),
    Advance(Option<Local<Node<K, V>>>),
    Finished(bool),
}

impl<K, V> List<K, V>
where
    K: Default + Ord + Send + Sync,
    V: Default + Send + Sync,
{
    /// Creates a new list.
    #[inline]
    pub fn new() -> Self {
        List {
            head: Atomic::new(Node::head()),
        }
    }

    /// Clean up a chain of logically removed nodes in each traversal.
    #[inline]
    fn find_harris(&self, key: &K) -> Result<(bool, Cursor<K, V>), ()> {
        use HStepResult::*;

        // Finding phase
        // - cursor.curr: first unmarked node w/ key >= search key (4)
        // - cursor.prev: the ref of .next in previous unmarked node (1 -> 2)
        // 1 -> 2 -x-> 3 -x-> 4 -> 5 -> ∅  (search key: 4)
        let mut cursor = Cursor::head(&self.head);
        let mut prev_next = cursor.curr.clone();
        let found = loop {
            let step_result = {
                let Some(curr_local) = cursor.curr.as_ref() else {
                    break false;
                };
                let curr_node = curr_local.borrow();
                let (next, next_tag) = curr_node.next.load();

                // - finding stage is done if cursor.curr advancement stops
                // - advance cursor.curr if (.next is marked) || (cursor.curr < key)
                // - stop cursor.curr if (not marked) && (cursor.curr >= key)
                // - advance cursor.prev if not marked

                if next_tag != 0 {
                    SkipMarked(next)
                } else {
                    match curr_node.key.cmp(key) {
                        Less => Advance(next),
                        Equal => Finished(true),
                        Greater => Finished(false),
                    }
                }
            };

            match step_result {
                SkipMarked(next) => {
                    cursor.curr = next;
                }
                Advance(next) => {
                    cursor.prev = cursor.curr.take().unwrap();
                    cursor.curr = next.clone();
                    prev_next = next;
                }
                Finished(is_equal) => break is_equal,
            }
        };

        // If prev and curr WERE adjacent, no need to clean up
        if Local::opt_ptr_eq(prev_next.as_ref(), cursor.curr.as_ref()) {
            return Ok((found, cursor));
        }

        // cleanup marked nodes between prev and curr
        if !cursor
            .prev
            .borrow()
            .next
            .compare_exchange((prev_next.as_ref(), 0), (cursor.curr.as_ref(), 0))
        {
            return Err(());
        }

        Ok((found, cursor))
    }

    #[inline]
    fn get<F>(&self, key: &K, find: F) -> Option<VHolder<K, V>>
    where
        F: Fn(&Self, &K) -> Result<(bool, Cursor<K, V>), ()>,
    {
        loop {
            let Ok((found, mut cursor)) = find(self, key) else {
                continue;
            };
            if found {
                return cursor.curr.take().map(|node| VHolder::new(node));
            }
            return None;
        }
    }

    #[inline]
    fn insert<F>(&self, key: K, value: V, find: F) -> bool
    where
        F: Fn(&Self, &K) -> Result<(bool, Cursor<K, V>), ()>,
    {
        let node = Local::new(Node::new(key, value));
        loop {
            let Ok((found, mut cursor)) = find(self, &node.borrow().key) else {
                continue;
            };
            if found {
                return false;
            }

            node.borrow().next.store(cursor.curr.as_ref(), 0);
            if cursor
                .prev
                .borrow()
                .next
                .compare_exchange((cursor.curr.as_ref(), 0), (Some(&node), 0))
            {
                cursor.curr = Some(node);
                return true;
            }
        }
    }

    #[inline]
    fn remove<F>(&self, key: &K, find: F) -> Option<VHolder<K, V>>
    where
        F: Fn(&Self, &K) -> Result<(bool, Cursor<K, V>), ()>,
    {
        loop {
            let Ok((found, cursor)) = find(self, key) else {
                continue;
            };
            if !found {
                return None;
            }

            let curr_node = cursor.curr.as_ref().unwrap().borrow();
            let (next, next_tag) = curr_node.next.load();
            if next_tag == 1
                || !curr_node
                    .next
                    .compare_exchange((next.as_ref(), 0), (next.as_ref(), 1))
            {
                continue;
            }

            let _ = cursor
                .prev
                .borrow()
                .next
                .compare_exchange((cursor.curr.as_ref(), 0), (next.as_ref(), 0));

            return cursor.curr.map(|node| VHolder::new(node));
        }
    }

    #[inline]
    pub fn harris_get(&self, key: &K) -> Option<VHolder<K, V>> {
        self.get(key, Self::find_harris)
    }

    #[inline]
    pub fn harris_insert(&self, key: K, value: V) -> bool {
        self.insert(key, value, Self::find_harris)
    }

    #[inline]
    pub fn harris_remove(&self, key: &K) -> Option<VHolder<K, V>> {
        self.remove(key, Self::find_harris)
    }
}

#[cfg(test)]
mod tests {
    extern crate rand;
    use super::*;
    use rand::prelude::SliceRandom;
    use std::thread::scope;

    const THREADS: i32 = 30;
    const ELEMENTS_PER_THREADS: i32 = 1000;

    fn smoke<G, I, R>(get: &G, insert: &I, remove: &R)
    where
        G: Sync + Fn(&List<i32, String>, &i32) -> Option<VHolder<i32, String>>,
        I: Sync + Fn(&List<i32, String>, i32, String) -> bool,
        R: Sync + Fn(&List<i32, String>, &i32) -> Option<VHolder<i32, String>>,
    {
        let map = &List::new();

        scope(|s| {
            for t in 0..THREADS {
                s.spawn(move || {
                    let mut rng = rand::rng();
                    let mut keys: Vec<i32> =
                        (0..ELEMENTS_PER_THREADS).map(|k| k * THREADS + t).collect();
                    keys.shuffle(&mut rng);
                    for i in keys {
                        assert!(insert(map, i, i.to_string()));
                    }
                });
            }
        });

        scope(|s| {
            for t in 0..(THREADS / 2) {
                s.spawn(move || {
                    let mut rng = rand::rng();
                    let mut keys: Vec<i32> =
                        (0..ELEMENTS_PER_THREADS).map(|k| k * THREADS + t).collect();
                    keys.shuffle(&mut rng);
                    for i in keys {
                        assert_eq!(
                            Some(&i.to_string()),
                            remove(map, &i).as_ref().map(|v| v.borrow())
                        );
                    }
                });
            }
        });

        scope(|s| {
            for t in (THREADS / 2)..THREADS {
                s.spawn(move || {
                    let mut rng = rand::rng();
                    let mut keys: Vec<i32> =
                        (0..ELEMENTS_PER_THREADS).map(|k| k * THREADS + t).collect();
                    keys.shuffle(&mut rng);
                    for i in keys {
                        assert_eq!(
                            Some(&i.to_string()),
                            get(map, &i).as_ref().map(|v| v.borrow())
                        );
                    }
                });
            }
        });
    }

    #[test]
    fn smoke_harris() {
        smoke(
            &List::harris_get,
            &List::harris_insert,
            &List::harris_remove,
        );
    }
}
