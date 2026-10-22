//! snp-ligase-platform: the operating-system boundary.
//!
//! The rest of the program never touches Win32, WinRT, WGC or a GPU directly.
//! It talks to this crate through the [`Platform`] trait and the plain-data
//! types below, so a future Linux or macOS implementation only has to fill in
//! the trait (SPEC section 5.1).
//!
//! Milestone M0 defined the trait, its shared types and the crate error type.
//! M1 adds the Windows implementation of monitor enumeration and capture: one
//! Windows.Graphics.Capture session per monitor, FP16 to 8-bit conversion
//! through [`snp_ligase_core::color`], and a GDI fallback.

use image::RgbaImage;

// The instance channel (LINUX_SUPPORT.md section 3, item 9): one running
// snp-ligase per session, and a second launch hands it what it was asked to
// do. The message type is the same on every platform; the socket transport is
// Unix-only.
pub mod instance;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(windows)]
pub mod windows;

/// The Linux implementation of [`Platform`], re-exported for the same reason.
#[cfg(target_os = "linux")]
pub use linux::LinuxPlatform;
/// The Windows implementation of [`Platform`], re-exported so the app does not
/// have to know which module it lives in.
#[cfg(windows)]
pub use windows::WindowsPlatform;

/// The [`Platform`] implementation for the OS this build targets.
///
/// The app names this, not a backend: `WindowsPlatform` on Windows,
/// `LinuxPlatform` on Linux (LINUX_SUPPORT.md).
#[cfg(windows)]
pub type CurrentPlatform = WindowsPlatform;
/// The [`Platform`] implementation for the OS this build targets.
#[cfg(target_os = "linux")]
pub type CurrentPlatform = LinuxPlatform;

#[cfg(target_os = "linux")]
pub use linux::keys;
/// The keyboard-poll API the app uses, from the backend for this target.
///
/// On Windows this is the `GetAsyncKeyState` poll; on Linux the X11
/// `XQueryKeymap` bitmap (and "nothing held" on Wayland, phase L3).
#[cfg(windows)]
pub use windows::keys;

/// A rectangle in **physical pixels**, in virtual-desktop coordinates.
///
/// Virtual-desktop coordinates are the one shared pixel space Win32 uses once
/// the process is Per-Monitor DPI Aware V2: every monitor sits in it and no
/// scale factor is ever applied (SPEC section 5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct RectI {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl RectI {
    /// A rectangle from its origin and size, with a negative size clamped to
    /// zero so an image is never asked for a negative dimension.
    #[must_use]
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            x,
            y,
            w: if w < 0 { 0 } else { w },
            h: if h < 0 { 0 } else { h },
        }
    }

    /// The right edge, `x + w`.
    #[must_use]
    pub const fn right(self) -> i32 {
        self.x + self.w
    }

    /// The bottom edge, `y + h`.
    #[must_use]
    pub const fn bottom(self) -> i32 {
        self.y + self.h
    }

    /// True when the rectangle has no area.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// True when the point is inside: the left and top edges belong to the
    /// rectangle, the right and bottom ones do not.
    #[must_use]
    pub const fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// True when `other` lies entirely inside `self`.
    ///
    /// This is "is the last region still usable", which the "repeat last
    /// region" mode needs: after a monitor is unplugged a stored region can
    /// point at pixels that no longer exist (SPEC section 5.7).
    #[must_use]
    pub const fn contains_rect(self, other: RectI) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    /// The shared area, or an empty rectangle sitting where the two stopped
    /// overlapping.
    #[must_use]
    pub fn intersection(self, other: Self) -> Self {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        Self::new(left, top, right - left, bottom - top)
    }

    /// `self` in the window-local coordinates of a window whose origin is
    /// `window`.
    ///
    /// The overlay's windows sit exactly on their monitor, so a selection
    /// tracked in virtual-desktop coordinates has to be shifted (and clipped)
    /// to be painted in one (SPEC section 5.6). Monitor origins can be
    /// negative, which is what makes this easy to get wrong.
    #[must_use]
    pub const fn to_local(self, window: Self) -> Self {
        Self {
            x: self.x - window.x,
            y: self.y - window.y,
            w: self.w,
            h: self.h,
        }
    }

    /// The same rectangle moved by `(dx, dy)`.
    #[must_use]
    pub const fn translated(self, dx: i32, dy: i32) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
            ..self
        }
    }
}

/// One monitor, as reported by the OS at the moment of a capture.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MonitorInfo {
    /// Stable device name, e.g. `\\.\DISPLAY1`.
    pub id: String,
    /// Physical pixels in virtual-desktop space.
    pub rect: RectI,
    /// 1.0, 1.25, 1.5, ... For logging and the UI only, never for maths.
    pub scale_factor: f32,
    /// True while the monitor is being driven in HDR.
    pub hdr_active: bool,
    /// SDR white level in nits, when the OS reports one.
    pub sdr_white_nits: Option<f32>,
    /// True for the monitor that owns the primary display.
    pub is_primary: bool,
}

/// Which API produced a frame (SPEC section 5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum CaptureApi {
    /// Windows.Graphics.Capture.
    #[serde(rename = "WGC")]
    Wgc,
    /// GDI `BitBlt` fallback.
    #[serde(rename = "GDI")]
    Gdi,
    /// X11 via MIT-SHM (LINUX_SUPPORT.md section 4).
    #[serde(rename = "X11Shm")]
    X11Shm,
    /// X11 plain `GetImage`, the fallback when MIT-SHM cannot be used.
    #[serde(rename = "X11Get")]
    X11Get,
    /// The desktop's Screenshot portal (Wayland fallback; L3b/L4a).
    #[serde(rename = "Portal")]
    Portal,
    /// KDE KWin native per-output capture.
    #[serde(rename = "KWinScreenShot2")]
    Kwin,
}

impl CaptureApi {
    /// Name used in logs and in `capture.json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wgc => "WGC",
            Self::Gdi => "GDI",
            Self::X11Shm => "X11Shm",
            Self::X11Get => "X11Get",
            Self::Portal => "Portal",
            Self::Kwin => "KWinScreenShot2",
        }
    }
}

impl std::fmt::Display for CaptureApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Facts about a single captured frame, logged so a wrong capture can be
/// diagnosed from the log file alone (SPEC section 5.4).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FrameDiagnostics {
    pub api_used: CaptureApi,
    /// Pixel format the frame arrived in, e.g. `Rgba16F`, `Bgra8`.
    pub pixel_format: String,
    pub width: u32,
    pub height: u32,
    /// Bytes between the start of one row and the next, 0 for the GDI fallback,
    /// which hands over tightly packed rows (SPEC section 13: pitch in the
    /// diagnostics, so a sheared capture can be explained).
    pub row_pitch: u32,
    /// Highest linear channel value seen in an FP16 frame, 0..=1 after scaling.
    pub max_channel_value: f32,
    /// SDR white level in nits that was used for the HDR conversion.
    pub sdr_white_nits: f32,
    /// Time from the start of the capture to holding the pixels, in milliseconds.
    pub elapsed_ms: f64,
}

