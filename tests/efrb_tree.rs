mod map_common;

#[macro_use]
extern crate bitflags;
use cdpt::{
    AtomicShared, AtomicSharedOption, Guard, Handle, Local, Shared, TraceObj, TracePtr, pin,
};
use map_common::{ConcurrentMap, ValueRef};

use std::sync::atomic::Ordering;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    struct UpdateTag: usize {
        const CLEAN = 0usize;
        const DFLAG = 1usize;
        const IFLAG = 2usize;
        const MARK = 3usize;
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Key<K> {
    Fin(K),
    Inf1,
    Inf2,
}

impl<K> PartialOrd for Key<K>
where
    K: PartialOrd,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Key::Fin(k1), Key::Fin(k2)) => k1.partial_cmp(k2),
            (Key::Fin(_), Key::Inf1) => Some(std::cmp::Ordering::Less),
            (Key::Fin(_), Key::Inf2) => Some(std::cmp::Ordering::Less),
            (Key::Inf1, Key::Fin(_)) => Some(std::cmp::Ordering::Greater),
            (Key::Inf1, Key::Inf1) => Some(std::cmp::Ordering::Equal),
            (Key::Inf1, Key::Inf2) => Some(std::cmp::Ordering::Less),
            (Key::Inf2, Key::Fin(_)) => Some(std::cmp::Ordering::Greater),
            (Key::Inf2, Key::Inf1) => Some(std::cmp::Ordering::Greater),
            (Key::Inf2, Key::Inf2) => Some(std::cmp::Ordering::Equal),
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
    key: Key<K>,
    value: Option<V>,
    // tag on low bits: {Clean, DFlag, IFlag, Mark}
    update: AtomicSharedOption<Update<K, V>>,
    left: AtomicSharedOption<Node<K, V>>,
    right: AtomicSharedOption<Node<K, V>>,
}

#[derive(TraceObj)]
pub enum Update<K, V>
where
    K: 'static + Sync + Send,
    V: 'static + Sync + Send,
{
    Insert {
        p: Shared<Node<K, V>>,
        new_internal: Shared<Node<K, V>>,
        l: Shared<Node<K, V>>,
    },
    Delete {
        gp: Shared<Node<K, V>>,
        p: Shared<Node<K, V>>,
        l: Shared<Node<K, V>>,
        pupdate: Option<Shared<Update<K, V>>>,
    },
}

