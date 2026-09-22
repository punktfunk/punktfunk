//! Generate `include/punktfunk_console.h` from the console's `extern "C"` surface, on Apple
//! targets only: elsewhere the module is compiled out. The header is checked in.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src/console.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    if env::var("CARGO_CFG_TARGET_VENDOR").as_deref() != Ok("apple") {
        return;
    }
    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out = PathBuf::from(&crate_dir).join("../../../include/punktfunk_console.h");
    match cbindgen::generate(&crate_dir) {
        Ok(bindings) => {
            bindings.write_to_file(&out);
        }
        Err(e) => {
            println!("cargo:warning=punktfunk-client-apple: cbindgen failed ({e}); header not regenerated");
        }
    }
}