/// How stored image pixels relate to the monitor's virtual rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredPixels {
    /// One stored pixel per virtual pixel (portal, Windows and X11).
    Virtual,
    /// Native output pixels; map using image dimensions / virtual rectangle.
    Native,
}

/// One captured monitor: what it showed, plus how it was captured.
#[derive(Debug, Clone)]
pub struct MonitorFrame {
    pub info: MonitorInfo,
    /// 8-bit straight sRGB, alpha forced to 255.
    pub image: RgbaImage,
    pub diagnostics: FrameDiagnostics,
    /// Storage geometry used by crop, independent of capture diagnostics.
    pub stored_pixels: StoredPixels,
    /// This frame's pixels as a fraction of the desktop's virtual space, per
    /// axis (LINUX_SUPPORT.md section 6, L3b).
    ///
    /// KWin's native stored frames use 1.0: their image-to-virtual ratio is
    /// derived separately from image dimensions and `info.rect`.
    /// 1.0 when a frame pixel *is* a virtual pixel — the case on Windows and
    /// X11, whose virtual space is real pixels — and below 1.0 for an output
    /// the capture _upscaled_ to another output's scale: the Wayland Screenshot
    /// portal composes the whole desktop at the largest scale, so a 100% output
    /// next to a 150% one arrives 1.5× too big and its frame's native scale is
    /// ⅔. [`VirtualCapture::crop`] uses it for the one-output
    /// native-resolution rule: a selection lying inside one monitor comes out
    /// at that monitor's own physical resolution, not upscaled.
    ///
    /// Kept per axis (not one factor) because width and height can round to
    /// slightly different ratios.
    pub native_scale_x: f64,
    pub native_scale_y: f64,
}

/// One frame from every monitor, taken at (nearly) the same instant.
#[derive(Debug, Clone, Default)]
pub struct VirtualCapture {
    pub frames: Vec<MonitorFrame>,
}

impl VirtualCapture {
    /// The union of every monitor rectangle, in virtual-desktop pixels.
    ///
    /// Used by the "all monitors" capture mode, which is just this rectangle
    /// fed back through [`VirtualCapture::crop`]. The union needs the smallest
    /// left/top and the largest right/bottom seen, independently: taking the
    /// width from whichever frame was visited last loses the right edge of a
    /// monitor visited earlier (found in M5, with monitors side by side on the
    /// agent's own desk).
    #[must_use]
    pub fn bounds(&self) -> RectI {
        let mut union: Option<(i32, i32, i32, i32)> = None; // left, top, right, bottom
        for frame in &self.frames {
            let rect = frame.info.rect;
            union = Some(match union {
                None => (rect.x, rect.y, rect.right(), rect.bottom()),
                Some((left, top, right, bottom)) => (
                    left.min(rect.x),
                    top.min(rect.y),
                    right.max(rect.right()),
                    bottom.max(rect.bottom()),
                ),
            });
        }
        match union {
            Some((left, top, right, bottom)) => RectI::new(left, top, right - left, bottom - top),
            None => RectI::default(),
        }
    }

    /// Crops the captured frames to `r`, in virtual-desktop pixels.
    ///
    /// Monitors can have negative coordinates and there can be gaps between
    /// them. Pixels that no frame covers stay transparent (SPEC section 5.1).
    /// A frame whose pixels do not match its rectangle is used as-is, so a
    /// mismatched capture degrades instead of panicking.
    ///
    /// A selection lying wholly inside one output comes out at that output's
    /// own physical resolution instead of the capture's virtual-space pixels —
    /// the one-output native-resolution rule (LINUX_SUPPORT.md section 6,
    /// L3b). The reading frame's [`MonitorFrame::native_scale_x`/`_y`] say how
    /// much its pixels were rescaled when the capture was composed; Windows
    /// and X11 frames keep their own pixels (native scale 1.0), so for them
    /// this is the plain copy it always was.
    #[must_use]
    pub fn crop(&self, r: RectI) -> RgbaImage {
        let width = u32::try_from(r.w.max(0)).unwrap_or(0);
        let height = u32::try_from(r.h.max(0)).unwrap_or(0);

        if width == 0 || height == 0 {
            return RgbaImage::new(width, height);
        }

        // KWin stores native pixels while rectangles stay in portal virtual
        // space. A one-output selection copies rounded source edges directly.
        if let Some(frame) = self
            .frames
            .iter()
            .find(|f| f.stored_pixels == StoredPixels::Native && f.info.rect.contains_rect(r))
        {
            let rect = frame.info.rect;
            let x0 = native_edge(r.x - rect.x, rect.w, frame.image.width());
            let y0 = native_edge(r.y - rect.y, rect.h, frame.image.height());
            let x1 = native_edge(r.right() - rect.x, rect.w, frame.image.width());
            let y1 = native_edge(r.bottom() - rect.y, rect.h, frame.image.height());
            return image::imageops::crop_imm(&frame.image, x0, y0, x1 - x0, y1 - y0).to_image();
        }

        // Unit-scale frames retain the original stitch path, including the
        // paint order when Windows/X11 monitor rectangles overlap.
        if let Some(frame) = self
            .frames
            .iter()
            .find(|f| (f.native_scale_x != 1.0 || f.native_scale_y != 1.0) && frame_owns(f, r))
        {
            let rect = frame.info.rect;
            let src_x = u32::try_from(r.x - rect.x).unwrap_or(0);
            let src_y = u32::try_from(r.y - rect.y).unwrap_or(0);
            let region = copy_region(&frame.image, src_x, src_y, width, height);

            let native_w = native_size(r.w, frame.native_scale_x, frame.image.width());
            let native_h = native_size(r.h, frame.native_scale_y, frame.image.height());
            if native_w != width || native_h != height {
                let scaled = downscale_rgba(&region, width, height, native_w, native_h);
                if let Some(out) = RgbaImage::from_raw(native_w, native_h, scaled) {
                    return out;
                }
                // `downscale_rgba` writes exactly `native_w × native_h × 4`
                // bytes, so this cannot happen; the stitch below degrades.
            } else if let Some(out) = RgbaImage::from_raw(width, height, region) {
                // The frame owned the selection, so the region is its size.
                return out;
            }
        }

        // No single frame owns the selection (it spans monitors, or it lies in
        // a gap): the old per-pixel stitch, transparent in uncovered pixels.
        let mut out = RgbaImage::new(width, height);

        for frame in &self.frames {
            let rect = frame.info.rect;
            let x0 = r.x.max(rect.x);
            let y0 = r.y.max(rect.y);
            let x1 = (r.x + r.w).min(rect.x + rect.w);
            let y1 = (r.y + r.h).min(rect.y + rect.h);
            if x1 <= x0 || y1 <= y0 {
                continue; // no overlap with this monitor
            }

            let src_x = u32::try_from(x0 - rect.x).unwrap_or(0);
            let src_y = u32::try_from(y0 - rect.y).unwrap_or(0);
            let dst_x = u32::try_from(x0 - r.x).unwrap_or(0);
            let dst_y = u32::try_from(y0 - r.y).unwrap_or(0);

            for row in 0..u32::try_from(y1 - y0).unwrap_or(0) {
                for column in 0..u32::try_from(x1 - x0).unwrap_or(0) {
                    // A spanning native capture keeps virtual resolution:
                    // nearest source pixels, no down-then-up scaling of a
                    // selection that belongs to just one output.
                    let (sx, sy) = if frame.stored_pixels == StoredPixels::Native {
                        (
                            virtual_sample(src_x + column, rect.w, frame.image.width()),
                            virtual_sample(src_y + row, rect.h, frame.image.height()),
                        )
                    } else {
                        (src_x + column, src_y + row)
                    };
                    let Some(pixel) = frame.image.get_pixel_checked(sx, sy) else {
                        continue; // frame smaller than its rectangle
                    };
                    out.put_pixel(dst_x + column, dst_y + row, *pixel);
                }
            }
        }

        out
    }
}

