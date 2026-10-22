//! The Wayland overlay's coordinate systems (LINUX_SUPPORT.md section 6), pure.
//!
//! A Wayland overlay lives in three coordinate systems at once, and this
//! module is the only place that converts between them:
//!
//! - **window-local**: the pointer coordinates a window reports. Window
//!   pixels are the output's own physical pixels (`2561×1440` for the VM's
//!   150% output, including KDE's extra column, S2).
//! - **frame**: the frozen capture's pixels, at the output's native
//!   resolution (the child pre-scales the compose-upscaled portal crops to
//!   that). The frame is blitted at the window's top-left corner, 1:1, so
//!   a frame pixel *is* a window-local pixel.
//! - **virtual**: the portal image's pixel space, what the selection state
//!   machine and the parent's [`crate::VirtualCapture::crop`] work in. One
//!   virtual pixel is `scale_out / S` frame pixels (`S` = image px per
//!   logical px, `1.5` on the VM; `scale_out`, the output's own scale, is
//!   derived from stored dimensions/virtual rect × native scale). Native
//!   ScreenShot2 storage has native scale 1 and smaller dimensions at 100%.
//!
//! A window-local (`lx`, `ly`) then maps to virtual
//! `origin + round(l × S/scale_out)` — the same affine relation the
//! compositor used to place the surface, so a drag that leaves the pressed
//! window (Wayland's implicit grab extrapolates coordinates) lands on the
//! right spot on the *other* monitor. The one correction on top: the
//! answer is clamped into the **desktop union** instead of the monitor's
//! rect, because a monitor's window can be one column wider than its own
//! virtual space (the 2561-vs-2560 extra column) and a pointer there would
//! otherwise invent a virtual position that is not on the desktop (noted in
//! DECISIONS.md).

use crate::RectI;
use crate::linux::x11::overlay::LayoutMonitor;

/// One virtual pixel in frame (window-local) pixels, per axis.
///
/// The caller derives native-per-virtual from stored dimensions and scale;
/// this is its inverse, `S / scale_out`. A scale the layout could not measure (zero,
/// which would invert into no world at all) degrades to counting frame
/// pixels as virtual ones, the X11 behaviour.
#[must_use]
fn virtual_per_frame(native_scale: f64) -> f64 {
    if native_scale > 0.0 {
        1.0 / native_scale
    } else {
        1.0
    }
}

/// Native window pixels per virtual pixel, independent of stored resolution.
fn mapping_scale(monitor: &LayoutMonitor) -> (f64, f64) {
    (
        monitor.native_scale_x * monitor.frame_w as f64 / monitor.w.max(1) as f64,
        monitor.native_scale_y * monitor.frame_h as f64 / monitor.h.max(1) as f64,
    )
}

/// Turns a window-local point into a virtual one.
///
/// `local` is in the monitor's frame pixels (the pointer position winit
/// reports, physical); the answer is the desktop's virtual pixel that point
/// sits on, clamped into `desktop_union` (see the module docs).
#[must_use]
pub(crate) fn virtual_from_local(
    local: (f64, f64),
    monitor: &LayoutMonitor,
    desktop_union: RectI,
) -> (i32, i32) {
    let rect = monitor.rect();
    let x = rect.x as f64 + local.0 * virtual_per_frame(mapping_scale(monitor).0);
    let y = rect.y as f64 + local.1 * virtual_per_frame(mapping_scale(monitor).1);
    clamp_to_union(x.round() as i32, y.round() as i32, desktop_union)
}

/// A virtual rectangle in one monitor's frame pixels, edges mapped.
///
/// The painter works on this: the frame is blitted at the window's top-left
/// corner, so a frame pixel *is* a window-local pixel. Edges are rounded
/// separately, so a selection's part stays within a frame pixel of truth
/// through the ⅔ scale of a 100% output.
#[must_use]
pub(crate) fn local_from_virtual(rect: RectI, monitor: &LayoutMonitor) -> RectI {
    let origin = monitor.rect();
    let left = round_virtual_offset(rect.x - origin.x, mapping_scale(monitor).0);
    let top = round_virtual_offset(rect.y - origin.y, mapping_scale(monitor).1);
    let right = round_virtual_offset(rect.right() - origin.x, mapping_scale(monitor).0);
    let bottom = round_virtual_offset(rect.bottom() - origin.y, mapping_scale(monitor).1);
    RectI::new(left, top, (right - left).max(0), (bottom - top).max(0))
}

