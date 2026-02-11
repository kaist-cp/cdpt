#[path = "common/mod.rs"]
mod map_common;

#[macro_use]
extern crate bitflags;
use cdpt::{AtomicShared, AtomicSharedOption, Guard, Handle, Local, TraceObj, TracePtr, pin};
use map_common::{ConcurrentMap, ValueRef};

use std::cell::{LazyCell, UnsafeCell};
use std::cmp;
use std::sync::atomic::Ordering;

bitflags! {
    /// TODO
    /// A remove operation is registered by marking the corresponding edges: the (parent, target)
    /// edge is _flagged_ and the (parent, sibling) edge is _tagged_.
    struct Marks: usize {
        const FLAG = 1usize.wrapping_shl(1);
        const TAG  = 1usize.wrapping_shl(0);
    }
}

impl Marks {
    fn new(flag: bool, tag: bool) -> Self {
        (if flag { Marks::FLAG } else { Marks::empty() })
            | (if tag { Marks::TAG } else { Marks::empty() })
    }

    fn flag(self) -> bool {
        !(self & Marks::FLAG).is_empty()
    }

    fn tag(self) -> bool {
        !(self & Marks::TAG).is_empty()
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Key<K> {
    Fin(K),
    Inf,
}

impl<K> PartialOrd for Key<K>
where
    K: PartialOrd,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Key::Fin(k1), Key::Fin(k2)) => k1.partial_cmp(k2),
            (Key::Fin(_), Key::Inf) => Some(std::cmp::Ordering::Less),
            (Key::Inf, Key::Fin(_)) => Some(std::cmp::Ordering::Greater),
            (Key::Inf, Key::Inf) => Some(std::cmp::Ordering::Equal),
        }
    }
}

impl<K> PartialEq<K> for Key<K>
where
    K: PartialEq,
{
    fn eq(&self, rhs: &K) -> bool {
        match self {
            Key::Fin(k) => k == rhs,
            _ => false,
        }
    }
}

impl<K> PartialOrd<K> for Key<K>
where
    K: PartialOrd,
{
    fn partial_cmp(&self, rhs: &K) -> Option<std::cmp::Ordering> {
        match self {
            Key::Fin(k) => k.partial_cmp(rhs),
            _ => Some(std::cmp::Ordering::Greater),
        }
    }
}

impl<K> Key<K>
where
    K: Ord,
{
    fn cmp(&self, rhs: &K) -> std::cmp::Ordering {
        match self {
            Key::Fin(k) => k.cmp(rhs),
            _ => std::cmp::Ordering::Greater,
        }
    }
}

#[derive(TraceObj)]
pub struct Node<K, V>
where
    K: 'static + Sync + Send,
    V: 'static + Sync + Send,
{
    // A key of a node can be mutated only during insertion (but before a linearization point
    // where makes it globally visible). After a successful insertion, it is virtually immutable
    // and multiple threads may read concurrently.
    key: UnsafeCell<Key<K>>,
    value: Option<V>,
    left: AtomicSharedOption<Node<K, V>>,
    right: AtomicSharedOption<Node<K, V>>,
}

unsafe impl<K, V> Sync for Node<K, V>
where
    K: 'static + Sync + Send,
    V: 'static + Sync + Send,
{
}

unsafe impl<K, V> Send for Node<K, V>
where
    K: 'static + Sync + Send,
    V: 'static + Sync + Send,
{
}

impl<K, V> Node<K, V>
where
    K: 'static + Sync + Send + Clone,
    V: 'static + Sync + Send + Clone,
{
    fn new_leaf(key: Key<K>, value: Option<V>) -> Node<K, V> {
        Node {
            key: UnsafeCell::new(key),
            value,
            left: AtomicSharedOption::none(),
            right: AtomicSharedOption::none(),
        }
    }

    /// Make a new internal node, consuming the given left and right nodes,
    /// using the right node's key.
    fn new_internal(left: Node<K, V>, right: Node<K, V>, guard: &Guard) -> Node<K, V> {
        Node {
            key: UnsafeCell::new(right.key().clone()),
            value: None,
            left: AtomicSharedOption::some(left, guard),
            right: AtomicSharedOption::some(right, guard),
        }
    }

    /// # Safety
    ///
    /// This node is not yet globally visible (i.e., not yet inserted to the tree).
    unsafe fn set_key(&self, key: Key<K>) {
        unsafe { *self.key.get() = key };
    }

    fn key(&self) -> &Key<K> {
        unsafe { &*self.key.get() }
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
        self.node.value.as_ref().unwrap()
    }
}

