extern crate bindgen;
extern crate cc;

// Building custom barrier library must be conducted
// only in MacOS-based systems.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn main() {
    const ALLOWED_TYPES: [&'static str; 5] = [
        "x86_unified_thread_state_t",
        "arm_unified_thread_state_t",
        "x86_thread_state64_t",
        "arm_thread_state64_t",
        "thread_state_t",
    ];

    const ALLOWED_FUNCTIONS: [&'static str; 3] = [
        "thread_get_register_pointer_values",
        "thread_get_state",
        "mach_port_deallocate",
    ];

    const ALLOWED_CONSTANTS: [&'static str; 2] = ["x86_THREAD_STATE64", "ARM_THREAD_STATE64"];

    use bindgen;
    use std::path::PathBuf;
    use std::{env, fs};

    let libdir_path = PathBuf::from("apple")
        // Canonicalize the path as `rustc-link-search` requires an absolute
        // path.
        .canonicalize()
        .expect("cannot canonicalize path");
    let libdir_path_str = libdir_path.to_str().expect("Path is not a valid string");

    let headers_path = libdir_path.join("mach.h");
    let headers_path_str = headers_path.to_str().expect("Path is not a valid string");

    // Tell cargo to invalidate the built crate whenever the header changes.
    println!("cargo:rerun-if-changed={}", libdir_path_str);

    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    let mut builder = bindgen::Builder::default();

    for t in ALLOWED_TYPES {
        builder = builder.allowlist_type(t);
    }
    for f in ALLOWED_FUNCTIONS {
        builder = builder.allowlist_function(f);
    }
    for c in ALLOWED_CONSTANTS {
        builder = builder.allowlist_var(c);
    }

    let bindings = builder
        .use_core()
        .header(headers_path_str)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/mach.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("mach.rs");
    bindings
        .write_to_file(out_path.clone())
        .expect("Couldn't write bindings!");

    let generated =
        fs::read_to_string(out_path).expect("Couldn't read the generated binding file!");

    if generated.contains("thread_get_register_pointer_values") {
        println!("cargo:rustc-cfg=register_pointer_values");
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn main() {}
