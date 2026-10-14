//! Pasted images: the pixels themselves, drawn 1:1 at `pos` (NEXT_STEPS step
//! 11).
//!
//! An Image annotation is not an effect: its pixels are its own, they move
//! with it, and they take part in the compositing order's first layer — the
//! base image plus every pasted image — that Blur and Highlight read.

use crate::effects::premultiply;
use crate::geom::Rect;
use crate::model::ImageA;

use super::Canvas;

/// The rectangle the image covers, at `pos`.
///
/// `pos` is kept on whole pixels (the app snaps it on create, move and nudge),
/// so the image is drawn 1:1 and never resampled.
#[must_use]
pub fn shape_bounds(image: &ImageA) -> Rect {
    Rect::new(
        image.pos.x,
        image.pos.y,
        image.image.width() as f32,
        image.image.height() as f32,
    )
}

/// Copies the image's pixels, premultiplied, into the canvas.
///
/// The canvas is `bounds()` sized and positioned at its own origin, so the
/// image lands at its position relative to that.
pub(crate) fn draw(canvas: &mut Canvas, image: &ImageA) {
    let offset_x = (image.pos.x - canvas.origin.0 as f32).round() as i32;
    let offset_y = (image.pos.y - canvas.origin.1 as f32).round() as i32;
    let width = image.image.width() as i32;
    let height = image.image.height() as i32;
    let canvas_width = canvas.pixmap.width() as i32;
    let canvas_height = canvas.pixmap.height() as i32;

    for row in 0..height {
        let y = offset_y + row;
        if y < 0 || y >= canvas_height {
            continue;
        }
        for column in 0..width {
            let x = offset_x + column;
            if x < 0 || x >= canvas_width {
                continue;
            }
            let mut pixel = image.image.get_pixel(column as u32, row as u32).0;
            premultiply(&mut pixel);
            let at = (y as usize * canvas_width as usize + x as usize) * 4;
            canvas.pixmap.data_mut()[at..at + 4].copy_from_slice(&pixel);
        }
    }
}
