# Benchmarking CDPT

This crate ships a stand-alone microbenchmark that exercises CDPT against
several lock-free concurrent map data structures and reports throughput,
peak/average memory consumption, and (optionally) per-operation latency.

## What is measured

Reported metrics:

- **Throughput**: total ops / duration, in operations per second
  (and millions of operations per second).
- **Peak / average memory**: jemalloc `stats.allocated` peak and mean
  across all 1 ms samples.
- **Latency** (optional): p50, p90, p99, p99.9 in microseconds.

For each invocation, the benchmark:

1. Constructs an empty map.
2. Prefills it with `key_range / 2` random keys.
3. Spawns `--threads` worker threads plus one auxiliary memory-sampling
   thread, all synchronised on a `Barrier`.
4. Each worker repeatedly samples a key uniformly from `0..key_range`,
   selects an operation according to the configured mix, executes it,
   and increments its op counter — looping until `--duration-secs`
   elapses.
5. The auxiliary thread queries `jemalloc`'s `stats.allocated` every 1 ms
   throughout the timed phase and records the peak and the running mean.
6. Optionally (`--measure-latency`), each worker records the wall-clock
   nanoseconds of every ~10th operation after a 2-second warmup, capped
   at 100k samples per thread. Percentiles are computed over the merged,
   sorted set.

The benchmark uses jemalloc as the global allocator
(`tikv_jemallocator::Jemalloc`) so that memory measurements are
allocator-controlled.

## Available data structures

| `--ds` | Type | Citation |
|---|---|---|
| `hlist` | Harris's lock-free linked list | [Harris01] |
| `hmlist` | Harris–Michael linked list | [Harris01], [Michael02] |
| `hhslist` | Harris–Herlihy–Shavit linked list (wait-free `get`) | [Harris01], [Art] |
| `hashmap` | Chaining hash map with `hhslist` per bucket (30,000 buckets) | [Harris01] |
| `nmtree` | Natarajan–Mittal lock-free internal BST | [NM14] |
| `efrbtree` | Ellen's non-blocking BST | [EFRB10] |

The recommended key range is **100** for the linked lists (their O(n)
traversal makes large ranges infeasible) and **100,000** for the trees
and the hash map.

## Quick start

Build the bench binary (the `tag` feature is required for the examples
that share the data-structure code):

```sh
cargo build --release --example bench --features tag
```

Run a single configuration (short flags shown; long forms below):

```sh
cargo run --release --example bench --features tag -- \
    -d nmtree -t 32 -g 2 -i 10 -r 100000
```

Example output:

```
[bench] config: ds=Nmtree threads=32 g=2(90%) duration=10s key_range=100000
[bench] result:
  total ops:    762979314
  throughput:   76297931 ops/s (76.298 M ops/s)
  peak memory:  63.53 MiB
  avg memory:   19.37 MiB
```

Add `-l` (or `--measure-latency`) to also collect tail latencies:

```sh
cargo run --release --example bench --features tag -- \
    -d nmtree -t 32 -g 2 -i 10 -r 100000 -l
```

```
  ...
  latency (n=400000):
    p50   =       0.36 us
    p90   =       0.62 us
    p99   =       1.08 us
    p99.9 =       3.42 us
```

## CLI reference

| Short | Long | Type | Required | Default | Meaning |
|---|---|---|---|---|---|
| `-d` | `--ds` | enum | yes | — | one of `hlist`, `hmlist`, `hhslist`, `hashmap`, `nmtree`, `efrbtree` |
| `-t` | `--threads` | usize | yes | — | number of worker threads |
| `-g` | `--get-rate` | u32 | yes | — | operation mix code: `0`, `1`, or `2` (see table below) |
| `-i` | `--duration-secs` | u64 | no | `10` | length of the timed phase, in seconds |
| `-r` | `--key-range` | usize | yes | — | keys are sampled uniformly from `0..key_range` |
| `-l` | `--measure-latency` | flag | no | off | record p50/p90/p99/p99.9 latencies (10 % sampling, 2 s warmup) |

The exact operation mixes for each `-g` code:

| `-g` | Get | Insert | Remove |
|---|---|---|---|
| `0` | 0 %  | 50 % | 50 % |
| `1` | 50 % | 25 % | 25 % |
| `2` | 90 % | 5 %  | 5 %  |

## Running multiple configurations at once

`run_benchmarks.sh` runs multiple configurations in
sequence: `{1, nproc/2, nproc}` threads × `-g {0, 1, 2}` (write-only,
50 % get, 90 % get) × all six data structures, with `-r 100` for the
lists and `-r 100000` for the trees and the hash map. Each configuration
runs for 10 seconds.

```sh
bash run_benchmarks.sh                  # streams 54 result blocks to stdout
bash run_benchmarks.sh | tee bench.log  # also keep a transcript
```

## OS requirement

This benchmark requires **Unix-like OS (Linux or macOS).** The bench installs
[`tikv-jemallocator`](https://docs.rs/tikv-jemallocator) as the global
allocator so memory measurements are allocator-controlled. Jemalloc
has no Windows support, so the bench's jemalloc dev-deps are gated
behind `cfg(unix)` and on Windows the binary prints a notice and
exits — the rest of the CDPT crate (library and other examples) still
builds normally.

## References

- **[Harris01]** Timothy L. Harris. 2001. *A Pragmatic Implementation of
  Non-Blocking Linked-Lists.* DISC '01.
- **[Michael02]** Maged M. Michael. 2002. *Safe Memory Reclamation for
  Dynamic Lock-Free Objects Using Atomic Reads and Writes.* PODC '02.
  https://doi.org/10.1145/571825.571829
- **[Art]** Maurice Herlihy and Nir Shavit. 2012. *The Art of
  Multiprocessor Programming, Revised Reprint.* Morgan Kaufmann.
- **[NM14]** Aravind Natarajan and Neeraj Mittal. 2014. *Fast Concurrent
  Lock-Free Binary Search Trees.* PPoPP '14.
  https://doi.org/10.1145/2555243.2555256
- **[EFRB10]** Faith Ellen, Panagiota Fatourou, Eric Ruppert, and Franck
  van Breugel. 2010. *Non-Blocking Binary Search Trees.* PODC '10.
  https://doi.org/10.1145/1835698.1835736
