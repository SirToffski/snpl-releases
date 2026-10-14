//! Lines and arrows: straight segments or a smooth curve, with arrowheads
//! (SPEC section 4.2).
//!
//! The three rules that make an arrow look right are all here:
//!
//! - the head is a filled triangle whose **tip is exactly the path's end
//!   point**, so the point the user dragged to is the point the arrow points at;
//! - the triangle is `L = max(12, 4 * width)` long, and the stroke stops
//!   `0.8 * L` short of the tip, so the line cannot poke out through it and
//!   there is no hairline seam on the triangle's base;
//! - an end with no head gets a **round cap**, which is drawn as the circle
//!   tiny-skia's stroke does not add for us (it uses one cap style for both
//!   ends of a path).

use tiny_skia::{FillRule, LineJoin, PathBuilder, Stroke};

use super::Canvas;
use crate::geom::{Point, Rect};
use crate::model::PathA;
use crate::path::{self, ArrowPlacement, Segment};

/// The rectangle the path itself needs: stroke width and arrowheads included.
#[must_use]
pub fn shape_bounds(path: &PathA) -> Rect {
    // A path needs two points (SPEC section 4): fewer draws nothing at all, not
    // even a round cap.
    if path.points.len() < 2 {
        return Rect::ZERO;
    }
    let points = path::polyline(&path.points, path.curved);
    if points.is_empty() {
        return Rect::ZERO;
    }

    let mut bounds = Rect::from_points(&points).inflated(path.stroke.width / 2.0);
    for placement in arrows(path) {
        let head = placement.head;
        bounds = bounds.union(Rect::from_points(&[
            head.tip,
            head.base_left,
            head.base_right,
        ]));
    }
    bounds
}

/// The arrowheads this path has, if they can be placed.
fn arrows(path: &PathA) -> Vec<ArrowPlacement> {
    let mut out = Vec::new();
    if path.head_end {
        out.extend(path::arrow_at_end(path));
    }
    if path.head_start {
        out.extend(path::arrow_at_start(path));
    }
    out
}

/// The segments to stroke, with the ends pulled back where a head sits.
fn stroked_segments(path: &PathA) -> Vec<Segment> {
    let mut segments = path::segments(&path.points, path.curved);
    if let (true, Some(last)) = (path.head_end, segments.last_mut())
        && let Some(pulled_back) = arrow_pull_back(*last, path.stroke.width, true)
    {
        *last = shortened_end(*last, pulled_back);
    }
    if let (true, Some(first)) = (path.head_start, segments.first_mut())
        && let Some(pulled_back) = arrow_pull_back(*first, path.stroke.width, false)
    {
        *first = shortened_start(*first, pulled_back);
    }
    segments
}

/// How far the stroke must stop short of the arrowhead's tip. `at_end` picks the
/// end of the segment the head sits on.
fn arrow_pull_back(segment: Segment, width: f32, at_end: bool) -> Option<Point> {
    let (towards, tip) = if at_end {
        segment.end_tangent()?
    } else {
        segment.start_tangent()?
    };
    let head = crate::path::ArrowHead::new(tip, towards, width)?;
    let unit = unit_towards(tip, towards);
    Some(tip - unit * head.pull_back_distance())
}

/// The unit vector from `towards` to `tip`.
fn unit_towards(tip: Point, towards: Point) -> Point {
    let direction = tip - towards;
    let length = direction.distance(Point::ZERO);
    if length <= f32::EPSILON {
        return Point::new(1.0, 0.0);
    }
    direction / length
}

/// A segment with a new end point. For a cubic the control points stay where
/// they are (SPEC section 4.2), so the curve bends into the arrowhead.
fn shortened_end(segment: Segment, end: Point) -> Segment {
    // A 12 px head pulls a 4 px stroke back by 9.6 px, so a very short segment
    // would otherwise draw a stub behind the point the user placed.
    let end = keep_within(segment.start(), segment.end(), end);
    match segment {
        Segment::Line { a, .. } => Segment::Line { a, b: end },
        Segment::Cubic { a, c1, c2, .. } => Segment::Cubic { a, c1, c2, b: end },
    }
}

/// A segment with a new start point, for a head on the first point.
fn shortened_start(segment: Segment, start: Point) -> Segment {
    let start = keep_within(segment.end(), segment.start(), start);
    match segment {
        Segment::Line { b, .. } => Segment::Line { a: start, b },
        Segment::Cubic { c1, c2, b, .. } => Segment::Cubic {
            a: start,
            c1,
            c2,
            b,
        },
    }
}

/// Keeps a pulled-back endpoint on its own side of the segment: if it has been
/// moved past the far end, use the far end.
fn keep_within(fixed: Point, far: Point, moved: Point) -> Point {
    let reach = far.distance(fixed);
    if moved.distance(fixed) <= reach {
        moved
    } else {
        far
    }
}

/// Draws the path into a canvas already sized for it.
pub(crate) fn draw(canvas: &mut Canvas, path: &PathA) {
    let segments = stroked_segments(path);
    if segments.is_empty() {
        return;
    }

    let color = path.stroke.color;
    let Some(stroked) = build(&segments) else {
        return;
    };

    let style = Stroke {
        // A zero width would become a hairline, which is never what a tool that
        // offers widths 1-12 means.
        width: path.stroke.width.max(0.1),
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    canvas.pixmap.stroke_path(
        &stroked,
        &Canvas::paint(color),
        &style,
        canvas.transform,
        None,
    );

    // Round caps, drawn by hand: tiny-skia gives a path one cap style for both
    // ends, and SPEC wants a round cap only where there is no head.
    for at in cap_positions(path) {
        fill_circle(canvas, at, path.stroke.width / 2.0, color);
    }

    for placement in arrows(path) {
        let head = placement.head;
        let Some(triangle) = triangle([head.tip, head.base_left, head.base_right]) else {
            continue;
        };
        canvas.pixmap.fill_path(
            &triangle,
            &Canvas::paint(color),
            FillRule::Winding,
            canvas.transform,
            None,
        );
    }
}

/// A closed three-point path.
fn triangle(points: [Point; 3]) -> Option<tiny_skia::Path> {
    let mut builder = PathBuilder::new();
    builder.move_to(points[0].x, points[0].y);
    builder.line_to(points[1].x, points[1].y);
    builder.line_to(points[2].x, points[2].y);
    builder.close();
    builder.finish()
}

/// The points that need a round cap: the ends with no arrowhead.
fn cap_positions(path: &PathA) -> Vec<Point> {
    let first = path.points.first().copied();
    let last = path.points.last().copied();
    let mut out = Vec::new();
    if !path.head_start
        && let Some(point) = first
    {
        out.push(point);
    }
    if !path.head_end
        && let Some(point) = last
    {
        out.push(point);
    }
    out
}

/// Fills a circle, which is what a round cap is.
fn fill_circle(canvas: &mut Canvas, center: Point, radius: f32, color: crate::model::Rgba) {
    let Some(circle) = PathBuilder::from_circle(center.x, center.y, radius.max(0.1)) else {
        return;
    };
    canvas.pixmap.fill_path(
        &circle,
        &Canvas::paint(color),
        FillRule::Winding,
        canvas.transform,
        None,
    );
}

/// Turns segments into a path, cubic control points included.
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
