//! Capturing the root window, monitor by monitor (LINUX_SUPPORT.md section 4,
//! phase L1a).
//!
//! X11 has one root window that every monitor is a rectangle of, so a monitor's
//! capture is a `GetImage` of its rectangle. The pixels arrive in the server's
//! own format ([`super::pixels`]) and the frames are assembled here: one frame
//! per monitor, each with its rectangle and the diagnostics `capture.json`
//! promises (SPEC section 5.4).
//!
//! MIT-SHM is the fast path and plain `GetImage` the fallback; when the fallback
//! is used the log says so, the way it does for the Windows GDI path. Monitors
//! are read one after another on purpose: one X connection serves them in
//! order, so threads would only add bookkeeping.
//!
//! X11 has no HDR and no pointer in `GetImage`, so `sdr_white_nits` is 0.0 and
//! `include_cursor` is refused with a warning rather than silently accepted.

use std::time::Instant;

use x11rb::protocol::shm;
use x11rb::protocol::xproto;

use super::{X11Connection, monitors, pixels, shm as shm_segment};
use crate::{
    CaptureApi, CaptureOptions, Error, FrameDiagnostics, MonitorFrame, RectI, Result,
    VirtualCapture,
};

/// One frame from every monitor, in the server's order.
pub(crate) fn capture_all(conn: &X11Connection, opts: &CaptureOptions) -> Result<VirtualCapture> {
    if opts.include_cursor {
        // LINUX_SUPPORT.md section 4: `XGetImage` never includes the pointer,
        // and compositing it back in (XQueryPointer plus XFixes) is out of
        // L1a's scope. Saying so beats a capture that quietly differs from the
        // setting.
        tracing::warn!(
            "include_cursor was asked for, but an X11 capture cannot draw the pointer into the \
             frame yet (LINUX_SUPPORT.md section 4)"
        );
    }

    // The root window's own rectangle: every capture has to stay inside it.
    let root = RectI::new(
        0,
        0,
        i32::from(conn.root_width),
        i32::from(conn.root_height),
    );

    let mut regions = Vec::new();
    for mut info in monitors::enumerate(conn)? {
        let clipped = clamp_to_root(info.rect, root);
        if clipped != info.rect {
            tracing::warn!(
                monitor = %info.id,
                rect = ?info.rect,
                clipped = ?clipped,
                "this monitor is not entirely inside the root window; capturing the overlap"
            );
            // The frame's rectangle is where its pixels really are, so the
            // rest of the program can still stitch monitors together.
            info.rect = clipped;
        }
        if clipped.is_empty() {
            tracing::warn!(
                monitor = %info.id,
                "this monitor has no pixels in the root window; leaving it out of the capture"
            );
            continue;
        }
        regions.push((info, clipped));
    }
    if regions.is_empty() {
        tracing::warn!("the display has no capturable monitors");
        return Ok(VirtualCapture::default());
    }

    // One segment for the largest monitor, reused for all of them.
    let largest = largest_region_bytes(&conn.layout, regions.iter().map(|(_, rect)| *rect));
    let segment = match conn.shm_version {
        Some(version) => match shm_segment::Segment::new(&conn.conn, largest) {
            Ok(segment) => {
                tracing::debug!(
                    bytes = largest,
                    version = %format!("{}.{}", version.0, version.1),
                    "capturing through MIT-SHM"
                );
                Some(segment)
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "MIT-SHM is not usable; falling back to plain GetImage, which is much slower"
                );
                None
            }
        },
        None => {
            tracing::warn!(
                "the X server has no MIT-SHM; falling back to plain GetImage, which is much slower"
            );
            None
        }
    };

    let mut frames = Vec::with_capacity(regions.len());
    for (info, rect) in regions {
        let started = Instant::now();
        let (image, api_used, max_channel_value) =
            grab(conn, rect, segment.as_ref()).map_err(|message| Error::Capture {
                message: format!(
                    "could not capture {} ({}x{} at {},{}): {message}",
                    info.id, rect.w, rect.h, rect.x, rect.y
                ),
            })?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

        let diagnostics = FrameDiagnostics {
            api_used,
            pixel_format: conn.layout.name(),
            width: image.width(),
            height: image.height(),
            row_pitch: u32::try_from(conn.layout.pitch(image.width())).unwrap_or(u32::MAX),
            max_channel_value,
            // LINUX_SUPPORT.md section 2: an X11 capture is 8-bit sRGB, so no
            // white level was used for anything.
            sdr_white_nits: 0.0,
            elapsed_ms,
        };

        // One line per frame, like the Windows capture: a wrong capture has to
        // be diagnosable from the log file alone (SPEC section 0, rule 11).
        tracing::info!(
            monitor = %info.id,
            rect = ?info.rect,
            scale = info.scale_factor,
            api = %diagnostics.api_used,
            format = %diagnostics.pixel_format,
            size = %format!("{}x{}", diagnostics.width, diagnostics.height),
            pitch = diagnostics.row_pitch,
            max_channel = diagnostics.max_channel_value,
            elapsed_ms = diagnostics.elapsed_ms,
            "frame captured"
        );

        frames.push(MonitorFrame {
            info,
            image,
            diagnostics,
            // An X11 frame pixel is a virtual pixel.
            stored_pixels: crate::StoredPixels::Virtual,
            native_scale_x: 1.0,
            native_scale_y: 1.0,
        });
    }

    Ok(VirtualCapture { frames })
}

