//! `cargo run -p gen-headers`: regenerate `include/punktfunk_core.h` and
//! `include/punktfunk_console.h` from each crate's `cbindgen.toml`. A parse failure
//! exits non-zero and leaves the checked-in header as it was.

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;

/// (crate dir, header) pairs, relative to the workspace root.
const HEADERS: [(&str, &str); 2] = [
    ("crates/client/punktfunk-ffi", "include/punktfunk_core.h"),
    ("clients/apple/native", "include/punktfunk_console.h"),
];

fn main() -> ExitCode {
    // `cargo run` names this checkout at run time. A target dir shared between checkouts can hand
    // one checkout's build to another, so the compiled-in path is only the fallback.
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR");
    let manifest = manifest
        .as_deref()
        .map_or(Path::new(env!("CARGO_MANIFEST_DIR")), Path::new);
    let root = manifest.join("../..");
    let mut failed = false;
    for (crate_dir, header) in HEADERS {
        match cbindgen::generate(root.join(crate_dir)) {
            Ok(bindings) => {
                let changed = bindings.write_to_file(root.join(header));
                println!(
                    "{header}: {}",
                    if changed { "written" } else { "unchanged" }
                );
            }
            Err(e) => {
                eprintln!("{header}: cbindgen {crate_dir}: {e}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
