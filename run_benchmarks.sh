#!/usr/bin/env bash
# Sweep the throughput / memory / latency benchmark across the configuration
# matrix described in the artifact:
#   threads:   1, half of available, all available
#   get-rate:  0 (write-only), 1 (50% get), 2 (90% get)
#   duration:  10 seconds
#   key range: 100 for lists, 100000 for trees / hashmap
#
# Each invocation prints a labelled summary to stdout. Tee to a file if you
# want to keep a transcript:
#     bash run_benchmarks.sh | tee bench.log

set -euo pipefail
cd "$(dirname "$0")"

NPROC=$(nproc)
HALF=$(( NPROC / 2 ))
if [ "$HALF" -lt 1 ]; then HALF=1; fi

THREAD_COUNTS=(1 "$HALF" "$NPROC")
GET_RATES=(0 1 2)
LIST_DS=(hlist hmlist hhslist)
TREE_DS=(nmtree efrbtree hashmap)

echo "[run_benchmarks] building..."
cargo build --release --example bench --features tag

run_one() {
  local ds=$1 t=$2 g=$3 kr=$4
  echo
  echo "===== ds=$ds threads=$t g=$g key_range=$kr ====="
  cargo run --quiet --release --example bench --features tag -- \
    -d "$ds" -t "$t" -g "$g" -i 10 -r "$kr"
}

for ds in "${LIST_DS[@]}"; do
  for t in "${THREAD_COUNTS[@]}"; do
    for g in "${GET_RATES[@]}"; do
      run_one "$ds" "$t" "$g" 100
    done
  done
done

for ds in "${TREE_DS[@]}"; do
  for t in "${THREAD_COUNTS[@]}"; do
    for g in "${GET_RATES[@]}"; do
      run_one "$ds" "$t" "$g" 100000
    done
  done
done
