//! Build the vendored PyroWave codec (C++/Vulkan compute) as static archives via
//! CMake and generate bindings over its C API (`pyrowave.h`).
//!
//! Linux, Windows, and 64-bit Android — the targets whose hosts/clients run the
//! Vulkan codec path (design/pyrowave-codec-plan.md §5). Other targets get an empty
//! bindings file so the workspace builds everywhere (the Apple client is a native
//! Metal port, §4.7 — it never links this crate).
//!
//! 32-bit Android is stubbed with them. The C++ builds fine there, but Vulkan's
//! armv7 calling convention (`__attribute__((pcs("aapcs-vfp")))` on every VKAPI
//! entry point) has no bindgen representation, and it panics rather than skipping
//! it. Nothing is lost: armeabi-v7a is the 32-bit TV-stick ABI, and those boxes
//! have neither the link nor the Vulkan 1.3 device this codec needs. The gate is
//! mirrored by `target_pointer_width` cfgs in src/lib.rs and pf-client-core.
//!
//! Everything compiles from the committed vendor tree: no network, no system
//! pyrowave, no pkg-config — CI, MSVC, and the offline flatpak builder all get
//! reproducible builds (§4.1). Vulkan headers come vendored too; only a libclang
//! for bindgen is required on the build machine.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=CMakeLists.txt");
    println!("cargo:rerun-if-changed=vendor/pyrowave");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bindings_path = out.join("bindings.rs");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let bits64 = env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_default() == "64";
    let build_here =
        target_os == "linux" || target_os == "windows" || (target_os == "android" && bits64);
    if !build_here {
        std::fs::write(
            &bindings_path,
            "// pyrowave-sys: Linux/Windows/64-bit-Android only, empty on this target\n",
        )
        .unwrap();
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor/pyrowave");
    let vk_include = vendor.join("Granite/third_party/khronos/vulkan-headers/include");

    // Always Release: a debug build of the wavelet kernels' host code buys no
    // debuggability (the hot path is GPU shaders baked into the source) and the
    // MSVC debug CRT would clash with Rust's release CRT.
    let mut cfg = cmake::Config::new(&manifest_dir);
    cfg.profile("Release").build_target("pyrowave-capi");
    // A `+crt-static` target (the UMDF driver workspace) must get static-CRT objects:
    // CMP0091 makes CMake ignore the `-MT` in the flags and default to the DLL runtime.
    if env::var("CARGO_CFG_TARGET_FEATURE").is_ok_and(|f| f.split(',').any(|t| t == "crt-static")) {
        cfg.define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreaded");
    }
    let dst = cfg.build();
    let build = dst.join("build");

    // Static link closure, dependents before dependencies (GNU ld is order-sensitive).
    println!("cargo:rustc-link-search=native={}", build.display());
    for sub in [
        "pyrowave",
        "Granite/vulkan",
        "Granite/util",
        "Granite/math",
        "Granite/third_party",
    ] {
        println!(
            "cargo:rustc-link-search=native={}",
            build.join(sub).display()
        );
        // MSVC multi-config generators put archives in a Release/ subdir.
        println!(
            "cargo:rustc-link-search=native={}",
            build.join(sub).join("Release").display()
        );
    }
    println!(
        "cargo:rustc-link-search=native={}",
        build.join("Release").display()
    );
    for lib in [
        "pyrowave-capi",
        "pyrowave",
        "granite-vulkan",
        "granite-util",
        "granite-math",
        "granite-volk",
    ] {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    if target_os == "linux" {
        println!("cargo:rustc-link-lib=dylib=stdc++");
        // volk loads the Vulkan loader at runtime.
        println!("cargo:rustc-link-lib=dylib=dl");
        println!("cargo:rustc-link-lib=dylib=pthread");
    }
    if target_os == "android" {
        // STATIC libc++ so the client `.so` stays self-contained, and unprefixed (not
        // `static=`) because the archives live in the NDK sysroot, which rustc does not
        // search — the name is passed through for the NDK's clang. Same spelling
        // skia-bindings uses for this `.so`, which keeps ONE C++ runtime in it.
        println!("cargo:rustc-link-lib=c++_static");
        println!("cargo:rustc-link-lib=c++abi");
    }
    if target_os == "windows" {
        // Granite's breadcrumbs tracker raises a MessageBoxA on device hang.
        println!("cargo:rustc-link-lib=dylib=user32");
    }

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", vendor.display()))
        .clang_arg(format!("-I{}", vk_include.display()))
        .allowlist_function("pyrowave_.*")
        .allowlist_type("pyrowave_.*")
        .allowlist_var("PYROWAVE_.*")
        // The Vk* handles/structs the API mentions come along via the allowlist
        // closure; they are ABI-identical to ash's, callers cast at the seam.
        .derive_default(true)
        .generate()
        .expect("bindgen failed for pyrowave.h");
    bindings
        .write_to_file(&bindings_path)
        .expect("write the pyrowave bindings");
}