/// Rounded selection edges for direct native copying; clamped at the output.
fn native_edge(offset: i32, virtual_length: i32, native_length: u32) -> u32 {
    if virtual_length <= 0 {
        return 0;
    }
    (offset as f64 * f64::from(native_length) / f64::from(virtual_length))
        .round()
        .clamp(0.0, f64::from(native_length)) as u32
}

/// Nearest native sample at the virtual pixel centre for spanning crops.
fn virtual_sample(offset: u32, virtual_length: i32, native_length: u32) -> u32 {
    if virtual_length <= 0 {
        return 0;
    }
    ((f64::from(offset) + 0.5) * f64::from(native_length) / f64::from(virtual_length))
        .floor()
        .max(0.0) as u32
}

/// True when `frame`'s rectangle contains `r` and its image also covers it in
/// full, so a selection at `r` can be made from this frame alone.
///
/// Pure because [`VirtualCapture::crop`] picks the owner frame with it and the
/// tests prove the choice on the VM's real numbers.
fn frame_owns(frame: &MonitorFrame, r: RectI) -> bool {
    let rect = frame.info.rect;
    let wants_w = u32::try_from(r.w.max(0)).unwrap_or(0);
    let wants_h = u32::try_from(r.h.max(0)).unwrap_or(0);
    rect.x <= r.x
        && rect.y <= r.y
        && r.x + r.w <= rect.x + rect.w
        && r.y + r.h <= rect.y + rect.h
        && wants_w <= frame.image.width()
        && wants_h <= frame.image.height()
}

/// A length in an output's own pixels, from the selection's length in virtual
/// space and the output's native scale, never larger than the frame can give.
///
/// Rounded, so a 1920-wide selection at native scale ⅔ asks for exactly 1280.
/// The clamp makes a frame smaller than its rectangle degrade instead of
/// panicking.
fn native_size(length: i32, scale: f64, source: u32) -> u32 {
    let wanted = (length as f64 * scale).round().max(0.0);
    let source = source as f64;
    if wanted < source {
        wanted as u32
    } else {
        source as u32
    }
}

/// Copies the `w × h` region at `(x, y)` out of `image` into a tightly packed
/// RGBA buffer. Pixels off the image stay transparent — callers that need the
/// region in full check first ([`frame_owns`]), so this is only the guard.
pub(crate) fn copy_region(image: &RgbaImage, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for row in 0..h {
        for column in 0..w {
            let Some(pixel) = image.get_pixel_checked(x + column, y + row) else {
                continue;
            };
            let i = (row as usize * w as usize + column as usize) * 4;
            out[i..i + 4].copy_from_slice(&pixel.0);
        }
    }
    out
}

/// Downscales a tightly packed RGBA buffer with a premultiplied area filter.
///
/// [`VirtualCapture::crop`] uses it for the one-output native-resolution rule,
/// and the Wayland overlay child for its per-frame pre-scaling: both shrink a
/// region of an image composited at the desktop's largest scale down to the
/// output's own resolution. Averaging straight alpha would bleed neighbouring
/// colours through semi-transparent pixels, so each source pixel is first
/// premultiplied (`rgb × a/255`), the destination pixel accumulates the exact
/// overlap weights of its source-area box, and the result is un-premultiplied
/// again.
///
/// Alpha is rounded to the nearest byte, keeping opaque regions opaque.
pub(crate) fn downscale_rgba(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Vec<u8> {
    let mut dst = vec![0u8; dst_w as usize * dst_h as usize * 4];
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return dst;
    }
    let sw = src_w as usize;
    let sh = src_h as usize;
    let dw = dst_w as usize;
    let dh = dst_h as usize;
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() < sw * sh * 4 {
        return dst;
    }
    let stride = sw * 4;

    for dy in 0..dh {
        let y_lo = dy as f64 * sh as f64 / dh as f64;
        let y_hi = (dy as f64 + 1.0) * sh as f64 / dh as f64;
        for dx in 0..dw {
            let x_lo = dx as f64 * sw as f64 / dw as f64;
            let x_hi = (dx as f64 + 1.0) * sw as f64 / dw as f64;

            let mut red = 0.0f64;
            let mut green = 0.0f64;
            let mut blue = 0.0f64;
            let mut alpha = 0.0f64;
            let mut weight_sum = 0.0f64;
            for sy in (y_lo.floor() as usize)..((y_hi.ceil() as usize).min(sh)) {
                let wy = ((sy + 1) as f64).min(y_hi) - (sy as f64).max(y_lo);
                for sx in (x_lo.floor() as usize)..((x_hi.ceil() as usize).min(sw)) {
                    let wx = ((sx + 1) as f64).min(x_hi) - (sx as f64).max(x_lo);
                    let w = wx * wy;
                    let i = sy * stride + sx * 4;
                    let pa = src[i + 3] as f64 / 255.0;
                    red += src[i] as f64 * pa * w;
                    green += src[i + 1] as f64 * pa * w;
                    blue += src[i + 2] as f64 * pa * w;
                    alpha += src[i + 3] as f64 * w;
                    weight_sum += w;
                }
            }

            // The weights above sum to the destination pixel's source-area box,
            // so dividing by `weight_sum` is the box average again.
            let o = (dy * dw + dx) * 4;
            if weight_sum <= 0.0 {
                continue; // empty box: stays transparent
            }
            let a = alpha / weight_sum;
            if a <= 0.0 {
                continue; // fully transparent: rgb is meaningless, stays 0
            }
            // Un-premultiply: divide the premultiplied averages by the average
            // alpha, back to straight alpha.
            let unpremultiply = 255.0 / a;
            dst[o] = clamp_byte(red / weight_sum * unpremultiply);
            dst[o + 1] = clamp_byte(green / weight_sum * unpremultiply);
            dst[o + 2] = clamp_byte(blue / weight_sum * unpremultiply);
            dst[o + 3] = clamp_byte(a);
        }
    }
    dst
}

