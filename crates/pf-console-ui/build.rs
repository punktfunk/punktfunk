//! System libraries skia-bindings leaves out. It has no tvOS platform, so on tvOS it links
//! nothing: these are the ones its iOS platform names. Skia's Metal backend also calls
//! Foundation, which an app links anyway and a bare binary does not. Elsewhere this does nothing.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let mut libs = Vec::new();
    if os == "tvos" {
        libs.extend([
            "c++",
            "framework=CoreFoundation",
            "framework=CoreGraphics",
            "framework=CoreText",
            "framework=ImageIO",
            "framework=MobileCoreServices",
            "framework=UIKit",
        ]);
    }
    let apple = matches!(os.as_str(), "macos" | "ios" | "tvos");
    if apple && std::env::var_os("CARGO_FEATURE_METAL").is_some() {
        libs.extend(["framework=Foundation", "framework=Metal"]);
    }
    for lib in libs {
        println!("cargo:rustc-link-lib={lib}");
    }
}
