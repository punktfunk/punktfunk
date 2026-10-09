//! Dark mode, DPI-scaled glyphs, and themed popup menus for the Windows tray.
//!
//! Win32 popup menus have no documented dark-mode opt-in. This module calls the
//! undocumented uxtheme ordinals `SetPreferredAppMode` (135) and `FlushMenuThemes`
//! (136), stable since Windows 10 1809. A missing ordinal leaves the classic light menu.
//!
//! Glyphs come from Segoe Fluent Icons (Segoe MDL2 Assets on Windows 10; same
//! codepoints), drawn into premultiplied ARGB bitmaps so the themed menu composites
//! in both light and dark. Sizing helpers assume the PerMonitorV2 manifest
//! `build.rs` embeds — without it, Windows virtualizes DPI and GDI-stretches the menu.

use std::sync::OnceLock;

use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, DeleteDC, DeleteObject, DrawTextW,
    GdiFlush, GetTextFaceW, SelectObject, SetBkMode, SetTextColor, ANTIALIASED_QUALITY, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_NOCLIP, DT_SINGLELINE,
    DT_VCENTER, HBITMAP, HFONT, LOGFONTW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    SetMenuItemInfoW, HMENU, MENUITEMINFOW, MIIM_BITMAP,
};

// Same codepoints in Segoe Fluent Icons and Segoe MDL2 Assets.
pub const GLYPH_GLOBE: u16 = 0xE774;
pub const GLYPH_APPROVE: u16 = 0xE73E;
pub const GLYPH_DISPLAY: u16 = 0xE7F4;
pub const GLYPH_SHIELD: u16 = 0xE7EF;
pub const GLYPH_FOLDER: u16 = 0xE8B7;
pub const GLYPH_POWER: u16 = 0xE7E8;

type FnSetPreferredAppMode = unsafe extern "system" fn(mode: i32) -> i32;
type FnVoid = unsafe extern "system" fn();

/// Undocumented uxtheme ordinals. Any may be absent (pre-1809); each caller checks.
struct UxTheme {
    set_preferred_app_mode: Option<FnSetPreferredAppMode>,
    refresh_immersive_colors: Option<FnVoid>,
    flush_menu_themes: Option<FnVoid>,
}

static UXTHEME: OnceLock<UxTheme> = OnceLock::new();

fn uxtheme() -> &'static UxTheme {
    UXTHEME.get_or_init(|| {
        let none = UxTheme {
            set_preferred_app_mode: None,
            refresh_immersive_colors: None,
            flush_menu_themes: None,
        };
        // SAFETY: system32-only load of a Windows-supplied DLL; ordinals 135/104/136
        // return None when absent. Transmutes match those signatures. On 1809, 135 is
        // AllowDarkModeForApp(bool); SetPreferredAppMode(1) is the same "allow dark".
        unsafe {
            let Ok(lib) = LoadLibraryExW(w!("uxtheme.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            else {
                return none;
            };
            let ord = |n: u16| GetProcAddress(lib, PCSTR(n as usize as *const u8));
            UxTheme {
                set_preferred_app_mode: ord(135).map(|f| std::mem::transmute(f)),
                refresh_immersive_colors: ord(104).map(|f| std::mem::transmute(f)),
                flush_menu_themes: ord(136).map(|f| std::mem::transmute(f)),
            }
        }
    })
}

/// Allow (do not force) dark menus. Call once before the first menu.
pub fn init_dark_mode() {
    let ux = uxtheme();
    if let Some(set) = ux.set_preferred_app_mode {
        // SAFETY: resolved above with this signature; 1 = AllowDark.
        unsafe { set(1) };
    }
    if let Some(refresh) = ux.refresh_immersive_colors {
        // SAFETY: resolved above; takes no arguments.
        unsafe { refresh() };
    }
}

/// Flush the cached menu theme so the next popup follows the new system colors.
pub fn on_color_scheme_changed() {
    let ux = uxtheme();
    if let Some(refresh) = ux.refresh_immersive_colors {
        // SAFETY: resolved in uxtheme() with this signature.
        unsafe { refresh() };
    }
    if let Some(flush) = ux.flush_menu_themes {
        // SAFETY: resolved in uxtheme() with this signature.
        unsafe { flush() };
    }
}

/// `lparam` is the ImmersiveColorSet WM_SETTINGCHANGE broadcast.
pub fn is_color_scheme_change(lparam: LPARAM) -> bool {
    const NAME: &[u16] = &[
        b'I' as u16,
        b'm' as u16,
        b'm' as u16,
        b'e' as u16,
        b'r' as u16,
        b's' as u16,
        b'i' as u16,
        b'v' as u16,
        b'e' as u16,
        b'C' as u16,
        b'o' as u16,
        b'l' as u16,
        b'o' as u16,
        b'r' as u16,
        b'S' as u16,
        b'e' as u16,
        b't' as u16,
    ];
    let p = lparam.0 as *const u16;
    if p.is_null() {
        return false;
    }
    // SAFETY: WM_SETTINGCHANGE documents lParam as a nul-terminated string (or null, handled
    // above); the read is bounded to the compared length + terminator.
    unsafe {
        for (i, &want) in NAME.iter().enumerate() {
            if *p.add(i) != want {
                return false;
            }
        }
        *p.add(NAME.len()) == 0
    }
}

/// `AppsUseLightTheme` = 0. Glyph colors follow this so they match the menu.
pub fn apps_use_dark() -> bool {
    let mut data: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: RegGetValueW writes at most `size` bytes into `data`; missing value is an error we
    // treat as light (the OS default).
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    r.is_ok() && data == 0
}

