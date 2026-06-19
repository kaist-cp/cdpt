#!/usr/bin/env bash
set -euo pipefail

# Run all cdpt tests (unit tests + integration tests + example tests)
# in debug, release, and release+AddressSanitizer modes.

PASS=0
FAIL=0
FAILURES=()

run() {
    local label="$1"
    shift
    echo "──────────────────────────────────────────────────────────"
    echo "  $label"
    echo "  $ $*"
    echo "──────────────────────────────────────────────────────────"
    if "$@"; then
        PASS=$((PASS + 1))
        echo "  => PASSED"
    else
        FAIL=$((FAIL + 1))
        FAILURES+=("$label")
        echo "  => FAILED"
    fi
    echo ""
}

# ── 1. Debug mode ────────────────────────────────────────────────

run "debug: unit + integration tests" \
    cargo test --tests

run "debug: doctests" \
    cargo test --doc

run "debug: example tests (stack)" \
    cargo test --example stack

run "debug: example tests (lists, tag feature)" \
    cargo test --example lists --features tag

run "debug: example tests (efrb_tree, tag feature)" \
    cargo test --example efrb_tree --features tag

run "debug: example tests (natarajan_mittal_tree, tag feature)" \
    cargo test --example natarajan_mittal_tree --features tag

# ── 2. Release mode ─────────────────────────────────────────────

run "release: unit + integration tests" \
    cargo test --release --tests

run "release: example tests (stack)" \
    cargo test --release --example stack

run "release: example tests (lists, tag feature)" \
    cargo test --release --example lists --features tag

run "release: example tests (efrb_tree, tag feature)" \
    cargo test --release --example efrb_tree --features tag

run "release: example tests (natarajan_mittal_tree, tag feature)" \
    cargo test --release --example natarajan_mittal_tree --features tag

# ── 3. Release + AddressSanitizer ────────────────────────────────
#
# ASan needs nightly-only `-Z` flags, so this block runs via `cargo +nightly`;
# everything above uses the repo's default toolchain (stable).

ASAN_FLAGS="-Z sanitizer=address"
SKIP_ASAN=false

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)   ASAN_TARGET=x86_64-unknown-linux-gnu ;;
    Linux-aarch64)   ASAN_TARGET=aarch64-unknown-linux-gnu ;;
    Darwin-x86_64)   ASAN_TARGET=x86_64-apple-darwin ;;
    Darwin-arm64)    ASAN_TARGET=aarch64-apple-darwin ;;
    *)
        echo "WARNING: unsupported platform $(uname -s)-$(uname -m) for ASan — skipping ASan tests"
        SKIP_ASAN=true
        ;;
esac

if [ "$SKIP_ASAN" = false ]; then
    ASAN_ARGS=(--release --target "$ASAN_TARGET" -Z build-std)

    run "release+asan: unit + integration tests" \
        env RUSTFLAGS="$ASAN_FLAGS" cargo +nightly test "${ASAN_ARGS[@]}" --tests

    run "release+asan: example tests (stack)" \
        env RUSTFLAGS="$ASAN_FLAGS" cargo +nightly test "${ASAN_ARGS[@]}" --example stack

    run "release+asan: example tests (lists, tag feature)" \
        env RUSTFLAGS="$ASAN_FLAGS" cargo +nightly test "${ASAN_ARGS[@]}" --example lists --features tag

    run "release+asan: example tests (efrb_tree, tag feature)" \
        env RUSTFLAGS="$ASAN_FLAGS" cargo +nightly test "${ASAN_ARGS[@]}" --example efrb_tree --features tag

    run "release+asan: example tests (natarajan_mittal_tree, tag feature)" \
        env RUSTFLAGS="$ASAN_FLAGS" cargo +nightly test "${ASAN_ARGS[@]}" --example natarajan_mittal_tree --features tag
fi

# ── Summary ──────────────────────────────────────────────────────

echo "======================================================"
echo "  RESULTS: $PASS passed, $FAIL failed"
if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "  Failed:"
    for f in "${FAILURES[@]}"; do
        echo "    - $f"
    done
    echo "======================================================"
    exit 1
else
    echo "======================================================"
fi
