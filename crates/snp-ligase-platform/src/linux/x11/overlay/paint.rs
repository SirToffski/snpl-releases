//! Painting one overlay window (SPEC section 5.6), pure.
//!
//! The pixels of an overlay window are decided here, with no window and no
//! event loop involved: the dimmed frozen frame, the original pixels inside the
//! selection, the 1 px white line with its 1 px black line around it, and the
//! `W × H` label near the selection's top-left corner.
//!
//! Two shapes are worth knowing about:
//!
//! - The selection is in **root-window pixels** and a window covers one
//!   monitor, so every selection is clipped to what the window really shows and
//!   shifted by the window's origin (which can be negative) before it is
//!   painted. A selection crossing a monitor edge is painted as its part in
//!   each window, and the outline is drawn only on the selection's real edges:
//!   without that, the clipping edge would draw a white line through the middle
//!   of the selection at the monitor seam.
//! - A frame carries its own rectangle in root-window pixels — where the pixels
//!   froze on the desktop — and the window carries its own. They are placed
//!   against each other: a window manager is free to move or resize a window
//!   (XFCE's xfwm4 keeps windows out of its top panel's work area), and the
//!   frozen frame, the selection, the outline and the label all follow the
//!   window, not the monitor it was asked to cover. A frame smaller than its
//!   rectangle (SPEC 5.4 uses a mismatched capture as it is) leaves the strips
//!   it does not cover black, so no stale pixel from a previous frame survives.
//!
//! The label uses a 5×7 bitmap font scaled 2× — a copy of the frozen frame is
//! up in milliseconds and must not wait for a font stack to initialise.

use crate::RectI;

/// How much dimmer the unselected pixels are (SPEC section 5.6: 60%).
pub const DIM_FACTOR: f32 = 0.6;

/// One glyph is 5×7 bits, drawn `LABEL_SCALE` pixels per bit.
const GLYPH_W: i32 = 5;
const GLYPH_H: i32 = 7;
const LABEL_SCALE: i32 = 2;
/// The gap between two glyphs, at the drawn scale.
const LABEL_SPACING: i32 = 2;
/// Padding around the text inside its box, and its distance from the
/// selection's corner (the Windows overlay uses the same 3 px).
pub const LABEL_PADDING: i32 = 3;

/// Black, the colour of the strips a frame does not cover and of the label's
/// background.
const BLACK: u32 = 0x0000_0000;
/// White, the colour of the outline's inner line and of the label's text.
const WHITE: u32 = 0x00FF_FFFF;
/// The colour of the outline's outer line.
const OUTLINE_OUTER: u32 = 0x0000_0000;

/// One frame as the overlay paints it: the pixels plus the dimmed copy.
pub struct Frame<'a> {
    /// The frozen frame, `0x00RRGGBB` per pixel, row-major from the top left.
    pub original: &'a [u32],
    /// The same dimmed to [`DIM_FACTOR`]; painted outside the selection.
    pub dimmed: &'a [u32],
    pub width: u32,
    pub height: u32,
    /// Where the frame's pixels belong, in root-window pixels: the pixel at
    /// row 0, column 0 is the monitor's top-left corner. The frame can be
    /// smaller than this rectangle (SPEC section 5.4 uses such a capture as it
    /// is); the part without pixels stays black.
    pub rect: RectI,
}

/// The part of a frame that really has pixels.
///
/// A frame can be smaller than its monitor's rectangle (SPEC section 5.4), in
/// which case the pixels fill its top-left corner and the rest is black.
#[must_use]
fn pixel_rect(frame: &Frame<'_>) -> RectI {
    RectI::new(
        frame.rect.x,
        frame.rect.y,
        frame
            .rect
            .w
            .min(i32::try_from(frame.width).unwrap_or(i32::MAX)),
        frame
            .rect
            .h
            .min(i32::try_from(frame.height).unwrap_or(i32::MAX)),
    )
}

/// A captured RGBA frame as overlay pixels, alpha dropped.
///
/// X11 captures are always opaque (the capture path forces alpha to 255), and
/// the overlay window is not layered, so alpha has nothing to blend with. The
/// length is checked here, so a frame file that does not hold what the layout
/// promised is an error instead of a sheared picture.
pub fn pixels_from_rgba(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u32>, String> {
    let expected = width as usize * height as usize * 4;
    if bytes.len() != expected {
        return Err(format!(
            "a {width}x{height} frame is {expected} bytes of RGBA, not {}",
            bytes.len()
        ));
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| (u32::from(pixel[0]) << 16) | (u32::from(pixel[1]) << 8) | u32::from(pixel[2]))
        .collect())
}

