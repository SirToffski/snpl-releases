//! The application icon at runtime: the window's title bar and taskbar entry,
//! and the tray.
//!
//! The exe's own icon (Explorer, shortcuts, pinned taskbar items) is embedded
//! by `build.rs` from `assets/snpl.ico`. The window and the tray need pixels at
//! runtime, so the same artwork is compiled in as two small PNGs and decoded
//! once. Decoding cannot really fail (the bytes are ours), but if it ever did,
//! the caller logs it and falls back rather than refusing to start.

/// 256 px, for the window: Windows picks the size it needs from it.
const WINDOW_PNG: &[u8] = include_bytes!("../assets/snpl-256.png");

/// 32 px, for the tray: drawn at 16-32 px depending on the display scale.
/// Windows only: there is no tray icon on Linux yet (LINUX_SUPPORT.md section 1).
#[cfg(windows)]
const TRAY_PNG: &[u8] = include_bytes!("../assets/snpl-32.png");

/// Straight-alpha RGBA pixels and their size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconPixels {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Decodes one of the embedded PNGs.
///
/// # Errors
///
/// A message naming the image, when the bytes are not a PNG the decoder
/// accepts.
fn decode(png: &[u8], what: &str) -> Result<IconPixels, String> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|err| format!("the {what} icon could not be decoded: {err}"))?
        .to_rgba8();
    let (width, height) = image.dimensions();
    Ok(IconPixels {
        rgba: image.into_raw(),
        width,
        height,
    })
}

/// The window icon (title bar, taskbar, Alt+Tab).
///
/// # Errors
///
/// See [`decode`].
pub fn window_icon() -> Result<egui::IconData, String> {
    let pixels = decode(WINDOW_PNG, "window")?;
    Ok(egui::IconData {
        rgba: pixels.rgba,
        width: pixels.width,
        height: pixels.height,
    })
}

/// The tray icon's pixels (Windows only, like the tray).
///
/// # Errors
///
/// See [`decode`].
#[cfg(windows)]
pub fn tray_pixels() -> Result<IconPixels, String> {
    decode(TRAY_PNG, "tray")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The alpha of the pixel at (x, y).
    fn alpha(pixels: &IconPixels, x: u32, y: u32) -> u8 {
        pixels.rgba[((y * pixels.width + x) * 4 + 3) as usize]
    }

    #[test]
    fn the_window_icon_decodes_at_256_px() {
        let icon = window_icon().expect("the embedded window icon decodes");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }

    #[cfg(windows)]
    #[test]
    fn the_tray_icon_decodes_at_32_px_with_a_transparent_background() {
        let icon = tray_pixels().expect("the embedded tray icon decodes");
        assert_eq!((icon.width, icon.height), (32, 32));
        // The artwork is a rounded square on a transparent background: the
        // corners are see-through and the middle is solid.
        assert_eq!(alpha(&icon, 0, 0), 0, "the corner is not transparent");
        assert_eq!(alpha(&icon, 16, 16), 255, "the middle is not opaque");
    }

    #[test]
    fn the_window_icon_has_a_transparent_background() {
        let icon = window_icon().expect("the embedded window icon decodes");
        let pixels = IconPixels {
            rgba: icon.rgba,
            width: icon.width,
            height: icon.height,
        };
        assert_eq!(alpha(&pixels, 0, 0), 0);
        assert_eq!(alpha(&pixels, 255, 255), 0);
        assert_eq!(alpha(&pixels, 128, 128), 255);
    }
}