enum Direction {
    L,
    R,
}

/// All Shared<_> are unmarked.
///
/// All of the edges of path from `successor` to `parent` are in the process of removal.
struct SeekRecord<'g, K, V>
where
    K: 'static + Sync + Send + Clone,
    V: 'static + Sync + Send + Clone,
{
    /// Parent of `successor`
    ancestor: Local<'g, Guard, Node<K, V>>,
    /// The first internal node with a marked outgoing edge
    successor: Local<'g, Guard, Node<K, V>>,
    /// The direction of successor from ancestor.
    successor_dir: Direction,
    /// Parent of `leaf`
    parent: Local<'g, Guard, Node<K, V>>,
    /// The end of the access path.
    leaf: Local<'g, Guard, Node<K, V>>,
    /// The direction of leaf from parent.
    leaf_dir: Direction,
}

impl<'g, K, V> SeekRecord<'g, K, V>
where
    K: 'static + Sync + Send + Clone,
    V: 'static + Sync + Send + Clone,
{
    fn successor_addr(&'g self) -> &'g AtomicSharedOption<Node<K, V>> {
        match self.successor_dir {
            Direction::L => &self.ancestor.left,
            Direction::R => &self.ancestor.right,
        }
    }

    fn leaf_addr(&'g self) -> &'g AtomicSharedOption<Node<K, V>> {
        match self.leaf_dir {
            Direction::L => &self.parent.left,
            Direction::R => &self.parent.right,
        }
    }

    fn leaf_sibling_addr(&'g self) -> &'g AtomicSharedOption<Node<K, V>> {
        match self.leaf_dir {
            Direction::L => &self.parent.right,
            Direction::R => &self.parent.left,
        }
    }
}

pub struct NMTreeMap<K, V>
where
    K: 'static + Sync + Send + Clone,
    V: 'static + Sync + Send + Clone,
{
    r: AtomicShared<Node<K, V>>,
}

impl<K, V> Default for NMTreeMap<K, V>
where
    K: 'static + Sync + Send + Clone + Ord,
    V: 'static + Sync + Send + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> NMTreeMap<K, V>
where
    K: 'static + Sync + Send + Clone + Ord,
    V: 'static + Sync + Send + Clone,
{
    pub fn new() -> Self {
        // An empty tree has 5 default nodes with infinite keys so that the SeekRecord is allways
        // well-defined.
        //          r
        //         / \
        //        s  inf2
        //       / \
        //   inf0   inf1
        let guard = &pin();
        let inf0 = Node::new_leaf(Key::Inf, None);
        let inf1 = Node::new_leaf(Key::Inf, None);
        let inf2 = Node::new_leaf(Key::Inf, None);
        let s = Node::new_internal(inf0, inf1, guard);
        let r = Node::new_internal(s, inf2, guard);
        NMTreeMap {
            r: AtomicShared::new(r, guard),
        }
    }

    // All `Shared<_>` fields are unmarked.
    fn seek<'g>(&'g self, key: &K, guard: &'g Guard) -> SeekRecord<'g, K, V> {
        let r = self.r.load(Ordering::Relaxed, guard);
        let s = r.left.load(Ordering::Relaxed, guard).unwrap();
        let (leaf, leaf_tag) = s.left.load_with_tag(Ordering::Relaxed, guard);
        let leaf = leaf.unwrap();

        let mut record = SeekRecord {
            ancestor: r,
            successor: s,
            successor_dir: Direction::L,
            parent: s,
            leaf,
            leaf_dir: Direction::L,
        };

        let mut prev_tag = Marks::from_bits_truncate(leaf_tag).tag();
        let mut curr_dir = Direction::L;
        let (mut curr, mut curr_tag) = leaf.left.load_with_tag(Ordering::Relaxed, guard);

        while let Some(curr_node) = curr {
            if !prev_tag {
                // untagged edge: advance ancestor and successor pointers
                record.ancestor = record.parent;
                record.successor = record.leaf;
                record.successor_dir = record.leaf_dir;
            }

            // advance parent and leaf pointers
            record.parent = record.leaf;
            record.leaf = curr_node;
            record.leaf_dir = curr_dir;

            // update other variables
            prev_tag = Marks::from_bits_truncate(curr_tag).tag();
            (curr_dir, (curr, curr_tag)) = if curr_node.key().cmp(key) == cmp::Ordering::Greater {
                (
                    Direction::L,
                    curr_node.left.load_with_tag(Ordering::Acquire, guard),
                )
            } else {
                (
                    Direction::R,
                    curr_node.right.load_with_tag(Ordering::Acquire, guard),
                )
            }
        }

        record
    }

    /// Similar to `seek`, but traverse the tree with only two pointers
    fn seek_leaf<'g>(&'g self, key: &K, guard: &'g Guard) -> Local<'g, Guard, Node<K, V>> {
        let r = self.r.load(Ordering::Relaxed, guard);
        let s = r.left.load(Ordering::Relaxed, guard).unwrap();

        let mut leaf = s.left.load(Ordering::Acquire, guard).unwrap();
        let mut curr = leaf.left.load(Ordering::Acquire, guard);

        while let Some(curr_node) = curr {
            leaf = curr_node;

            if curr_node.key().cmp(key) == cmp::Ordering::Greater {
                curr = curr_node.left.load(Ordering::Acquire, guard);
            } else {
                curr = curr_node.right.load(Ordering::Acquire, guard);
            }
        }
        leaf
    }

    /// Physically removes node.
    ///
    /// Returns true if it successfully unlinks the flagged node in `record`.
    fn cleanup(&self, record: &SeekRecord<'_, K, V>, guard: &Guard) -> bool {
        // Identify the node(subtree) that will replace `successor`.
        let (_, leaf_m_tag) = record.leaf_addr().load_with_tag(Ordering::Acquire, guard);
        let leaf_flag = Marks::from_bits_truncate(leaf_m_tag).flag();
        let target_sibling_addr = if leaf_flag {
            record.leaf_sibling_addr()
        } else {
            record.leaf_addr()
        };

        // NOTE: the ibr implementation uses CAS
        // tag (parent, sibling) edge -> all of the parent's edges can't change now
        // TODO: Is Release enough?
        target_sibling_addr.fetch_tag_or(Marks::TAG.bits(), Ordering::AcqRel, guard);

        // Try to replace (ancestor, successor) w/ (ancestor, sibling).
        // Since (parent, sibling) might have been concurrently flagged, copy
        // the flag to the new edge (ancestor, sibling).
        let (target_sibling, target_sibling_tag) =
            target_sibling_addr.load_with_tag(Ordering::Acquire, guard);
        let flag = Marks::from_bits_truncate(target_sibling_tag).flag();
        let is_unlinked = record
            .successor_addr()
            .compare_exchange_with_tag(
                (Some(&record.successor), 0),
                (target_sibling.as_ref(), Marks::new(flag, false).bits()),
                Ordering::AcqRel,
                Ordering::Acquire,
                guard,
            )
            .is_ok();

        is_unlinked
    }

    pub fn get<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        let guard = &handle.pin();
        let leaf = self.seek_leaf(key, guard);

        if leaf.key().cmp(key) != cmp::Ordering::Equal {
            return None;
        }

        Some(VHolder::new(leaf, handle))
    }

    pub fn insert(&self, key: K, value: V, handle: &Handle) -> bool {
        let guard = &handle.pin();
        let new_leaf =
            LazyCell::new(|| Local::new(Node::new_leaf(Key::Fin(key.clone()), Some(value)), guard));

        let new_internal = LazyCell::new(|| {
            Local::new(
                Node {
                    key: UnsafeCell::new(Key::Inf), // temporary placeholder
                    value: None,
                    left: AtomicSharedOption::none(),
                    right: AtomicSharedOption::none(),
                },
                guard,
            )
        });

        loop {
            let record = self.seek(&key, guard);
            let leaf = record.leaf;

            let (new_left, new_right) = match leaf.key().cmp(&key) {
                cmp::Ordering::Equal => return false,
                cmp::Ordering::Greater => (&*new_leaf, &leaf),
                cmp::Ordering::Less => (&leaf, &*new_leaf),
            };

            // Safety: `new_internal` is not yet inserted (i.e., not globally visible).
            unsafe { new_internal.set_key(new_right.key().clone()) };
            new_internal
                .left
                .store(Some(&new_left), Ordering::Relaxed, guard);
            new_internal
                .right
                .store(Some(&new_right), Ordering::Relaxed, guard);

            // NOTE: record.leaf_addr is called childAddr in the paper.
            match record.leaf_addr().compare_exchange_with_tag(
                (Some(&record.leaf), Marks::empty().bits()),
                (Some(&new_internal), Marks::empty().bits()),
                Ordering::AcqRel,
                Ordering::Acquire,
                guard,
            ) {
                Ok(_) => return true,
                Err((current, _)) => {
                    // Insertion failed. Help the conflicting remove operation if needed.
                    // NOTE: The paper version checks if any of the mark is set, which is redundant.
                    if Local::opt_ptr_eq(current.as_ref(), Some(&record.leaf)) {
                        self.cleanup(&record, guard);
                    }
                }
            }
        }
    }

    pub fn remove<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        let guard = &handle.pin();
        // `leaf` and `value` are the snapshot of the node to be deleted.
        // NOTE: The paper version uses one big loop for both phases.
        // injection phase
        let leaf = loop {
            let record = self.seek(key, guard);

            // candidates
            let leaf = record.leaf;

            if leaf.key().cmp(key) != cmp::Ordering::Equal {
                return None;
            }

            // Try injecting the deletion flag.
            match record.leaf_addr().compare_exchange_with_tag(
                (Some(&record.leaf), Marks::empty().bits()),
                (Some(&record.leaf), Marks::new(true, false).bits()),
                Ordering::AcqRel,
                Ordering::Acquire,
                guard,
            ) {
                Ok(_) => {
                    // Finalize the node to be removed
                    if self.cleanup(&record, guard) {
                        return Some(VHolder::new(leaf, handle));
                    }
                    // In-place cleanup failed. Enter the cleanup phase.
                    break leaf;
                }
                Err((current, _)) => {
                    // Flagging failed.
                    // case 1. record.leaf_addr(e.current) points to another node: restart.
                    // case 2. Another thread flagged/tagged the edge to leaf: help and restart
                    // NOTE: The paper version checks if any of the mark is set, which is redundant.
                    if Local::opt_ptr_eq(current.as_ref(), Some(&record.leaf)) {
                        self.cleanup(&record, guard);
                    }
                }
            }
        };

        // cleanup phase
        loop {
            let record = self.seek(key, guard);
            if !Local::ptr_eq(&record.leaf, &leaf) {
                // The edge to leaf flagged for deletion was removed by a helping thread
                return Some(VHolder::new(leaf, handle));
            }

            // leaf is still present in the tree.
            if self.cleanup(&record, guard) {
                return Some(VHolder::new(leaf, handle));
            }
        }
    }
}

impl<K, V> ConcurrentMap<K, V> for NMTreeMap<K, V>
where
    K: 'static + Sync + Send + Clone + Ord,
    V: 'static + Sync + Send + Clone,
{
    type ValueRef<'h> = VHolder<'h, K, V>;

    fn new() -> Self {
        Self::new()
    }

    #[inline(always)]
    fn get<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.get(key, handle)
    }
    #[inline(always)]
    fn insert(&self, key: K, value: V, handle: &Handle) -> bool {
        self.insert(key, value, handle)
    }
    #[inline(always)]
    fn remove<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.remove(key, handle)
    }
}

