//! Stand-alone throughput / memory / latency benchmark for CDPT data structures.
//!
//! On Unix-like systems (Linux, macOS) the bench installs jemalloc as the global
//! allocator and samples `stats.allocated` to report peak / average memory usage.
//! Jemalloc has no Windows support, so on Windows the binary prints a notice and
//! exits — the rest of the CDPT crate still builds normally.

#![allow(dead_code)]

#[path = "ds/efrb_tree.rs"]
mod efrb_tree;
#[path = "ds/lists.rs"]
mod lists;
#[path = "common/mod.rs"]
mod map_common;
#[path = "ds/nm_tree.rs"]
mod nm_tree;

use clap::{Parser, ValueEnum};

#[cfg(unix)]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Ds {
    Hlist,
    Hmlist,
    Hhslist,
    Hashmap,
    Nmtree,
    Efrbtree,
}

#[derive(Parser, Debug)]
#[command(about = "Throughput / memory / latency benchmark for CDPT data structures")]
struct Args {
    /// Data structure to benchmark.
    #[arg(short = 'd', long, value_enum)]
    ds: Ds,

    /// Number of worker threads.
    #[arg(short = 't', long)]
    threads: usize,

    /// Operation mix code: 0 = write-only (insert 50%, remove 50%),
    /// 1 = read-write (get 50%, insert 25%, remove 25%),
    /// 2 = read-most (get 90%, insert 5%, remove 5%).
    #[arg(short = 'g', long)]
    get_rate: u32,

    /// Benchmark duration in seconds.
    #[arg(short = 'i', long, default_value_t = 10)]
    duration_secs: u64,

    /// Key range (keys are sampled uniformly from 0..key_range).
    #[arg(short = 'r', long)]
    key_range: usize,

    /// Sample per-operation latency (10% sampling, after a 2-second warmup).
    #[arg(short = 'l', long, default_value_t = false)]
    measure_latency: bool,
}

#[cfg(not(unix))]
fn main() {
    eprintln!(
        "The CDPT bench requires a Unix-like OS (Linux/macOS); jemalloc has no Windows support."
    );
    std::process::exit(1);
}

#[cfg(unix)]
fn main() {
    let args = Args::parse();
    match args.ds {
        Ds::Hlist => run::<lists::HList<usize, usize>>(&args),
        Ds::Hmlist => run::<lists::HMList<usize, usize>>(&args),
        Ds::Hhslist => run::<lists::HHSList<usize, usize>>(&args),
        Ds::Hashmap => run::<lists::HashMap<usize, usize>>(&args),
        Ds::Nmtree => run::<nm_tree::NMTreeMap<usize, usize>>(&args),
        Ds::Efrbtree => run::<efrb_tree::EFRBTree<usize, usize>>(&args),
    }
}