impl<K, V> Node<K, V>
where
    K: 'static + Sync + Send,
    V: 'static + Sync + Send,
{
    pub fn internal(key: Key<K>, value: Option<V>, left: Self, right: Self, guard: &Guard) -> Self {
        Self {
            key,
            value,
            update: AtomicSharedOption::none(),
            left: AtomicSharedOption::some(left, guard),
            right: AtomicSharedOption::some(right, guard),
        }
    }

    pub fn leaf(key: Key<K>, value: Option<V>) -> Self {
        Self {
            key,
            value,
            update: AtomicSharedOption::none(),
            left: AtomicSharedOption::none(),
            right: AtomicSharedOption::none(),
        }
    }

    #[inline]
    pub fn is_leaf(&self, guard: &Guard) -> bool {
        self.left.load(Ordering::Acquire, guard).is_none()
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

struct Cursor<'g, K, V>
where
    K: 'static + Sync + Send,
    V: 'static + Sync + Send,
{
    gp: Option<Local<'g, Guard, Node<K, V>>>,
    p: Option<Local<'g, Guard, Node<K, V>>>,
    l: Local<'g, Guard, Node<K, V>>,
    // Pairs of a reference and an `UpdateTag`.
    pupdate: (Option<Local<'g, Guard, Update<K, V>>>, usize),
    gpupdate: (Option<Local<'g, Guard, Update<K, V>>>, usize),
}

impl<'g, K, V> Cursor<'g, K, V>
where
    K: 'static + Sync + Send + Ord + Clone,
    V: 'static + Sync + Send + Clone,
{
    fn new(root: Local<'g, Guard, Node<K, V>>) -> Self {
        Self {
            gp: None,
            p: None,
            l: root,
            pupdate: (None, 0),
            gpupdate: (None, 0),
        }
    }

    /// Used by Insert, Delete and Find to traverse a branch of the BST.
    ///
    /// # Safety
    /// It satisfies following postconditions:
    ///
    /// 1. l points to a Leaf node and p points to an Internal node
    /// 2. Either p → left has contained l (if k<p → key) or p → right has contained l (if k ≥ p → key)
    /// 3. p → update has contained pupdate
    /// 4. if l → key != Inf1, then the following three statements hold:
    ///     - gp points to an Internal node
    ///     - either gp → left has contained p (if k < gp → key) or gp → right has contained p (if k ≥ gp → key)
    ///     - gp → update has contained gpupdate
    #[inline]
    fn search(&mut self, key: &K, guard: &'g Guard) {
        loop {
            if self.l.is_leaf(guard) {
                break;
            }
            self.gp = self.p;
            self.p = Some(self.l);
            self.gpupdate = self.pupdate;
            self.pupdate = self.l.update.load_with_tag(Ordering::Acquire, guard);
            self.l = match self.l.key.cmp(key) {
                std::cmp::Ordering::Greater => self.l.left.load(Ordering::Acquire, guard).unwrap(),
                _ => self.l.right.load(Ordering::Acquire, guard).unwrap(),
            }
        }
    }
}

pub struct EFRBTree<K, V>
where
    K: 'static + Sync + Send,
    V: 'static + Sync + Send,
{
    root: AtomicShared<Node<K, V>>,
}

impl<K, V> Default for EFRBTree<K, V>
where
    K: 'static + Sync + Send + Ord + Clone,
    V: 'static + Sync + Send + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> EFRBTree<K, V>
where
    K: 'static + Sync + Send + Ord + Clone,
    V: 'static + Sync + Send + Clone,
{
    pub fn new() -> Self {
        let guard = &pin();
        Self {
            root: AtomicShared::new(
                Node::internal(
                    Key::Inf2,
                    None,
                    Node::leaf(Key::Inf1, None),
                    Node::leaf(Key::Inf2, None),
                    guard,
                ),
                guard,
            ),
        }
    }

    pub fn find<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        let guard = handle.pin();
        let mut cursor = Cursor::new(self.root.load(Ordering::Relaxed, &guard));
        cursor.search(key, &guard);
        if cursor.l.key.eq(key) {
            Some(VHolder::new(cursor.l, handle))
        } else {
            None
        }
    }

    pub fn insert(&self, key: &K, value: V, handle: &Handle) -> bool {
        let guard = &handle.pin();
        loop {
            let mut cursor = Cursor::new(self.root.load(Ordering::Relaxed, guard));
            cursor.search(key, guard);
            let p_node = cursor.p.unwrap();

            if cursor.l.key == *key {
                return false;
            } else if cursor.pupdate.1 != UpdateTag::CLEAN.bits() {
                self.help(cursor.pupdate.0.unwrap(), cursor.pupdate.1, guard);
            } else {
                let new = Node::leaf(Key::Fin(key.clone()), Some(value.clone()));
                let new_sibling = Node::leaf(cursor.l.key.clone(), cursor.l.value.clone());

                let (left, right) = match new.key.partial_cmp(&new_sibling.key) {
                    Some(std::cmp::Ordering::Less) => (new, new_sibling),
                    _ => (new_sibling, new),
                };

                let new_internal = Local::new(
                    Node::internal(
                        // key field max(k, l → key)
                        right.key.clone(),
                        None,
                        // two child fields equal to new and newSibling
                        // (the one with the smaller key is the left child)
                        left,
                        right,
                        guard,
                    ),
                    guard,
                );

                let op = Update::Insert {
                    p: p_node.as_shared(),
                    new_internal: new_internal.as_shared(),
                    l: cursor.l.as_shared(),
                };

                let new_pupdate = Local::new(op, guard);

                match p_node.update.compare_exchange_with_tag(
                    (cursor.pupdate.0.as_ref(), cursor.pupdate.1),
                    (Some(&new_pupdate), UpdateTag::IFLAG.bits()),
                    Ordering::Release,
                    Ordering::Relaxed,
                    guard,
                ) {
                    Ok(_) => {
                        self.help_insert(new_pupdate, guard);
                        return true;
                    }
                    Err((e, tag)) => {
                        self.help(e.unwrap(), tag, guard);
                    }
                }
            }
        }
    }

    pub fn delete<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        let guard = &handle.pin();
        loop {
            let mut cursor = Cursor::new(self.root.load(Ordering::Relaxed, guard));
            cursor.search(key, guard);

            if cursor.gp.is_none() {
                // The tree is empty. There's no more things to do.
                return None;
            }

            if cursor.l.key != Key::Fin(key.clone()) {
                return None;
            }
            if cursor.gpupdate.1 != UpdateTag::CLEAN.bits() {
                self.help(cursor.gpupdate.0.unwrap(), cursor.gpupdate.1, guard);
            } else if cursor.pupdate.1 != UpdateTag::CLEAN.bits() {
                self.help(cursor.pupdate.0.unwrap(), cursor.pupdate.1, guard);
            } else {
                let op = Update::Delete {
                    // gp: AtomicSharedOption::from(cursor.gp),
                    gp: cursor.gp.unwrap().as_shared(),
                    p: cursor.p.unwrap().as_shared(),
                    l: cursor.l.as_shared(),
                    pupdate: cursor.pupdate.0.map(|l| l.as_shared()),
                };
                let new_update = Local::new(op, guard);
                match cursor.gp.unwrap().update.compare_exchange_with_tag(
                    (cursor.gpupdate.0.as_ref(), cursor.gpupdate.1),
                    (Some(&new_update), UpdateTag::DFLAG.bits()),
                    Ordering::Release,
                    Ordering::Relaxed,
                    guard,
                ) {
                    Ok(_) => {
                        if self.help_delete(new_update, guard) {
                            return Some(VHolder::new(cursor.l, handle));
                        }
                    }
                    Err((e, tag)) => {
                        self.help(e.unwrap(), tag, guard);
                    }
                }
            }
        }
    }

    #[inline]
    fn help<'g>(&self, update: Local<'g, Guard, Update<K, V>>, tag: usize, guard: &'g Guard) {
        match UpdateTag::from_bits_truncate(tag) {
            UpdateTag::IFLAG => self.help_insert(update, guard),
            UpdateTag::MARK => self.help_marked(update, guard),
            UpdateTag::DFLAG => {
                let _ = self.help_delete(update, guard);
            }
            _ => {}
        }
    }

    fn help_delete<'g>(&self, op: Local<'g, Guard, Update<K, V>>, guard: &'g Guard) -> bool {
        // Precondition: op points to a DInfo record (i.e., it is not ⊥)
        let Update::Delete { gp, p, pupdate, .. } = &*op else {
            panic!("op is not pointing to a DInfo record");
        };

        let new_pupdate = pupdate.as_ref().map(|s| s.as_local(guard));

        match p.update.compare_exchange_with_tag(
            (new_pupdate.as_ref(), UpdateTag::CLEAN.bits()),
            (Some(&op), UpdateTag::MARK.bits()),
            Ordering::Release,
            Ordering::Acquire,
            guard,
        ) {
            Ok(_) => {
                // (prev value) = op → pupdate
                self.help_marked(op, guard);
                true
            }
            Err((e, e_tag)) => {
                if Local::opt_ptr_eq(e.as_ref(), Some(&op)) && e_tag == UpdateTag::MARK.bits() {
                    // (prev value) = <Mark, op>
                    self.help_marked(op, guard);
                    return true;
                }
                if let Some(e) = e {
                    self.help(e, e_tag, guard);
                    let _ = gp.update.compare_exchange_with_tag(
                        (Some(&op), UpdateTag::DFLAG.bits()),
                        (Some(&op), UpdateTag::CLEAN.bits()),
                        Ordering::Release,
                        Ordering::Relaxed,
                        guard,
                    );
                }
                false
            }
        }
    }

    fn help_marked<'g>(&self, op: Local<'g, Guard, Update<K, V>>, guard: &'g Guard) {
        // Precondition: op points to a DInfo record (i.e., it is not ⊥)
        let Update::Delete { gp, p, l, .. } = &*op else {
            panic!("op is not pointing to a DInfo record");
        };

        // Set other to point to the sibling of the node to which op → l points
        let other = if Local::opt_ptr_eq(
            p.right.load(Ordering::Acquire, guard).as_ref(),
            Some(&l.as_local(guard)),
        ) {
            &p.left
        } else {
            &p.right
        };
        // Splice the node to which op → p points out of the tree, replacing it by other
        let other_sh = other.load(Ordering::Acquire, guard).unwrap();

        self.cas_child(gp.as_local(guard), p.as_local(guard), other_sh, guard);

        let _ = gp.update.compare_exchange_with_tag(
            (Some(&op), UpdateTag::DFLAG.bits()),
            (Some(&op), UpdateTag::CLEAN.bits()),
            Ordering::Release,
            Ordering::Relaxed,
            guard,
        );
    }

    fn help_insert<'g>(&self, op: Local<'g, Guard, Update<K, V>>, guard: &'g Guard) {
        // Precondition: op points to an IInfo record (i.e., it is not ⊥)
        let Update::Insert { p, new_internal, l } = &*op else {
            panic!("op is not pointing to an IInfo record");
        };

        self.cas_child(
            p.as_local(guard),
            l.as_local(guard),
            new_internal.as_local(guard),
            guard,
        );

        let _ = p.update.compare_exchange_with_tag(
            (Some(&op), UpdateTag::IFLAG.bits()),
            (Some(&op), UpdateTag::CLEAN.bits()),
            Ordering::Release,
            Ordering::Relaxed,
            guard,
        );
    }

    #[inline]
    fn cas_child<'g>(
        &self,
        parent: Local<'g, Guard, Node<K, V>>,
        old: Local<'g, Guard, Node<K, V>>,
        new: Local<'g, Guard, Node<K, V>>,
        guard: &'g Guard,
    ) {
        // Precondition: parent points to an Internal node and new points to a Node (i.e., neither is ⊥)
        // This routine tries to change one of the child fields of the node that parent points to from old to new.
        let node_to_cas = if new.key < parent.key {
            &parent.left
        } else {
            &parent.right
        };
        let _ = node_to_cas.compare_exchange(
            Some(&old),
            Some(&new),
            Ordering::Release,
            Ordering::Acquire,
            guard,
        );
    }
}

