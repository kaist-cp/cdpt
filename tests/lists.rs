mod map_common;

use cdpt::{AtomicShared, AtomicSharedOption, Guard, Handle, Local, TraceObj, TracePtr, pin};
use map_common::{ConcurrentMap, ValueRef};

use std::cmp::Ordering::{Equal, Greater, Less};
use std::sync::atomic::Ordering;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[derive(TraceObj)]
struct Node<K, V>
where
    K: 'static + Send + Sync,
    V: 'static + Send + Sync,
{
    next: AtomicSharedOption<Self>,
    key: K,
    value: V,
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
            next: AtomicSharedOption::none(),
            key,
            value,
        }
    }

    /// Creates a dummy head.
    /// We never deref key and value of this head node.
    fn head() -> Self {
        Self {
            next: AtomicSharedOption::none(),
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
    curr: Option<Local<'g, Guard, Node<K, V>>>,
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
        let curr = prev.next.load(Ordering::Acquire, guard);
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
}

impl<K, V> ValueRef<V> for VHolder<'_, K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
    fn borrow(&self) -> &V {
        &self.node.value
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
            let (next, next_tag) = curr_node.next.load_with_tag(Ordering::Acquire, guard);

            if next_tag != 0 {
                cursor.curr = next;
                continue;
            }

            match curr_node.key.cmp(key) {
                Less => {
                    cursor.prev = cursor.curr.unwrap();
                    cursor.curr = next;
                    prev_next = next;
                }
                Equal => break true,
                Greater => break false,
            }
        };

        // If prev and curr WERE adjacent, no need to clean up
        if Local::opt_ptr_eq(prev_next.as_ref(), cursor.curr.as_ref()) {
            return Ok((found, cursor));
        }

        // cleanup tagged nodes between anchor and curr
        cursor
            .prev
            .next
            .compare_exchange_with_tag(
                (prev_next.as_ref(), 0),
                (cursor.curr.as_ref(), 0),
                Ordering::Release,
                Ordering::Relaxed,
                guard,
            )
            .map_err(|_| ())?;

        Ok((found, cursor))
    }

    #[inline]
    fn find_harris_michael<'g>(
        &self,
        key: &K,
        guard: &'g Guard,
    ) -> Result<(bool, Cursor<'g, K, V>), ()> {
        let mut cursor = Cursor::head(&self.head, guard);
        loop {
            let Some(curr_node) = cursor.curr.as_ref() else {
                break Ok((false, cursor));
            };
            let (next, next_tag) = curr_node.next.load_with_tag(Ordering::Acquire, guard);

            if next_tag != 0 {
                cursor
                    .prev
                    .next
                    .compare_exchange(
                        cursor.curr.as_ref(),
                        next.as_ref(),
                        Ordering::Release,
                        Ordering::Relaxed,
                        guard,
                    )
                    .map_err(|_| ())?;
                cursor.curr = next;
                continue;
            }

            match curr_node.key.cmp(key) {
                Less => {
                    cursor.prev = cursor.curr.unwrap();
                    cursor.curr = next;
                }
                Equal => return Ok((true, cursor)),
                Greater => return Ok((false, cursor)),
            }
        }
    }

    /// Gotta go fast. Doesn't fail.
    #[inline]
    fn find_harris_herlihy_shavit<'g>(
        &self,
        key: &K,
        guard: &'g Guard,
    ) -> Result<(bool, Cursor<'g, K, V>), ()> {
        let mut cursor = Cursor::head(&self.head, guard);
        Ok(loop {
            let Some(curr_node) = cursor.curr.as_ref() else {
                break (false, cursor);
            };
            let (next, next_tag) = curr_node.next.load_with_tag(Ordering::Acquire, guard);
            match curr_node.key.cmp(key) {
                Less => {
                    cursor.prev = cursor.curr.unwrap();
                    cursor.curr = next;
                    continue;
                }
                Equal => break (next_tag == 0, cursor),
                Greater => break (false, cursor),
            }
        })
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
                    return Some(VHolder::new(cursor.curr.unwrap(), handle));
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
            let (found, cursor) = match find(self, &node.key, &guard) {
                Ok(result) => result,
                Err(_) => continue,
            };
            if found {
                return false;
            }

            node.next
                .store(cursor.curr.as_ref(), Ordering::Relaxed, &guard);

            match cursor.prev.next.compare_exchange_with_tag(
                (cursor.curr.as_ref(), 0),
                (Some(&node), 0),
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

            let curr_node = cursor.curr.as_ref().unwrap();

            let (next, next_tag) = curr_node.next.fetch_tag_or(1, Ordering::AcqRel, &guard);
            if next_tag == 1 {
                continue;
            }

            let _ = cursor.prev.next.compare_exchange_with_tag(
                (cursor.curr.as_ref(), 0),
                (next.as_ref(), next_tag),
                Ordering::Release,
                Ordering::Relaxed,
                &guard,
            );

            return Some(VHolder::new(cursor.curr.unwrap(), handle));
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

    #[inline]
    pub fn harris_michael_get<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.get(key, Self::find_harris_michael, handle)
    }

    #[inline]
    pub fn harris_michael_insert(&self, key: K, value: V, handle: &Handle) -> bool {
        self.insert(key, value, Self::find_harris_michael, handle)
    }

    #[inline]
    pub fn harris_michael_remove<'h>(
        &self,
        key: &K,
        handle: &'h Handle,
    ) -> Option<VHolder<'h, K, V>> {
        self.remove(key, Self::find_harris_michael, handle)
    }

    #[inline]
    pub fn harris_herlihy_shavit_get<'h>(
        &self,
        key: &K,
        handle: &'h Handle,
    ) -> Option<VHolder<'h, K, V>> {
        self.get(key, Self::find_harris_herlihy_shavit, handle)
    }
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
    type ValueRef<'h> = VHolder<'h, K, V>;

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

