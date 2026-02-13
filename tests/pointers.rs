use std::sync::atomic::Ordering;

use cdpt::{
    AtomicShared, AtomicSharedOption, Guard, Local, Shared, TraceObj, TracePtr, handle, pin,
};

// ---- Helper types ----

#[derive(TraceObj)]
struct SimpleNode {
    value: usize,
    next: AtomicSharedOption<Self>,
}

// ---- Guard & Handle tests ----

#[test]
fn handle_pin_unpin() {
    let h = handle();
    assert!(!h.is_pinned());

    let guard = h.pin();
    assert!(h.is_pinned());

    drop(guard);
    assert!(!h.is_pinned());
}

#[test]
fn nested_guards() {
    let h = handle();
    assert!(!h.is_pinned());

    let g1 = h.pin();
    assert!(h.is_pinned());

    let g2 = h.pin();
    assert!(h.is_pinned());

    drop(g1);
    // Still pinned because g2 is alive.
    assert!(h.is_pinned());

    drop(g2);
    assert!(!h.is_pinned());
}

#[test]
fn guard_repin() {
    let h = handle();
    let mut guard = h.pin();
    assert!(h.is_pinned());

    guard.repin();
    assert!(h.is_pinned());

    drop(guard);
    assert!(!h.is_pinned());
}

#[test]
fn handle_clone() {
    let h1 = handle();
    let h2 = h1.clone();

    let g1 = h1.pin();
    // Both handles share the same local, so both should see pinned.
    assert!(h1.is_pinned());
    assert!(h2.is_pinned());

    drop(g1);
    assert!(!h1.is_pinned());
    assert!(!h2.is_pinned());
}