/// One virtual coordinate relative to the monitor's origin, in frame pixels.
fn round_virtual_offset(offset: i32, native_scale: f64) -> i32 {
    (offset as f64 * native_scale).round() as i32
}

/// A monitor's window's rectangle in virtual pixels, from its inner size.
///
/// The window can be a pixel wider than its monitor's virtual space (KDE's
/// extra column), which is exactly why [`virtual_from_local`] clamps: this
/// rectangle says what the window *would* claim, and the clamp keeps a
/// pointer on its far edge inside the desktop's virtual space.
#[must_use]
pub(crate) fn window_virtual(size: (u32, u32), monitor: &LayoutMonitor) -> RectI {
    let rect = monitor.rect();
    let width = (size.0 as f64 * virtual_per_frame(mapping_scale(monitor).0)).round() as i32;
    let height = (size.1 as f64 * virtual_per_frame(mapping_scale(monitor).1)).round() as i32;
    RectI::new(rect.x, rect.y, width.max(0), height.max(0))
}

/// A monitor's frame's size at the monitor's native resolution: what the
/// child pre-scales its frozen frame to, and what the painter blits 1:1.
#[must_use]
pub(crate) fn native_frame_size(monitor: &LayoutMonitor) -> (u32, u32) {
    (
        (monitor.frame_w as f64 * monitor.native_scale_x)
            .round()
            .max(0.0) as u32,
        (monitor.frame_h as f64 * monitor.native_scale_y)
            .round()
            .max(0.0) as u32,
    )
}

/// Keeps a virtual point on the desktop.
/// An empty (or degenerate) union is mapped to "no clamp at all": a broken
/// layout must not swallow a real position.
fn clamp_to_union(x: i32, y: i32, union: RectI) -> (i32, i32) {
    if union.w <= 0 || union.h <= 0 {
        return (x, y);
    }
    (
        x.clamp(union.x, union.right() - 1),
        y.clamp(union.y, union.bottom() - 1),
    )
}

/// Every monitor's union, in virtual pixels: the desktop the overlay covers.
#[must_use]
pub(crate) fn desktop_union(monitors: &[LayoutMonitor]) -> RectI {
    let mut left = i32::MAX;
    let mut top = i32::MAX;
    let mut right = i32::MIN;
    let mut bottom = i32::MIN;
    for monitor in monitors {
        let rect = monitor.rect();
        left = left.min(rect.x);
        top = top.min(rect.y);
        right = right.max(rect.right());
        bottom = bottom.max(rect.bottom());
    }
    RectI::new(left, top, right - left, bottom - top)
}

