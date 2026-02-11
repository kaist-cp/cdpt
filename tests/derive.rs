use std::sync::atomic::Ordering;

use std::sync::Arc;

use cdpt::{AtomicShared, AtomicSharedOption, Local, Shared, TraceObj, TracePtr, pin};

// ---- Struct with all pointer types ----

#[derive(TraceObj)]
struct AllPointers {
    shared: Shared<Leaf>,
    atomic: AtomicShared<Leaf>,
    atomic_opt: AtomicSharedOption<Leaf>,
}

#[derive(TraceObj)]
struct Leaf {
    value: usize,
}

#[test]
fn derive_struct_with_all_pointer_types() {
    let guard = pin();
    let leaf1 = Shared::new(Leaf { value: 1 }, &guard);
    let leaf2 = AtomicShared::new(Leaf { value: 2 }, &guard);
    let leaf3 = AtomicSharedOption::some(Leaf { value: 3 }, &guard);

    let parent = Local::new(
        AllPointers {
            shared: leaf1,
            atomic: leaf2,
            atomic_opt: leaf3,
        },
        &guard,
    );

    assert_eq!(parent.shared.value, 1);
    assert_eq!(parent.atomic.load(Ordering::SeqCst, &guard).value, 2);
    assert_eq!(
        parent
            .atomic_opt
            .load(Ordering::SeqCst, &guard)
            .unwrap()
            .value,
        3
    );
}

// ---- Struct with Option<AtomicSharedOption<T>> ----

#[derive(TraceObj)]
struct WithOption {
    maybe: Option<AtomicSharedOption<Leaf>>,
}

#[test]
fn derive_struct_with_option_some() {
    let guard = pin();
    let opt = AtomicSharedOption::some(Leaf { value: 42 }, &guard);
    let s = Local::new(WithOption { maybe: Some(opt) }, &guard);
    let inner = s.maybe.as_ref().unwrap();
    assert_eq!(inner.load(Ordering::SeqCst, &guard).unwrap().value, 42);
}

#[test]
fn derive_struct_with_option_none() {
    let guard = pin();
    let s = Local::new(WithOption { maybe: None }, &guard);
    assert!(s.maybe.is_none());
}

// ---- Struct with Vec<Shared<T>> ----

#[derive(TraceObj)]
struct WithVec {
    items: Vec<Shared<Leaf>>,
}

#[test]
fn derive_struct_with_vec() {
    let guard = pin();
    let items: Vec<_> = (0..5)
        .map(|i| Shared::new(Leaf { value: i }, &guard))
        .collect();
    let s = Local::new(WithVec { items }, &guard);
    for (i, item) in s.items.iter().enumerate() {
        assert_eq!(item.value, i);
    }
}

// ---- Struct with Box<[Shared<T>]> (boxed slice, iterable) ----

#[derive(TraceObj)]
struct WithBoxedSlice {
    items: Box<[Shared<Leaf>]>,
}

#[test]
fn derive_struct_with_boxed_slice() {
    let guard = pin();
    let items: Box<[Shared<Leaf>]> = vec![
        Shared::new(Leaf { value: 10 }, &guard),
        Shared::new(Leaf { value: 20 }, &guard),
    ]
    .into_boxed_slice();
    let s = Local::new(WithBoxedSlice { items }, &guard);
    assert_eq!(s.items[0].value, 10);
    assert_eq!(s.items[1].value, 20);
}

// ---- Struct with tuple ----

#[derive(TraceObj)]
struct WithTuple {
    pair: (Shared<Leaf>, Shared<Leaf>),
}

#[test]
fn derive_struct_with_tuple() {
    let guard = pin();
    let a = Shared::new(Leaf { value: 10 }, &guard);
    let b = Shared::new(Leaf { value: 20 }, &guard);
    let s = Local::new(WithTuple { pair: (a, b) }, &guard);
    assert_eq!(s.pair.0.value, 10);
    assert_eq!(s.pair.1.value, 20);
}

// ---- Struct with non-traced fields ----

#[derive(TraceObj)]
struct Mixed {
    name: String,
    count: usize,
    link: AtomicSharedOption<Leaf>,
}

#[test]
fn derive_struct_with_non_traced_fields() {
    let guard = pin();
    let s = Local::new(
        Mixed {
            name: "hello".to_string(),
            count: 42,
            link: AtomicSharedOption::some(Leaf { value: 7 }, &guard),
        },
        &guard,
    );
    assert_eq!(s.name, "hello");
    assert_eq!(s.count, 42);
    assert_eq!(s.link.load(Ordering::SeqCst, &guard).unwrap().value, 7);
}

// ---- Recursive struct ----

#[derive(TraceObj)]
struct Recursive {
    value: i32,
    next: AtomicSharedOption<Self>,
}