/// Rounds a channel value and keeps it a byte's worth of 0..=255.
fn clamp_byte(value: f64) -> u8 {
    let value = value.round();
    if value <= 0.0 {
        0
    } else if value >= 255.0 {
        255
    } else {
        value as u8
    }
}

/// Cursor visibility and the HDR highlight policy for one capture.
///
/// `sdr_white_override_nits` is SPEC section 10's "SDR white level override
/// (nits; empty = automatic)": when set, it replaces whatever the OS reports
/// for every monitor, which is the escape hatch for hardware whose white level
/// is missing or wrong (SPEC section 13).
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize)]
pub struct CaptureOptions {
    pub include_cursor: bool,
    pub hdr_highlights: HdrHighlights,
    pub sdr_white_override_nits: Option<f32>,
}

/// What to do with values above the SDR white level on an HDR monitor
/// (SPEC section 5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HdrHighlights {
    /// Clip everything above SDR white to white. The default.
    #[default]
    Clip,
    /// Compress highlights instead. P2, not implemented yet.
    RollOff,
}

/// How global hotkeys can work on this platform (LINUX_SUPPORT.md section 3,
/// item 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeySupport {
    /// The app registers the keys itself: Windows today, X11 in L1.
    Native,
    /// Through the desktop's GlobalShortcuts portal: Wayland, L3.
    Portal,
    /// No global hotkeys until the backend is built.
    None,
}

/// What a [`Platform`] backend can really do on this machine.
///
/// The app asks for this instead of guessing per operating system: a capture
/// mode or a Settings control whose capability is missing is **disabled with a
/// reason** in its hover text (SPEC 8.3), and the controller refuses a capture
/// it cannot run before it hides the window (LINUX_SUPPORT.md section 3,
/// item 2). Windows reports everything it has today; the Linux stub reports
/// nothing until the backends fill it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Silent capture of every monitor: no picker, no permission prompt per
    /// frame.
    pub silent_capture: bool,
    /// Its own overlay windows for region selection, at exact monitor
    /// rectangles.
    pub overlay: bool,
    /// Reading the foreground window's rectangle.
    pub active_window_rect: bool,
    /// Reading the cursor position.
    pub cursor_position: bool,
    /// The capture backend can include cursor pixels.
    pub capture_cursor: bool,
    /// KWin can capture compositor-selected targets without exposing rectangles.
    pub compositor_targets: bool,
    /// Hiding the main window and showing it again from the app.
    pub hide_main_window: bool,
    /// How global hotkeys work here.
    pub global_hotkeys: HotkeySupport,
    /// A tray icon can be installed (LINUX_SUPPORT.md section 1: none on
    /// Linux for now).
    pub tray: bool,
    /// Why a disabled capability is disabled, for the UI's hover text (SPEC
    /// 8.3). `None` when everything reported true is genuinely true — the
    /// shape Windows reports — or when the reason is simply "no backend".
    pub capture_reason: Option<&'static str>,
}

impl Capabilities {
    /// Everything a native desktop backend has today: what
    /// [`crate::WindowsPlatform`] reports.
    pub const FULL: Self = Self {
        silent_capture: true,
        overlay: true,
        active_window_rect: true,
        cursor_position: true,
        capture_cursor: true,
        compositor_targets: false,
        hide_main_window: true,
        global_hotkeys: HotkeySupport::Native,
        tray: true,
        capture_reason: None,
    };

    /// Nothing: what the Linux stub reports until the backends fill it in.
    pub const NONE: Self = Self {
        silent_capture: false,
        overlay: false,
        active_window_rect: false,
        cursor_position: false,
        capture_cursor: false,
        compositor_targets: false,
        hide_main_window: false,
        global_hotkeys: HotkeySupport::None,
        tray: false,
        capture_reason: None,
    };
}

/// Facts about this platform and session that a bug report needs
/// (LINUX_SUPPORT.md sections 4 and 10).
///
/// Deliberately plain data: it is logged at startup, written into
/// `--debug-capture`'s `capture.json`, and compared in tests.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PlatformEnvironment {
    /// The backend that answers the [`Platform`] calls: `windows`, `x11`, or
    /// `stub` when the session has no usable backend yet.
    pub backend: &'static str,
    /// The display server this session runs on: `x11`, `wayland`, or the
    /// session's own word when it named one. `None` where the platform has no
    /// such thing (Windows).
    pub session_type: Option<String>,
    /// The desktop's name, e.g. `XFCE` or `KDE`. `None` where the platform
    /// does not name one (Windows).
    pub desktop: Option<String>,
}

impl PlatformEnvironment {
    /// What the Windows backend reports.
    pub const WINDOWS: Self = Self {
        backend: "windows",
        session_type: None,
        desktop: None,
    };
}

/// Capture selection resolved by the backend, in its own coordinate system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTarget {
    AllMonitors,
    CurrentMonitor,
    ActiveWindow,
}

/// The one way the rest of the program reaches the operating system.
///
/// Every capture mode is assembled from the five capture calls (SPEC section
/// 5.1), plus [`Platform::capabilities`] which says which of them this machine
/// can actually run.
pub trait Platform: Send + Sync {
    /// What this backend can do on this machine (LINUX_SUPPORT.md section 3,
    /// item 2).
    ///
    /// Cheap and side-effect free: the UI asks for it when it builds its menus
    /// and the controller asks again before every capture.
    fn capabilities(&self) -> Capabilities;

    /// Which backend is running, on which session, in which desktop
    /// (LINUX_SUPPORT.md sections 4 and 10).
    ///
    /// Cheap and side-effect free. Linux bug reports are diagnosed from the
    /// session type, the desktop and the backend, so `--debug-capture` writes
    /// this into `capture.json` and the app logs it at startup.
    fn environment(&self) -> PlatformEnvironment;

    /// Fresh monitor list. Re-enumerated on every capture: monitors move, dock
    /// and switch HDR on and off.
    fn monitors(&self) -> crate::Result<Vec<MonitorInfo>>;

    /// One frame from every monitor, converted to 8-bit sRGB.
    fn capture_all(&self, opts: &CaptureOptions) -> crate::Result<VirtualCapture>;

    /// Native backends retain their cursor/foreground selection after capture.
    /// Compositors may instead return a frame already restricted to the target.
    fn capture_target(
        &self,
        target: CaptureTarget,
        opts: &CaptureOptions,
    ) -> crate::Result<(VirtualCapture, RectI)> {
        capture_native_target(self, target, opts)
    }

