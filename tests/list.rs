use cdpt::{AtomicShared, Guard, Handle, Local, TraceObj, TracePtr, pin};

use std::cmp::Ordering::{Equal, Greater, Less};
use std::sync::atomic::Ordering;

struct Node<K, V>
where
    K: 'static + Send + Sync,
    V: 'static + Send + Sync,
{
    next: AtomicShared<Self>,
    key: K,
    value: V,
}

unsafe impl<K, V> TraceObj for Node<K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
    fn unroot_outgoings(&self, guard: &Guard) {
        self.next.unroot(guard);
    }

    fn shade_outgoings(&self, guard: &Guard) {
        self.next.shade(guard);
    }
}

struct List<K, V>
where
    K: 'static + Send + Sync,
    V: 'static + Send + Sync,
{
    head: AtomicShared<Node<K, V>>,
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
            next: AtomicShared::null(),
            key,
            value,
        }
    }

    /// Creates a dummy head.
    /// We never deref key and value of this head node.
    fn head() -> Self {
        Self {
            next: AtomicShared::null(),
            key: K::default(),
            value: V::default(),
        }
    }
}

struct Cursor<'g, K, V>
where
    K: 'static + Send + Sync,
    V: 'static + Send + Sync,
{
    prev: Local<'g, Guard, Node<K, V>>,
    curr: Local<'g, Guard, Node<K, V>>,
}

impl<'g, K, V> Cursor<'g, K, V>
where
    K: Ord + Send + Sync,
    V: Send + Sync,
{
    /// Creates the head cursor.
    #[inline]
    pub fn head(head: &AtomicShared<Node<K, V>>, guard: &'g Guard) -> Cursor<'g, K, V> {
        let prev = head.load(Ordering::Relaxed, guard);
        let curr = unsafe { prev.deref() }.next.load(Ordering::Acquire, guard);
        Self { prev, curr }
    }
}

pub struct VHolder<'h, K, V>
where
    K: 'static + Send + Sync,
    V: 'static + Send + Sync,
{
    node: Local<'h, Handle, Node<K, V>>,
}

impl<'h, K, V> VHolder<'h, K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
    fn new<'g>(node: Local<'g, Guard, Node<K, V>>, handle: &'h Handle) -> Self {
        Self {
            node: node.protect(handle),
        }
    }

    pub fn borrow(&self) -> &V {
        self.node.as_ref().map(|node| &node.value).unwrap()
    }
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
            head: AtomicShared::new(Node::head(), &pin()),
        }
    }

    /// Clean up a chain of logically removed nodes in each traversal.
    #[inline]
    fn find_harris<'g>(&self, key: &K, guard: &'g Guard) -> Result<(bool, Cursor<'g, K, V>), ()> {
        let mut cursor = Cursor::head(&self.head, guard);
        let mut prev_next = cursor.curr;
        let found = loop {
            let Some(curr_node) = cursor.curr.as_ref() else {
                break false;
            };
            let next = curr_node.next.load(Ordering::Acquire, guard);

            if next.tag() != 0 {
                // We add a 0 tag here so that `self.curr`s tag is always 0.
                cursor.curr = next.with_tag(0);
                continue;
            }

            match curr_node.key.cmp(key) {
                Less => {
                    cursor.prev = cursor.curr;
                    cursor.curr = next;
                    prev_next = next;
                }
                Equal => break true,
                Greater => break false,
            }
        };

        // If prev and curr WERE adjacent, no need to clean up
        if Local::ptr_eq(&prev_next, &cursor.curr) {
            return Ok((found, cursor));
        }

        // cleanup tagged nodes between anchor and curr
        unsafe { cursor.prev.deref() }
            .next
            .compare_exchange(
                &prev_next,
                &cursor.curr,
                Ordering::Release,
                Ordering::Relaxed,
                guard,
            )
            .map_err(|_| ())?;

        Ok((found, cursor))
    }

    #[inline]
    fn get<'h, F>(&self, key: &K, find: F, handle: &'h Handle) -> Option<VHolder<'h, K, V>>
    where
        F: for<'g> Fn(&Self, &K, &'g Guard) -> Result<(bool, Cursor<'g, K, V>), ()>,
    {
        let guard = handle.pin();
        loop {
            if let Ok((found, cursor)) = find(self, key, &guard) {
                if found {
                    return Some(VHolder::new(cursor.curr, handle));
                } else {
                    return None;
                }
            }
        }
    }

    #[inline]
    fn insert<'h, F>(&self, key: K, value: V, find: F, handle: &'h Handle) -> bool
    where
        F: for<'g> Fn(&Self, &K, &'g Guard) -> Result<(bool, Cursor<'g, K, V>), ()>,
    {
        let guard = handle.pin();
        let node = Local::new(Node::new(key, value), &guard);
        loop {
            let (found, cursor) = match find(self, unsafe { &node.deref().key }, &guard) {
                Ok(result) => result,
                Err(_) => continue,
            };
            if found {
                return false;
            }

            unsafe { node.deref() }
                .next
                .store(&cursor.curr, Ordering::Relaxed, &guard);

            match unsafe { cursor.prev.deref() }.next.compare_exchange(
                &cursor.curr,
                &node,
                Ordering::Release,
                Ordering::Relaxed,
                &guard,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    #[inline]
    fn remove<'h, F>(&self, key: &K, find: F, handle: &'h Handle) -> Option<VHolder<'h, K, V>>
    where
        F: for<'g> Fn(&Self, &K, &'g Guard) -> Result<(bool, Cursor<'g, K, V>), ()>,
    {
        let guard = handle.pin();
        loop {
            let (found, cursor) = match find(self, key, &guard) {
                Ok(result) => result,
                Err(_) => continue,
            };
            if !found {
                return None;
            }

            let curr_node = unsafe { cursor.curr.deref() };
            let next = curr_node.next.fetch_tag_or(1, Ordering::AcqRel, &guard);
            if next.tag() == 1 {
                continue;
            }

            let _ = unsafe { cursor.prev.deref() }.next.compare_exchange(
                &cursor.curr,
                &next,
                Ordering::Release,
                Ordering::Relaxed,
                &guard,
            );

            return Some(VHolder::new(cursor.curr, handle));
        }
    }

    #[inline]
    pub fn harris_get<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.get(key, Self::find_harris, handle)
    }

    #[inline]
    pub fn harris_insert(&self, key: K, value: V, handle: &Handle) -> bool {
        self.insert(key, value, Self::find_harris, handle)
    }

    #[inline]
    pub fn harris_remove<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.remove(key, Self::find_harris, handle)
    }
}