/// One monitor's pixels: MIT-SHM when there is a segment, plain `GetImage` when
/// that failed.
fn grab(
    conn: &X11Connection,
    rect: RectI,
    segment: Option<&shm_segment::Segment>,
) -> std::result::Result<(image::RgbaImage, CaptureApi, f32), String> {
    if let Some(segment) = segment {
        match grab_shm(conn, rect, segment) {
            Ok(frame) => return Ok(frame),
            Err(err) => tracing::warn!(
                rect = ?rect,
                error = %err,
                "the MIT-SHM capture failed; retrying this monitor with plain GetImage"
            ),
        }
    }
    grab_plain(conn, rect)
}

/// `ShmGetImage` into the segment, then the conversion from the server's pixel
/// format.
fn grab_shm(
    conn: &X11Connection,
    rect: RectI,
    segment: &shm_segment::Segment,
) -> std::result::Result<(image::RgbaImage, CaptureApi, f32), String> {
    let (x, y, width, height) = geometry(rect)?;
    let cookie = shm::get_image(
        &conn.conn,
        conn.root,
        x,
        y,
        width,
        height,
        // Every plane of the drawable, which is what a screenshot wants.
        u32::MAX,
        u8::from(xproto::ImageFormat::Z_PIXMAP),
        segment.seg(),
        0,
    )
    .map_err(|err| format!("ShmGetImage could not be sent: {err}"))?;
    let reply = cookie
        .reply()
        .map_err(|err| format!("ShmGetImage failed: {err}"))?;

    let expected = region_bytes(&conn.layout, rect);
    let written = usize::try_from(reply.size).unwrap_or(usize::MAX);
    if written < expected {
        // The server promised fewer bytes than the image needs; the conversion
        // stays inside the segment and the missing pixels stay transparent.
        tracing::warn!(
            rect = ?rect,
            written,
            expected,
            "the shared memory image is smaller than the rectangle"
        );
    }

    let (image, peak) = pixels::to_rgba(
        segment.as_slice(),
        &conn.layout,
        u32::from(width),
        u32::from(height),
    );
    Ok((image, CaptureApi::X11Shm, peak))
}

/// Plain `GetImage`: the pixels travel through the connection in the reply.
fn grab_plain(
    conn: &X11Connection,
    rect: RectI,
) -> std::result::Result<(image::RgbaImage, CaptureApi, f32), String> {
    let (x, y, width, height) = geometry(rect)?;
    let cookie = xproto::get_image(
        &conn.conn,
        xproto::ImageFormat::Z_PIXMAP,
        conn.root,
        x,
        y,
        width,
        height,
        u32::MAX,
    )
    .map_err(|err| format!("GetImage could not be sent: {err}"))?;
    let reply = cookie
        .reply()
        .map_err(|err| format!("GetImage failed: {err}"))?;

    let expected = region_bytes(&conn.layout, rect);
    if reply.data.len() < expected {
        tracing::warn!(
            rect = ?rect,
            got = reply.data.len(),
            expected,
            "the GetImage reply is smaller than the rectangle"
        );
    }

    let (image, peak) = pixels::to_rgba(
        &reply.data,
        &conn.layout,
        u32::from(width),
        u32::from(height),
    );
    Ok((image, CaptureApi::X11Get, peak))
}

/// The clamp: what a capture rectangle has to be for the root window to have
/// pixels for every last one of it.
///
/// A monitor that hangs over the root window's edge (a virtual desktop larger
/// than the framebuffer, an X screen smaller than the desktop) is captured as
/// its overlap, never as a shifted or padded image.
#[must_use]
fn clamp_to_root(rect: RectI, root: RectI) -> RectI {
    rect.intersection(root)
}

/// `rect` in the request's 16-bit fields.
///
/// `GetImage` names its origin with signed 16-bit values and its size with
/// unsigned ones, so a screen past the 32767th pixel (or a rectangle that is
/// not inside the root window to begin with) cannot be addressed by one
/// request at all; that is an error naming the value, not a wrapped
/// coordinate.
fn geometry(rect: RectI) -> std::result::Result<(i16, i16, u16, u16), String> {
    let origin = |what: &str, value: i32| match i16::try_from(value) {
        Ok(value) if value >= 0 => Ok(value),
        _ => Err(format!(
            "the {what} origin {value} is outside the 0..=32767 an X11 capture can address"
        )),
    };
    let size = |what: &str, value: i32| {
        u16::try_from(value).map_err(|_| {
            format!("the {what} {value} is not a size an X11 capture can address (0..=65535)")
        })
    };
    Ok((
        origin("x", rect.x)?,
        origin("y", rect.y)?,
        size("width", rect.w)?,
        size("height", rect.h)?,
    ))
}

