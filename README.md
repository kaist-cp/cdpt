# CDPT: Concurrent Deferred Partial Tracing

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Paper: PLDI 2026](https://img.shields.io/badge/paper-PLDI%202026-brightgreen.svg)](https://doi.org/10.1145/3808310)

CDPT is an automatic, safe, and concurrent garbage collector for Rust. It lets
you build lock-free and concurrent data structures without manual memory
reclamation, and without stop-the-world pauses, and it
reclaims cyclic garbage automatically.

CDPT is based on *partial tracing*: it keeps reference counts only for the
roots and reclaims the rest of the heap by tracing. This makes it safe (the
roots are precise) and easy to use (tracing reclaims cycles for you). A highly
concurrent design keeps it efficient: tracing runs concurrently with your
threads, and deferred reference counting amortizes the root-count updates, so
traversals touch no counts at all. The design, its correctness, and an
evaluation are described in our PLDI 2026 paper.

> Jeonghyeon Kim, Jongse Park, Youngjin Kwon, and Jeehoon Kang. 2026. Revisiting Partial Tracing for Safe, Efficient, and Concurrent Garbage Collection in Unmanaged Languages. *Proc. ACM Program. Lang.* 10, PLDI, Article 232 (June 2026), 27 pages. https://doi.org/10.1145/3808310

## Shared references with `Shared<T>` and `AtomicShared<T>`

`Shared<T>` is CDPT's counterpart to `std::sync::Arc<T>`: an immutable,
thread-safe reference that keeps a managed object alive. Unlike `Arc<T>`:

* Reference cycles between `Shared<T>`s are reclaimed automatically by the
  collector instead of leaking, so you never need weak references to break a
  cycle.
* It can be stored in an `AtomicShared<T>` (non-nullable) or
  `AtomicSharedOption<T>` (nullable), the atomic pointers used for the mutable
  edges of a concurrent data structure.
* Their `store`, `swap`, and `compare_exchange` operations apply the collector's
  write barriers for you, so updates stay safe while collection runs
  concurrently.

## Efficient traversal with `Local<'g, T>`

You can traverse the managed heap without updating any reference counts. To
access the heap, pin the current thread with `cdpt::pin()`, which returns a
`Guard`. Pinning is cheap and does not exclude other threads: any number of
threads may hold a guard at once, so a guard is nothing like a mutex.

```rust
let guard = cdpt::pin();
let node = some_atomic.load(Ordering::Acquire, &guard);
println!("{}", node.value); // `Local` dereferences to `&T`
// `guard` is dropped here, letting the collector make progress.
```

While the guard is held, loading an atomic edge returns a `Local<'g, T>`: an
uncounted, temporary reference that dereferences to `&T` and stays valid for
the guard's lifetime. A traversal is just a chain of such loads, so it never
touches a reference count. To keep a reference past the guard, promote it to a
`Shared<T>` (root-counted, sendable) or to a `Local<'h, Handle, T>`
(hazard-pointer protected); both dereference freely, with no guard.

## Automatic cycle collection

Because CDPT traces the heap, a group of objects that reference each other in a
cycle is reclaimed automatically once the whole group becomes unreachable. You
do not write weak references or break cycles by hand, as you would with plain
reference counting. Cyclic data structures such as doubly linked lists and
graphs work out of the box.

## Comparison with other approaches

|                                   | Tracing (BDWGC) | Ref. counting (`Arc`, CIRC) | CDPT |
| --------------------------------- | :-------------: | :-------------------: | :--: |
| No stop-the-world pause           |        ✗        |           ✓           |  ✓   |
| Precise roots (no conservative scan) |     ✗        |           ✓           |  ✓   |
| Automatic cycle collection        |        ✓        |           ✗           |  ✓   |
| Cheap writes (no per-edge counting) |        ✓        |           ✗           |  ✓   |

CDPT avoids the tradeoffs the other approaches make. Conservative tracing
collectors such as BDWGC must stop the world and scan memory word by word.
Reference counting (`Arc<T>`) and deferred reference counting (such as
[CIRC](https://github.com/kaist-cp/circ)) avoid pauses, but they update a count
on every internal edge, which slows write-heavy workloads that insert and
remove nodes. Deferring those updates, as CIRC does, softens this cost without
removing it, and neither reclaims cycles without weak references. CDPT counts
only roots and traces the rest, so internal-edge writes stay cheap, cycles are
still collected, and there are no pauses. In the paper's
benchmarks it outperforms BDWGC and CIRC and is comparable to manual schemes
such as epoch-based RCU and hazard pointers. See the
[paper](https://doi.org/10.1145/3808310) for the full evaluation.

## Example

Derive `TraceObj`, pin a guard, then allocate and dereference managed objects:

```rust
use cdpt::*;

#[derive(TraceObj)]
struct Node {
    value: usize,
    next: AtomicSharedOption<Node>,
}

// Pin the current thread to access the managed heap.
let handle = handle();
let guard = handle.pin();

// Allocate and dereference while the guard is alive.
let node = Local::new(Node { value: 42, next: AtomicSharedOption::none() }, &guard);
assert_eq!(node.value, 42); // `Local` dereferences to `&T`

// Promote to references that outlive the guard.
let kept = node.protect(&handle); // `Local<Handle, Node>`: cheap, this thread only
let shared = node.as_shared();    // `Shared<Node>`: root-counted, like `Arc`

drop(guard); // the collector can now make progress

assert_eq!(kept.value, 42);  // still valid
let alias = shared.clone();  // `Shared` is `Clone` + `Send + Sync`
assert_eq!(alias.value, 42); // still valid
```

More complete data structures (a Harris linked list, a hash map, and the
Natarajan–Mittal and Ellen et al. trees) live in the [`examples/`](examples)
directory.

## Installation

Add CDPT to your `Cargo.toml`:

```toml
[dependencies]
cdpt = "0.1"
```

Optional Cargo features:

* `tag`: low-bit pointer tagging (the `*_with_tag` and `fetch_tag_*` APIs), for
  lock-free algorithms that mark pointers.

API documentation is on [docs.rs](https://docs.rs/cdpt). For benchmarking, see
[`BENCH.md`](BENCH.md).

## Limitations

* **No finalizers.** When an object is reclaimed its fields are dropped
  normally, but you cannot run custom `Drop` logic on a managed type, and the
  timing of reclamation is not observable.
* **No unboxing.** Once a managed object becomes a heap edge of another object
  it cannot be moved back onto the stack; `Shared<T>` and `Local<T>`
  dereference to `&T` only.
* **`Send + Sync` only.** Managed objects are shared with other threads and the
  collector, so any interior-mutable state must use a thread-safe primitive
  (`Mutex`, atomics, and so on).
* **Higher memory footprint.** Tracing keeps garbage until the next cycle
  confirms it dead, and every object carries a small header, so peak memory is
  higher than hand-tuned manual reclamation.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