impl<K, V> ConcurrentMap<K, V> for EFRBTree<K, V>
where
    K: 'static + Sync + Send + Ord + Clone,
    V: 'static + Sync + Send + Clone,
{
    type ValueRef<'h> = VHolder<'h, K, V>;

    fn new() -> Self {
        EFRBTree::new()
    }

    #[inline(always)]
    fn get<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.find(key, handle)
    }

    #[inline(always)]
    fn insert(&self, key: K, value: V, handle: &Handle) -> bool {
        self.insert(&key, value, handle)
    }

    #[inline(always)]
    fn remove<'h>(&self, key: &K, handle: &'h Handle) -> Option<VHolder<'h, K, V>> {
        self.delete(key, handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Smoke test
    #[test]
    fn smoke_efrb_tree() {
        map_common::smoke::<EFRBTree<i32, String>>();
    }

    // Basic operation tests
    #[test]
    fn basic_operations_efrb_tree() {
        map_common::test_basic_operations::<EFRBTree<i32, String>>();
    }

    // Multiple elements tests
    #[test]
    fn multiple_elements_efrb_tree() {
        map_common::test_multiple_elements::<EFRBTree<i32, String>>();
    }

    // Reverse order insert tests
    #[test]
    fn reverse_order_insert_efrb_tree() {
        map_common::test_reverse_order_insert::<EFRBTree<i32, String>>();
    }

    // Concurrent insert/remove tests
    #[test]
    fn concurrent_insert_remove_efrb_tree() {
        map_common::test_concurrent_insert_remove::<EFRBTree<i32, String>>();
    }

    // Stress tests (disabled by default)
    // To run: cargo test -- --ignored
    // To run with address sanitizer: RUSTFLAGS="-Z sanitizer=address" cargo +nightly test -- --ignored
    #[test]
    #[ignore]
    #[serial]
    fn stress_efrb_tree() {
        map_common::stress_test::<EFRBTree<i32, String>>();
    }
}