pub fn window_dpi(hwnd: HWND) -> u32 {
    // SAFETY: valid on any window handle; returns 0 only for an invalid one.
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => 96, // Windows default DPI
        d => d,
    }
}

/// Per-popup `hbmpItem` bitmaps. `DestroyMenu` does not free them; Drop does.
pub struct MenuGlyphs {
    size: i32,
    color: u32, // 0x00RRGGBB
    face: Option<PCWSTR>,
    bitmaps: Vec<HBITMAP>,
}

impl MenuGlyphs {
    pub fn new(hwnd: HWND) -> Self {
        let dpi = window_dpi(hwnd) as i32;
        MenuGlyphs {
            size: (16 * dpi + 48) / 96,
            // Themed menu text: near-white on dark, near-black on light.
            color: if apps_use_dark() {
                0x00EBEBEB
            } else {
                0x00202020
            },
            face: resolve_icon_font(),
            bitmaps: Vec::new(),
        }
    }

    /// No-op if the icon font is missing or GDI fails. The menu is usable without bitmaps.
    pub fn set(&mut self, menu: HMENU, id: usize, glyph: u16) {
        let Some(face) = self.face else { return };
        let Some(bmp) = glyph_bitmap(face, glyph, self.size, self.color) else {
            return;
        };
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_BITMAP,
            hbmpItem: bmp,
            ..Default::default()
        };
        // SAFETY: mii is fully initialized with a correct cbSize; menu/id belong to the caller.
        if unsafe { SetMenuItemInfoW(menu, id as u32, false, &mii) }.is_ok() {
            self.bitmaps.push(bmp);
        } else {
            // SAFETY: created above, attached nowhere.
            unsafe {
                let _ = DeleteObject(bmp.into());
            }
        }
    }
}

impl Drop for MenuGlyphs {
    fn drop(&mut self) {
        for bmp in self.bitmaps.drain(..) {
            // SAFETY: bitmaps created by glyph_bitmap; the menu that referenced them is destroyed
            // before the guard goes out of scope in show_menu.
            unsafe {
                let _ = DeleteObject(bmp.into());
            }
        }
    }
}

fn resolve_icon_font() -> Option<PCWSTR> {
    [w!("Segoe Fluent Icons"), w!("Segoe MDL2 Assets")]
        .into_iter()
        .find(|f| font_exists(*f))
}

/// GDI never fails font creation — it substitutes. Compare the selected face.
fn font_exists(face: PCWSTR) -> bool {
    // SAFETY: local DC/font pair created and freed here; GetTextFaceW writes a bounded,
    // nul-terminated name into buf.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return false;
        }
        let font = create_font(face, 16);
        let old = SelectObject(dc, font.into());
        let mut buf = [0u16; 64];
        let n = GetTextFaceW(dc, Some(&mut buf)) as usize;
        SelectObject(dc, old);
        let _ = DeleteObject(font.into());
        let _ = DeleteDC(dc);
        let got = &buf[..n.min(buf.len())];
        let got = got.strip_suffix(&[0]).unwrap_or(got);
        n > 0 && got == face.as_wide()
    }
}

fn create_font(face: PCWSTR, height: i32) -> HFONT {
    let mut lf = LOGFONTW {
        lfHeight: -height, // negative = character height; MDL2/Fluent glyphs fill the em square
        lfWeight: 400,
        lfCharSet: DEFAULT_CHARSET,
        // Grayscale AA, not ClearType: the alpha pass reads one channel; subpixel leaves fringes.
        lfQuality: ANTIALIASED_QUALITY,
        ..Default::default()
    };
    // SAFETY: both candidate face literals fit LF_FACESIZE (32) incl. nul; CreateFontIndirectW
    // copies the struct.
    unsafe {
        let name = face.as_wide();
        lf.lfFaceName[..name.len()].copy_from_slice(name);
        CreateFontIndirectW(&lf)
    }
}

/// White-on-black coverage → premultiplied ARGB in `color`. Themed menus need this format.
fn glyph_bitmap(face: PCWSTR, glyph: u16, size: i32, color: u32) -> Option<HBITMAP> {
    // SAFETY: every handle created here is released here (the returned bitmap by
    // MenuGlyphs::drop). GdiFlush completes pending writes before the bits are read.
    // The pixel loop stays inside the size*size allocation CreateDIBSection made.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return None;
        };
        let font = create_font(face, size);
        let old_bmp = SelectObject(dc, bmp.into());
        let old_font = SelectObject(dc, font.into());

        std::ptr::write_bytes(bits as *mut u8, 0, (size * size * 4) as usize);
        SetTextColor(dc, COLORREF(0x00FF_FFFF));
        SetBkMode(dc, TRANSPARENT);
        let mut text = [glyph];
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: size,
            bottom: size,
        };
        DrawTextW(
            dc,
            &mut text,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP,
        );
        let _ = GdiFlush();

        let px = bits as *mut u32;
        let (r, g, b) = ((color >> 16) & 0xFF, (color >> 8) & 0xFF, color & 0xFF);
        for i in 0..(size * size) as usize {
            let a = *px.add(i) & 0xFF; // white-on-black: any channel is the coverage
            *px.add(i) = (a << 24) | ((r * a / 255) << 16) | ((g * a / 255) << 8) | (b * a / 255);
        }

        SelectObject(dc, old_font);
        SelectObject(dc, old_bmp);
        let _ = DeleteObject(font.into());
        let _ = DeleteDC(dc);
        Some(bmp)
    }
}
