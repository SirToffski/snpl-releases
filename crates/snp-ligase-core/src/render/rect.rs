//! Rectangles: fill first, then a stroke centred on the edge (SPEC section 4.2).

use tiny_skia::{FillRule, LineJoin, PathBuilder, Stroke};

use super::Canvas;
use crate::geom::Rect;
use crate::model::RectA;

/// The rectangle the shape itself needs, fill and stroke included.
#[must_use]
pub fn shape_bounds(rect: &RectA) -> Rect {
    if rect.fill.is_none() && rect.stroke.is_none() {
        // Nothing to draw: an empty sprite rectangle, so rasterize says None.
        return Rect::ZERO;
    }
    let mut bounds = rect.rect;
    if let Some(stroke) = rect.stroke {
        bounds = bounds.inflated(stroke.width / 2.0);
    }
    bounds
}

/// Draws the rectangle into a canvas already sized for it.
pub(crate) fn draw(canvas: &mut Canvas, rect: &RectA) {
    let Some(path) = build(rect) else {
        return;
    };

    if let Some(fill) = rect.fill {
        canvas.pixmap.fill_path(
            &path,
            &Canvas::paint(fill),
            FillRule::Winding,
            canvas.transform,
            None,
        );
    }

    if let Some(stroke) = rect.stroke {
        let width = stroke.width.max(0.1);
        let style = Stroke {
            width,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        canvas.pixmap.stroke_path(
            &path,
            &Canvas::paint(stroke.color),
            &style,
            canvas.transform,
            None,
        );
    }
}

/// The outline of the rectangle, rounded if it has a corner radius.
fn build(rect: &RectA) -> Option<tiny_skia::Path> {
    let radius = rect
        .corner_radius
        .max(0.0)
        .min(rect.rect.w / 2.0)
        .min(rect.rect.h / 2.0);
    if radius <= 0.0 {
        let area = tiny_skia::Rect::from_xywh(rect.rect.x, rect.rect.y, rect.rect.w, rect.rect.h)?;
        return Some(PathBuilder::from_rect(area));
    }
    rounded(rect.rect, radius)
}

/// A rectangle with quarter-circle corners.
///
/// The corners are cubic Beziers using the usual circle constant, which is
/// within a thousandth of a pixel of a true circle at these sizes.
fn rounded(area: Rect, radius: f32) -> Option<tiny_skia::Path> {
    const KAPPA: f32 = 0.552_284_8;
    let control = radius * KAPPA;
    let (left, top, right, bottom) = (area.x, area.y, area.right(), area.bottom());

    let mut builder = PathBuilder::new();
    builder.move_to(left + radius, top);
    builder.line_to(right - radius, top);
    builder.cubic_to(
        right - radius + control,
        top,
        right,
        top + radius - control,
        right,
        top + radius,
    );
    builder.line_to(right, bottom - radius);
    builder.cubic_to(
        right,
        bottom - radius + control,
        right - radius + control,
        bottom,
        right - radius,
        bottom,
    );
    builder.line_to(left + radius, bottom);
    builder.cubic_to(
        left + radius - control,
        bottom,
        left,
        bottom - radius + control,
        left,
        bottom - radius,
    );
    builder.line_to(left, top + radius);
    builder.cubic_to(
        left,
        top + radius - control,
        left + radius - control,
        top,
        left + radius,
        top,
    );
    builder.close();
    builder.finish()
}
