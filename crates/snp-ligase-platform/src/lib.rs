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

pub mod windows;

/// The Windows implementation of [`Platform`], re-exported so the app does not
/// have to know which module it lives in.
pub use windows::WindowsPlatform;

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
#[serde(rename_all = "UPPERCASE")]
pub enum CaptureApi {
    /// Windows.Graphics.Capture.
    Wgc,
    /// GDI `BitBlt` fallback.
    Gdi,
}

impl CaptureApi {
    /// Name used in logs and in `capture.json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wgc => "WGC",
            Self::Gdi => "GDI",
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

/// One captured monitor: what it showed, plus how it was captured.
#[derive(Debug, Clone)]
pub struct MonitorFrame {
    pub info: MonitorInfo,
    /// 8-bit straight sRGB, alpha forced to 255.
    pub image: RgbaImage,
    pub diagnostics: FrameDiagnostics,
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
    #[must_use]
    pub fn crop(&self, r: RectI) -> RgbaImage {
        let width = u32::try_from(r.w.max(0)).unwrap_or(0);
        let height = u32::try_from(r.h.max(0)).unwrap_or(0);
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
                    let Some(pixel) = frame.image.get_pixel_checked(src_x + column, src_y + row)
                    else {
                        continue; // frame smaller than its rectangle
                    };
                    out.put_pixel(dst_x + column, dst_y + row, *pixel);
                }
            }
        }

        out
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

/// The one way the rest of the program reaches the operating system.
///
/// Every capture mode is assembled from these five calls (SPEC section 5.1).
pub trait Platform: Send + Sync {
    /// Fresh monitor list. Re-enumerated on every capture: monitors move, dock
    /// and switch HDR on and off.
    fn monitors(&self) -> crate::Result<Vec<MonitorInfo>>;

    /// One frame from every monitor, converted to 8-bit sRGB.
    fn capture_all(&self, opts: &CaptureOptions) -> crate::Result<VirtualCapture>;

    /// Blocking region selection over a frozen capture. `None` means cancelled.
    fn select_region(&self, capture: &VirtualCapture) -> crate::Result<Option<RectI>>;

    /// The rectangle of the foreground window, if there is one.
    fn foreground_window_rect(&self) -> crate::Result<Option<RectI>>;

    /// Cursor position in virtual-desktop physical pixels.
    fn cursor_pos(&self) -> crate::Result<(i32, i32)>;
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
    #[error("Windows API call failed: {0}")]
    Windows(#[from] ::windows::core::Error),

    /// A conversion in `snp_ligase_core` failed: a frame buffer that did not
    /// hold its promised pixels, or a highlight mode that is not built yet.
    #[error("pixel conversion failed: {0}")]
    Color(#[from] snp_ligase_core::Error),

    /// A capture failed, with the reason from the capture API or the fallback.
    #[error("capture failed: {message}")]
    Capture { message: String },

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
