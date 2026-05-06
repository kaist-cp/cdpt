#[path = "ds/lists.rs"]
mod ds;
#[path = "common/mod.rs"]
mod map_common;

pub use ds::{HHSList, HList, HMList, HashMap};

fn main() {
    map_common::stress_test::<HashMap<i32, String>>();
}

#[cfg(test)]
mod tests {
    use super::*;

    // Smoke tests
    #[test]
    fn smoke_harris() {
        map_common::smoke::<HList<i32, String>>();
    }

    #[test]
    fn smoke_harris_michael() {
        map_common::smoke::<HMList<i32, String>>();
    }

    #[test]
    fn smoke_harris_herlihy_shavit() {
        map_common::smoke::<HHSList<i32, String>>();
    }

    #[test]
    fn smoke_hash_map() {
        map_common::smoke::<HashMap<i32, String>>();
    }

    // Basic operation tests
    #[test]
    fn basic_operations_harris() {
        map_common::test_basic_operations::<HList<i32, String>>();
    }

    #[test]
    fn basic_operations_harris_michael() {
        map_common::test_basic_operations::<HMList<i32, String>>();
    }

    #[test]
    fn basic_operations_harris_herlihy_shavit() {
        map_common::test_basic_operations::<HHSList<i32, String>>();
    }

    #[test]
    fn basic_operations_hash_map() {
        map_common::test_basic_operations::<HashMap<i32, String>>();
    }

    // Multiple elements tests
    #[test]
    fn multiple_elements_harris() {
        map_common::test_multiple_elements::<HList<i32, String>>();
    }

    #[test]
    fn multiple_elements_harris_michael() {
        map_common::test_multiple_elements::<HMList<i32, String>>();
    }

    #[test]
    fn multiple_elements_harris_herlihy_shavit() {
        map_common::test_multiple_elements::<HHSList<i32, String>>();
    }

    #[test]
    fn multiple_elements_hash_map() {
        map_common::test_multiple_elements::<HashMap<i32, String>>();
    }

    // Reverse order insert tests
    #[test]
    fn reverse_order_insert_harris() {
        map_common::test_reverse_order_insert::<HList<i32, String>>();
    }

    #[test]
    fn reverse_order_insert_harris_michael() {
        map_common::test_reverse_order_insert::<HMList<i32, String>>();
    }

    #[test]
    fn reverse_order_insert_harris_herlihy_shavit() {
        map_common::test_reverse_order_insert::<HHSList<i32, String>>();
    }

    #[test]
    fn reverse_order_insert_hash_map() {
        map_common::test_reverse_order_insert::<HashMap<i32, String>>();
    }

    // Concurrent insert/remove tests
    #[test]
    fn concurrent_insert_remove_harris() {
        map_common::test_concurrent_insert_remove::<HList<i32, String>>();
    }

    #[test]
    fn concurrent_insert_remove_harris_michael() {
        map_common::test_concurrent_insert_remove::<HMList<i32, String>>();
    }

    #[test]
    fn concurrent_insert_remove_harris_herlihy_shavit() {
        map_common::test_concurrent_insert_remove::<HHSList<i32, String>>();
    }

    #[test]
    fn concurrent_insert_remove_hash_map() {
        map_common::test_concurrent_insert_remove::<HashMap<i32, String>>();
    }

    // Stress tests (disabled by default)
    // Recommended: cargo test --release --all-targets -- --ignored
    // With address sanitizer: RUSTFLAGS="-Z sanitizer=address" cargo +nightly test --release --all-targets -- --ignored
    // (Set `--target` for your machine: https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html)
    #[test]
    #[ignore]
    fn stress_harris() {
        map_common::stress_test_list::<HList<i32, String>>();
    }

    #[test]
    #[ignore]
    fn stress_harris_michael() {
        map_common::stress_test_list::<HMList<i32, String>>();
    }

    #[test]
    #[ignore]
    fn stress_harris_herlihy_shavit() {
        map_common::stress_test_list::<HHSList<i32, String>>();
    }

    #[test]
    #[ignore]
    fn stress_hash_map() {
        map_common::stress_test::<HashMap<i32, String>>();
    }
}
