//! Names the `desktop` gate the ladder modules repeat; pf-client-core spells the same alias.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    cfg_aliases::cfg_aliases! {
        // The decode ladder: Linux and Windows with the `desktop` feature.
        desktop: { all(feature = "desktop", any(target_os = "linux", windows)) },
    }
}