#[cfg(unix)]
fn run<M>(args: &Args)
where
    M: map_common::ConcurrentMap<usize, usize> + Send + Sync,
{
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread::scope;
    use std::time::{Duration, Instant};

    use cdpt::{global, handle};

    let (w_get, w_ins, w_rem, get_pct): (u32, u32, u32, u32) = match args.get_rate {
        0 => (0, 1, 1, 0),
        1 => (2, 1, 1, 50),
        2 => (18, 1, 1, 90),
        other => panic!("--get-rate must be 0, 1, or 2 (got {})", other),
    };
    let total_weight = w_get + w_ins + w_rem;

    println!(
        "[bench] config: ds={:?} threads={} g={}({}%) duration={}s key_range={}{}",
        args.ds,
        args.threads,
        args.get_rate,
        get_pct,
        args.duration_secs,
        args.key_range,
        if args.measure_latency {
            " (latency=on)"
        } else {
            ""
        }
    );

    // Disable background collection so the prefill inserts run uninterrupted.
    global().enable_collection(false);

    let map = M::new();
    let prefill = args.key_range / 2;

    // Prefill on the main thread; the block scope drops the handle before the timed phase begins.
    {
        let h = handle();
        let mut rng = fastrand::Rng::with_seed(0xfeed_face);
        for _ in 0..prefill {
            let k = rng.usize(0..args.key_range);
            map.insert(k, k, &h);
        }
    }

    // Re-enable collection for the timed phase.
    global().enable_collection(true);

    let duration = Duration::from_secs(args.duration_secs);
    let barrier = Arc::new(Barrier::new(args.threads + 1));
    let (ops_tx, ops_rx) = mpsc::channel::<u64>();
    let (lat_tx, lat_rx) = mpsc::channel::<Vec<u128>>();
    let (mem_tx, mem_rx) = mpsc::channel::<(usize, usize)>();

    scope(|s| {
        // Memory-sampling auxiliary thread.
        {
            let barrier = barrier.clone();
            let mem_tx = mem_tx.clone();
            s.spawn(move || {
                let e = tikv_jemalloc_ctl::epoch::mib().unwrap();
                let alloc = tikv_jemalloc_ctl::stats::allocated::mib().unwrap();

                barrier.wait();

                let mut peak = 0usize;
                let mut acc = 0usize;
                let mut samples = 0usize;
                let start = Instant::now();
                let sample_period = Duration::from_millis(1);
                let mut next = start + sample_period;
                while start.elapsed() < duration {
                    let now = Instant::now();
                    if now >= next {
                        let _ = e.advance();
                        if let Ok(used) = alloc.read() {
                            peak = peak.max(used);
                            acc = acc.saturating_add(used);
                            samples += 1;
                        }
                        next = now + sample_period;
                    }
                    std::thread::sleep(Duration::from_micros(100));
                }
                let avg = if samples > 0 { acc / samples } else { 0 };
                let _ = mem_tx.send((peak, avg));
            });
        }

        // Worker threads.
        for tid in 0..args.threads {
            let barrier = barrier.clone();
            let ops_tx = ops_tx.clone();
            let lat_tx = lat_tx.clone();
            let measure_latency = args.measure_latency;
            let key_range = args.key_range;
            let map_ref = &map;
            s.spawn(move || {
                let h = handle();
                let mut rng = fastrand::Rng::with_seed(0xc0ffee ^ (tid as u64).wrapping_add(1));

                barrier.wait();

                let mut ops: u64 = 0;
                let mut latencies: Vec<u128> = if measure_latency {
                    Vec::with_capacity(100_000)
                } else {
                    Vec::new()
                };
                let warmup = Duration::from_secs(2);
                let start = Instant::now();
                while start.elapsed() < duration {
                    let key = rng.usize(0..key_range);
                    let r = rng.u32(0..total_weight);

                    let op_start = if measure_latency {
                        Some(Instant::now())
                    } else {
                        None
                    };

                    if r < w_get {
                        let _ = map_ref.get(&key, &h);
                    } else if r < w_get + w_ins {
                        map_ref.insert(key, key, &h);
                    } else {
                        let _ = map_ref.remove(&key, &h);
                    }

                    if measure_latency && start.elapsed() > warmup && latencies.len() < 100_000 {
                        // ~10% sampling.
                        if rng.u8(0..10) == 0 {
                            let elapsed = (Instant::now() - op_start.unwrap()).as_nanos();
                            latencies.push(elapsed);
                        }
                    }

                    ops += 1;
                }

                let _ = ops_tx.send(ops);
                let _ = lat_tx.send(latencies);
            });
        }
    });

    // Drop our copies so the rx loops terminate.
    drop(ops_tx);
    drop(lat_tx);
    drop(mem_tx);

    let mut total_ops: u64 = 0;
    while let Ok(o) = ops_rx.recv() {
        total_ops += o;
    }

    let mut all_lat: Vec<u128> = Vec::new();
    while let Ok(v) = lat_rx.recv() {
        all_lat.extend(v);
    }

    let (peak_mem, avg_mem) = mem_rx.recv().unwrap_or((0, 0));

    let throughput = total_ops as f64 / args.duration_secs as f64;
    println!("[bench] result:");
    println!("  total ops:    {}", total_ops);
    println!(
        "  throughput:   {:.0} ops/s ({:.3} M ops/s)",
        throughput,
        throughput / 1.0e6
    );
    println!(
        "  peak memory:  {:.2} MiB",
        peak_mem as f64 / (1024.0 * 1024.0)
    );
    println!(
        "  avg memory:   {:.2} MiB",
        avg_mem as f64 / (1024.0 * 1024.0)
    );

    if args.measure_latency {
        all_lat.sort_unstable();
        if all_lat.is_empty() {
            println!("  latency:      <no samples>");
        } else {
            let pct = |q: f64| -> u128 {
                let i = ((all_lat.len() as f64 * q).ceil() as usize)
                    .saturating_sub(1)
                    .min(all_lat.len() - 1);
                all_lat[i]
            };
            let to_us = |ns: u128| ns as f64 / 1000.0;
            println!("  latency (n={}):", all_lat.len());
            println!("    p50   = {:>10.2} us", to_us(pct(0.50)));
            println!("    p90   = {:>10.2} us", to_us(pct(0.90)));
            println!("    p99   = {:>10.2} us", to_us(pct(0.99)));
            println!("    p99.9 = {:>10.2} us", to_us(pct(0.999)));
        }
    }
}