#[test]
fn pin_convenience() {
    // `pin()` is a convenience function that creates a temporary handle and pins it.
    let guard = pin();
    // Just verify we can use it to allocate.
    let _local = Local::new(
        SimpleNode {
            value: 42,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
}

// ---- Local tests ----

#[test]
fn local_new_and_deref() {
    let guard = pin();
    let local = Local::new(
        SimpleNode {
            value: 99,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    assert_eq!(local.value, 99);
}

#[test]
fn local_as_shared() {
    let guard = pin();
    let local = Local::new(
        SimpleNode {
            value: 7,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let shared = local.as_shared();
    assert_eq!(shared.value, 7);
}

#[test]
fn local_as_atomic_shared() {
    let guard = pin();
    let local = Local::new(
        SimpleNode {
            value: 11,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let atomic = local.as_atomic_shared();
    let loaded = atomic.load(Ordering::SeqCst, &guard);
    assert_eq!(loaded.value, 11);
}

#[test]
fn local_ptr_eq() {
    let guard = pin();
    let a = Local::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let b = a; // Copy
    assert!(Local::ptr_eq(&a, &b));

    let c = Local::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    assert!(!Local::ptr_eq(&a, &c));
}

#[test]
fn local_opt_ptr_eq() {
    let guard = pin();
    let a = Local::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let none: Option<&Local<Guard, SimpleNode>> = None;
    assert!(Local::opt_ptr_eq(none, none));
    assert!(Local::opt_ptr_eq(Some(&a), Some(&a)));
    assert!(!Local::opt_ptr_eq(Some(&a), none));
    assert!(!Local::opt_ptr_eq(none, Some(&a)));
}

#[test]
fn local_value_eq() {
    let guard = pin();
    let a = Local::new(
        SimpleNode {
            value: 42,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let b = Local::new(
        SimpleNode {
            value: 42,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    // Value equality (checking field instead of requiring PartialEq on the struct).
    assert_eq!(a.value, b.value);
}

#[test]
fn local_copy_semantics() {
    let guard = pin();
    let a = Local::new(
        SimpleNode {
            value: 5,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let b = a;
    // Both should still be usable (Copy trait).
    assert_eq!(a.value, 5);
    assert_eq!(b.value, 5);
}

#[test]
fn local_protect_with_handle() {
    let h = handle();
    let guard = h.pin();
    let local_guard = Local::new(
        SimpleNode {
            value: 77,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    // Protect the local reference with a Handle (creates a hazard pointer).
    let local_hp = local_guard.protect(&h);
    assert_eq!(local_hp.value, 77);
    // The HP-protected local can outlive the guard.
    drop(guard);
    assert_eq!(local_hp.value, 77);
}

// ---- Shared tests ----

#[test]
fn shared_new_and_deref() {
    let guard = pin();
    let s = Shared::new(
        SimpleNode {
            value: 42,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    assert_eq!(s.value, 42);
}

#[test]
fn shared_clone() {
    let guard = pin();
    let s1 = Shared::new(
        SimpleNode {
            value: 10,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let s2 = s1.clone();
    assert_eq!(s1.value, 10);
    assert_eq!(s2.value, 10);
    assert!(Shared::ptr_eq(&s1, &s2));
}

#[test]
fn shared_ptr_eq() {
    let guard = pin();
    let a = Shared::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let b = a.clone();
    assert!(Shared::ptr_eq(&a, &b));

    let c = Shared::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    assert!(!Shared::ptr_eq(&a, &c));
}

#[test]
fn shared_opt_ptr_eq() {
    let guard = pin();
    let a = Shared::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    assert!(Shared::<SimpleNode>::opt_ptr_eq(None, None));
    assert!(Shared::opt_ptr_eq(Some(&a), Some(&a)));
    assert!(!Shared::opt_ptr_eq(Some(&a), None));
    assert!(!Shared::opt_ptr_eq(None, Some(&a)));
}

#[test]
fn shared_as_local() {
    let guard = pin();
    let s = Shared::new(
        SimpleNode {
            value: 88,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let local = s.as_local(&guard);
    assert_eq!(local.value, 88);
}

#[test]
fn shared_as_ref() {
    let guard = pin();
    let s = Shared::new(
        SimpleNode {
            value: 55,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let r: &SimpleNode = s.as_ref();
    assert_eq!(r.value, 55);
}

#[test]
fn shared_survives_guard_drop() {
    let guard = pin();
    let s = Shared::new(
        SimpleNode {
            value: 100,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    drop(guard);
    // Shared is root-counted, so it should still be accessible.
    assert_eq!(s.value, 100);
}

#[test]
fn shared_multiple_clones() {
    let guard = pin();
    let s = Shared::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let clones: Vec<_> = (0..10).map(|_| s.clone()).collect();
    for c in &clones {
        assert!(Shared::ptr_eq(&s, c));
        assert_eq!(c.value, 1);
    }
}

// ---- AtomicSharedOption tests ----

#[test]
fn atomic_shared_option_none() {
    let guard = pin();
    let opt: AtomicSharedOption<SimpleNode> = AtomicSharedOption::none();
    assert!(opt.load(Ordering::SeqCst, &guard).is_none());
}

#[test]
fn atomic_shared_option_some() {
    let guard = pin();
    let opt = AtomicSharedOption::some(
        SimpleNode {
            value: 42,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let loaded = opt.load(Ordering::SeqCst, &guard);
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().value, 42);
}

#[test]
fn atomic_shared_option_store_some() {
    let guard = pin();
    let opt: AtomicSharedOption<SimpleNode> = AtomicSharedOption::none();

    let node = Local::new(
        SimpleNode {
            value: 5,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    opt.store(Some(&node), Ordering::SeqCst, &guard);

    let loaded = opt.load(Ordering::SeqCst, &guard).unwrap();
    assert_eq!(loaded.value, 5);
}

#[test]
fn atomic_shared_option_store_none() {
    let guard = pin();
    let opt = AtomicSharedOption::some(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    opt.store(None::<&Local<Guard, SimpleNode>>, Ordering::SeqCst, &guard);
    assert!(opt.load(Ordering::SeqCst, &guard).is_none());
}

#[test]
fn atomic_shared_option_swap() {
    let guard = pin();
    let opt = AtomicSharedOption::some(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let new = Local::new(
        SimpleNode {
            value: 2,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let old = opt.swap(Some(&new), Ordering::SeqCst, &guard);
    assert_eq!(old.unwrap().value, 1);
    assert_eq!(opt.load(Ordering::SeqCst, &guard).unwrap().value, 2);
}

#[test]
fn atomic_shared_option_take() {
    let guard = pin();
    let opt = AtomicSharedOption::some(
        SimpleNode {
            value: 99,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let taken = opt.take(Ordering::SeqCst, &guard);
    assert_eq!(taken.unwrap().value, 99);
    assert!(opt.load(Ordering::SeqCst, &guard).is_none());
}

#[test]
fn atomic_shared_option_compare_exchange_success() {
    let guard = pin();
    let opt = AtomicSharedOption::some(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let current = opt.load(Ordering::SeqCst, &guard);
    let new = Local::new(
        SimpleNode {
            value: 2,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let result = opt.compare_exchange(
        current.as_ref(),
        Some(&new),
        Ordering::SeqCst,
        Ordering::SeqCst,
        &guard,
    );
    assert!(result.is_ok());
    assert_eq!(opt.load(Ordering::SeqCst, &guard).unwrap().value, 2);
}

#[test]
fn atomic_shared_option_compare_exchange_failure() {
    let guard = pin();
    let opt = AtomicSharedOption::some(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    // Use a different pointer as expected value.
    let wrong = Local::new(
        SimpleNode {
            value: 999,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let new = Local::new(
        SimpleNode {
            value: 2,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let result = opt.compare_exchange(
        Some(&wrong),
        Some(&new),
        Ordering::SeqCst,
        Ordering::SeqCst,
        &guard,
    );
    assert!(result.is_err());
    // Value should be unchanged.
    assert_eq!(opt.load(Ordering::SeqCst, &guard).unwrap().value, 1);
}

#[test]
#[cfg(feature = "tag")]
fn atomic_shared_option_load_with_tag() {
    let guard = pin();
    let opt = AtomicSharedOption::some_with_tag(
        SimpleNode {
            value: 7,
            next: AtomicSharedOption::none(),
        },
        1,
        &guard,
    );

    let (loaded, tag) = opt.load_with_tag(Ordering::SeqCst, &guard);
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().value, 7);
    assert_eq!(tag, 1);
}

#[test]
#[cfg(feature = "tag")]
fn atomic_shared_option_none_with_tag() {
    let guard = pin();
    let opt: AtomicSharedOption<SimpleNode> = AtomicSharedOption::none_with_tag(1);
    let (loaded, tag) = opt.load_with_tag(Ordering::SeqCst, &guard);
    assert!(loaded.is_none());
    assert_eq!(tag, 1);
}

#[test]
fn atomic_shared_option_default_is_none() {
    let guard = pin();
    let opt: AtomicSharedOption<SimpleNode> = AtomicSharedOption::default();
    assert!(opt.load(Ordering::SeqCst, &guard).is_none());
}

// ---- AtomicSharedOption conversions ----

#[test]
fn atomic_shared_option_from_shared() {
    let guard = pin();
    let s = Shared::new(
        SimpleNode {
            value: 33,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let opt = AtomicSharedOption::from(s);
    assert_eq!(opt.load(Ordering::SeqCst, &guard).unwrap().value, 33);
}

#[test]
fn atomic_shared_option_from_option_shared_some() {
    let guard = pin();
    let s = Shared::new(
        SimpleNode {
            value: 44,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let opt = AtomicSharedOption::from(Some(s));
    assert_eq!(opt.load(Ordering::SeqCst, &guard).unwrap().value, 44);
}

#[test]
fn atomic_shared_option_from_option_shared_none() {
    let guard = pin();
    let opt = AtomicSharedOption::<SimpleNode>::from(None::<Shared<SimpleNode>>);
    assert!(opt.load(Ordering::SeqCst, &guard).is_none());
}

#[test]
fn atomic_shared_option_from_local() {
    let guard = pin();
    let local = Local::new(
        SimpleNode {
            value: 55,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let opt = AtomicSharedOption::from(local);
    assert_eq!(opt.load(Ordering::SeqCst, &guard).unwrap().value, 55);
}

#[test]
fn atomic_shared_option_from_option_local_some() {
    let guard = pin();
    let local = Local::new(
        SimpleNode {
            value: 66,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let opt = AtomicSharedOption::from(Some(local));
    assert_eq!(opt.load(Ordering::SeqCst, &guard).unwrap().value, 66);
}

#[test]
fn atomic_shared_option_from_option_local_none() {
    let guard = pin();
    let opt = AtomicSharedOption::<SimpleNode>::from(None::<Local<Guard, SimpleNode>>);
    assert!(opt.load(Ordering::SeqCst, &guard).is_none());
}

// ---- AtomicShared tests ----

#[test]
fn atomic_shared_new_and_load() {
    let guard = pin();
    let a = AtomicShared::new(
        SimpleNode {
            value: 42,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let loaded = a.load(Ordering::SeqCst, &guard);
    assert_eq!(loaded.value, 42);
}

#[test]
fn atomic_shared_store() {
    let guard = pin();
    let a = AtomicShared::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let new_node = Local::new(
        SimpleNode {
            value: 2,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    a.store(&new_node, Ordering::SeqCst, &guard);

    assert_eq!(a.load(Ordering::SeqCst, &guard).value, 2);
}

#[test]
fn atomic_shared_swap() {
    let guard = pin();
    let a = AtomicShared::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let new_node = Local::new(
        SimpleNode {
            value: 2,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let old = a.swap(&new_node, Ordering::SeqCst, &guard);
    assert_eq!(old.value, 1);
    assert_eq!(a.load(Ordering::SeqCst, &guard).value, 2);
}

#[test]
fn atomic_shared_compare_exchange_success() {
    let guard = pin();
    let a = AtomicShared::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let current = a.load(Ordering::SeqCst, &guard);
    let new_node = Local::new(
        SimpleNode {
            value: 2,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let result = a.compare_exchange(
        &current,
        &new_node,
        Ordering::SeqCst,
        Ordering::SeqCst,
        &guard,
    );
    assert!(result.is_ok());
    assert_eq!(a.load(Ordering::SeqCst, &guard).value, 2);
}

#[test]
fn atomic_shared_compare_exchange_failure() {
    let guard = pin();
    let a = AtomicShared::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let wrong = Local::new(
        SimpleNode {
            value: 999,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let new_node = Local::new(
        SimpleNode {
            value: 2,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );

    let result = a.compare_exchange(
        &wrong,
        &new_node,
        Ordering::SeqCst,
        Ordering::SeqCst,
        &guard,
    );
    assert!(result.is_err());
    assert_eq!(a.load(Ordering::SeqCst, &guard).value, 1);
}

#[test]
fn atomic_shared_take() {
    let guard = pin();
    let a = AtomicShared::new(
        SimpleNode {
            value: 77,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let taken = a.take(Ordering::SeqCst, &guard);
    assert_eq!(taken.value, 77);
}

#[test]
#[cfg(feature = "tag")]
fn atomic_shared_load_with_tag() {
    let guard = pin();
    let a = AtomicShared::new_with_tag(
        SimpleNode {
            value: 3,
            next: AtomicSharedOption::none(),
        },
        1,
        &guard,
    );
    let (loaded, tag) = a.load_with_tag(Ordering::SeqCst, &guard);
    assert_eq!(loaded.value, 3);
    assert_eq!(tag, 1);
}

#[test]
fn atomic_shared_from_shared() {
    let guard = pin();
    let s = Shared::new(
        SimpleNode {
            value: 88,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let a = AtomicShared::from(s);
    assert_eq!(a.load(Ordering::SeqCst, &guard).value, 88);
}

#[test]
fn atomic_shared_from_local() {
    let guard = pin();
    let l = Local::new(
        SimpleNode {
            value: 99,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let a = AtomicShared::from(l);
    assert_eq!(a.load(Ordering::SeqCst, &guard).value, 99);
}

// ---- Linked structure tests ----

#[test]
fn simple_linked_list() {
    let guard = pin();

    let node3 = Local::new(
        SimpleNode {
            value: 3,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let node2 = Local::new(
        SimpleNode {
            value: 2,
            next: AtomicSharedOption::from(Some(&node3)),
        },
        &guard,
    );
    let node1 = Local::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::from(Some(&node2)),
        },
        &guard,
    );

    assert_eq!(node1.value, 1);
    let n2 = node1.next.load(Ordering::SeqCst, &guard).unwrap();
    assert_eq!(n2.value, 2);
    let n3 = n2.next.load(Ordering::SeqCst, &guard).unwrap();
    assert_eq!(n3.value, 3);
    assert!(n3.next.load(Ordering::SeqCst, &guard).is_none());
}

#[test]
fn modify_linked_list() {
    let guard = pin();

    let node2 = Local::new(
        SimpleNode {
            value: 2,
            next: AtomicSharedOption::none(),
        },
        &guard,
    );
    let node1 = Local::new(
        SimpleNode {
            value: 1,
            next: AtomicSharedOption::from(Some(&node2)),
        },
        &guard,
    );

    // Insert node3 between node1 and node2.
    let node3 = Local::new(
        SimpleNode {
            value: 3,
            next: AtomicSharedOption::from(Some(&node2)),
        },
        &guard,
    );
    node1.next.store(Some(&node3), Ordering::SeqCst, &guard);

    // Verify: 1 -> 3 -> 2
    assert_eq!(node1.value, 1);
    let n3 = node1.next.load(Ordering::SeqCst, &guard).unwrap();
    assert_eq!(n3.value, 3);
    let n2 = n3.next.load(Ordering::SeqCst, &guard).unwrap();
    assert_eq!(n2.value, 2);
}

// ---- Multiple allocations ----

#[test]
fn many_allocations() {
    let guard = pin();
    let mut nodes = Vec::new();
    for i in 0..100 {
        let node = Local::new(
            SimpleNode {
                value: i,
                next: AtomicSharedOption::none(),
            },
            &guard,
        );
        nodes.push(node);
    }
    for (i, node) in nodes.iter().enumerate() {
        assert_eq!(node.value, i);
    }
}

#[test]
fn shared_vec() {
    let guard = pin();
    let mut shareds = Vec::new();
    for i in 0..50 {
        shareds.push(Shared::new(
            SimpleNode {
                value: i,
                next: AtomicSharedOption::none(),
            },
            &guard,
        ));
    }
    for (i, s) in shareds.iter().enumerate() {
        assert_eq!(s.value, i);
    }
    // Cloning all shareds.
    let clones: Vec<_> = shareds.iter().map(|s| s.clone()).collect();
    for (i, c) in clones.iter().enumerate() {
        assert_eq!(c.value, i);
        assert!(Shared::ptr_eq(&shareds[i], c));
    }
}
