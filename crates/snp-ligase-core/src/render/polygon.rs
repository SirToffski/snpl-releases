//! Polygons: a closed shape with straight edges, filled then stroked
//! (SPEC section 4.2).

use tiny_skia::{FillRule, LineJoin, PathBuilder, Stroke};

use super::Canvas;
use crate::geom::{Point, Rect};
use crate::model::PolygonA;

/// The rectangle the shape itself needs.
#[must_use]
pub fn shape_bounds(polygon: &PolygonA) -> Rect {
    if polygon.points.len() < 3 {
        return Rect::ZERO;
    }
    let mut bounds = Rect::from_points(&polygon.points);
    if let Some(stroke) = polygon.stroke {
        bounds = bounds.inflated(stroke.width / 2.0);
    }
    bounds
}

/// Draws the polygon into a canvas already sized for it.
pub(crate) fn draw(canvas: &mut Canvas, polygon: &PolygonA) {
    let Some(path) = build(&polygon.points) else {
        return;
    };

    if let Some(fill) = polygon.fill {
        canvas.pixmap.fill_path(
            &path,
            &Canvas::paint(fill),
            FillRule::EvenOdd,
            canvas.transform,
            None,
        );
    }

    if let Some(stroke) = polygon.stroke {
        let style = Stroke {
            width: stroke.width.max(0.1),
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

/// The closed outline through the points. Fewer than three points make no
/// shape, which is why SPEC wants at least three.
fn build(points: &[Point]) -> Option<tiny_skia::Path> {
    let (first, rest) = points.split_first()?;
    if rest.len() < 2 {
        return None;
    }

    let mut builder = PathBuilder::new();
    builder.move_to(first.x, first.y);
    for point in rest {
        builder.line_to(point.x, point.y);
    }
    builder.close();
    builder.finish()
}
