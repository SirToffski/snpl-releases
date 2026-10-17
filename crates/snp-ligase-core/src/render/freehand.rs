//! Freehand strokes: a smooth curve through the simplified samples, drawn as
//! **one** stroked path (NEXT_STEPS step 14a).
//!
//! One path on purpose: drawing the segments one by one would blend a
//! translucent stroke twice wherever it crossed itself, so a "marker" would
//! darken at its own crossings. tiny-skia fills a stroked path once, which
//! keeps the alpha uniform.
//!
//! Both ends get a round cap and every joint is round, because a scribble has
//! no direction and no corners (`path.rs`'s Catmull-Rom curve is C1, so a
//! miter would only ever show at a hair-thin angle).

use tiny_skia::{LineCap, LineJoin, PathBuilder, Stroke as SkiaStroke};

use super::Canvas;
use crate::geom::Rect;
use crate::model::FreehandA;
use crate::path::{self, Segment};

/// The rectangle the stroke needs: the curve, plus half the stroke width.
#[must_use]
pub fn shape_bounds(freehand: &FreehandA) -> Rect {
    // A stroke needs two points (SPEC section 4): fewer draws nothing at all.
    if freehand.points.len() < 2 {
        return Rect::ZERO;
    }
    let points = path::polyline(&freehand.points, true);
    if points.is_empty() {
        return Rect::ZERO;
    }
    Rect::from_points(&points).inflated(freehand.stroke.width / 2.0)
}

/// Draws the stroke into a canvas already sized for it.
pub(crate) fn draw(canvas: &mut Canvas, freehand: &FreehandA) {
    let segments = path::segments(&freehand.points, true);
    let Some(path) = build(&segments) else {
        return;
    };

    let style = SkiaStroke {
        // A zero width would become a hairline, which is never what a tool that
        // offers widths 1-12 means.
        width: freehand.stroke.width.max(0.1),
        line_join: LineJoin::Round,
        line_cap: LineCap::Round,
        ..SkiaStroke::default()
    };
    canvas.pixmap.stroke_path(
        &path,
        &Canvas::paint(freehand.stroke.color),
        &style,
        canvas.transform,
        None,
    );
}

/// The curve as one path, cubic control points included.
fn build(segments: &[Segment]) -> Option<tiny_skia::Path> {
    let mut builder = PathBuilder::new();
    for (index, segment) in segments.iter().enumerate() {
        let start = segment.start();
        if index == 0 {
            builder.move_to(start.x, start.y);
        }
        match segment.control_points() {
            Some((c1, c2)) => {
                builder.cubic_to(c1.x, c1.y, c2.x, c2.y, segment.end().x, segment.end().y)
            }
            None => {
                let end = segment.end();
                builder.line_to(end.x, end.y);
            }
        }
    }
    builder.finish()
}