/// The dimmed copy of [`to_pixels`]'s output.
#[must_use]
pub fn dim(pixels: &[u32]) -> Vec<u32> {
    pixels.iter().map(|pixel| dim_pixel(*pixel)).collect()
}

/// One channel of the dimmed copy (the same rounding as the Windows overlay).
#[must_use]
pub fn dim_channel(value: u8) -> u8 {
    (f32::from(value) * DIM_FACTOR).round().clamp(0.0, 255.0) as u8
}

/// One pixel, dimmed.
fn dim_pixel(pixel: u32) -> u32 {
    let dim = |shift: u32| u32::from(dim_channel(((pixel >> shift) & 0xFF) as u8));
    (dim(16) << 16) | (dim(8) << 8) | dim(0)
}

/// Paints one overlay window.
///
/// `target` is the window's buffer, `size` its size in pixels and `window` the
/// window's own rectangle in root-window pixels. `selection` is in that same
/// space, or `None` while the user has not dragged anything.
///
/// The frame says where its pixels belong on the desktop and the window says
/// where it is, and the two need not agree: a window manager can move or resize
/// a window at any time — XFCE's xfwm4 keeps windows out of its top panel's
/// work area here — so everything is placed by its root-window rectangle and
/// clipped to what overlaps. Whatever ends up outside the desktop is simply not
/// visible.
pub fn paint_window(
    target: &mut [u32],
    size: (u32, u32),
    frame: &Frame<'_>,
    selection: Option<RectI>,
    window: RectI,
) {
    target.fill(BLACK);
    copy_frame_area(target, size, frame.dimmed, frame, window, window);

    let Some(selection) = selection else {
        return;
    };
    // The part of the selection this window shows, in root-window pixels, and
    // the same in window-local ones. The two are needed together: the edges are
    // decided in root-window pixels, the painting happens in local ones.
    let visible = selection.intersection(window);
    let local = visible.to_local(window);
    if local.is_empty() {
        return;
    }

    // The bright part: the frame's own pixels, inside the selection's share of
    // this window.
    copy_frame_area(target, size, frame.original, frame, visible, window);

    // The outline, on the selection's own edges only, so a selection crossing
    // a monitor edge is not cut in half by a line at the seam.
    stroke_edge(target, size, local, visible, selection, true);
    stroke_edge(target, size, local, visible, selection, false);

    // The label belongs to the window that owns the selection's top-left
    // corner, so a selection spanning two monitors shows it exactly once.
    if window.contains(selection.x, selection.y) {
        let text = format!("{} × {}", selection.w, selection.h);
        let label = label_rect(local, (window.w, window.h), label_size(&text));
        // A window smaller than the label (and its padding) shows no label:
        // its black background would otherwise paint the whole window over.
        if label.w <= window.w && label.h <= window.h {
            draw_label(target, size, label, &text);
        }
    }
}

/// Copies the frame's pixels of the root-window rectangle `area` into `target`,
/// moved so that `window`'s top-left corner is `target`'s.
///
/// Everything the frame does not cover is left alone: the strips of a frame
/// smaller than its monitor, and the edges of a window the window manager moved
/// away from its monitor's rectangle, stay black.
fn copy_frame_area(
    target: &mut [u32],
    size: (u32, u32),
    pixels: &[u32],
    frame: &Frame<'_>,
    area: RectI,
    window: RectI,
) {
    let visible = area.intersection(pixel_rect(frame));
    if visible.is_empty() {
        return;
    }
    let local = visible.to_local(window);
    // `visible` lies inside both `area` and the frame's rectangle, so the copy
    // starts at or after the window's top-left corner; only its far edges can
    // run past what the window shows.
    let left = local.x.max(0);
    let top = local.y.max(0);
    let right = local.right().min(i32::try_from(size.0).unwrap_or(i32::MAX));
    let bottom = local
        .bottom()
        .min(i32::try_from(size.1).unwrap_or(i32::MAX));
    if right <= left || bottom <= top {
        return;
    }

    let width = u32::try_from(right - left).unwrap_or(0);
    let column = u32::try_from(visible.x - frame.rect.x).unwrap_or(0);
    for row in top..bottom {
        let frame_row = u32::try_from(row + window.y - frame.rect.y).unwrap_or(0);
        let src = (frame_row * frame.width + column) as usize;
        let dst =
            (u32::try_from(row).unwrap_or(0) * size.0 + u32::try_from(left).unwrap_or(0)) as usize;
        let Some(target_row) = target.get_mut(dst..dst + width as usize) else {
            continue;
        };
        let Some(source_row) = pixels.get(src..src + width as usize) else {
            continue;
        };
        target_row.copy_from_slice(source_row);
    }
}