/// How many bytes one region's image data takes in the server's format: the
/// pitched rows times the height (SPEC section 5.4's `row_pitch`).
#[must_use]
fn region_bytes(layout: &pixels::PixelLayout, rect: RectI) -> usize {
    let width = u32::try_from(rect.w.max(0)).unwrap_or(0);
    let height = usize::try_from(rect.h.max(0)).unwrap_or(0);
    layout.pitch(width).saturating_mul(height)
}

/// The bytes the segment has to hold: as much as the largest monitor needs.
#[must_use]
fn largest_region_bytes(
    layout: &pixels::PixelLayout,
    rects: impl IntoIterator<Item = RectI>,
) -> usize {
    rects
        .into_iter()
        .map(|rect| region_bytes(layout, rect))
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{ImageOrder, VisualClass};

    /// The usual Xorg root window layout, for the byte maths.
    fn bgrx_layout() -> pixels::PixelLayout {
        pixels::PixelLayout {
            depth: 24,
            bits_per_pixel: 32,
            scanline_pad: 32,
            byte_order: ImageOrder::LSB_FIRST,
            class: VisualClass::TRUE_COLOR,
            red_mask: 0x00ff_0000,
            green_mask: 0x0000_ff00,
            blue_mask: 0x0000_00ff,
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> RectI {
        RectI::new(x, y, w, h)
    }

    #[test]
    fn a_monitor_inside_the_root_window_is_captured_as_it_is() {
        let root = rect(0, 0, 1920, 1080);
        assert_eq!(clamp_to_root(rect(0, 0, 1920, 1080), root), root);
        assert_eq!(
            clamp_to_root(rect(0, 0, 800, 600), root),
            rect(0, 0, 800, 600)
        );
    }

    #[test]
    fn a_monitor_hanging_over_the_edge_is_captured_as_the_overlap() {
        let root = rect(0, 0, 1920, 1080);

        // Half of a monitor to the left of and above the origin.
        assert_eq!(
            clamp_to_root(rect(-100, -50, 1920, 1080), root),
            rect(0, 0, 1820, 1030)
        );
        // A monitor reaching past the right and bottom edges.
        assert_eq!(
            clamp_to_root(rect(1000, 900, 1920, 1080), root),
            rect(1000, 900, 920, 180)
        );
        // Entirely outside: nothing to capture.
        assert!(clamp_to_root(rect(-2000, 0, 1920, 1080), root).is_empty());
        assert!(clamp_to_root(rect(0, 2000, 1920, 1080), root).is_empty());
    }

    #[test]
    fn the_region_size_is_the_pitched_data_of_the_whole_rectangle() {
        let layout = bgrx_layout();
        // 32 bits per pixel, so the pitch is four bytes per column.
        assert_eq!(region_bytes(&layout, rect(0, 0, 3, 2)), 24);
        assert_eq!(
            region_bytes(&layout, rect(0, 0, 1920, 1080)),
            1920 * 4 * 1080
        );
        // An empty rectangle takes no bytes at all.
        assert_eq!(region_bytes(&layout, rect(5, 5, 0, 10)), 0);
    }

    #[test]
    fn the_segment_is_sized_for_the_largest_monitor() {
        // Three monitors of the same height, the third the widest and the only
        // one whose 101 pixels do not fill whole 32-bit scanlines; a packed
        // 24-bit layout so the padding is in the answer as well.
        let mut layout = bgrx_layout();
        layout.bits_per_pixel = 24;
        layout.depth = 24;

        let rects = [
            rect(0, 0, 100, 10),   // 300 bytes per row
            rect(100, 0, 50, 10),  // 150, padded to 152
            rect(200, 0, 101, 10), // 303, padded to 304
        ];
        assert_eq!(region_bytes(&layout, rect(200, 0, 101, 1)), 304, "padded");
        assert_eq!(largest_region_bytes(&layout, rects), 304 * 10);
        assert_eq!(largest_region_bytes(&layout, []), 0);
    }

    #[test]
    fn a_rectangle_the_request_cannot_hold_is_an_error_not_a_wrapped_coordinate() {
        // GetImage takes signed 16-bit origins and unsigned 16-bit sizes, so a
        // screen past 32767 pixels, and an origin outside the root window, have
        // to say so.
        assert_eq!(geometry(rect(0, 0, 1920, 1080)), Ok((0, 0, 1920, 1080)));
        assert_eq!(geometry(rect(32767, 32767, 1, 1)), Ok((32767, 32767, 1, 1)));
        assert!(
            geometry(rect(40000, 0, 100, 100)).is_err(),
            "x past i16::MAX"
        );
        assert!(
            geometry(rect(0, -1, 100, 100)).is_err(),
            "a negative origin"
        );
        assert!(geometry(rect(0, 0, 70000, 100)).is_err(), "w past u16::MAX");
    }
}
