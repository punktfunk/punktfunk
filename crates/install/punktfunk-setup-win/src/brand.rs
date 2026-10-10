//! The wizard's brand pieces, staged to disk because reactor's image element and window
//! icon both take file paths: the lens highlight, the four "funk" letters in each colour
//! scheme, and `punktfunk.ico`. The rasters are rendered by `scripts/gen-setup-brand.sh`
//! from the website's exact path data — regenerate and commit when the brand changes. The
//! mark's two circles are drawn live as ellipses (`wizard::lockup`) so they can orbit.

use std::path::PathBuf;
use std::sync::OnceLock;

use windows_reactor::{Color, ColorScheme};

/// pf-console-ui's dark-appearance brand value; the terminal theme carries the same one.
pub const VIOLET: Color = Color {
    a: 255,
    r: 0x86,
    g: 0x78,
    b: 0xf5,
};

/// The mark's palette (`web/public/favicon.svg`), the same in both schemes.
pub const LIGHT_CIRCLE: Color = Color {
    a: 255,
    r: 0xa7,
    g: 0x9f,
    b: 0xf8,
};
pub const DEEP_CIRCLE: Color = Color {
    a: 255,
    r: 0x6c,
    g: 0x5b,
    b: 0xf3,
};

const FILES: [(&str, &[u8]); 10] = [
    (
        "punktfunk.ico",
        include_bytes!("../../../../packaging/windows/branding/punktfunk.ico"),
    ),
    (
        "lens-highlight.png",
        include_bytes!("../assets/lens-highlight.png"),
    ),
    (
        "wordmark-dark-0.png",
        include_bytes!("../assets/wordmark-dark-0.png"),
    ),
    (
        "wordmark-dark-1.png",
        include_bytes!("../assets/wordmark-dark-1.png"),
    ),
    (
        "wordmark-dark-2.png",
        include_bytes!("../assets/wordmark-dark-2.png"),
    ),
    (
        "wordmark-dark-3.png",
        include_bytes!("../assets/wordmark-dark-3.png"),
    ),
    (
        "wordmark-light-0.png",
        include_bytes!("../assets/wordmark-light-0.png"),
    ),
    (
        "wordmark-light-1.png",
        include_bytes!("../assets/wordmark-light-1.png"),
    ),
    (
        "wordmark-light-2.png",
        include_bytes!("../assets/wordmark-light-2.png"),
    ),
    (
        "wordmark-light-3.png",
        include_bytes!("../assets/wordmark-light-3.png"),
    ),
];

fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("punktfunk").join("setup-brand"))
}

/// Materialize every piece (idempotent; a size mismatch rewrites so a re-baked asset lands).
pub fn install() {
    let Some(dir) = dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return; // the pages just render without the brand pieces
    }
    for (name, bytes) in FILES {
        let p = dir.join(name);
        let fresh = std::fs::metadata(&p)
            .map(|m| m.len() != bytes.len() as u64)
            .unwrap_or(true);
        if fresh {
            let _ = std::fs::write(&p, bytes);
        }
    }
}

/// A staged piece's path; `None` (render without it) when staging failed.
pub fn path(name: &str) -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    let p = DIR.get_or_init(dir).as_ref()?.join(name);
    p.exists().then_some(p)
}

/// The `file:///` URI reactor's image element wants.
pub fn uri(name: &str) -> Option<String> {
    let p = path(name)?;
    Some(format!(
        "file:///{}",
        p.display().to_string().replace('\\', "/")
    ))
}

/// The i-th "funk" letter in the scheme's colour (the site's highlight on dark, brand on light).
pub fn letter(scheme: ColorScheme, i: usize) -> Option<String> {
    let scheme = match scheme {
        ColorScheme::Dark => "dark",
        ColorScheme::Light => "light",
    };
    uri(&format!("wordmark-{scheme}-{i}.png"))
}