    /// Blocking region selection over a frozen capture. `None` means cancelled.
    fn select_region(&self, capture: &VirtualCapture) -> crate::Result<Option<RectI>>;

    /// Selection plus one-use activation permission from the same overlay gesture.
    fn select_region_activated(&self, capture: &VirtualCapture) -> crate::Result<RegionSelection> {
        self.select_region(capture).map(|region| RegionSelection {
            region,
            activation_token: None,
        })
    }

    /// The rectangle of the foreground window, if there is one.
    fn foreground_window_rect(&self) -> crate::Result<Option<RectI>>;

    /// Cursor position in virtual-desktop physical pixels.
    fn cursor_pos(&self) -> crate::Result<(i32, i32)>;
}

/// Shared native crop semantics; the Linux wrapper delegates X11 here.
fn capture_native_target<P: Platform + ?Sized>(
    platform: &P,
    target: CaptureTarget,
    opts: &CaptureOptions,
) -> crate::Result<(VirtualCapture, RectI)> {
    let capture = platform.capture_all(opts)?;
    let bounds = capture.bounds();
    let region = match target {
        CaptureTarget::AllMonitors => bounds,
        CaptureTarget::CurrentMonitor => {
            let cursor = platform.cursor_pos().ok();
            capture
                .frames
                .iter()
                .find(|f| cursor.is_some_and(|(x, y)| f.info.rect.contains(x, y)))
                .or_else(|| capture.frames.iter().find(|f| f.info.is_primary))
                .or_else(|| capture.frames.first())
                .ok_or_else(|| Error::Capture {
                    message: "The desktop reported no monitors.".into(),
                })?
                .info
                .rect
        }
        CaptureTarget::ActiveWindow => platform
            .foreground_window_rect()
            .ok()
            .flatten()
            .ok_or_else(|| Error::Capture {
                message: "No foreground window to capture.".into(),
            })?
            .intersection(bounds),
    };
    Ok((capture, region))
}