pub struct HMList<K, V>
where
    K: 'static + Send + Sync,
    V: 'static + Send + Sync,
{
    inner: List<K, V>,
}

impl<K, V> ConcurrentMap<K, V> for HMList<K, V>
where
    K: Send + Sync + Ord + Default,
    V: Send + Sync + Default,
{
    type ValueRef<'h> = VHolder<'h, K, V>;

    fn new() -> Self {
        HMList { inner: List::new() }
    }

    #[inline(always)]
    fn get<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.inner.harris_michael_get(key, handle)
    }
    #[inline(always)]
    fn insert(&self, key: K, value: V, handle: &Handle) -> bool {
        self.inner.harris_michael_insert(key, value, handle)
    }
    #[inline(always)]
    fn remove<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.inner.harris_michael_remove(key, handle)
    }
}

pub struct HHSList<K, V>
where
    K: 'static + Send + Sync,
    V: 'static + Send + Sync,
{
    inner: List<K, V>,
}

impl<K, V> ConcurrentMap<K, V> for HHSList<K, V>
where
    K: Send + Sync + Ord + Default,
    V: Send + Sync + Default,
{
    type ValueRef<'h> = VHolder<'h, K, V>;

    fn new() -> Self {
        HHSList { inner: List::new() }
    }

    #[inline(always)]
    fn get<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.inner.harris_herlihy_shavit_get(key, handle)
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

pub struct HashMap<K, V>
where
    K: 'static + Send + Sync + Ord + Default,
    V: 'static + Send + Sync + Default,
{
    buckets: Vec<HHSList<K, V>>,
}

impl<K, V> HashMap<K, V>
where
    K: 'static + Send + Sync + Ord + Default + Hash,
    V: 'static + Send + Sync + Default,
{
    pub fn with_capacity(n: usize) -> Self {
        let mut buckets = Vec::with_capacity(n);
        for _ in 0..n {
            buckets.push(HHSList::new());
        }

        HashMap { buckets }
    }

    #[inline]
    pub fn get_bucket(&self, index: usize) -> &HHSList<K, V> {
        unsafe { self.buckets.get_unchecked(index % self.buckets.len()) }
    }

    #[inline]
    fn hash(k: &K) -> usize {
        let mut s = DefaultHasher::new();
        k.hash(&mut s);
        s.finish() as usize
    }
}

impl<K, V> ConcurrentMap<K, V> for HashMap<K, V>
where
    K: 'static + Send + Sync + Ord + Default + Hash,
    V: 'static + Send + Sync + Default,
{
    type ValueRef<'h> = VHolder<'h, K, V>;

    fn new() -> Self {
        Self::with_capacity(30000)
    }

    #[inline(always)]
    fn get<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        let i = Self::hash(key);
        self.get_bucket(i).get(key, handle)
    }
    #[inline(always)]
    fn insert(&self, key: K, value: V, handle: &Handle) -> bool {
        let i = Self::hash(&key);
        self.get_bucket(i).insert(key, value, handle)
    }
    #[inline(always)]
    fn remove<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        let i = Self::hash(key);
        self.get_bucket(i).remove(key, handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Smoke tests
    #[test]
    fn smoke_harris() {
        map_common::smoke::<HList<i32, String>>();
    }

    #[test]
    fn smoke_harris_michael() {
        map_common::smoke::<HMList<i32, String>>();
    }

    #[test]
    fn smoke_harris_herlihy_shavit() {
        map_common::smoke::<HHSList<i32, String>>();
    }

    #[test]
    fn smoke_hash_map() {
        map_common::smoke::<HashMap<i32, String>>();
    }

    // Basic operation tests
    #[test]
    fn basic_operations_harris() {
        map_common::test_basic_operations::<HList<i32, String>>();
    }

    #[test]
    fn basic_operations_harris_michael() {
        map_common::test_basic_operations::<HMList<i32, String>>();
    }

    #[test]
    fn basic_operations_harris_herlihy_shavit() {
        map_common::test_basic_operations::<HHSList<i32, String>>();
    }

    #[test]
    fn basic_operations_hash_map() {
        map_common::test_basic_operations::<HashMap<i32, String>>();
    }

    // Multiple elements tests
    #[test]
    fn multiple_elements_harris() {
        map_common::test_multiple_elements::<HList<i32, String>>();
    }

    #[test]
    fn multiple_elements_harris_michael() {
        map_common::test_multiple_elements::<HMList<i32, String>>();
    }

    #[test]
    fn multiple_elements_harris_herlihy_shavit() {
        map_common::test_multiple_elements::<HHSList<i32, String>>();
    }

    #[test]
    fn multiple_elements_hash_map() {
        map_common::test_multiple_elements::<HashMap<i32, String>>();
    }

    // Reverse order insert tests
    #[test]
    fn reverse_order_insert_harris() {
        map_common::test_reverse_order_insert::<HList<i32, String>>();
    }

    #[test]
    fn reverse_order_insert_harris_michael() {
        map_common::test_reverse_order_insert::<HMList<i32, String>>();
    }

    #[test]
    fn reverse_order_insert_harris_herlihy_shavit() {
        map_common::test_reverse_order_insert::<HHSList<i32, String>>();
    }

    #[test]
    fn reverse_order_insert_hash_map() {
        map_common::test_reverse_order_insert::<HashMap<i32, String>>();
    }

    // Concurrent insert/remove tests
    #[test]
    fn concurrent_insert_remove_harris() {
        map_common::test_concurrent_insert_remove::<HList<i32, String>>();
    }

    #[test]
    fn concurrent_insert_remove_harris_michael() {
        map_common::test_concurrent_insert_remove::<HMList<i32, String>>();
    }

    #[test]
    fn concurrent_insert_remove_harris_herlihy_shavit() {
        map_common::test_concurrent_insert_remove::<HHSList<i32, String>>();
    }

    #[test]
    fn concurrent_insert_remove_hash_map() {
        map_common::test_concurrent_insert_remove::<HashMap<i32, String>>();
    }

    // Stress tests (disabled by default)
    // Recommended: cargo test --release -- --ignored
    // With address sanitizer: RUSTFLAGS="-Z sanitizer=address" cargo +nightly test --release -- --ignored
    #[test]
    #[ignore]
    #[serial]
    fn stress_harris() {
        map_common::stress_test_list::<HList<i32, String>>();
    }

    #[test]
    #[ignore]
    #[serial]
    fn stress_harris_michael() {
        map_common::stress_test_list::<HMList<i32, String>>();
    }

    #[test]
    #[ignore]
    #[serial]
    fn stress_harris_herlihy_shavit() {
        map_common::stress_test_list::<HHSList<i32, String>>();
    }

    #[test]
    #[ignore]
    #[serial]
    fn stress_hash_map() {
        map_common::stress_test_list::<HashMap<i32, String>>();
    }
}