fn main() {
    map_common::stress_test::<NMTreeMap<i32, String>>();
}

#[cfg(test)]
mod tests {
    use super::*;

    // Smoke test
    #[test]
    fn smoke_nm_tree() {
        map_common::smoke::<NMTreeMap<i32, String>>();
    }

    // Basic operation tests
    #[test]
    fn basic_operations_nm_tree() {
        map_common::test_basic_operations::<NMTreeMap<i32, String>>();
    }

    // Multiple elements tests
    #[test]
    fn multiple_elements_nm_tree() {
        map_common::test_multiple_elements::<NMTreeMap<i32, String>>();
    }

    // Reverse order insert tests
    #[test]
    fn reverse_order_insert_nm_tree() {
        map_common::test_reverse_order_insert::<NMTreeMap<i32, String>>();
    }

    // Concurrent insert/remove tests
    #[test]
    fn concurrent_insert_remove_nm_tree() {
        map_common::test_concurrent_insert_remove::<NMTreeMap<i32, String>>();
    }

    // Stress tests (disabled by default)
    // To run: cargo test --release --all-targets -- --ignored
    // To run with address sanitizer: RUSTFLAGS="-Z sanitizer=address" cargo +nightly test --release --all-targets -- --ignored
    // (Set `--target` for your machine: https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html)
    #[test]
    #[ignore]
    fn stress_nm_tree() {
        map_common::stress_test::<NMTreeMap<i32, String>>();
    }
}
