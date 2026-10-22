//! Painting one Wayland overlay window, pure — in the window's own physical
//! pixels (LINUX_SUPPORT.md section 6).
//!
//! The X11 overlay's painter paints root-window pixels, which *are* window
//! pixels there; on Wayland the two come apart: the window shows one
//! output's physical pixels (the VM's 150% output: 2561×1440, extra column
//! and all), while the selection is tracked in the portal image's virtual
//! space. This module paints the **frame at native size** into the window,
//! at 1:1 (frame pixel = window-local pixel) from its top-left corner, so
//! the pixels are exactly the output's own resolution, and pads whatever is
//! left — KDE's extra column, a window the compositor made bigger than the
//! monitor — with black. That is why a frame smaller than its window is an
//! expected shape here, not a degraded capture.
//!
//! The selection arrives in virtual pixels and is mapped per monitor
//! ([`map::local_from_virtual`]); which of its edges are *real* in this
//! window is [`selection_in_window`]'s decision, so a selection crossing the
//! monitor seam is not cut in half by an outline at the shared edge.
//!
//! The label ("`W × H`") names the size the answer really comes out at —
//! see [`map::label_text`] — and the caller passes it only to the window
//! that contains the selection's origin, so it appears exactly once.
//!
//! Like the X11 one, this painter is pure: no window, no event loop, and
//! every rule is a pixel test with the VM's real numbers.

use super::map;
use crate::RectI;
use crate::linux::x11::overlay::LayoutMonitor;
use crate::linux::x11::overlay::paint::{draw_label, fill_rect, label_rect, label_size};

/// Black, the colour of the strips the frame does not cover and of the
/// outline's outer line and of the label's background.
const BLACK: u32 = 0x0000_0000;
/// White, the colour of the outline's inner line and of the label's text.
const WHITE: u32 = 0x00FF_FFFF;

/// One frozen frame as this painter wants it: at the monitor's native size,
/// original and dimmed, `0x00RRGGBB` per pixel.
pub struct Frame<'a> {
    /// The frozen frame at its native size.
    pub original: &'a [u32],
    /// The same dimmed to, as everywhere, SPEC 5.6's 60%: painted outside
    /// the selection.
    pub dimmed: &'a [u32],
    pub width: u32,
    pub height: u32,
}

/// The selection as this window shows it.
///
/// `local` is in window-local pixels, where the frame is blitted 1:1; the
/// four flags say which selection edges are *real* here — drawn, not a seam
/// where the selection continues on the neighbour monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectionInWindow {
    pub local: RectI,
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

/// Which part of the virtual selection this window paints, and its real edges.
///
/// The edges are real when they fall inside the monitor's own virtual span:
/// the window that shows a selection's start draws its start edge, and the
/// window that shows only its continuation draws none there.
#[must_use]
pub(crate) fn selection_in_window(
    selection: RectI,
    monitor: &LayoutMonitor,
    size: (u32, u32),
) -> Option<SelectionInWindow> {
    if selection.is_empty() {
        return None;
    }
    let virtual_span = monitor.rect();
    // The part of the selection that falls on this monitor's virtual space,
    // and then in this window's local pixels: painting happens 1:1 with the
    // frame, which the window-local coordinates are already in.
    let visible_virtual = selection.intersection(virtual_span);
    if visible_virtual.is_empty() {
        return None;
    }
    let local = map::local_from_virtual(visible_virtual, monitor);
    let window = RectI::new(
        0,
        0,
        i32::try_from(size.0).unwrap_or(1),
        i32::try_from(size.1).unwrap_or(1),
    );
    let visible_local = local.intersection(window);
    if visible_local.is_empty() {
        return None;
    }
    Some(SelectionInWindow {
        local: visible_local,
        left: visible_virtual.x == selection.x,
        right: visible_virtual.right() == selection.right(),
        top: visible_virtual.y == selection.y,
        bottom: visible_virtual.bottom() == selection.bottom(),
    })
}