/// Draws the 1 px outline of the selection, inside this window.
///
/// `visible` is the part of the selection this window shows and `local` the
/// same in window coordinates. A white line is drawn on an edge the selection
/// really has inside this window (`inner`), and the black line one pixel
/// outside it otherwise. Comparing `visible` (root-window pixels) with
/// `selection` is what keeps the seam between two monitors clean: the window
/// that shows only a clipped part of the selection draws no line at the
/// monitor edge, only at the selection's own edges.
fn stroke_edge(
    target: &mut [u32],
    size: (u32, u32),
    local: RectI,
    visible: RectI,
    selection: RectI,
    inner: bool,
) {
    let colour = if inner { WHITE } else { OUTLINE_OUTER };
    let offset = if inner { 0 } else { 1 };

    // The vertical edges: the selection's left edge is in this window when the
    // visible part starts where the selection starts, and so on.
    let mut edges: Vec<RectI> = Vec::with_capacity(4);
    if visible.x == selection.x {
        edges.push(RectI::new(local.x - offset, local.y, 1, local.h));
    }
    if visible.right() == selection.right() {
        edges.push(RectI::new(local.right() - 1 + offset, local.y, 1, local.h));
    }
    if visible.y == selection.y {
        edges.push(RectI::new(local.x, local.y - offset, local.w, 1));
    }
    if visible.bottom() == selection.bottom() {
        edges.push(RectI::new(local.x, local.bottom() - 1 + offset, local.w, 1));
    }
    // A one-pixel rectangle is drawn twice by the pairs above; the colour is
    // the same, so that is invisible.
    for edge in edges {
        fill_rect(target, size, edge, colour);
    }
}

/// Fills a window-local rectangle, clipped to the window.
///
/// Shared with the Wayland overlay's painter (`wayland::overlay::paint`),
/// which draws the same 1 px edge and label machinery on physical buffers.
pub(crate) fn fill_rect(target: &mut [u32], size: (u32, u32), rect: RectI, colour: u32) {
    let left = rect.x.max(0);
    let top = rect.y.max(0);
    let right = rect.right().min(i32::try_from(size.0).unwrap_or(i32::MAX));
    let bottom = rect.bottom().min(i32::try_from(size.1).unwrap_or(i32::MAX));
    if right <= left || bottom <= top {
        return;
    }
    let width = u32::try_from(right - left).unwrap_or(0);
    for row in top..bottom {
        let row_start = (u32::try_from(row).unwrap_or(0) * size.0) as usize;
        let start = row_start + u32::try_from(left).unwrap_or(0) as usize;
        let Some(target_row) = target.get_mut(start..start + width as usize) else {
            continue;
        };
        target_row.fill(colour);
    }
}

/// Where the `W × H` label goes: just inside the selection's top-left corner,
/// clamped so it stays inside the monitor (SPEC section 5.6).
///
/// The same rule as the Windows overlay's, so the label sits in the same place
/// on both platforms.
#[must_use]
pub fn label_rect(selection_local: RectI, monitor: (i32, i32), label: (i32, i32)) -> RectI {
    let max_x = (monitor.0 - label.0 - LABEL_PADDING).max(0);
    let max_y = (monitor.1 - label.1 - LABEL_PADDING).max(0);
    RectI {
        x: (selection_local.x + LABEL_PADDING).clamp(0, max_x),
        y: (selection_local.y + LABEL_PADDING).clamp(0, max_y),
        w: label.0.max(0),
        h: label.1.max(0),
    }
}

/// The size of `text` as this module draws it.
#[must_use]
pub fn label_size(text: &str) -> (i32, i32) {
    let glyphs = i32::try_from(text.chars().count()).unwrap_or(0);
    if glyphs == 0 {
        return (0, 0);
    }
    (
        glyphs * GLYPH_W * LABEL_SCALE + (glyphs - 1) * LABEL_SPACING,
        GLYPH_H * LABEL_SCALE,
    )
}