#[test]
fn derive_recursive_struct() {
    let guard = pin();
    let tail = Local::new(
        Recursive {
            value: 2,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let head = Local::new(
        Recursive {
            value: 1,
            next: AtomicSharedOption::from(Some(&tail)),
        },
        &guard,
    );

    assert_eq!(head.value, 1);
    let next = head.next.load(Ordering::SeqCst, &guard).unwrap();
    assert_eq!(next.value, 2);
    assert!(next.next.load(Ordering::SeqCst, &guard).is_none());
}

// ---- Generic struct ----

#[derive(TraceObj)]
struct GenericNode<T: 'static + Send + Sync> {
    item: T,
    next: AtomicSharedOption<Self>,
}

#[test]
fn derive_generic_struct() {
    let guard = pin();
    let node = Local::new(
        GenericNode::<usize> {
            item: 42,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    assert_eq!(node.item, 42);
}

#[test]
fn derive_generic_struct_string() {
    let guard = pin();
    let node = Local::new(
        GenericNode::<String> {
            item: "test".to_string(),
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    assert_eq!(node.item, "test");
}

// ---- Enum ----

#[derive(TraceObj)]
enum TreeNode {
    Leaf {
        _value: i32,
    },
    Internal {
        left: AtomicSharedOption<TreeNode>,
        right: AtomicSharedOption<TreeNode>,
    },
    Empty,
}

#[test]
fn derive_enum_leaf() {
    let guard = pin();
    let leaf = Local::new(TreeNode::Leaf { _value: 42 }, &guard);
    match &*leaf {
        TreeNode::Leaf { _value } => assert_eq!(*_value, 42),
        _ => panic!("Expected Leaf"),
    }
}

#[test]
fn derive_enum_internal() {
    let guard = pin();
    let left = Local::new(TreeNode::Leaf { _value: 1 }, &guard);
    let right = Local::new(TreeNode::Leaf { _value: 2 }, &guard);
    let internal = Local::new(
        TreeNode::Internal {
            left: AtomicSharedOption::from(Some(&left)),
            right: AtomicSharedOption::from(Some(&right)),
        },
        &guard,
    );

    match &*internal {
        TreeNode::Internal { left, right } => {
            match &*left.load(Ordering::SeqCst, &guard).unwrap() {
                TreeNode::Leaf { _value } => assert_eq!(*_value, 1),
                _ => panic!("Expected Leaf"),
            }
            match &*right.load(Ordering::SeqCst, &guard).unwrap() {
                TreeNode::Leaf { _value } => assert_eq!(*_value, 2),
                _ => panic!("Expected Leaf"),
            }
        }
        _ => panic!("Expected Internal"),
    }
}

#[test]
fn derive_enum_empty() {
    let guard = pin();
    let empty = Local::new(TreeNode::Empty, &guard);
    match &*empty {
        TreeNode::Empty => {}
        _ => panic!("Expected Empty"),
    }
}

// ---- Unnamed (tuple) struct ----

#[derive(TraceObj)]
struct TupleStruct(usize, AtomicSharedOption<Leaf>);

#[test]
fn derive_tuple_struct() {
    let guard = pin();
    let s = Local::new(
        TupleStruct(10, AtomicSharedOption::some(Leaf { value: 20 }, &guard)),
        &guard,
    );
    assert_eq!(s.0, 10);
    assert_eq!(s.1.load(Ordering::SeqCst, &guard).unwrap().value, 20);
}

// ---- Unnamed (tuple) enum variants ----

#[derive(TraceObj)]
#[allow(dead_code)]
enum TupleEnum {
    Single(Shared<Leaf>),
    Pair(Shared<Leaf>, Shared<Leaf>),
    None,
}

#[test]
fn derive_tuple_enum_single() {
    let guard = pin();
    let leaf = Shared::new(Leaf { value: 7 }, &guard);
    let e = Local::new(TupleEnum::Single(leaf), &guard);
    match &*e {
        TupleEnum::Single(l) => assert_eq!(l.value, 7),
        _ => panic!("Expected Single"),
    }
}

#[test]
fn derive_tuple_enum_pair() {
    let guard = pin();
    let a = Shared::new(Leaf { value: 1 }, &guard);
    let b = Shared::new(Leaf { value: 2 }, &guard);
    let e = Local::new(TupleEnum::Pair(a, b), &guard);
    match &*e {
        TupleEnum::Pair(a, b) => {
            assert_eq!(a.value, 1);
            assert_eq!(b.value, 2);
        }
        _ => panic!("Expected Pair"),
    }
}

// ---- Box<Shared<T>> (single boxed pointer, deref) ----

#[derive(TraceObj)]
struct WithBoxedShared {
    item: Box<Shared<Leaf>>,
}

#[test]
fn derive_struct_with_boxed_shared() {
    let guard = pin();
    let item = Box::new(Shared::new(Leaf { value: 99 }, &guard));
    let s = Local::new(WithBoxedShared { item }, &guard);
    assert_eq!(s.item.value, 99);
}

// ---- Arc<[Shared<T>]> (Arc of slice) ----

#[derive(TraceObj)]
struct WithArcSlice {
    items: Arc<[Shared<Leaf>]>,
}

#[test]
fn derive_struct_with_arc_slice() {
    let guard = pin();
    let items: Arc<[Shared<Leaf>]> = vec![
        Shared::new(Leaf { value: 1 }, &guard),
        Shared::new(Leaf { value: 2 }, &guard),
        Shared::new(Leaf { value: 3 }, &guard),
    ]
    .into();
    let s = Local::new(WithArcSlice { items }, &guard);
    assert_eq!(s.items[0].value, 1);
    assert_eq!(s.items[1].value, 2);
    assert_eq!(s.items[2].value, 3);
}

// ---- Array of Shared ----

#[derive(TraceObj)]
struct WithArray {
    items: [Shared<Leaf>; 3],
}

#[test]
fn derive_struct_with_array() {
    let guard = pin();
    let items = [
        Shared::new(Leaf { value: 10 }, &guard),
        Shared::new(Leaf { value: 20 }, &guard),
        Shared::new(Leaf { value: 30 }, &guard),
    ];
    let s = Local::new(WithArray { items }, &guard);
    assert_eq!(s.items[0].value, 10);
    assert_eq!(s.items[1].value, 20);
    assert_eq!(s.items[2].value, 30);
}
