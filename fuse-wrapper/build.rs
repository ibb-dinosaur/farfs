#[cfg(windows)]
fn main() {
    use std::path::PathBuf;

    let builder = bindgen::Builder::default()
        .allowlist_recursively(false)
        .allowlist_type("fuse.*")
        .allowlist_function("fuse.*")
        .allowlist_var("FUSE.*")
        .allowlist_type("libfuse.*")
        .blocklist_type("fuse_log_func_t")
        .blocklist_function("fuse_set_log_func")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    let bindings = builder
        .header("C:\\Program Files (x86)\\WinFsp\\inc\\fuse3\\fuse.h")
        .generate()
        .expect("Unable to generate bindings");
    let bindings_path = PathBuf::from(std::env::var("OUT_DIR").unwrap())
        .join("fuse_bindings.rs");
    bindings.write_to_file(bindings_path).unwrap();

    cc::Build::new()
        .file("static_fns_wrapper.c")
        .compile("static_fns_wrapper");

    println!("cargo:rustc-link-search=native=C:\\Program Files (x86)\\WinFsp\\lib");
    println!("cargo:rustc-link-lib=static=winfsp-x64");
}

#[cfg(unix)]
fn main() {}