pub trait ConcurrentMap<K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
    fn new() -> Self;
    fn get<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>>;
    fn insert(&self, key: K, value: V, handle: &Handle) -> bool;
    fn remove<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>>;
}

pub struct HList<K, V>
where
    K: 'static + Send + Sync,
    V: 'static + Send + Sync,
{
    inner: List<K, V>,
}

impl<K, V> ConcurrentMap<K, V> for HList<K, V>
where
    K: Send + Sync + Ord + Default,
    V: Send + Sync + Default,
{
    fn new() -> Self {
        HList { inner: List::new() }
    }

    #[inline(always)]
    fn get<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.inner.harris_get(key, handle)
    }
    #[inline(always)]
    fn insert(&self, key: K, value: V, handle: &Handle) -> bool {
        self.inner.harris_insert(key, value, handle)
    }
    #[inline(always)]
    fn remove<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.inner.harris_remove(key, handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastrand::shuffle;
    use cdpt::handle;
    use std::thread::scope;

    const THREADS: i32 = 30;
    const ELEMENTS_PER_THREADS: i32 = 1000;

    fn smoke<M: ConcurrentMap<i32, String> + Send + Sync>() {
        let map = &M::new();

        scope(|s| {
            for t in 0..THREADS {
                s.spawn(move || {
                    let handle = handle();
                    let mut keys: Vec<i32> =
                        (0..ELEMENTS_PER_THREADS).map(|k| k * THREADS + t).collect();
                    shuffle(&mut keys);
                    for i in keys {
                        assert!(map.insert(i, i.to_string(), &handle));
                    }
                });
            }
        });

        scope(|s| {
            for t in 0..(THREADS / 2) {
                s.spawn(move || {
                    let handle = handle();
                    let mut keys: Vec<i32> =
                        (0..ELEMENTS_PER_THREADS).map(|k| k * THREADS + t).collect();
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

        scope(|s| {
            for t in (THREADS / 2)..THREADS {
                s.spawn(move || {
                    let handle = handle();
                    let mut keys: Vec<i32> =
                        (0..ELEMENTS_PER_THREADS).map(|k| k * THREADS + t).collect();
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

    #[test]
    fn smoke_harris() {
        smoke::<HList<i32, String>>();
    }
}