/// Paints one overlay window.
///
/// `size` is the window's inner size in its own pixels, `frame` the monitor's
/// frozen pixels at native size, `selection` the selection in **virtual**
/// pixels or `None` while nothing is selected, and `monitor` this window's
/// monitor from the layout (the rect, and the scales the mapping divides
/// by). `label` comes pre-decided: `Some(text)` only for the window that
/// contains the selection's top-left corner ([`map::label_text`] decides the
/// text).
pub fn paint_window_physical(
    target: &mut [u32],
    size: (u32, u32),
    frame: &Frame<'_>,
    selection: Option<RectI>,
    monitor: &LayoutMonitor,
    label: Option<&str>,
) {
    target.fill(BLACK);
    copy_frame(
        target,
        size,
        frame.dimmed,
        frame,
        RectI::new(
            0,
            0,
            i32::try_from(size.0).unwrap_or(0),
            i32::try_from(size.1).unwrap_or(0),
        ),
    );

    let Some(selection) = selection else {
        return;
    };
    let Some(in_window) = selection_in_window(selection, monitor, size) else {
        return;
    };

    // The bright part: the frame's own pixels inside the selection's part
    // this window shows. 1:1, so this is the monitor's native resolution.
    copy_frame(target, size, frame.original, frame, in_window.local);

    // The outline: a white line on each real edge, a black one outside it.
    // A seam edge is not drawn, so the monitor seams don't cut the outline.
    stroke_edges(target, size, &in_window);

    if monitor.rect().contains(selection.x, selection.y)
        && let Some(text) = label
    {
        let label_shape = label_size(text);
        let rect = label_rect(in_window.local, (size.0 as i32, size.1 as i32), label_shape);
        // A window smaller than the label shows none, the X11 rule too: a
        // black box otherwise would paint a whole window over.
        if rect.w <= size.0 as i32 && rect.h <= size.1 as i32 {
            draw_label(target, size, rect, text);
        }
    }
}

/// Copies the `area` part of the frame — dimmed or original — into `target`,
/// 1:1, the frame being at the window's top-left corner.
///
/// Everything past the frame's own size stays black: KDE's extra column and
/// any window the compositor grew beyond the monitor.
fn copy_frame(
    target: &mut [u32],
    size: (u32, u32),
    pixels: &[u32],
    frame: &Frame<'_>,
    area: RectI,
) {
    let frame_rect = RectI::new(
        0,
        0,
        i32::try_from(frame.width).unwrap_or(0),
        i32::try_from(frame.height).unwrap_or(0),
    );
    let visible = area.intersection(frame_rect);
    if visible.is_empty() {
        return;
    }
    let left = visible.x.max(0) as usize;
    let top = visible.y.max(0) as usize;
    let right = (visible.right().min(i32::try_from(size.0).unwrap_or(0))).max(left as i32) as usize;
    let bottom =
        (visible.bottom().min(i32::try_from(size.1).unwrap_or(0))).max(top as i32) as usize;
    if right <= left || bottom <= top {
        return;
    }
    let width = right - left;
    for row in top..bottom {
        let src = row * frame.width as usize + left;
        let dst = row * size.0 as usize + left;
        let Some(target_row) = target.get_mut(dst..dst + width) else {
            continue;
        };
        let Some(source_row) = pixels.get(src..src + width) else {
            continue;
        };
        target_row.copy_from_slice(source_row);
    }
}