/// Errors returned by [`Platform`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An operating-system call (Win32 or WinRT) failed.
    #[error("operating system call failed: {message}")]
    Os { message: String },

    /// A `windows`-crate call failed, carried through for its source chain.
    ///
    /// The `::` is deliberate: this crate's own `windows` module shadows the
    /// crate name in type position.
    #[cfg(windows)]
    #[error("Windows API call failed: {0}")]
    Windows(#[from] ::windows::core::Error),

    /// A conversion in `snp_ligase_core` failed: a frame buffer that did not
    /// hold its promised pixels, or a highlight mode that is not built yet.
    #[error("pixel conversion failed: {0}")]
    Color(#[from] snp_ligase_core::Error),

    /// A capture failed, with the reason from the capture API or the fallback.
    #[error("capture failed: {message}")]
    Capture { message: String },

    /// What was asked for is not implemented on this operating system yet.
    ///
    /// The Linux stub returns this from every call until the X11 backend (L1)
    /// fills it in (LINUX_SUPPORT.md section 3).
    #[error("not supported on this platform yet: {message}")]
    Unsupported { message: String },

    /// The instance channel failed (LINUX_SUPPORT.md section 3, item 9).
    ///
    /// "Nobody is listening" is not this: it is `Ok(false)` from
    /// [`instance::send`], which means the caller starts a window instead.
    #[error("instance channel failed: {message}")]
    Instance { message: String },

    /// A thread could not be started for a per-monitor capture.
    #[error("could not start the capture thread: {0}")]
    Thread(#[from] std::io::Error),
}

/// Result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// A solid-colour monitor at `rect`, so stitching can be checked by colour.
    fn frame(rect: RectI, id: &str, fill: Rgba<u8>) -> MonitorFrame {
        frame_at_scale(rect, id, fill, 1.0, 1.0)
    }

    /// Like [`frame`], but with the frame's own native scale, so mixed-scale
    /// captures (the VM's Wayland portal layout) can be tested.
    fn frame_at_scale(
        rect: RectI,
        id: &str,
        fill: Rgba<u8>,
        native_scale_x: f64,
        native_scale_y: f64,
    ) -> MonitorFrame {
        let image = RgbaImage::from_pixel(rect.w.max(0) as u32, rect.h.max(0) as u32, fill);
        MonitorFrame {
            info: MonitorInfo {
                id: id.to_owned(),
                rect,
                scale_factor: 1.0,
                hdr_active: false,
                sdr_white_nits: None,
                is_primary: false,
            },
            image,
            diagnostics: diagnostics(),
            stored_pixels: crate::StoredPixels::Virtual,
            native_scale_x,
            native_scale_y,
        }
    }

    fn diagnostics() -> FrameDiagnostics {
        FrameDiagnostics {
            api_used: CaptureApi::Wgc,
            pixel_format: "Bgra8".to_owned(),
            width: 0,
            height: 0,
            row_pitch: 0,
            max_channel_value: 0.0,
            sdr_white_nits: 200.0,
            elapsed_ms: 0.0,
        }
    }

    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const BLUE: Rgba<u8> = Rgba([0, 0, 255, 255]);

    fn pixel(capture: &RgbaImage, x: u32, y: u32) -> Rgba<u8> {
        *capture.get_pixel(x, y)
    }

    /// The VM's real Wayland-portal layout (Spike S2, LINUX_SUPPORT.md
    /// section 6): a 3627×1080-logical desktop of a 0%-offset 100% output and
    /// a 1920-logical-offset 150% output, captured by the portal into
    /// 5441×1620 at S = 1.5, whose quirks — V2 arriving ⅔-of-S pixels, the
    /// rounding column at x = 5440 — the native-resolution rule lives to fix.
    ///
    /// 1920×1080 @100% at logical (0,0); 2560×1440 @150% at logical (1920,0).
    fn vm_capture() -> VirtualCapture {
        VirtualCapture {
            frames: vec![
                // V2, the 100% output: the portal upscaled it 1.5× to match
                // the 150% output, so its frame pixels are ⅔ of an S pixel.
                frame_at_scale(
                    RectI {
                        x: 0,
                        y: 0,
                        w: 2880,
                        h: 1620,
                    },
                    "Virtual-1",
                    RED,
                    2.0 / 3.0,
                    2.0 / 3.0,
                ),
                // V1, the 150% output: the portal composed it at its own
                // scale, so its frame pixels are already S pixels.
                frame_at_scale(
                    RectI {
                        x: 2880,
                        y: 0,
                        w: 2560,
                        h: 1440,
                    },
                    "Virtual-2",
                    BLUE,
                    1.0,
                    1.0,
                ),
            ],
        }
    }

    #[test]
    fn spanning_native_frames_sample_in_virtual_space_and_keep_gaps() {
        let mut left = frame(RectI::new(-6, 0, 6, 6), "left", Rgba([0, 0, 0, 255]));
        left.diagnostics.api_used = CaptureApi::Kwin;
        left.stored_pixels = StoredPixels::Native;
        left.image = RgbaImage::from_fn(4, 4, |x, y| Rgba([x as u8, y as u8, 0, 255]));
        let right = frame(RectI::new(0, 0, 4, 4), "right", Rgba([255, 0, 0, 255]));
        let image = VirtualCapture {
            frames: vec![left, right],
        }
        .crop(RectI::new(-3, 0, 5, 6));
        assert_eq!(image.dimensions(), (5, 6));
        assert_eq!(image.get_pixel(0, 0).0, [2, 0, 0, 255]);
        assert_eq!(image.get_pixel(2, 5).0, [3, 3, 0, 255]);
        assert_eq!(image.get_pixel(3, 0).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(3, 5).0, [0, 0, 0, 0]);
    }

    #[test]
    fn native_frame_crop_maps_virtual_edges_without_resampling() {
        let mut frame = frame(RectI::new(0, 0, 6, 6), "native", Rgba([0, 0, 0, 255]));
        frame.diagnostics.api_used = CaptureApi::Kwin;
        frame.stored_pixels = StoredPixels::Native;
        frame.image = RgbaImage::from_fn(4, 4, |x, y| image::Rgba([x as u8, y as u8, 0, 255]));
        let capture = VirtualCapture {
            frames: vec![frame],
        };
        let image = capture.crop(RectI::new(0, 0, 3, 3));
        assert_eq!(image.dimensions(), (2, 2));
        assert_eq!(image.get_pixel(1, 1).0, [1, 1, 0, 255]);
        assert_eq!(capture.crop(RectI::new(1, 1, -1, 3)).dimensions(), (0, 3));
        let offset = capture.crop(RectI::new(1, 1, 3, 3));
        assert_eq!(offset.dimensions(), (2, 2));
        assert_eq!(offset.get_pixel(0, 0).0, [1, 1, 0, 255]);
    }

    #[test]
    fn crop_storage_property_is_independent_of_diagnostics() {
        for (storage, api, scale, stored_size, expected_size) in [
            (StoredPixels::Native, CaptureApi::Portal, 1.0, 4, 2),
            (StoredPixels::Virtual, CaptureApi::Kwin, 2.0 / 3.0, 6, 2),
            (StoredPixels::Virtual, CaptureApi::Kwin, 1.0, 6, 3),
        ] {
            let mut frame = frame_at_scale(RectI::new(0, 0, 6, 6), "test", RED, scale, scale);
            frame.stored_pixels = storage;
            frame.diagnostics.api_used = api;
            frame.image = RgbaImage::from_pixel(stored_size, stored_size, RED);
            let image = VirtualCapture {
                frames: vec![frame],
            }
            .crop(RectI::new(0, 0, 3, 3));
            assert_eq!(image.dimensions(), (expected_size, expected_size));
            assert!(image.pixels().all(|pixel| *pixel == RED));
        }
    }

    #[test]
    fn spanning_crop_uses_storage_property_without_diagnostic_api() {
        let mut native = frame(RectI::new(0, 0, 6, 6), "native", RED);
        native.stored_pixels = StoredPixels::Native;
        native.image = RgbaImage::from_fn(4, 4, |x, y| Rgba([x as u8, y as u8, 0, 255]));
        let mut unit = frame(RectI::new(6, 0, 3, 6), "unit", BLUE);
        unit.diagnostics.api_used = CaptureApi::Kwin;
        let image = VirtualCapture {
            frames: vec![native, unit],
        }
        .crop(RectI::new(3, 0, 6, 6));
        assert_eq!(image.dimensions(), (6, 6));
        assert_eq!(pixel(&image, 2, 5).0, [3, 3, 0, 255]);
        assert_eq!(pixel(&image, 5, 5), BLUE);
    }

    #[test]
    fn frame_owns_requires_full_rect_and_full_pixels() {
        // Strictly inside V2: owned.
        assert!(frame_owns(
            &vm_capture().frames[0],
            RectI {
                x: 100,
                y: 100,
                w: 1920,
                h: 1080
            }
        ));
        // Touching V2's right edge and V1's left: two candidates, neither owns.
        assert!(!frame_owns(
            &vm_capture().frames[0],
            RectI {
                x: 2800,
                y: 0,
                w: 200,
                h: 100
            }
        ));
        // A frame smaller than its rectangle degrades instead of owning.
        let mut mismatched = frame(
            RectI {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            },
            "mismatched",
            RED,
        );
        mismatched.image = RgbaImage::new(5, 5);
        assert!(!frame_owns(
            &mismatched,
            RectI {
                x: 0,
                y: 0,
                w: 10,
                h: 10
            }
        ));
    }

    #[test]
    fn crop_of_one_vm_output_comes_out_at_native_resolution() {
        let capture = vm_capture();

        // A 1920×1080 selection on the 100% output comes out 1920×⅔ = 1280
        // wide — the monitor's own pixels, not the upscaled portal ones.
        let cropped = capture.crop(RectI {
            x: 100,
            y: 100,
            w: 1920,
            h: 1080,
        });
        assert_eq!(
            (cropped.width(), cropped.height()),
            (1280, 720),
            "the \
			outgoing selection keeps the 100% monitor's own resolution"
        );
        assert_eq!(pixel(&cropped, 0, 0), RED);
        assert_eq!(pixel(&cropped, 1279, 719), RED);

        // The 150% output's cut is 1:1: its frame pixels already are S pixels.
        let cropped = capture.crop(RectI {
            x: 2890,
            y: 10,
            w: 256,
            h: 144,
        });
        assert_eq!((cropped.width(), cropped.height()), (256, 144));
        assert_eq!(pixel(&cropped, 255, 143), BLUE);
    }

    #[test]
    fn crop_of_a_spanning_selection_stays_in_virtual_space() {
        let capture = vm_capture();

        // A cross-output selection has no single owner: the old stitch, in S
        // pixels.
        let cropped = capture.crop(RectI {
            x: 2700,
            y: 0,
            w: 400,
            h: 100,
        });
        assert_eq!((cropped.width(), cropped.height()), (400, 100));
        assert_eq!(pixel(&cropped, 170, 0), RED, "left half still V2");
        assert_eq!(pixel(&cropped, 180, 0), BLUE, "right half already V1");

        // The portal's rounding column at x = 5440 belongs to no frame: it
        // stays the transparent gap it was.
        let cropped = capture.crop(RectI {
            x: 5439,
            y: 0,
            w: 2,
            h: 1,
        });
        assert_eq!((cropped.width(), cropped.height()), (2, 1));
        assert_eq!(pixel(&cropped, 0, 0), BLUE);
        assert_eq!(pixel(&cropped, 1, 0), Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn native_size_rounds_to_the_selections_own_pixels() {
        assert_eq!(native_size(1920, 2.0 / 3.0, 2880), 1280);
        assert_eq!(native_size(1080, 2.0 / 3.0, 1620), 720);
        // Murky portals rounding differently stay inside the frame they read.
        assert_eq!(native_size(1000, 1.0004, 999), 999);
        assert_eq!(native_size(100, 1.0, 50), 50);
        assert_eq!(native_size(0, 2.0 / 3.0, 10), 0);
    }

    #[test]
    fn downscale_premultiplies_so_neighbours_do_not_bleed() {
        // Left half: opaque red. Right half: fully transparent. A straight
        // average would turn the border pixel blackish red; premultiplied, its
        // colour keeps exactly the red of the pixels it actually covers.
        let src = vec![
            255, 0, 0, 255, 0, 0, 0, 0, // one row: red, transparent
            255, 0, 0, 255, 0, 0, 0, 0,
        ];
        let dst = downscale_rgba(&src, 2, 2, 2, 2);
        assert_eq!(&dst, &src[..], "a 1:1 request is the original");

        // Halved: every column covers half red, half transparent.
        let dst = downscale_rgba(&src, 2, 2, 1, 2);
        assert_eq!(dst[0], 255, "red stays full red through premultiply");
        assert_eq!(dst[1], 0);
        assert_eq!(dst[2], 0);
        assert_eq!(dst[3], 128, "alpha halves to 127.5, rounds to 128");

        // Fully transparent pixels stay fully transparent.
        let zero = [0u8; 16];
        assert_eq!(downscale_rgba(&zero, 1, 1, 1, 1), [0, 0, 0, 0]);
    }

    #[test]
    fn downscale_keeps_opaque_regions_opaque() {
        // Mixing alphas 255 and 254: the average is 254.5, which must not
        // round a 'fully opaque' region down to a 254 fringe.
        let src = vec![10, 20, 30, 255u8, 40, 50, 60, 254];
        let dst = downscale_rgba(&src, 2, 1, 1, 1);
        assert_eq!(dst[3], 255, "the 254.5 average is forced opaque");
        assert_eq!(dst[0], 25, "colour is the straight-alpha average");

        // The exact colours survive a ⅔ shrink of a solid block.
        let mut src = vec![0u8; 2880 * 1620 * 4];
        src.chunks_mut(4).for_each(|px| {
            px.copy_from_slice(&[80, 120, 160, 255]);
        });
        let dst = downscale_rgba(&src, 2880, 1620, 1920, 1080);
        assert_eq!(dst[3], 255);
        assert_eq!((dst[0], dst[1], dst[2]), (80, 120, 160));
    }

    #[test]
    fn unit_scale_frames_keep_the_existing_overlap_paint_order() {
        // Windows/X11 use unit scale. In a mirrored/overlapping layout the
        // later frame has always painted over the earlier one.
        let rect = RectI::new(0, 0, 4, 4);
        let capture = VirtualCapture {
            frames: vec![frame(rect, "first", RED), frame(rect, "last", BLUE)],
        };
        let crop = capture.crop(RectI::new(1, 1, 2, 2));
        assert!(crop.pixels().all(|pixel| *pixel == BLUE));
    }

    #[test]
    fn crop_of_one_monitor_keeps_the_right_pixels() {
        let capture = VirtualCapture {
            frames: vec![frame(
                RectI {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 4,
                },
                "\\\\.\\DISPLAY1",
                RED,
            )],
        };

        let cropped = capture.crop(RectI {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
        });

        assert_eq!((cropped.width(), cropped.height()), (2, 2));
        assert_eq!(pixel(&cropped, 0, 0), RED);
        assert_eq!(pixel(&cropped, 1, 1), RED);
    }

    #[test]
    fn crop_stitches_monitors_with_negative_coordinates() {
        let capture = VirtualCapture {
            frames: vec![
                frame(
                    RectI {
                        x: 0,
                        y: 0,
                        w: 4,
                        h: 4,
                    },
                    "\\\\.\\DISPLAY2",
                    RED,
                ),
                frame(
                    RectI {
                        x: -4,
                        y: 0,
                        w: 4,
                        h: 4,
                    },
                    "\\\\.\\DISPLAY1",
                    BLUE,
                ),
            ],
        };

        // (-2,0) to (4,2): two columns from the left monitor, four from the right.
        let cropped = capture.crop(RectI {
            x: -2,
            y: 0,
            w: 6,
            h: 2,
        });

        assert_eq!((cropped.width(), cropped.height()), (6, 2));
        assert_eq!(pixel(&cropped, 0, 0), BLUE);
        assert_eq!(pixel(&cropped, 1, 1), BLUE);
        assert_eq!(pixel(&cropped, 2, 0), RED);
        assert_eq!(pixel(&cropped, 5, 1), RED);
    }

    #[test]
    fn gaps_between_monitors_stay_transparent() {
        let capture = VirtualCapture {
            frames: vec![
                frame(
                    RectI {
                        x: 0,
                        y: 0,
                        w: 4,
                        h: 4,
                    },
                    "\\\\.\\DISPLAY1",
                    RED,
                ),
                frame(
                    RectI {
                        x: 10,
                        y: 0,
                        w: 4,
                        h: 4,
                    },
                    "\\\\.\\DISPLAY2",
                    BLUE,
                ),
            ],
        };

        let cropped = capture.crop(RectI {
            x: 0,
            y: 0,
            w: 14,
            h: 4,
        });

        assert_eq!(cropped.width(), 14);
        assert_eq!(pixel(&cropped, 0, 0), RED);
        assert_eq!(pixel(&cropped, 13, 0), BLUE);
        // columns 4..10 belong to no monitor
        assert_eq!(pixel(&cropped, 5, 2), Rgba([0, 0, 0, 0]));
        assert_eq!(pixel(&cropped, 9, 0), Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn a_crop_outside_every_monitor_is_empty_but_the_right_size() {
        let capture = VirtualCapture {
            frames: vec![frame(
                RectI {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 4,
                },
                "\\\\.\\DISPLAY1",
                RED,
            )],
        };

        let cropped = capture.crop(RectI {
            x: 100,
            y: 100,
            w: 3,
            h: 3,
        });

        assert_eq!((cropped.width(), cropped.height()), (3, 3));
        assert_eq!(pixel(&cropped, 1, 1), Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn a_zero_sized_crop_is_empty() {
        let capture = VirtualCapture::default();

        let both = capture.crop(RectI {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        });
        assert_eq!((both.width(), both.height()), (0, 0));

        // A crop that collapsed on one axis is still a valid, empty image.
        let thin = capture.crop(RectI {
            x: 5,
            y: 5,
            w: 0,
            h: 9,
        });
        assert_eq!((thin.width(), thin.height()), (0, 9));
    }

    #[test]
    fn a_frame_smaller_than_its_rectangle_is_used_as_is() {
        // What a mismatched capture produces (SPEC 5.4): use the frame as-is,
        // positioned at the rect origin, and never panic.
        let mut small = frame(
            RectI {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            },
            "\\\\.\\DISPLAY1",
            RED,
        );
        small.info.rect = RectI {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        };
        let capture = VirtualCapture {
            frames: vec![small],
        };

        let cropped = capture.crop(RectI {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        });

        assert_eq!(pixel(&cropped, 0, 0), RED);
        assert_eq!(pixel(&cropped, 3, 3), Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn bounds_covers_every_monitor_including_negative_origins() {
        let capture = VirtualCapture {
            frames: vec![
                frame(
                    RectI {
                        x: -200,
                        y: 0,
                        w: 100,
                        h: 50,
                    },
                    "\\\\.\\DISPLAY1",
                    RED,
                ),
                frame(
                    RectI {
                        x: 0,
                        y: 100,
                        w: 300,
                        h: 200,
                    },
                    "\\\\.\\DISPLAY2",
                    BLUE,
                ),
            ],
        };

        assert_eq!(
            capture.bounds(),
            RectI {
                x: -200,
                y: 0,
                w: 500,
                h: 300
            }
        );
    }

    #[test]
    fn bounds_keeps_the_right_edge_of_every_monitor_not_just_the_last() {
        // The M5 bug: with monitors side by side, the width came from whichever
        // frame was visited last, so the right monitor's edge was lost when the
        // left one was visited afterwards.
        let capture = VirtualCapture {
            frames: vec![
                frame(
                    RectI {
                        x: 0,
                        y: 0,
                        w: 1920,
                        h: 1080,
                    },
                    "\\\\.\\DISPLAY2",
                    RED,
                ),
                frame(
                    RectI {
                        x: -2560,
                        y: 0,
                        w: 2560,
                        h: 1440,
                    },
                    "\\\\.\\DISPLAY1",
                    BLUE,
                ),
            ],
        };
        assert_eq!(
            capture.bounds(),
            RectI {
                x: -2560,
                y: 0,
                w: 4480,
                h: 1440
            }
        );

        // The same for monitors stacked vertically, and for a monitor after
        // the one with the larger bottom edge.
        let stacked = VirtualCapture {
            frames: vec![
                frame(
                    RectI {
                        x: 0,
                        y: 1080,
                        w: 1920,
                        h: 1080,
                    },
                    "\\\\.\\DISPLAY2",
                    RED,
                ),
                frame(
                    RectI {
                        x: 0,
                        y: 0,
                        w: 1920,
                        h: 1080,
                    },
                    "\\\\.\\DISPLAY1",
                    BLUE,
                ),
            ],
        };
        assert_eq!(
            stacked.bounds(),
            RectI {
                x: 0,
                y: 0,
                w: 1920,
                h: 2160
            }
        );
    }

    #[test]
    fn a_point_on_the_shared_edge_belongs_to_the_left_rectangle() {
        let rect = RectI {
            x: 10,
            y: 20,
            w: 100,
            h: 50,
        };
        assert!(rect.contains(10, 20), "the top-left corner is inside");
        assert!(rect.contains(109, 69), "the last pixel is inside");
        assert!(!rect.contains(110, 20), "the right edge is outside");
        assert!(!rect.contains(10, 70), "the bottom edge is outside");
        assert!(!rect.contains(9, 20), "the left edge is outside");
    }

    #[test]
    fn a_selection_spanning_two_monitors_is_one_intersection_per_monitor() {
        // Two monitors side by side, the left one with a negative origin.
        let left = RectI {
            x: -1920,
            y: 0,
            w: 1920,
            h: 1080,
        };
        let right = RectI {
            x: 0,
            y: 0,
            w: 2560,
            h: 1440,
        };
        // A selection from the middle of the left monitor to the middle of the
        // right one.
        let selection = RectI {
            x: -500,
            y: 100,
            w: 1000,
            h: 400,
        };

        // Window-local on the left monitor: shifted by +1920 and clipped there.
        let on_left = selection.intersection(left).to_local(left);
        assert_eq!(
            on_left,
            RectI {
                x: 1420,
                y: 100,
                w: 500,
                h: 400
            }
        );
        let on_right = selection.intersection(right).to_local(right);
        assert_eq!(
            on_right,
            RectI {
                x: 0,
                y: 100,
                w: 500,
                h: 400
            }
        );
        // Together they are the whole selection.
        assert_eq!(on_left.w + on_right.w, selection.w);
    }

    #[test]
    fn a_selection_that_misses_a_monitor_has_no_local_rectangle() {
        let monitor = RectI {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        };
        let elsewhere = RectI {
            x: 200,
            y: 200,
            w: 50,
            h: 50,
        };
        assert!(elsewhere.intersection(monitor).is_empty());
        assert!(elsewhere.intersection(monitor).to_local(monitor).is_empty());
    }

    #[test]
    fn the_last_region_is_only_reused_while_it_still_exists() {
        // The bounds of a two-monitor desktop, one with a negative origin.
        let bounds = RectI {
            x: -1920,
            y: 0,
            w: 4480,
            h: 1440,
        };
        assert!(bounds.contains_rect(RectI {
            x: -1920,
            y: 0,
            w: 100,
            h: 100
        }));
        assert!(bounds.contains_rect(RectI {
            x: 2000,
            y: 1300,
            w: 100,
            h: 140
        }));
        assert!(
            !bounds.contains_rect(RectI {
                x: 2559,
                y: 0,
                w: 100,
                h: 100
            }),
            "a region past the right edge was accepted"
        );
        assert!(
            !bounds.contains_rect(RectI {
                x: 0,
                y: 1400,
                w: 100,
                h: 100
            }),
            "a region past the bottom edge was accepted"
        );
        assert!(
            !bounds.contains_rect(RectI {
                x: -2000,
                y: 0,
                w: 100,
                h: 100
            }),
            "a region past the left edge was accepted"
        );
        assert!(
            !bounds.contains_rect(RectI {
                x: 4400,
                y: 0,
                w: 200,
                h: 100
            }),
            "a region half off the desktop was accepted"
        );
    }

    #[test]
    fn a_negative_size_is_clamped_when_a_rectangle_is_built() {
        let rect = RectI::new(10, 10, -5, 20);
        assert_eq!(rect.w, 0);
        assert!(rect.is_empty());
        assert_eq!(rect.translated(1, 1).x, 11);
    }
}

/// One overlay result. Tokens must never survive a cancelled or dropped request.
#[derive(Debug, Default)]
pub struct RegionSelection {
    pub region: Option<RectI>,
    pub activation_token: Option<String>,
}
