#[path = "ds/nm_tree.rs"]
mod ds;
#[path = "common/mod.rs"]
mod map_common;

pub use ds::NMTreeMap;

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
