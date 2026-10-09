//! Names the two platform gates the modules repeat. `portable` lists the same families as the
//! per-target dependency tables in Cargo.toml that declare punktfunk-core.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    cfg_aliases::cfg_aliases! {
        // The data modules every client links: trust, settings, library, deep links.
        portable: {
            any(
                target_os = "linux",
                windows,
                target_os = "android",
                target_vendor = "apple",
                target_family = "wasm"
            )
        },
        // The desktop half: session pump, decode ladder, audio, gamepads.
        desktop: { all(feature = "desktop", any(target_os = "linux", windows)) },
    }
}