/// The `W × H` label's text: the size the answer really comes out at.
///
/// A selection lying wholly inside one monitor comes out of
/// [`crate::VirtualCapture::crop`] at that monitor's own resolution, so the
/// label names its native size; one that spans monitors (or a gap) keeps the
/// capture's virtual pixels, so the label names the virtual size. The text is
/// drawn by whichever window contains the selection's origin.
#[must_use]
pub(crate) fn label_text(selection: RectI, monitors: &[LayoutMonitor]) -> String {
    let owner = monitors
        .iter()
        .find(|monitor| monitor.rect().contains_rect(selection));
    if let Some(monitor) = owner {
        if monitor.frame_w as i32 != monitor.w || monitor.frame_h as i32 != monitor.h {
            // Native storage: name the same rounded source edges crop copies.
            let local = local_from_virtual(selection, monitor);
            return format!("{} × {}", local.w, local.h);
        }
        let native_w = round_virtual_offset(selection.w, mapping_scale(monitor).0).max(0);
        let native_h = round_virtual_offset(selection.h, mapping_scale(monitor).1).max(0);
        format!("{native_w} × {native_h}")
    } else {
        format!("{} × {}", selection.w, selection.h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_stored_frame_keeps_virtual_mapping_and_native_buffer() {
        let mut monitor = v2();
        monitor.frame_w = 1920;
        monitor.frame_h = 1080;
        monitor.native_scale_x = 1.0;
        monitor.native_scale_y = 1.0;
        assert_eq!(native_frame_size(&monitor), (1920, 1080));
        assert_eq!(
            virtual_from_local((400.0, 300.0), &monitor, desktop_union(&[v2(), v1()])),
            (600, 450)
        );
        assert_eq!(
            local_from_virtual(RectI::new(0, 0, 600, 450), &monitor),
            RectI::new(0, 0, 400, 300)
        );
        assert_eq!(
            label_text(RectI::new(0, 0, 600, 450), std::slice::from_ref(&monitor)),
            "400 × 300"
        );
        assert_eq!(
            label_text(RectI::new(1, 1, 301, 301), &[monitor]),
            "200 × 200"
        );
    }

    /// The VM's real layout, from the `layout` spike on 2026-10-20: the
    /// compositor lists `Virtual-2` (100%, first) then `Virtual-1` (150%,
    /// second), and the portal image is 5441×1620.
    fn monitor(name: &str, x: i32, w: i32, h: i32, native_scale: f64) -> LayoutMonitor {
        LayoutMonitor {
            name: name.to_owned(),
            x,
            y: 0,
            w,
            h,
            frame_w: w.max(0) as u32,
            frame_h: h.max(0) as u32,
            file: format!("frame_{name}.rgba"),
            primary: false,
            native_scale_x: native_scale,
            native_scale_y: native_scale,
        }
    }

    /// `Virtual-2`: 100% scale, 1920×1080 physical, its part of the image
    /// upscaled to 2880×1620 (native scale ⅔).
    fn v2() -> LayoutMonitor {
        monitor("Virtual-2", 0, 2880, 1620, 2.0 / 3.0)
    }

    /// `Virtual-1`: 150% scale, 2560×1440 physical, whose window is 1 pixel
    /// wider than `KWin`'s rounding needs (S2), image-rect 2560×1440 at
    /// native scale 1.
    fn v1() -> LayoutMonitor {
        monitor("Virtual-1", 2880, 2560, 1440, 1.0)
    }

    #[test]
    fn a_frame_pixel_is_one_point_three_thirds_of_a_virtual_pixel_on_150() {
        // Virtual-1 maps 1:1 (native scale 1.0), Virtual-2 maps ⅔: the numbers
        // every drag has to survive, measured in the VM.
        assert_eq!(virtual_per_frame(1.0), 1.0);
        assert_eq!(virtual_per_frame(2.0 / 3.0), 1.5);
    }

    #[test]
    fn window_local_maps_to_virtual_through_the_measured_scales() {
        let union = desktop_union(&[v2(), v1()]);

        // Virtual-2's corners: a frame pixel there is 1.5 virtual ones. The
        // corner (1920,1080) is the monitor's last *edge* coordinate, one
        // past the desktop's virtual space, so the clamp puts it back on the
        // last real pixel.
        assert_eq!(virtual_from_local((0.0, 0.0), &v2(), union), (0, 0));
        assert_eq!(
            virtual_from_local((1920.0, 1080.0), &v2(), union),
            (2880, 1619)
        );
        assert_eq!(
            virtual_from_local((960.0, 540.0), &v2(), union),
            (1440, 810)
        );

        // Virtual-1 maps 1:1: the output's pixels are already native ones.
        // The right edge (2560,1440) sits past the desktop horizontally
        // (KDE's extra column) but the y stays inside the 1620-tall space.
        assert_eq!(virtual_from_local((0.0, 0.0), &v1(), union), (2880, 0));
        assert_eq!(
            virtual_from_local((2560.0, 1440.0), &v1(), union),
            (5439, 1440)
        );

        // A drag that leaves the pressed window (the implicit grab
        // extrapolates surface-local coordinates) lands on the other
        // monitor's virtual position: the pointer 500 frame pixels right of
        // Virtual-2's edge reports from Virtual-2 as `local = 1920 + 500`.
        assert_eq!(
            virtual_from_local((2420.0, 540.0), &v2(), union),
            (3630, 810)
        );
    }

    #[test]
    fn the_desktop_union_clamps_kde_s_extra_column() {
        let union = desktop_union(&[v2(), v1()]);

        // A pointer on Virtual-1's extra column (S2: inner width 2561)
        // extrapolates to virtual x 5440, one past the desktop; the clamp
        // keeps it on the last real column instead of outside every monitor.
        assert_eq!(
            virtual_from_local((2560.0, 720.0), &v1(), union),
            (5439, 720)
        );

        // A runaway drag left through a compositor that keeps extrapolated
        // coordinates still maps: 500 frame pixels left of Virtual-1's edge
        // lands in Virtual-2's virtual space, the affine mapping carries it,
        // and only a position outside every monitor is clamped.
        assert_eq!(
            virtual_from_local((-500.0, 720.0), &v1(), union),
            (2380, 720)
        );
        assert_eq!(virtual_from_local((-5000.0, 720.0), &v1(), union), (0, 720));
        assert_eq!(
            virtual_from_local((50000.0, 720.0), &v1(), union),
            (5439, 720)
        );
    }

    #[test]
    fn the_desktop_union_is_the_monitors_rect() {
        // First Virtual-2 then Virtual-1: the union is their extents.
        let union = desktop_union(&[v2(), v1()]);
        assert_eq!(union, RectI::new(0, 0, 5440, 1620));
        // The portal image is one column wider still (5441, its rounding
        // column); the union covers what the monitors own, not the whitespace.
        assert_ne!(union, RectI::new(0, 0, 5441, 1620));
    }

    #[test]
    fn a_virtual_selection_maps_to_frame_pixels_edge_by_edge() {
        // A 1280×720-image-px selection on Virtual-2 comes out at ⅔…
        let local = local_from_virtual(RectI::new(100, 50, 1280, 720), &v2());
        assert_eq!(local, RectI::new(67, 33, 853, 480));

        // …and on Virtual-1, unchanged (native scale 1).
        let local = local_from_virtual(RectI::new(3000, 100, 200, 200), &v1());
        assert_eq!(local, RectI::new(120, 100, 200, 200));

        // A selection that starts left of the monitor: negative, as it must
        // be — the window paints only the part inside it.
        let local = local_from_virtual(RectI::new(2400, 0, 600, 100), &v1());
        assert_eq!(local, RectI::new(-480, 0, 600, 100));
    }

    #[test]
    fn a_window_is_its_monitor_at_the_native_scale() {
        // KDE's extra column: the window on Virtual-1 claims one frame pixel
        // more than its monitor's virtual space owns.
        assert_eq!(
            window_virtual((2561, 1440), &v1()),
            RectI::new(2880, 0, 2561, 1440)
        );
        assert_eq!(
            window_virtual((1920, 1080), &v2()),
            RectI::new(0, 0, 2880, 1620)
        );
    }

    #[test]
    fn the_label_names_the_resolution_the_answer_comes_out_at() {
        // Wholly inside Virtual-2: crop() gives the monitor's own pixels, so
        // the label names the native size (⅔ of the virtual one).
        let inside_v2 = RectI::new(500, 400, 1000, 200);
        assert_eq!(label_text(inside_v2, &[v2(), v1()]), "667 × 133");

        // Wholly inside Virtual-1: native scale 1, the label is the selection
        // itself.
        let inside_v1 = RectI::new(3600, 200, 500, 300);
        assert_eq!(label_text(inside_v1, &[v2(), v1()]), "500 × 300");

        // Spanning both: the capture's pixels, virtual size.
        let spanning = RectI::new(2400, 100, 2000, 500);
        assert_eq!(label_text(spanning, &[v2(), v1()]), "2000 × 500");

        // No monitors at all (a broken layout) says the virtual size too, and
        // nothing else is claimed.
        assert_eq!(label_text(spanning, &[]), "2000 × 500");
    }
}
