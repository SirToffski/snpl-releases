//! Text annotations: a box of the layout size plus padding, background first,
//! then the outline centred on the box edge, then the glyphs (SPEC section 4.2
//! and NEXT_STEPS step 8).

use tiny_skia::{FillRule, LineJoin, PathBuilder, Stroke as SkiaStroke};

use super::Canvas;
use crate::geom::Rect;
use crate::model::{Rgba, Stroke, TextA};
use crate::text::{self, Fonts};

/// The text box: the layout size plus `padding` on all sides (SPEC 4.2).
///
/// This stays the geometric box even when the annotation has an outline: hit
/// testing covers the box, and [`super::bounds`] grows the sprite by half the
/// outline width separately.
#[must_use]
pub fn shape_bounds(text: &TextA, fonts: &mut Fonts) -> Rect {
    let layout = text::layout(text, fonts);
    let padding = text.padding.max(0.0);
    Rect::new(
        text.pos.x,
        text.pos.y,
        layout.width + padding * 2.0,
        layout.height + padding * 2.0,
    )
}

/// Draws the background, if there is one, then the outline, if there is one,
/// and then the glyphs (SPEC 4.2).
pub(crate) fn draw(canvas: &mut Canvas, text: &TextA, fonts: &mut Fonts) {
    let padding = text.padding.max(0.0);
    let box_rect = shape_bounds(text, fonts);
    if let Some(background) = text.background {
        fill_box(canvas, box_rect, background);
    }
    if let Some(outline) = text.outline {
        stroke_box(canvas, box_rect, outline);
    }
    text::draw_run(
        canvas,
        &text::Run {
            text: &text.text,
            family: &text.font_family,
            size: text.size,
            bold: text.bold,
            italic: text.italic,
            colour: text.color,
        },
        (text.pos.x + padding, text.pos.y + padding),
        fonts,
    );
}

/// Fills the text box with its background colour.
fn fill_box(canvas: &mut Canvas, box_rect: Rect, colour: Rgba) {
    let Some(area) = tiny_skia::Rect::from_xywh(box_rect.x, box_rect.y, box_rect.w, box_rect.h)
    else {
        return;
    };
    let path = PathBuilder::from_rect(area);
    canvas.pixmap.fill_path(
        &path,
        &Canvas::paint(colour),
        FillRule::Winding,
        canvas.transform,
        None,
    );
}

/// Strokes the text box's outline, centred on the box edge, exactly like Rect
/// (SPEC 4.2 and NEXT_STEPS step 8).
fn stroke_box(canvas: &mut Canvas, box_rect: Rect, outline: Stroke) {
    let Some(area) = tiny_skia::Rect::from_xywh(box_rect.x, box_rect.y, box_rect.w, box_rect.h)
    else {
        return;
    };
    let path = PathBuilder::from_rect(area);
    let style = SkiaStroke {
        width: outline.width.max(0.1),
        line_join: LineJoin::Round,
        ..SkiaStroke::default()
    };
    canvas.pixmap.stroke_path(
        &path,
        &Canvas::paint(outline.color),
        &style,
        canvas.transform,
        None,
    );
}
