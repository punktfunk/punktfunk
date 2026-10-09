//! Forward-compat mapping for stored decoder-preference strings. pf-client-core
//! compiles this file into its portable half too: the Skia settings screen reads
//! it on targets that never build this crate. `video` re-exports the function.

/// Map stored `vulkan` / `vaapi` / `d3d11va` pins onto the matching `native-*`
/// family. Those names selected libavcodec's rungs; refusing them would turn
/// a hardware pin in a settings file into a hard session error.
///
/// Pure and silent: Settings dialogs look the stored string up in their preset
/// list, and an unmatched value displays as Automatic. Logging here would warn
/// once per dialog open; [`Decoder::new`] logs when a session actually starts
/// on a migrated value.
///
/// The store is not rewritten. A later downgrade still finds the original pin.
pub fn migrate_decoder_pref(pref: &str) -> String {
    match pref {
        "vulkan" => "native-vulkan".to_string(),
        "vaapi" => "native-vaapi".to_string(),
        "d3d11va" => "native-d3d11va".to_string(),
        _ => pref.to_string(),
    }
}