/// Draws `text` as black-on-white... rather, white on a black box, inside
/// `rect`. Shared with the Wayland overlay's painter.
pub(crate) fn draw_label(target: &mut [u32], size: (u32, u32), rect: RectI, text: &str) {
    fill_rect(target, size, rect, BLACK);
    let mut x = rect.x;
    for character in text.chars() {
        draw_glyph(target, size, x, rect.y, character);
        x += GLYPH_W * LABEL_SCALE + LABEL_SPACING;
    }
}

/// Draws one glyph with its top-left corner at `(x, y)`.
fn draw_glyph(target: &mut [u32], size: (u32, u32), x: i32, y: i32, character: char) {
    let rows = glyph(character);
    for (row, bits) in rows.iter().enumerate() {
        for column in 0..GLYPH_W {
            // Bit 4 is the leftmost pixel of the row.
            if bits & (1 << (GLYPH_W - 1 - column)) == 0 {
                continue;
            }
            fill_rect(
                target,
                size,
                RectI::new(
                    x + column * LABEL_SCALE,
                    y + i32::try_from(row).unwrap_or(0) * LABEL_SCALE,
                    LABEL_SCALE,
                    LABEL_SCALE,
                ),
                WHITE,
            );
        }
    }
}

/// A 5×7 glyph, one bit per pixel, bit 4 the leftmost.
///
/// Only what the label needs: the ten digits, a space, the multiplication sign
/// (a lowercase `x` shape, and `x` itself for a log line that had to spell it
/// out).
#[must_use]
fn glyph(character: char) -> [u8; GLYPH_H as usize] {
    match character {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        '×' | 'x' => [
            0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b00000,
        ],
        _ => [0; GLYPH_H as usize],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> RectI {
        RectI { x, y, w, h }
    }

    /// One monitor's frozen frame: one colour everywhere, its pixels belonging
    /// at `rect` in root-window pixels.
    ///
    /// Kept as a type of its own because [`Frame`] borrows; a test that paints
    /// two monitors needs two of these alive at once.
    struct TestFrame {
        original: Vec<u32>,
        dimmed: Vec<u32>,
        width: u32,
        height: u32,
        rect: RectI,
    }

    impl TestFrame {
        fn frame(&self) -> Frame<'_> {
            Frame {
                original: &self.original,
                dimmed: &self.dimmed,
                width: self.width,
                height: self.height,
                rect: self.rect,
            }
        }
    }

    /// A frame of one colour, `rect` pixels of it.
    fn monitor_frame(rect: RectI, colour: u32) -> TestFrame {
        let width = u32::try_from(rect.w).expect("a positive width");
        let height = u32::try_from(rect.h).expect("a positive height");
        let original = vec![colour; (width * height) as usize];
        let dimmed = dim(&original);
        TestFrame {
            original,
            dimmed,
            width,
            height,
            rect,
        }
    }

    /// A frame whose every pixel says where it is: `y * 8 + x`, as a colour.
    fn numbered_frame(rect: RectI) -> TestFrame {
        let width = u32::try_from(rect.w).expect("a positive width");
        let height = u32::try_from(rect.h).expect("a positive height");
        let original: Vec<u32> = (0..width * height).collect();
        let dimmed = dim(&original);
        TestFrame {
            original,
            dimmed,
            width,
            height,
            rect,
        }
    }

    fn pixel(buffer: &[u32], width: u32, x: u32, y: u32) -> u32 {
        buffer[(y * width + x) as usize]
    }

    /// Every white pixel in a window buffer.
    fn white_pixels(target: &[u32]) -> usize {
        target.iter().filter(|pixel| **pixel == WHITE).count()
    }

    #[test]
    fn the_dimmed_copy_uses_the_spec_factor() {
        assert_eq!(dim_channel(0), 0);
        assert_eq!(dim_channel(255), 153);
        assert_eq!(dim_channel(100), 60);
        assert_eq!(dim_pixel(0x00FF_FFFF), 0x0099_9999);
        assert_eq!(dim_pixel(0x0012_3456), 0x000B_1F34);
    }

    #[test]
    fn a_frame_becomes_opaque_overlay_pixels() {
        let bytes = [0x12, 0x34, 0x56, 0x00, 0xAA, 0xBB, 0xCC, 0xFF];
        let pixels = pixels_from_rgba(&bytes, 2, 1).expect("two pixels");
        assert_eq!(pixels, vec![0x0012_3456, 0x00AA_BBCC]);

        // A frame file that is not the size the layout promised is an error,
        // not a sheared picture.
        let error = pixels_from_rgba(&bytes, 3, 1).expect_err("too short");
        assert!(error.contains("12"), "{error}");
    }

    #[test]
    fn outside_the_selection_is_dimmed_and_inside_is_the_original() {
        let width = 20;
        let monitor = monitor_frame(rect(0, 0, 20, 10), 0x00FF_FFFF);
        let mut target = vec![0; (width * 10) as usize];

        paint_window(
            &mut target,
            (width, 10),
            &monitor.frame(),
            Some(rect(5, 2, 8, 6)),
            rect(0, 0, 20, 10),
        );

        assert_eq!(pixel(&target, width, 0, 0), 0x0099_9999, "outside: dimmed");
        assert_eq!(pixel(&target, width, 6, 3), 0x00FF_FFFF, "inside: bright");
        assert_eq!(pixel(&target, width, 12, 7), 0x00FF_FFFF, "inside: bright");
        assert_eq!(
            pixel(&target, width, 13, 3),
            OUTLINE_OUTER,
            "one past the right edge, the black outside line"
        );
        assert_eq!(
            pixel(&target, width, 5, 8),
            OUTLINE_OUTER,
            "below the bottom edge, the black outside line"
        );
        assert_eq!(
            pixel(&target, width, 14, 3),
            0x0099_9999,
            "two past the right edge: dimmed again"
        );
        assert_eq!(
            pixel(&target, width, 5, 9),
            0x0099_9999,
            "two below the bottom edge: dimmed again"
        );
    }

    #[test]
    fn the_outline_is_white_on_the_selection_and_black_outside_it() {
        let width = 20;
        let monitor = monitor_frame(rect(0, 0, 20, 10), 0x0066_6666);
        let mut target = vec![0; (width * 10) as usize];

        paint_window(
            &mut target,
            (width, 10),
            &monitor.frame(),
            Some(rect(5, 2, 8, 6)),
            rect(0, 0, 20, 10),
        );

        // The white line is the selection's own edge...
        assert_eq!(pixel(&target, width, 5, 2), WHITE, "top-left corner");
        assert_eq!(
            pixel(&target, width, 12, 7),
            WHITE,
            "the bottom-right pixel"
        );
        // ...and the black line sits outside it, over the dimmed frame.
        assert_eq!(
            pixel(&target, width, 4, 2),
            OUTLINE_OUTER,
            "left of the edge"
        );
        assert_eq!(pixel(&target, width, 13, 3), OUTLINE_OUTER, "right of it");
        assert_eq!(pixel(&target, width, 5, 1), OUTLINE_OUTER, "above it");
        assert_eq!(pixel(&target, width, 5, 8), OUTLINE_OUTER, "below it");
        // Two pixels out is the frame again, dimmed: 0x66 × 0.6 rounds to 0x3D.
        assert_eq!(pixel(&target, width, 3, 2), 0x003D_3D3D);
    }

    #[test]
    fn a_selection_crossing_a_monitor_edge_is_clipped_without_a_line_at_the_seam() {
        // Two 10-pixel monitors side by side, each with its own colour; the
        // selection runs from (8, 2) to (14, 6), so the left window shows its
        // left part (8..10) and the right one the rest (10..14).
        let width = 10;
        let left_monitor = monitor_frame(rect(0, 0, 10, 10), 0x00FF_FFFF);
        let right_monitor = monitor_frame(rect(10, 0, 10, 10), 0x0011_2233);
        let selection = rect(8, 2, 6, 4);

        let mut target = vec![0; (width * 10) as usize];
        paint_window(
            &mut target,
            (width, 10),
            &left_monitor.frame(),
            Some(selection),
            rect(0, 0, 10, 10),
        );

        assert_eq!(pixel(&target, width, 8, 2), WHITE, "the top line");
        assert_eq!(pixel(&target, width, 9, 3), WHITE, "the bright part");
        // The seam at column 9..10 is the selection's continuation, not its
        // right edge: no white or black line may cut the selection there.
        assert_eq!(
            pixel(&target, width, 9, 5),
            WHITE,
            "no line at the monitor's right edge"
        );
        assert_eq!(pixel(&target, width, 8, 6), OUTLINE_OUTER, "below it");

        // The other half of the same selection, on the next monitor.
        let mut right = vec![0; (width * 10) as usize];
        paint_window(
            &mut right,
            (width, 10),
            &right_monitor.frame(),
            Some(selection),
            rect(10, 0, 10, 10),
        );
        assert_eq!(
            pixel(&right, width, 0, 2),
            WHITE,
            "the selection continues into this monitor"
        );
        assert_eq!(pixel(&right, width, 3, 3), WHITE, "its right edge");
        assert_eq!(
            pixel(&right, width, 4, 3),
            OUTLINE_OUTER,
            "black outside it"
        );
        // The bright part is this monitor's own pixels, not the left one's.
        assert_eq!(
            pixel(&right, width, 1, 3),
            0x0011_2233,
            "the right monitor's frame"
        );
    }

    #[test]
    fn the_label_belongs_to_the_monitor_that_owns_the_selections_corner() {
        // Two 60-pixel monitors side by side; the selection starts at x=58, so
        // the left one owns its top-left corner.
        let left_monitor = monitor_frame(rect(0, 0, 60, 30), 0x0044_4444);
        let right_monitor = monitor_frame(rect(60, 0, 60, 30), 0x0077_7777);
        let selection = rect(58, 2, 6, 4);

        let mut left = vec![0; 60 * 30];
        paint_window(
            &mut left,
            (60, 30),
            &left_monitor.frame(),
            Some(selection),
            rect(0, 0, 60, 30),
        );
        let mut right = vec![0; 60 * 30];
        paint_window(
            &mut right,
            (60, 30),
            &right_monitor.frame(),
            Some(selection),
            rect(60, 0, 60, 30),
        );

        // The right window shows the selection's other half with the outline
        // and nothing else: 4 + 4 pixels of top and bottom line plus the four
        // pixels of its right edge, two of which the lines already covered.
        assert_eq!(white_pixels(&right), 10, "the right window has no label");
        assert_eq!(
            pixel(&right, 60, 0, 2),
            WHITE,
            "the selection continues into the next monitor"
        );
        assert_eq!(pixel(&right, 60, 1, 3), 0x0077_7777, "bright inside");
        assert_eq!(
            pixel(&right, 60, 4, 3),
            0x0000_0000,
            "black outside its edge"
        );

        // The left window owns the corner, so it draws the label too.
        assert!(
            white_pixels(&left) > 10,
            "the label is missing: {}",
            white_pixels(&left)
        );
    }

    #[test]
    fn a_selection_at_a_monitor_seam_puts_its_label_on_the_owner() {
        // SPEC 5.6's label follows the selection's top-left corner, and a
        // corner exactly on the shared edge belongs to the window that starts
        // there (the same rule as `RectI::contains`).
        let left_monitor = monitor_frame(rect(0, 0, 20, 20), 0x00FF_FFFF);
        let right_monitor = monitor_frame(rect(20, 0, 20, 20), 0x0099_9999);
        let selection = rect(20, 0, 8, 8);

        let mut at_seam = vec![0; 20 * 20];
        paint_window(
            &mut at_seam,
            (20, 20),
            &right_monitor.frame(),
            Some(selection),
            rect(20, 0, 20, 20),
        );
        // The white outline plus the label's white glyphs: more than the
        // outline alone (both edges of an 8-pixel line plus the shared top and
        // bottom ones).
        let outline_only = 16 + 2;
        assert!(
            white_pixels(&at_seam) > outline_only,
            "the window at the seam owns the corner and draws the label: {}",
            white_pixels(&at_seam)
        );

        let mut before_seam = vec![0; 20 * 20];
        paint_window(
            &mut before_seam,
            (20, 20),
            &left_monitor.frame(),
            Some(selection),
            rect(0, 0, 20, 20),
        );
        assert_eq!(
            before_seam.iter().filter(|pixel| **pixel == WHITE).count(),
            0,
            "the left window shows none of that selection"
        );
    }

    #[test]
    fn a_window_the_window_manager_moved_shows_the_pixels_that_are_really_there() {
        // A window manager that keeps windows out of a panel's work area moves
        // the window down: here the monitor is 8x8 at the root origin and the
        // window really starts three rows lower.
        let width = 8;
        let monitor = numbered_frame(rect(0, 0, 8, 8));
        let window = rect(0, 3, 8, 8);
        let mut target = vec![0x00FF_00FF; (width * 8) as usize];

        paint_window(&mut target, (width, 8), &monitor.frame(), None, window);

        // The window's own first row is the desktop's row 3, which is the
        // frame's row 3: pixel 3 * 8 + 0 = 24, dimmed.
        assert_eq!(
            pixel(&target, width, 0, 0),
            dim_pixel(24),
            "the window's row 0"
        );
        assert_eq!(
            pixel(&target, width, 7, 0),
            dim_pixel(31),
            "the window's row 0, last column"
        );
        assert_eq!(
            pixel(&target, width, 0, 4),
            dim_pixel(56),
            "row 7 of the frame"
        );
        // Below the frame's last row there are no pixels at all: black, not a
        // stale value from the buffer.
        assert_eq!(pixel(&target, width, 0, 5), BLACK, "past the frame");
        assert_eq!(pixel(&target, width, 7, 7), BLACK, "past the frame");
    }

    #[test]
    fn a_moved_window_still_puts_the_selection_under_the_pointer() {
        // The same moved window, with a selection dragged over the desktop:
        // the bright part and the outline have to follow the window, and a
        // click's root-window rectangle is what the user dragged.
        let width = 8;
        let monitor = numbered_frame(rect(0, 0, 8, 8));
        let window = rect(0, 3, 8, 8);
        let selection = rect(2, 3, 3, 4);
        let mut target = vec![0; (width * 8) as usize];

        paint_window(
            &mut target,
            (width, 8),
            &monitor.frame(),
            Some(selection),
            window,
        );

        // The selection's top-left corner in root-window pixels is the
        // window's row 0, column 2.
        assert_eq!(
            pixel(&target, width, 2, 0),
            WHITE,
            "the selection's top line"
        );
        // One pixel in is past the white left edge: these are the desktop's
        // own pixels, one desktop row per window row (the window starts at
        // desktop row 3, so its row 1 is desktop row 4).
        assert_eq!(
            pixel(&target, width, 3, 1),
            monitor.original[3 + 4 * 8],
            "the bright pixels are the desktop's"
        );
        assert_eq!(
            pixel(&target, width, 3, 2),
            monitor.original[3 + 5 * 8],
            "and one row further down, one desktop row further down"
        );
        assert_eq!(
            pixel(&target, width, 3, 3),
            WHITE,
            "the selection's bottom line"
        );
        assert_eq!(
            pixel(&target, width, 1, 0),
            OUTLINE_OUTER,
            "the black line, one pixel outside"
        );
        assert_eq!(
            pixel(&target, width, 6, 0),
            dim_pixel(monitor.original[6 + 3 * 8]),
            "outside the selection, the frame's own row 3 is dimmed"
        );

        // A selection entirely outside the window the window manager moved: a
        // screen row the window does not cover is nobody's to paint.
        let mut other = vec![0; (width * 8) as usize];
        paint_window(
            &mut other,
            (width, 8),
            &monitor.frame(),
            Some(rect(2, 20, 3, 2)),
            window,
        );
        assert_eq!(
            other.iter().filter(|pixel| **pixel == WHITE).count(),
            0,
            "no part of that selection is in this window"
        );
    }

    #[test]
    fn the_size_label_is_kept_inside_the_monitor() {
        // The same clamping as the Windows overlay: a selection at the very
        // edge of a monitor must not push the label off it.
        let label = (60, 14);
        let corner = label_rect(rect(0, 0, 3, 3), (100, 100), label);
        assert_eq!((corner.x, corner.y), (LABEL_PADDING, LABEL_PADDING));

        let edge = label_rect(rect(95, 95, 5, 5), (100, 100), label);
        assert!(edge.x + label.0 <= 100 - LABEL_PADDING, "{edge:?}");
        assert!(edge.y + label.1 <= 100 - LABEL_PADDING, "{edge:?}");

        // A monitor too small for the label keeps it at the origin instead of
        // painting at a negative position, and the painter leaves it out.
        let tiny = label_rect(rect(0, 0, 1, 1), (10, 10), label);
        assert_eq!((tiny.x, tiny.y), (0, 0));
        assert!(tiny.w > 10 && tiny.h > 10, "it does not fit: {tiny:?}");
    }

    #[test]
    fn the_label_size_follows_the_text_length() {
        let (one_w, one_h) = label_size("5");
        let (three_w, three_h) = label_size("300");
        assert_eq!(one_h, GLYPH_H * LABEL_SCALE);
        assert_eq!(three_h, one_h);
        assert!(three_w > one_w, "three glyphs are wider than one");
        assert_eq!(label_size(""), (0, 0));
    }

    #[test]
    fn the_label_is_a_black_box_with_white_glyphs_in_it() {
        let width = 40;
        let mut target = vec![0x0011_1111; (width * 20) as usize];
        let text = "12 × 34";
        let rect = RectI::new(2, 3, label_size(text).0, label_size(text).1);
        draw_label(&mut target, (width, 20), rect, text);

        assert_eq!(
            pixel(
                &target,
                width,
                u32::try_from(rect.x).expect("positive"),
                u32::try_from(rect.y).expect("positive")
            ),
            BLACK,
            "the box"
        );
        let white = (0..20)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .filter(|(x, y)| pixel(&target, width, *x, *y) == WHITE)
            .count();
        assert!(white > 10, "the glyphs should be drawn: {white} pixels");
    }

    #[test]
    fn every_label_character_has_a_glyph() {
        // The label only ever shows digits, spaces and the multiplication sign;
        // a character without a glyph would be drawn as an empty box.
        for character in "0123456789×".chars() {
            let rows = glyph(character);
            assert_eq!(rows.len(), GLYPH_H as usize);
            assert!(
                rows.iter().any(|row| *row != 0),
                "{character:?} has an empty glyph"
            );
        }
        // A space is the one deliberately blank glyph: it separates the two
        // numbers of the label.
        assert!(glyph(' ').iter().all(|row| *row == 0), "a space is blank");
    }

    #[test]
    fn a_monitor_smaller_than_the_label_shows_none() {
        // A (very unusual) monitor smaller than the label box and its padding:
        // drawing the box would paint the whole window black, so the label is
        // left out and the window keeps its frame and outline.
        let width = 8;
        let monitor = monitor_frame(rect(0, 0, 8, 6), 0x00FF_FFFF);
        let mut target = vec![0; (width * 6) as usize];

        paint_window(
            &mut target,
            (width, 6),
            &monitor.frame(),
            Some(rect(0, 0, 3, 3)),
            rect(0, 0, 8, 6),
        );

        assert_eq!(
            pixel(&target, width, 0, 0),
            WHITE,
            "the outline, not a label"
        );
        assert_eq!(
            pixel(&target, width, 5, 5),
            0x0099_9999,
            "the frame is still there"
        );
    }

    #[test]
    fn a_frame_smaller_than_its_monitor_leaves_black_strips() {
        // The frame's rectangle is the whole monitor, but its pixels only fill
        // the top-left corner: SPEC 5.4's mismatched capture, used as it is.
        let monitor = TestFrame {
            original: vec![0x00FF_FFFF; 12],
            dimmed: vec![0x0099_9999; 12],
            width: 4,
            height: 3,
            rect: rect(0, 0, 8, 6),
        };
        let mut target = vec![0x0012_3456; (8 * 6) as usize];

        paint_window(
            &mut target,
            (8, 6),
            &monitor.frame(),
            None,
            rect(0, 0, 8, 6),
        );

        assert_eq!(
            pixel(&target, 8, 0, 0),
            0x0099_9999,
            "the frame's own pixel"
        );
        assert_eq!(pixel(&target, 8, 3, 2), 0x0099_9999, "its last pixel");
        assert_eq!(pixel(&target, 8, 4, 0), BLACK, "the uncovered strip");
        assert_eq!(pixel(&target, 8, 0, 3), BLACK, "the uncovered strip");
    }

    #[test]
    fn a_window_that_does_not_show_the_selection_is_all_dimmed_frame() {
        let monitor = monitor_frame(rect(0, 0, 10, 10), 0x00FF_FFFF);
        let mut target = vec![0; 100];

        paint_window(
            &mut target,
            (10, 10),
            &monitor.frame(),
            Some(rect(20, 20, 5, 5)),
            rect(0, 0, 10, 10),
        );

        for pixel in &target {
            assert_eq!(*pixel, 0x0099_9999);
        }
    }
}