/// Draws the 1 px outline of the selection, on its real edges only.
///
/// A white line on the edge itself and a black one one pixel outside it,
/// each clipped to the window: X11's outline, at scale 1:1 here because the
/// window's pixels already are the monitor's own.
fn stroke_edges(target: &mut [u32], size: (u32, u32), in_window: &SelectionInWindow) {
    let local = in_window.local;
    if in_window.left {
        fill_rect(
            target,
            size,
            RectI::new(local.x - 1, local.y, 1, local.h),
            BLACK,
        );
        fill_rect(
            target,
            size,
            RectI::new(local.x, local.y, 1, local.h),
            WHITE,
        );
    }
    if in_window.right {
        fill_rect(
            target,
            size,
            RectI::new(local.right(), local.y, 1, local.h),
            BLACK,
        );
        fill_rect(
            target,
            size,
            RectI::new(local.right() - 1, local.y, 1, local.h),
            WHITE,
        );
    }
    if in_window.top {
        fill_rect(
            target,
            size,
            RectI::new(local.x, local.y - 1, local.w, 1),
            BLACK,
        );
        fill_rect(
            target,
            size,
            RectI::new(local.x, local.y, local.w, 1),
            WHITE,
        );
    }
    if in_window.bottom {
        fill_rect(
            target,
            size,
            RectI::new(local.x, local.bottom(), local.w, 1),
            BLACK,
        );
        fill_rect(
            target,
            size,
            RectI::new(local.x, local.bottom() - 1, local.w, 1),
            WHITE,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One monitor plus its pre-scaled frame: `width×height` pixels of
    /// `colour_frame`, dimmed kept as a pixel-equal copy to be replaced.
    struct TestFrame {
        original: Vec<u32>,
        dimmed: Vec<u32>,
        width: u32,
        height: u32,
    }

    impl TestFrame {
        fn solid(width: u32, height: u32, colour: u32) -> Self {
            let original = vec![colour; (width as usize) * (height as usize)];
            let dimmed = x11_dim(&original);
            Self {
                original,
                dimmed,
                width,
                height,
            }
        }

        fn frame(&self) -> Frame<'_> {
            Frame {
                original: &self.original,
                dimmed: &self.dimmed,
                width: self.width,
                height: self.height,
            }
        }
    }

    fn x11_dim(pixels: &[u32]) -> Vec<u32> {
        pixels
            .iter()
            .map(|pixel| {
                let channel = |shift: u32| {
                    let value = (*pixel >> shift & 0xFF) as u8;
                    // The same 60% dim as the X11 overlay (SPEC 5.6).
                    (f32::from(value) * 0.6).round().clamp(0.0, 255.0) as u32
                };
                (channel(16) << 16) | (channel(8) << 8) | channel(0)
            })
            .collect()
    }

    const FRAME_COLOUR: u32 = 0x00AA_6633;

    fn lit(a: u32, b: u32) -> bool {
        a == b
    }

    fn monitor(x: i32, w: i32, h: i32, native_scale: f64) -> LayoutMonitor {
        LayoutMonitor {
            name: "virtual-test".to_owned(),
            x,
            y: 0,
            w,
            h,
            frame_w: w.max(0) as u32,
            frame_h: h.max(0) as u32,
            file: "frame.rgba".to_owned(),
            primary: false,
            native_scale_x: native_scale,
            native_scale_y: native_scale,
        }
    }

    #[test]
    fn the_frame_paints_dimmed_and_brightens_inside_the_selection() {
        // A 1:1 monitor (native scale 1) whose 6×6 frame is one colour; the
        // selection is its own pixels, so the mapping is trivial here.
        let frame = TestFrame::solid(6, 6, FRAME_COLOUR);
        let dimmed = frame.dimmed[0];
        let monitor = monitor(0, 6, 6, 1.0);
        let mut buffer = vec![0u32; 6 * 6];

        paint_window_physical(
            &mut buffer,
            (6, 6),
            &frame.frame(),
            Some(RectI::new(2, 1, 3, 3)),
            &monitor,
            None,
        );

        // The white inner line sits on the selection's edges, so a border
        // pixel is the line's; the inside is the frame's own pixel colour.
        assert!(lit(buffer[0], dimmed));
        assert!(lit(buffer[5], dimmed));
        // (x=2, y=1) is index 8.
        assert!(lit(buffer[8], WHITE), "the left edge's inner line");
        assert!(lit(buffer[3 + 2 * 6], FRAME_COLOUR), "the inside");
        assert!(lit(buffer[5 + 5 * 6], dimmed), "the far corner");
    }

    #[test]
    fn the_selection_outline_sits_on_its_real_edges_only() {
        let frame = TestFrame::solid(10, 5, FRAME_COLOUR);
        let monitor = monitor(0, 10, 5, 1.0);
        let mut buffer = vec![0u32; 10 * 5];

        paint_window_physical(
            &mut buffer,
            (10, 5),
            &frame.frame(),
            Some(RectI::new(2, 1, 6, 3)),
            &monitor,
            None,
        );

        // White on the selection's own edges, black one pixel outside.
        assert!(lit(buffer[2 + 2 * 10], WHITE), "the left edge's inner line");
        assert!(lit(buffer[1 + 2 * 10], BLACK), "the left edge's outer line");
        assert!(
            lit(buffer[7 + 2 * 10], WHITE),
            "the right edge's inner line"
        );
        assert!(
            lit(buffer[8 + 2 * 10], BLACK),
            "the right edge's outer line"
        );
        // Inside the selection: its own pixel, no line through it.
        assert!(lit(buffer[4 + 2 * 10], FRAME_COLOUR));
        // The window's pixel (0,0).
        assert!(lit(buffer[0], frame.dimmed[0]));
    }

    #[test]
    fn a_selection_across_the_seam_keeps_both_sides_clean() {
        // Two 1:1 monitors side by side, the VM's shape shrunk: A [0,10], B
        // [10,18]; the selection (4,1,10,3) spans the seam at 10.
        const W: u32 = 10;
        let frame_a = TestFrame::solid(W, 5, FRAME_COLOUR);
        let monitor_a = monitor(0, W as i32, 5, 1.0);
        let selection = RectI::new(4, 1, 10, 3);

        let mut buffer = vec![0u32; (W * 5) as usize];
        paint_window_physical(
            &mut buffer,
            (W, 5),
            &frame_a.frame(),
            Some(selection),
            &monitor_a,
            None,
        );
        // A's window shows the left edge (the selection starts on A) but not
        // the right one: the seam only looks like an edge to the painter. The
        // pixels up to the seam are bright, nothing is drawn through it.
        assert!(lit(buffer[4 + 2 * 10], WHITE), "the real left edge");
        assert_ne!(
            buffer[8 + 2 * 10],
            WHITE,
            "no line at the seam's A side: {:#x}",
            buffer[8 + 2 * 10]
        );
        assert!(
            lit(buffer[8 + 2 * 10], FRAME_COLOUR),
            "bright up to the seam"
        );
        assert!(lit(buffer[9 + 2 * 10], FRAME_COLOUR), "bright at the seam");

        let frame_b = TestFrame::solid(8, 5, FRAME_COLOUR);
        let monitor_b = monitor(10, 8, 5, 1.0);
        let mut buffer = vec![0u32; (8 * 5) as usize];
        paint_window_physical(
            &mut buffer,
            (8, 5),
            &frame_b.frame(),
            Some(selection),
            &monitor_b,
            None,
        );
        // B shows the right edge but not the left one, and stays bright from
        // its own first column: nothing is dimmed twice at the seam. The
        // selection reaches only part of B (4 of its 8 columns), so the
        // outline is on *its* last column.
        assert!(lit(buffer[3 + 2 * 8], WHITE), "the real right edge");
        // B's pixel (0,2): inside the selection, no outline drawn there.
        assert_ne!(
            buffer[16], WHITE,
            "no line at the seam's B side: {:#x}",
            buffer[16]
        );
        assert!(lit(buffer[16], FRAME_COLOUR), "bright at the seam");
        assert!(lit(buffer[1 + 2 * 8], FRAME_COLOUR), "bright inside B");
    }

    #[test]
    fn a_frame_smaller_than_its_window_leaves_an_extra_black_column() {
        // The VM's Virtual-1: a 2560×1440 frame whose KWin window is 1 pixel
        // wider (S2). The test takes a slightly wider margin than KDE's one
        // column on purpose: the outline's own outer line paints one black
        // pixel just past the selection's edge, which would mask a blit that
        // shifted pixels into the extra columns. Columns further out prove
        // that, even inside a selection that covers the whole monitor, the
        // pixels the frame has none for stay black.
        let frame = TestFrame::solid(2560, 100, FRAME_COLOUR);
        let monitor = monitor(2880, 2560, 100, 1.0);
        let mut buffer = vec![0u32; 2564 * 100];

        paint_window_physical(
            &mut buffer,
            (2564, 100),
            &frame.frame(),
            Some(RectI::new(2880, 0, 2560, 100)),
            &monitor,
            None,
        );

        // The frame's last real column carries the white inner outline (the
        // selection covers the whole monitor), the next column is the
        // outline's black outer line, and the columns the frame has no
        // pixels for stay black — not shifted pixels. Inside the frame it
        // is the frame's own colour.
        assert!(lit(buffer[2500 + 50 * 2564], FRAME_COLOUR), "the inside");
        assert!(lit(buffer[2559 + 50 * 2564], WHITE), "the inner line");
        assert!(lit(buffer[2560 + 50 * 2564], BLACK), "the outer line");
        assert!(lit(buffer[2562 + 50 * 2564], BLACK), "past the frame");
        // The white inner outline sits on the frame's last column, not on
        // the extra ones — on the window's first and last rows too.
        assert!(lit(buffer[2559], WHITE));
        assert!(lit(buffer[2559 + 99 * 2564], WHITE));
    }

    #[test]
    fn the_100_output_maps_its_selection_through_the_two_thirds() {
        // The VM's Virtual-2: 1920×1080 frame (the child's pre-scaled copy of
        // the 2880×1620 image crop), native scale ⅔, selection
        // (1500,750)–(1800,1050) in virtual pixels maps to
        // (1000,500)–(1200,700) in the window's own.
        let frame = TestFrame::solid(1920, 1080, FRAME_COLOUR);
        let dimmed = frame.dimmed[0];
        let monitor = monitor(0, 2880, 1620, 2.0 / 3.0);
        let mut buffer = vec![0u32; 1920 * 1080];

        paint_window_physical(
            &mut buffer,
            (1920, 1080),
            &frame.frame(),
            Some(RectI::new(1500, 750, 300, 300)),
            &monitor,
            None,
        );

        // The inner left edge is white and sits at the mapped column, the
        // outer line is black right before it.
        assert!(
            lit(buffer[1000 + 600 * 1920], WHITE),
            "the mapped left edge"
        );
        assert!(lit(buffer[999 + 600 * 1920], BLACK), "the outer line");
        // Inside: bright at the mapped rows, and dimmed before the selection.
        assert!(lit(buffer[1001 + 600 * 1920], FRAME_COLOUR));
        assert!(lit(buffer[500 + 600 * 1920], dimmed));
    }

    #[test]
    fn the_selection_s_own_size_is_the_label_s() {
        // On the ⅔ output the label names the selection's native size, so the
        // user reads the resolution they really get (crop's one-output rule).
        let frame = TestFrame::solid(1920, 1080, FRAME_COLOUR);
        let monitor = monitor(0, 2880, 1620, 2.0 / 3.0);
        let mut buffer = vec![0u32; 1920 * 1080];

        paint_window_physical(
            &mut buffer,
            (1920, 1080),
            &frame.frame(),
            Some(RectI::new(1500, 750, 300, 300)),
            &monitor,
            Some("200 × 200"),
        );

        // The label's black box starts 3 px inside the corner, and its white
        // glyph pixels are somewhere in the box's first row.
        let x = 1000 + 3;
        let y = 500 + 3;
        assert!(lit(buffer[x + y * 1920], BLACK), "the label's background");
        let mut has_white_glyph = false;
        for column in x..x + (20 * 2) + 3 {
            for row in y..y + 14 {
                if lit(buffer[column + row * 1920], WHITE) {
                    has_white_glyph = true;
                }
            }
        }
        assert!(has_white_glyph, "the label's digits are drawn");
    }

    #[test]
    fn a_window_without_a_selection_paints_only_the_frame() {
        let frame = TestFrame::solid(4, 2, FRAME_COLOUR);
        let dimmed = frame.dimmed[0];
        let monitor = monitor(0, 4, 2, 1.0);
        let mut buffer = vec![0xffffffff; 4 * 2];

        paint_window_physical(&mut buffer, (4, 2), &frame.frame(), None, &monitor, None);

        for pixel in buffer {
            assert!(lit(pixel, dimmed), "{pixel:#x}");
        }
    }

    #[test]
    fn a_stateless_empty_selection_paints_the_frame() {
        // A drag that has not left its anchor (0×1) is no selection: the
        // painter is told nothing and the frame is the whole answer.
        let frame = TestFrame::solid(4, 2, FRAME_COLOUR);
        let dimmed = frame.dimmed[0];
        let monitor = monitor(0, 4, 2, 1.0);
        let mut buffer = vec![0u32; 4 * 2];

        paint_window_physical(
            &mut buffer,
            (4, 2),
            &frame.frame(),
            Some(RectI::new(2, 1, 0, 0)),
            &monitor,
            None,
        );

        for pixel in buffer {
            assert!(lit(pixel, dimmed), "{pixel:#x}");
        }
    }

    #[test]
    fn selection_in_window_decides_which_edges_are_real() {
        // The seam sits at x=2880 on the VM: a selection starting at 1500 and
        // ending at 5000 spans both monitors.
        let selection = RectI::new(1500, 100, 3500, 400);
        let monitor_a = monitor(0, 2880, 1620, 1.0);
        let a = selection_in_window(selection, &monitor_a, (2880, 1620)).expect("visible on A");
        assert_eq!(a.local, RectI::new(1500, 100, 1380, 400));
        assert!(a.left && a.top && a.bottom, "A's real edges");
        assert!(!a.right, "the seam is not A's edge");

        let monitor_b = monitor(2880, 2560, 1440, 1.0);
        let b = selection_in_window(selection, &monitor_b, (2561, 1440)).expect("visible on B");
        assert_eq!(b.local, RectI::new(0, 100, 2120, 400));
        assert!(b.right && b.top && b.bottom, "B's real edges");
        assert!(!b.left, "the seam is not B's edge");
    }
}
