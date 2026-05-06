#[path = "ds/efrb_tree.rs"]
mod ds;
#[path = "common/mod.rs"]
mod map_common;

pub use ds::EFRBTree;

fn main() {
    map_common::stress_test::<EFRBTree<i32, String>>();
}

#[cfg(test)]
mod tests {
    use super::*;

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
    // To run: cargo test --release --all-targets -- --ignored
    // To run with address sanitizer: RUSTFLAGS="-Z sanitizer=address" cargo +nightly test --release --all-targets -- --ignored
    // (Set `--target` for your machine: https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html)
    #[test]
    #[ignore]
    fn stress_efrb_tree() {
        map_common::stress_test::<EFRBTree<i32, String>>();
    }
}
