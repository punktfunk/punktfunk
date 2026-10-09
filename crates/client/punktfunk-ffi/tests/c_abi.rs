//! Compiles `tests/c/harness.c`, links it to a freshly built `libpunktfunk_ffi.a`,
//! and asserts a lossy-loopback frame round-trip. Canonical path is `tests/c/run.sh`;
//! this mirrors it so `cargo test` alone covers the C boundary.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Extra native libs for the staticlib. Omit `-lSystem`/`-lc` — `cc` already
/// links them and duplicates warn. See `rustc --print native-static-libs`.
fn native_libs() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        // Workspace `quic` pulls rustls's platform verifier (Security/CoreFoundation). libopus
        // is bundled into the staticlib.
        &[
            "-liconv",
            "-lm",
            "-framework",
            "Security",
            "-framework",
            "CoreFoundation",
        ]
    } else if cfg!(target_os = "linux") {
        // libopus is bundled into the staticlib and needs `-lm`.
        &["-lgcc_s", "-lutil", "-lrt", "-lpthread", "-lm", "-ldl"]
    } else {
        &[]
    }
}

fn ensure_staticlib(profile_dir: &Path) -> PathBuf {
    // Nested `CARGO_TARGET_DIR`: this `cargo build --features quic` runs mid outer
    // `cargo test`. Sharing `target/<profile>` rebuilds the graph under different
    // metadata and the outer `--extern` rlib paths vanish. A leftover featureless
    // `.a` from a plain `cargo build` would also fail the quic link.
    let nested = profile_dir
        .parent()
        .expect("target dir")
        .join("c-abi-harness");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let _ = Command::new(cargo)
        .args(["build", "-p", "punktfunk-ffi", "--features", "quic"])
        .env("CARGO_TARGET_DIR", &nested)
        .status();
    nested.join("debug").join("libpunktfunk_ffi.a")
}

#[test]
fn c_abi_harness_round_trips() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let harness = manifest.join("tests/c/harness.c");
    let include = manifest.join("../../../include");

    let exe = std::env::current_exe().expect("current_exe");
    // current_exe is `.../target/<profile>/deps/c_abi-<hash>`; two parents is the profile dir.
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("profile dir")
        .to_path_buf();

    let staticlib = ensure_staticlib(&profile_dir);
    assert!(
        staticlib.exists(),
        "staticlib not found at {} (run `cargo build -p punktfunk-ffi`)",
        staticlib.display()
    );
    assert!(
        include.join("punktfunk_core.h").exists(),
        "generated header missing; run `cargo run -p gen-headers`"
    );

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let out = profile_dir.join("punktfunk_c_harness");

    let mut compile = Command::new(&cc);
    compile
        // Match `ensure_staticlib`'s `quic` build so the harness can use that header surface.
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-O2",
            "-DPUNKTFUNK_FEATURE_QUIC",
            "-I",
        ])
        .arg(&include);
    compile
        .arg(&harness)
        .arg(&staticlib)
        .args(native_libs())
        .arg("-o")
        .arg(&out);

    match compile.status() {
        Ok(s) => assert!(s.success(), "C harness failed to compile/link"),
        Err(e) => {
            // No C toolchain. Skip rather than fail the suite; `tests/c/run.sh` covers CI.
            eprintln!("skipping C ABI test: cannot invoke `{cc}`: {e}");
            return;
        }
    }

    let run = Command::new(&out).status().expect("run C harness");
    assert!(run.success(), "C harness reported a round-trip failure");
}
