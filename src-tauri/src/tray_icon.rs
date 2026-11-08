// Copyright (c) Kensaku contributors. SPDX-License-Identifier: MIT
//! Tray-icon helpers: picks the right icon bytes for the current app theme.
//!
//! On Windows the tray sits on a taskbar whose colour follows the system
//! "Taskbar behaviours" setting. The standard teal glyph vanishes on a **dark**
//! taskbar, so we also ship a high-contrast light variant (`tray-light.png`).
//! The variant follows the **app** theme, per #46: `"dark"` uses the light
//! glyph, `"light"` keeps the standard icon. Other platforms keep today's
//! behaviour and always use the standard icon.
//!
//! # Design constraints
//! * No I/O at runtime: both PNGs are embedded with `include_bytes!`.
//! * Fallback-safe: a decode failure returns `None`; callers fall back to
//!   `app.default_window_icon()`.
//! * Testable without Tauri: [`tray_icon_bytes`] is a pure function, so icon
//!   selection is unit-tested; only [`decode_png`] touches image data.

/// Raw PNG bytes for the standard tray icon: teal glyph on transparency.
///
/// This is the same 32×32 source used by the rest of the app.
static TRAY_DARK_PNG: &[u8] = include_bytes!("../icons/tray-dark.png");

/// Raw PNG bytes for the high-contrast tray icon.
///
/// A white monochrome variant of the standard glyph so it remains legible on
/// dark Windows taskbars.
static TRAY_LIGHT_PNG: &[u8] = include_bytes!("../icons/tray-light.png");

/// Returns the PNG byte slice that matches `theme`.
///
/// | `theme` value                   | returned bytes   |
/// |---------------------------------|------------------|
/// | `"dark"` / `"void"`             | `TRAY_LIGHT_PNG` |
/// | `"light"` / `"paper"`           | `TRAY_DARK_PNG`  |
/// | anything else                   | `TRAY_LIGHT_PNG` |
///
/// The function is `pub` and free of side-effects so it can be unit-tested
/// without a running Tauri application.
pub fn tray_icon_bytes(theme: &str) -> &'static [u8] {
    match crate::config::normalize_theme(theme).as_str() {
        "dark" => TRAY_LIGHT_PNG,
        _ => TRAY_DARK_PNG,
    }
}

/// Decodes `bytes` as a PNG and returns a [`tauri::image::Image`].
///
/// Returns `None` when the PNG cannot be decoded (corrupt bytes, unsupported
/// colour-space, …).  Callers should fall back to
/// `app.default_window_icon()`.
pub fn decode_png(bytes: &[u8]) -> Option<tauri::image::Image<'static>> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?;
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    Some(tauri::image::Image::new_owned(
        rgba.into_raw(),
        width,
        height,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------------
    // tray_icon_bytes
    // ---------------------------------------------------------------------------

    #[test]
    fn dark_theme_returns_light_bytes() {
        assert!(std::ptr::eq(tray_icon_bytes("dark"), TRAY_LIGHT_PNG));
    }

    #[test]
    fn light_theme_returns_standard_bytes() {
        assert!(std::ptr::eq(tray_icon_bytes("light"), TRAY_DARK_PNG));
    }

    #[test]
    fn legacy_void_maps_to_light_bytes() {
        assert!(std::ptr::eq(tray_icon_bytes("void"), TRAY_LIGHT_PNG));
    }

    #[test]
    fn legacy_paper_maps_to_dark_bytes() {
        assert!(std::ptr::eq(tray_icon_bytes("paper"), TRAY_DARK_PNG));
    }

    #[test]
    fn unrecognised_theme_falls_back_to_dark() {
        // normalize_theme returns "dark" for unknown values
        assert!(std::ptr::eq(tray_icon_bytes("magenta"), TRAY_LIGHT_PNG));
    }

    #[test]
    fn case_insensitive_light() {
        assert!(std::ptr::eq(tray_icon_bytes("LIGHT"), TRAY_DARK_PNG));
    }

    #[test]
    fn case_insensitive_dark() {
        assert!(std::ptr::eq(tray_icon_bytes("DARK"), TRAY_LIGHT_PNG));
    }

    // ---------------------------------------------------------------------------
    // decode_png
    // ---------------------------------------------------------------------------

    #[test]
    fn dark_png_decodes_as_valid_32x32() {
        let img = decode_png(TRAY_DARK_PNG).expect("dark PNG must decode");
        assert_eq!(img.width(), 32);
        assert_eq!(img.height(), 32);
    }

    #[test]
    fn light_png_decodes_as_valid_32x32() {
        let img = decode_png(TRAY_LIGHT_PNG).expect("light PNG must decode");
        assert_eq!(img.width(), 32);
        assert_eq!(img.height(), 32);
    }

    #[test]
    fn dark_and_light_png_bytes_differ() {
        assert_ne!(
            TRAY_DARK_PNG, TRAY_LIGHT_PNG,
            "the two icon variants must not be identical"
        );
    }

    #[test]
    fn corrupt_bytes_return_none() {
        assert!(decode_png(b"not a png").is_none());
    }
}
