//! Hit testing, handles, dragging, moving and cropping (SPEC section 4.1 and
//! section 7.2).
//!
//! These are the operations the canvas performs on annotations. They are here,
//! in the core crate, rather than in the app because they are pure geometry:
//! every one of them is unit-tested here (SPEC section 11.4) instead of being
//! verified by clicking in a window nobody can see.

use std::sync::Arc;

use crate::geom::{Point, Rect, distance_to_segment};
use crate::model::{Annotation, Document, Kind, NumberA, PathA};
use crate::path;
use crate::render;
use crate::text::Fonts;

/// Which of the eight ways a rectangle can be grabbed (SPEC section 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeDir {
    NorthWest,
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
}

impl ResizeDir {
    /// The eight handles, in the order the properties panel lists corners
    /// first: corners, then edge midpoints.
    pub const ALL: [Self; 8] = [
        Self::NorthWest,
        Self::North,
        Self::NorthEast,
        Self::East,
        Self::SouthEast,
        Self::South,
        Self::SouthWest,
        Self::West,
    ];

    /// Where the handle sits on a rectangle.
    #[must_use]
    pub fn at(self, rect: Rect) -> Point {
        let left = rect.x;
        let center_x = rect.x + rect.w / 2.0;
        let right = rect.right();
        let top = rect.y;
        let center_y = rect.y + rect.h / 2.0;
        let bottom = rect.bottom();

        match self {
            Self::NorthWest => Point::new(left, top),
            Self::North => Point::new(center_x, top),
            Self::NorthEast => Point::new(right, top),
            Self::East => Point::new(right, center_y),
            Self::SouthEast => Point::new(right, bottom),
            Self::South => Point::new(center_x, bottom),
            Self::SouthWest => Point::new(left, bottom),
            Self::West => Point::new(left, center_y),
        }
    }
}

/// What a handle does when it is dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    /// One of a path's or polygon's own points.
    Point(usize),
    /// The small hollow handle at the middle of a segment: dragging it inserts
    /// a new point there.
    AddPoint(usize),
    /// One of the eight corners or edge midpoints of a rectangle-shaped
    /// annotation.
    Resize(ResizeDir),
    /// The tip of a number's pointer.
    PointerTip,
}

/// A draggable point on an annotation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Handle {
    pub kind: HandleKind,
    /// Where it is, in image pixels.
    pub at: Point,
}

/// A handle identified by its position in [`handles`].
///
/// The editor holds one of these across a drag, so it re-resolves the handle
/// list every mouse move rather than holding a reference into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandleId(pub usize);

/// The handles an annotation offers (SPEC section 7.2).
#[must_use]
pub fn handles(a: &Annotation) -> Vec<Handle> {
    match &a.kind {
        Kind::Rect(rect) => resize_handles(rect.rect),
        Kind::Highlight(highlight) => resize_handles(highlight.rect),
        Kind::Blur(blur) => resize_handles(blur.rect),
        Kind::Path(p) => point_handles(&p.points, p.curved),
        Kind::Polygon(p) => point_handles(&p.points, false),
        // Text moves but is not reshaped by handles.
        Kind::Text(_) => Vec::new(),
        // A pasted image moves only; resizing it is out of scope (NEXT_STEPS
        // step 11).
        Kind::Image(_) => Vec::new(),
        // A freehand stroke moves only: point handles on a scribble would be
        // hundreds of handles and no use (NEXT_STEPS step 14a).
        Kind::Freehand(_) => Vec::new(),
        Kind::Number(number) => number
            .pointer_tip
            .map(|tip| Handle {
                kind: HandleKind::PointerTip,
                at: tip,
            })
            .into_iter()
            .collect(),
    }
}

/// The eight handles of a rectangle.
fn resize_handles(rect: Rect) -> Vec<Handle> {
    ResizeDir::ALL
        .iter()
        .map(|&dir| Handle {
            kind: HandleKind::Resize(dir),
            at: dir.at(rect),
        })
        .collect()
}

/// One handle per point, plus an "add point" handle on every segment.
///
/// For a curved path the add handles sit **on the curve**, at the midpoint of
/// the cubic, so dragging one inserts a point where the user is pointing
/// (SPEC section 7.2).
fn point_handles(points: &[Point], curved: bool) -> Vec<Handle> {
    let mut out: Vec<Handle> = points
        .iter()
        .enumerate()
        .map(|(index, &at)| Handle {
            kind: HandleKind::Point(index),
            at,
        })
        .collect();

    for (index, segment) in path::segments(points, curved).iter().enumerate() {
        out.push(Handle {
            kind: HandleKind::AddPoint(index),
            at: segment.point_at(0.5),
        });
    }

    out
}

/// True when `p` is on this annotation, within `tolerance` (SPEC section 7.3).
///
/// A filled shape is hit anywhere inside it; an unfilled one only near its
/// outline. That is what makes clicking inside an unfilled rectangle select
/// whatever is behind it instead.
#[must_use]
pub fn hit_test(a: &Annotation, p: Point, tolerance: f32, fonts: &mut Fonts) -> bool {
    match &a.kind {
        Kind::Rect(rect) => hit_rect(
            rect.rect,
            rect.stroke.as_ref(),
            rect.fill.is_some(),
            p,
            tolerance,
        ),
        Kind::Polygon(polygon) => hit_polygon(
            &polygon.points,
            polygon.stroke.as_ref(),
            polygon.fill.is_some(),
            p,
            tolerance,
        ),
        Kind::Path(path) => hit_path(path, p, tolerance),
        Kind::Highlight(highlight) => rect_contains(highlight.rect, p, tolerance),
        Kind::Blur(blur) => rect_contains(blur.rect, p, tolerance),
        Kind::Text(_) => render::shape_bounds(a, fonts).contains(p),
        // A pasted image is hit anywhere inside it, like an effect.
        Kind::Image(_) => render::shape_bounds(a, fonts).contains(p),
        Kind::Number(number) => hit_number(number, p, tolerance),
        Kind::Freehand(stroke) => hit_freehand(stroke, p, tolerance),
    }
}

/// Near the outline, or anywhere inside when filled.
fn hit_rect(
    rect: Rect,
    stroke: Option<&crate::model::Stroke>,
    filled: bool,
    p: Point,
    tolerance: f32,
) -> bool {
    if filled {
        return rect.inflated(tolerance).contains(p);
    }
    let reach = stroke.map_or(0.0, |s| s.width / 2.0) + tolerance;
    edge_distance(rect, p) <= reach
}

fn rect_contains(rect: Rect, p: Point, tolerance: f32) -> bool {
    rect.inflated(tolerance).contains(p)
}

/// The distance from `p` to the nearest edge of `rect`.
fn edge_distance(rect: Rect, p: Point) -> f32 {
    if rect.is_empty() {
        return p.distance(rect.center());
    }
    let corners = [
        Point::new(rect.x, rect.y),
        Point::new(rect.right(), rect.y),
        Point::new(rect.right(), rect.bottom()),
        Point::new(rect.x, rect.bottom()),
    ];
    // The ring closes: the last edge joins back to the first, so the left edge
    // of the rectangle is measured too.
    corners
        .iter()
        .zip(corners.iter().cycle().skip(1))
        .map(|(from, to)| distance_to_segment(p, *from, *to))
        .fold(f32::INFINITY, f32::min)
}

fn hit_path(path: &PathA, p: Point, tolerance: f32) -> bool {
    let reach = path.stroke.width / 2.0 + tolerance;
    let line = path::polyline(&path.points, path.curved);
    line.windows(2)
        .any(|pair| distance_to_segment(p, pair[0], pair[1]) <= reach)
}

/// A freehand stroke is hit near its curve only: it is not a filled shape, so
/// the inside of its bounding box is not part of it (NEXT_STEPS step 14a).
fn hit_freehand(stroke: &crate::model::FreehandA, p: Point, tolerance: f32) -> bool {
    let reach = stroke.stroke.width / 2.0 + tolerance;
    let line = path::polyline(&stroke.points, true);
    line.windows(2)
        .any(|pair| distance_to_segment(p, pair[0], pair[1]) <= reach)
}

fn hit_polygon(
    points: &[Point],
    stroke: Option<&crate::model::Stroke>,
    filled: bool,
    p: Point,
    tolerance: f32,
) -> bool {
    if points.len() < 3 {
        return false;
    }
    let closed: Vec<Point> = points
        .iter()
        .copied()
        .chain(points.first().copied())
        .collect();
    if filled && point_in_polygon(p, points) {
        return true;
    }
    let reach = stroke.map_or(0.0, |s| s.width / 2.0) + tolerance;
    closed
        .windows(2)
        .any(|pair| distance_to_segment(p, pair[0], pair[1]) <= reach)
}

fn hit_number(number: &NumberA, p: Point, tolerance: f32) -> bool {
    let reach = number.radius + tolerance;
    if p.distance(number.center) <= reach {
        return true;
    }
    number
        .pointer_tip
        .is_some_and(|tip| distance_to_segment(p, number.center, tip) <= tolerance)
}

/// Even-odd ray casting, the same rule the fill uses.
fn point_in_polygon(p: Point, points: &[Point]) -> bool {
    let mut inside = false;
    let count = points.len();
    for index in 0..count {
        let a = points[index];
        let b = points[(index + 1) % count];
        if (a.y > p.y) != (b.y > p.y) {
            let crossing = (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x;
            if p.x < crossing {
                inside = !inside;
            }
        }
    }
    inside
}

/// Moves a handle to `to` (SPEC section 4.1).
///
/// A `Point` handle moves that point. A `Resize` handle drags the edges it
/// names, keeping the opposite ones where they are. An `AddPoint` handle
/// inserts a new point **after** the segment it belongs to and moves it there.
///
/// Note for the canvas: after an `AddPoint` insert, the new point's index is
/// `segment index + 1`, and the drag should continue as
/// [`HandleKind::Point(index + 1`], because the handle list has changed shape.
/// That is why this function returns nothing and why the canvas re-resolves
/// handles from a [`HandleId`] on every mouse move.
pub fn drag_handle(a: &mut Annotation, h: HandleId, to: Point) {
    let Some(handle) = handles(a).get(h.0).copied() else {
        return;
    };

    match (&mut a.kind, handle.kind) {
        (Kind::Path(path), HandleKind::Point(index))
        | (Kind::Path(path), HandleKind::AddPoint(index)) => {
            if matches!(handle.kind, HandleKind::AddPoint(_)) {
                // SPEC 11.4: inserting a point must not move the others.
                let at = index + 1;
                if at <= path.points.len() {
                    path.points.insert(at, to);
                }
                return;
            }
            if let Some(point) = path.points.get_mut(index) {
                *point = to;
            }
        }
        (Kind::Polygon(polygon), HandleKind::Point(index))
        | (Kind::Polygon(polygon), HandleKind::AddPoint(index)) => {
            if matches!(handle.kind, HandleKind::AddPoint(_)) {
                let at = index + 1;
                if at <= polygon.points.len() {
                    polygon.points.insert(at, to);
                }
                return;
            }
            if let Some(point) = polygon.points.get_mut(index) {
                *point = to;
            }
        }
        (Kind::Rect(rect), HandleKind::Resize(dir)) => rect.rect = resize(rect.rect, dir, to),
        (Kind::Highlight(highlight), HandleKind::Resize(dir)) => {
            highlight.rect = resize(highlight.rect, dir, to);
        }
        (Kind::Blur(blur), HandleKind::Resize(dir)) => {
            blur.rect = resize(blur.rect, dir, to);
        }
        (Kind::Number(number), HandleKind::PointerTip) => number.pointer_tip = Some(to),
        _ => {}
    }
}

/// Drags one edge of a rectangle. Sizes stay non-negative (SPEC section 4).
pub fn resize_rect(rect: Rect, dir: ResizeDir, to: Point) -> Rect {
    resize(rect, dir, to)
}

/// The internal name the annotation handles use.
fn resize(rect: Rect, dir: ResizeDir, to: Point) -> Rect {
    let (mut left, mut top, mut right, mut bottom) = (rect.x, rect.y, rect.right(), rect.bottom());
    match dir {
        ResizeDir::NorthWest => {
            left = to.x;
            top = to.y;
        }
        ResizeDir::North => top = to.y,
        ResizeDir::NorthEast => {
            right = to.x;
            top = to.y;
        }
        ResizeDir::East => right = to.x,
        ResizeDir::SouthEast => {
            right = to.x;
            bottom = to.y;
        }
        ResizeDir::South => bottom = to.y,
        ResizeDir::SouthWest => {
            left = to.x;
            bottom = to.y;
        }
        ResizeDir::West => left = to.x,
    }
    Rect::from_ltrb(left, top, right, bottom)
}

/// Moves an annotation by `(dx, dy)` in image pixels (SPEC section 4.1).
pub fn translate(a: &mut Annotation, dx: f32, dy: f32) {
    let delta = Point::new(dx, dy);
    match &mut a.kind {
        Kind::Rect(rect) => rect.rect = rect.rect.translated(dx, dy),
        Kind::Highlight(highlight) => highlight.rect = highlight.rect.translated(dx, dy),
        Kind::Blur(blur) => blur.rect = blur.rect.translated(dx, dy),
        Kind::Path(path) => {
            for point in &mut path.points {
                *point = *point + delta;
            }
        }
        Kind::Polygon(polygon) => {
            for point in &mut polygon.points {
                *point = *point + delta;
            }
        }
        Kind::Freehand(stroke) => {
            for point in &mut stroke.points {
                *point = *point + delta;
            }
        }
        Kind::Text(text) => text.pos = text.pos + delta,
        Kind::Number(number) => {
            number.center = number.center + delta;
            number.pointer_tip = number.pointer_tip.map(|tip| tip + delta);
        }
        // A pasted image stays on whole pixels, so it is never resampled
        // (NEXT_STEPS step 11).
        Kind::Image(image) => {
            image.pos = Point::new(
                (image.pos.x + delta.x).round(),
                (image.pos.y + delta.y).round(),
            );
        }
    }
}

/// The offset `to` is from `from`, when `to` is exactly `from` translated and
/// nothing else about it changed.
///
/// The editor's sprite cache uses this (SPEC 12 M7's performance note): while an
/// annotation is being dragged its shape does not change, so the rasterized
/// sprite can be reused and only its origin moves. Every field is compared
/// explicitly — the moving fields to within a thousandth of a pixel, because
/// `(p + d1) + d2` and `p + (d1 + d2)` are not always the same float, and an
/// exact comparison made the cache re-rasterize at random.
///
/// Blur and Highlight never qualify: their pixels come from the base image
/// *under* the rectangle, so moving them changes every pixel.
#[must_use]
pub fn translation_between(from: &Annotation, to: &Annotation) -> Option<(f32, f32)> {
    if from.id != to.id || from.shadow != to.shadow {
        return None;
    }
    let (dx, dy) = match (&from.kind, &to.kind) {
        (Kind::Path(a), Kind::Path(b)) => {
            let (first, last) = (a.points.first()?, b.points.first()?);
            (last.x - first.x, last.y - first.y)
        }
        (Kind::Rect(a), Kind::Rect(b)) => (b.rect.x - a.rect.x, b.rect.y - a.rect.y),
        (Kind::Polygon(a), Kind::Polygon(b)) => {
            let (first, last) = (a.points.first()?, b.points.first()?);
            (last.x - first.x, last.y - first.y)
        }
        (Kind::Text(a), Kind::Text(b)) => (b.pos.x - a.pos.x, b.pos.y - a.pos.y),
        (Kind::Number(a), Kind::Number(b)) => (b.center.x - a.center.x, b.center.y - a.center.y),
        (Kind::Image(a), Kind::Image(b)) => (b.pos.x - a.pos.x, b.pos.y - a.pos.y),
        (Kind::Freehand(a), Kind::Freehand(b)) => {
            let (first, last) = (a.points.first()?, b.points.first()?);
            (last.x - first.x, last.y - first.y)
        }
        _ => return None,
    };
    moved_shape_within(from, to, dx, dy).then_some((dx, dy))
}

/// How close two coordinates have to be to count as "the same pixel": far
/// below anything the rasterizer or the display can show, far above the
/// rounding error of adding two `f32`s a few thousand pixels wide.
const TRANSLATION_TOLERANCE: f32 = 0.001;

/// Whether `to` is `from` with every point moved by `(dx, dy)` and every other
/// field untouched.
fn moved_shape_within(from: &Annotation, to: &Annotation, dx: f32, dy: f32) -> bool {
    let near = |value: f32| value.abs() <= TRANSLATION_TOLERANCE;
    let moved = |a: Point, b: Point| near(a.x + dx - b.x) && near(a.y + dy - b.y);
    let same_points = |a: &[Point], b: &[Point]| {
        a.len() == b.len() && a.iter().zip(b).all(|(p, q)| moved(*p, *q))
    };

    match (&from.kind, &to.kind) {
        (Kind::Path(a), Kind::Path(b)) => {
            same_points(&a.points, &b.points)
                && a.curved == b.curved
                && a.stroke == b.stroke
                && a.head_start == b.head_start
                && a.head_end == b.head_end
        }
        (Kind::Rect(a), Kind::Rect(b)) => {
            a.stroke == b.stroke
                && a.fill == b.fill
                && near(a.corner_radius - b.corner_radius)
                && near(a.rect.x + dx - b.rect.x)
                && near(a.rect.y + dy - b.rect.y)
                && near(a.rect.w - b.rect.w)
                && near(a.rect.h - b.rect.h)
        }
        (Kind::Polygon(a), Kind::Polygon(b)) => {
            same_points(&a.points, &b.points) && a.stroke == b.stroke && a.fill == b.fill
        }
        (Kind::Text(a), Kind::Text(b)) => {
            a.text == b.text
                && a.font_family == b.font_family
                && near(a.size - b.size)
                && a.bold == b.bold
                && a.italic == b.italic
                && a.color == b.color
                && a.background == b.background
                && a.outline == b.outline
                && near(a.padding - b.padding)
                && moved(a.pos, b.pos)
        }
        (Kind::Number(a), Kind::Number(b)) => {
            a.value == b.value
                && a.fill == b.fill
                && near(a.radius - b.radius)
                && moved(a.center, b.center)
                && match (a.pointer_tip, b.pointer_tip) {
                    (None, None) => true,
                    (Some(tip), Some(other)) => moved(tip, other),
                    _ => false,
                }
        }
        // A pasted image's texture never changes, so a move reuses its pixels
        // (NEXT_STEPS step 11). The pixels are compared by address: two equal
        // images still rasterize the same, but only the same buffer can be
        // assumed to.
        (Kind::Image(a), Kind::Image(b)) => Arc::ptr_eq(&a.image, &b.image) && moved(a.pos, b.pos),
        // A freehand stroke's shape is its points and its style
        // (NEXT_STEPS step 14a).
        (Kind::Freehand(a), Kind::Freehand(b)) => {
            same_points(&a.points, &b.points) && a.stroke == b.stroke
        }
        // Effects read the base image under their rectangle: never reused.
        _ => false,
    }
}

/// Crops a document to `r`: a new base image, with the annotations moved to
/// match (SPEC section 4.1).
///
/// The rectangle is clamped to the base image, so a crop cannot ask for pixels
/// that are not there. Annotations are only ever moved, never deleted: one that
/// ends up outside the cropped image can still be dragged back, and undo puts
/// everything back anyway.
#[must_use]
pub fn crop(doc: &Document, r: Rect) -> Document {
    let area = clip_to_image(
        r.round_out(),
        doc.base.width() as i32,
        doc.base.height() as i32,
    );
    let width = area.width();
    let height = area.height();

    let base = if width == 0 || height == 0 {
        image::RgbaImage::new(0, 0)
    } else {
        image::DynamicImage::ImageRgba8(doc.base.as_ref().clone())
            .crop_imm(area.x as u32, area.y as u32, width, height)
            .to_rgba8()
    };

    let mut out = Document::new(base);
    out.next_number = doc.next_number;
    out.next_id = doc.next_id;
    out.annotations = doc.annotations.clone();

    for annotation in &mut out.annotations {
        translate(annotation, -(area.x as f32), -(area.y as f32));
    }

    out
}

/// Clamps an integer rectangle to the image, keeping its origin.
fn clip_to_image(rect: crate::geom::RectI, width: i32, height: i32) -> crate::geom::RectI {
    let left = rect.x.clamp(0, width.max(0));
    let top = rect.y.clamp(0, height.max(0));
    let right = rect.right.clamp(left, width.max(0));
    let bottom = rect.bottom.clamp(top, height.max(0));
    crate::geom::RectI {
        x: left,
        y: top,
        right,
        bottom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HighlightA, ImageA, PolygonA, RectA, Stroke, TextA, defaults};
    use image::RgbaImage;

    fn line(points: &[(f32, f32)], curved: bool) -> Annotation {
        Annotation {
            id: 1,
            shadow: false,
            kind: Kind::Path(PathA {
                points: points.iter().map(|&(x, y)| Point::new(x, y)).collect(),
                curved,
                stroke: Stroke {
                    width: 4.0,
                    ..defaults::stroke()
                },
                head_start: false,
                head_end: false,
            }),
        }
    }

    fn rectangle() -> Annotation {
        Annotation {
            id: 2,
            shadow: false,
            kind: Kind::Rect(RectA {
                rect: Rect::new(100.0, 100.0, 80.0, 40.0),
                stroke: Some(defaults::stroke()),
                fill: None,
                corner_radius: 0.0,
            }),
        }
    }

    fn highlight() -> Annotation {
        Annotation {
            id: 3,
            shadow: false,
            kind: Kind::Highlight(HighlightA {
                rect: Rect::new(0.0, 0.0, 20.0, 20.0),
                color: defaults::HIGHLIGHT_COLOR,
            }),
        }
    }

    fn pasted_image() -> Annotation {
        Annotation {
            id: 7,
            shadow: false,
            kind: Kind::Image(ImageA {
                pos: Point::new(30.0, 40.0),
                image: Arc::new(RgbaImage::from_pixel(5, 4, image::Rgba([9, 8, 7, 255]))),
            }),
        }
    }

    /// A horizontal freehand stroke at y = 20, 40 px long, 8 px wide
    /// (NEXT_STEPS step 14a).
    fn freehand() -> Annotation {
        Annotation {
            id: 8,
            shadow: false,
            kind: Kind::Freehand(crate::model::FreehandA {
                points: vec![Point::new(10.0, 20.0), Point::new(50.0, 20.0)],
                stroke: Stroke {
                    width: 8.0,
                    ..defaults::stroke()
                },
            }),
        }
    }

    fn fonts() -> Fonts {
        Fonts::bundled()
    }

    #[test]
    fn hitting_a_stroke_from_a_distance_misses() {
        // SPEC 11.4: hit testing hits on the stroke and misses 20 px away.
        let annotation = line(&[(10.0, 10.0), (100.0, 10.0)], false);

        assert!(hit_test(
            &annotation,
            Point::new(50.0, 10.0),
            4.0,
            &mut fonts()
        ));
        assert!(
            hit_test(&annotation, Point::new(50.0, 14.0), 4.0, &mut fonts()),
            "within tolerance"
        );
        assert!(!hit_test(
            &annotation,
            Point::new(50.0, 30.0),
            4.0,
            &mut fonts()
        ));
    }

    #[test]
    fn an_unfilled_rectangle_is_only_hit_on_its_edge() {
        let annotation = rectangle();
        let mut fonts = fonts();

        assert!(
            hit_test(&annotation, Point::new(100.0, 120.0), 4.0, &mut fonts),
            "on the left edge"
        );
        assert!(
            !hit_test(&annotation, Point::new(140.0, 120.0), 4.0, &mut fonts),
            "inside the hole"
        );
    }

    #[test]
    fn a_filled_rectangle_is_hit_anywhere_inside() {
        let mut annotation = rectangle();
        let Kind::Rect(rect) = &mut annotation.kind else {
            panic!("expected a rectangle");
        };
        rect.fill = Some([255, 0, 0, 255]);

        assert!(hit_test(
            &annotation,
            Point::new(140.0, 120.0),
            4.0,
            &mut fonts()
        ));
        assert!(
            !hit_test(&annotation, Point::new(95.0, 120.0), 4.0, &mut fonts()),
            "outside"
        );
    }

    #[test]
    fn effects_are_hit_anywhere_inside_their_rectangle() {
        let annotation = highlight();
        assert!(hit_test(
            &annotation,
            Point::new(10.0, 10.0),
            0.0,
            &mut fonts()
        ));
        assert!(!hit_test(
            &annotation,
            Point::new(30.0, 10.0),
            0.0,
            &mut fonts()
        ));
    }

    #[test]
    fn a_pasted_image_is_hit_inside_its_rectangle_and_has_no_handles() {
        let annotation = pasted_image();
        assert!(hit_test(
            &annotation,
            Point::new(32.0, 42.0),
            0.0,
            &mut fonts()
        ));
        assert!(!hit_test(
            &annotation,
            Point::new(29.0, 42.0),
            0.0,
            &mut fonts()
        ));
        assert!(!hit_test(
            &annotation,
            Point::new(32.0, 45.0),
            0.0,
            &mut fonts()
        ));
        assert!(
            handles(&annotation).is_empty(),
            "a pasted image has no handles"
        );
    }

    #[test]
    fn a_freehand_stroke_is_hit_on_the_stroke_and_not_inside_its_box() {
        // NEXT_STEPS step 14a: distance to the polyline <= tolerance + half the
        // width. A stroke is not a filled shape, so a point inside its bounding
        // box but away from the line must miss.
        let annotation = freehand();
        let mut fonts = fonts();

        assert!(hit_test(
            &annotation,
            Point::new(30.0, 20.0),
            1.0,
            &mut fonts
        ));
        // 4 px off the line: exactly at the edge of the stroke.
        assert!(hit_test(
            &annotation,
            Point::new(30.0, 24.0),
            1.0,
            &mut fonts
        ));
        // 6 px off: outside half the width plus the tolerance.
        assert!(!hit_test(
            &annotation,
            Point::new(30.0, 26.0),
            1.0,
            &mut fonts
        ));
        // Past the end: the round cap reaches half the width beyond it.
        assert!(hit_test(
            &annotation,
            Point::new(6.0, 20.0),
            1.0,
            &mut fonts
        ));
        assert!(!hit_test(
            &annotation,
            Point::new(3.0, 20.0),
            1.0,
            &mut fonts
        ));

        assert!(
            handles(&annotation).is_empty(),
            "a freehand stroke moves only"
        );

        // A diagonal: a point inside its bounding box but far from the stroke
        // must miss — the box is not the shape.
        let diagonal = Annotation {
            id: 9,
            shadow: false,
            kind: Kind::Freehand(crate::model::FreehandA {
                points: vec![Point::new(10.0, 10.0), Point::new(50.0, 50.0)],
                stroke: Stroke {
                    width: 4.0,
                    ..defaults::stroke()
                },
            }),
        };
        assert!(hit_test(&diagonal, Point::new(30.0, 30.0), 1.0, &mut fonts));
        assert!(!hit_test(
            &diagonal,
            Point::new(30.0, 14.0),
            1.0,
            &mut fonts
        ));
    }

    #[test]
    fn crop_moves_a_freehand_stroke_by_the_crop_origin() {
        let mut doc = Document::new(RgbaImage::new(100, 80));
        doc.push(freehand().kind);
        let cropped = crop(&doc, Rect::new(5.0, 10.0, 60.0, 40.0));

        let Kind::Freehand(stroke) = &cropped.annotations[0].kind else {
            panic!("expected a freehand stroke");
        };
        assert_eq!(
            stroke.points,
            vec![Point::new(5.0, 10.0), Point::new(45.0, 10.0)]
        );
    }

    #[test]
    fn a_restyled_freehand_stroke_is_not_a_translation() {
        // The sprite cache must not reuse the old texture when the stroke's
        // colour or width changes (NEXT_STEPS step 14a).
        let original = freehand();
        let mut restyled = original.clone();
        if let Kind::Freehand(stroke) = &mut restyled.kind {
            stroke.stroke.width = 12.0;
        }
        assert_eq!(translation_between(&original, &restyled), None);
    }

    #[test]
    fn a_polygon_is_hit_inside_when_filled() {
        let annotation = Annotation {
            id: 4,
            shadow: false,
            kind: Kind::Polygon(PolygonA {
                points: vec![
                    Point::new(0.0, 0.0),
                    Point::new(100.0, 0.0),
                    Point::new(0.0, 100.0),
                ],
                stroke: Some(defaults::stroke()),
                fill: None,
            }),
        };
        let mut fonts = fonts();

        assert!(
            hit_test(&annotation, Point::new(50.0, 50.0), 4.0, &mut fonts),
            "on the hypotenuse"
        );
        assert!(
            !hit_test(&annotation, Point::new(80.0, 80.0), 4.0, &mut fonts),
            "outside the triangle"
        );

        let mut filled = annotation.clone();
        let Kind::Polygon(polygon) = &mut filled.kind else {
            panic!("expected a polygon");
        };
        polygon.fill = Some([1, 2, 3, 255]);
        assert!(
            hit_test(&filled, Point::new(20.0, 20.0), 4.0, &mut fonts),
            "inside"
        );
        assert!(!hit_test(&filled, Point::new(80.0, 80.0), 4.0, &mut fonts));
    }

    #[test]
    fn rectangles_offer_eight_handles_at_the_edges() {
        let list = handles(&rectangle());
        assert_eq!(list.len(), 8);

        let north_west = list
            .iter()
            .find(|h| h.kind == HandleKind::Resize(ResizeDir::NorthWest))
            .expect("a north-west handle");
        assert_eq!(north_west.at, Point::new(100.0, 100.0));

        let east = list
            .iter()
            .find(|h| h.kind == HandleKind::Resize(ResizeDir::East))
            .expect("an east handle");
        assert_eq!(east.at, Point::new(180.0, 120.0));
    }

    #[test]
    fn effects_offer_the_same_eight_handles() {
        assert_eq!(handles(&highlight()).len(), 8);
    }

    #[test]
    fn a_path_offers_one_handle_per_point_plus_the_segment_middles() {
        let list = handles(&line(&[(0.0, 0.0), (10.0, 0.0), (20.0, 0.0)], false));
        // Three points and two segment midpoints.
        assert_eq!(list.len(), 5);
        assert_eq!(list[0].at, Point::new(0.0, 0.0));
        assert_eq!(
            list[3].at,
            Point::new(5.0, 0.0),
            "the first segment's middle"
        );
        assert_eq!(
            list[4].at,
            Point::new(15.0, 0.0),
            "the second segment's middle"
        );
    }

    #[test]
    fn add_point_handles_sit_on_the_curve() {
        let list = handles(&line(&[(0.0, 0.0), (20.0, 40.0), (40.0, 0.0)], true));
        let middle = list
            .iter()
            .find(|h| h.kind == HandleKind::AddPoint(0))
            .expect("an add-point handle");

        // The curve bulges above the straight line between the points.
        assert!(middle.at.y > 20.0, "the handle was at {}", middle.at.y);
    }

    #[test]
    fn text_has_no_handles_and_a_number_has_only_its_pointer() {
        let text = Annotation {
            id: 5,
            shadow: false,
            kind: Kind::Text(crate::model::TextA {
                pos: Point::new(0.0, 0.0),
                text: "hi".to_owned(),
                font_family: "Segoe UI".to_owned(),
                size: 28.0,
                bold: false,
                italic: false,
                color: [0, 0, 0, 255],
                background: None,
                padding: 0.0,
                outline: None,
            }),
        };
        assert!(handles(&text).is_empty());

        let with_pointer = Annotation {
            id: 6,
            shadow: false,
            kind: Kind::Number(NumberA {
                center: Point::new(10.0, 10.0),
                value: 1,
                radius: 16.0,
                fill: defaults::STROKE_COLOR,
                pointer_tip: Some(Point::new(90.0, 80.0)),
            }),
        };
        let list = handles(&with_pointer);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].at, Point::new(90.0, 80.0));
    }

    #[test]
    fn dragging_a_point_handle_moves_that_point_only() {
        let mut annotation = line(&[(0.0, 0.0), (10.0, 0.0), (20.0, 0.0)], false);
        drag_handle(&mut annotation, HandleId(1), Point::new(10.0, 30.0));

        let Kind::Path(path) = &annotation.kind else {
            panic!("expected a path");
        };
        assert_eq!(
            path.points,
            vec![
                Point::new(0.0, 0.0),
                Point::new(10.0, 30.0),
                Point::new(20.0, 0.0)
            ]
        );
    }

    #[test]
    fn dragging_an_add_point_handle_inserts_without_moving_the_others() {
        // SPEC 11.4: inserting a point must not move the other points.
        let mut annotation = line(&[(0.0, 0.0), (10.0, 0.0), (20.0, 0.0)], false);
        let before = handles(&annotation);
        let add = before
            .iter()
            .position(|h| h.kind == HandleKind::AddPoint(0))
            .expect("an add-point handle");

        drag_handle(&mut annotation, HandleId(add), Point::new(5.0, 5.0));

        let Kind::Path(path) = &annotation.kind else {
            panic!("expected a path");
        };
        assert_eq!(
            path.points,
            vec![
                Point::new(0.0, 0.0),
                Point::new(5.0, 5.0),
                Point::new(10.0, 0.0),
                Point::new(20.0, 0.0)
            ]
        );

        // The dragged point is now index 1, so the drag continues there.
        let after = handles(&annotation);
        let inserted = after
            .iter()
            .position(|h| h.kind == HandleKind::Point(1))
            .expect("the new point has a handle");
        assert_eq!(after[inserted].at, Point::new(5.0, 5.0));
    }

    #[test]
    fn a_corner_handle_resizes_and_keeps_the_opposite_corner() {
        let mut annotation = rectangle();
        let list = handles(&annotation);
        let south_east = list
            .iter()
            .position(|h| h.kind == HandleKind::Resize(ResizeDir::SouthEast))
            .expect("a south-east handle");

        drag_handle(
            &mut annotation,
            HandleId(south_east),
            Point::new(200.0, 160.0),
        );

        let Kind::Rect(rect) = &annotation.kind else {
            panic!("expected a rectangle");
        };
        assert_eq!(rect.rect, Rect::new(100.0, 100.0, 100.0, 60.0));
    }

    #[test]
    fn dragging_a_handle_past_the_opposite_edge_flips_the_rectangle() {
        // Sizes are never negative, so the rectangle turns inside out rather
        // than inverting (SPEC section 4).
        let mut annotation = rectangle();
        let list = handles(&annotation);
        let west = list
            .iter()
            .position(|h| h.kind == HandleKind::Resize(ResizeDir::West))
            .expect("a west handle");

        drag_handle(&mut annotation, HandleId(west), Point::new(150.0, 120.0));

        let Kind::Rect(rect) = &annotation.kind else {
            panic!("expected a rectangle");
        };
        assert_eq!(rect.rect, Rect::new(150.0, 100.0, 30.0, 40.0));
    }

    #[test]
    fn translating_moves_every_kind() {
        let mut annotations = vec![
            rectangle(),
            line(&[(0.0, 0.0), (10.0, 10.0)], false),
            Annotation {
                id: 9,
                shadow: false,
                kind: Kind::Polygon(PolygonA {
                    points: vec![
                        Point::new(0.0, 0.0),
                        Point::new(10.0, 0.0),
                        Point::new(5.0, 9.0),
                    ],
                    stroke: None,
                    fill: Some([0, 0, 0, 255]),
                }),
            },
            highlight(),
            pasted_image(),
            freehand(),
        ];

        let mut fonts = fonts();
        let before: Vec<Rect> = annotations
            .iter()
            .map(|a| render::shape_bounds(a, &mut fonts))
            .collect();

        for annotation in &mut annotations {
            translate(annotation, 5.0, -3.0);
        }

        let after: Vec<Rect> = annotations
            .iter()
            .map(|a| render::shape_bounds(a, &mut fonts))
            .collect();

        for (start, end) in before.iter().zip(after.iter()) {
            assert!((end.x - (start.x + 5.0)).abs() < 0.001);
            assert!((end.y - (start.y - 3.0)).abs() < 0.001);
            assert_eq!((end.w, end.h), (start.w, start.h));
        }
    }

    #[test]
    fn translating_a_number_takes_its_pointer_along() {
        let mut annotation = Annotation {
            id: 10,
            shadow: false,
            kind: Kind::Number(NumberA {
                center: Point::new(10.0, 10.0),
                value: 1,
                radius: 16.0,
                fill: defaults::STROKE_COLOR,
                pointer_tip: Some(Point::new(90.0, 80.0)),
            }),
        };

        translate(&mut annotation, 1.0, 2.0);

        let Kind::Number(number) = &annotation.kind else {
            panic!("expected a number");
        };
        assert_eq!(number.center, Point::new(11.0, 12.0));
        assert_eq!(number.pointer_tip, Some(Point::new(91.0, 82.0)));
    }

    #[test]
    fn translating_a_pasted_image_snaps_it_to_whole_pixels() {
        // NEXT_STEPS step 11: `pos` stays on whole pixels, so the image is
        // never resampled.
        let mut annotation = pasted_image();
        translate(&mut annotation, 1.4, -2.6);
        let Kind::Image(image) = &annotation.kind else {
            panic!("expected a pasted image");
        };
        assert_eq!(image.pos, Point::new(31.0, 37.0));

        // A move by a fraction of a pixel does not move it at all.
        translate(&mut annotation, 0.4, 0.4);
        let Kind::Image(image) = &annotation.kind else {
            panic!("expected a pasted image");
        };
        assert_eq!(image.pos, Point::new(31.0, 37.0));
    }

    #[test]
    fn crop_moves_a_pasted_image_by_the_crop_origin() {
        let mut doc = Document::new(RgbaImage::new(100, 80));
        doc.push(pasted_image().kind);
        let cropped = crop(&doc, Rect::new(5.0, 10.0, 60.0, 40.0));

        let Kind::Image(image) = &cropped.annotations[0].kind else {
            panic!("expected a pasted image");
        };
        assert_eq!(image.pos, Point::new(25.0, 30.0));
    }

    #[test]
    fn crop_moves_the_annotations_by_the_crop_origin() {
        // SPEC 11.4: crop shifts annotations by the crop origin.
        let mut doc = Document::new(RgbaImage::new(200, 100));
        doc.push(rectangle().kind);
        doc.push(line(&[(120.0, 20.0), (160.0, 60.0)], false).kind);

        let cropped = crop(&doc, Rect::new(50.0, 10.0, 60.0, 40.0));

        assert_eq!(cropped.size(), (60, 40));

        let mut fonts = fonts();
        let original: Vec<Rect> = doc
            .annotations
            .iter()
            .map(|a| render::shape_bounds(a, &mut fonts))
            .collect();
        let shifted: Vec<Rect> = cropped
            .annotations
            .iter()
            .map(|a| render::shape_bounds(a, &mut fonts))
            .collect();

        for (before, after) in original.iter().zip(shifted.iter()) {
            assert!((after.x - (before.x - 50.0)).abs() < 0.001);
            assert!((after.y - (before.y - 10.0)).abs() < 0.001);
        }
    }

    #[test]
    fn a_crop_outside_the_image_is_clamped() {
        let doc = Document::new(RgbaImage::new(50, 50));
        let cropped = crop(&doc, Rect::new(-20.0, -20.0, 200.0, 200.0));
        assert_eq!(cropped.size(), (50, 50));

        let empty = crop(&doc, Rect::new(500.0, 500.0, 10.0, 10.0));
        assert_eq!(empty.size(), (0, 0));
    }

    #[test]
    fn crop_keeps_the_numbering_state() {
        let mut doc = Document::new(RgbaImage::new(10, 10));
        doc.next_number = 12;
        doc.next_id = 99;

        let cropped = crop(&doc, Rect::new(0.0, 0.0, 5.0, 5.0));
        assert_eq!(cropped.next_number, 12);
        assert_eq!(cropped.next_id, 99);
    }

    // ---------------------------------------------------------------------
    // M7: the sprite-reuse check behind SPEC 12's drag budget
    // ---------------------------------------------------------------------

    /// Every movable kind: a translated copy is recognised, with the exact
    /// offset, so its sprite can be reused instead of re-rasterized.
    #[test]
    fn a_translation_is_recognised_for_every_kind_that_can_be_moved() {
        let cases: Vec<Annotation> = vec![
            rectangle(),
            line(&[(0.0, 0.0), (10.0, 10.0), (20.0, 5.0)], true),
            Annotation {
                id: 4,
                shadow: true,
                kind: Kind::Polygon(PolygonA {
                    points: vec![
                        Point::new(0.0, 0.0),
                        Point::new(10.0, 0.0),
                        Point::new(5.0, 9.0),
                    ],
                    stroke: None,
                    fill: Some([0, 0, 0, 255]),
                }),
            },
            Annotation {
                id: 5,
                shadow: false,
                kind: Kind::Text(TextA {
                    pos: Point::new(3.0, 4.0),
                    text: "hi".to_owned(),
                    font_family: "Inter".to_owned(),
                    size: 20.0,
                    bold: false,
                    italic: false,
                    color: [0, 0, 0, 255],
                    background: None,
                    padding: 4.0,
                    outline: None,
                }),
            },
            Annotation {
                id: 6,
                shadow: false,
                kind: Kind::Number(NumberA {
                    center: Point::new(10.0, 10.0),
                    value: 3,
                    radius: 16.0,
                    fill: defaults::STROKE_COLOR,
                    pointer_tip: Some(Point::new(90.0, 80.0)),
                }),
            },
            freehand(),
        ];

        for annotation in &cases {
            for (dx, dy) in [(7.0f32, -3.0f32), (0.0, 0.0), (0.5, 12.25)] {
                let mut moved = annotation.clone();
                translate(&mut moved, dx, dy);
                assert_eq!(
                    translation_between(annotation, &moved),
                    Some((dx, dy)),
                    "{:?} moved by ({dx}, {dy}) was not recognised",
                    annotation.kind
                );
            }
        }
    }

    /// A pasted image snaps to whole pixels, so its recognised translation is
    /// the snapped offset — the same one the sprite cache draws it at
    /// (NEXT_STEPS step 11).
    #[test]
    fn a_moved_pasted_image_is_recognised_with_its_whole_pixel_offset() {
        let annotation = pasted_image();
        let mut moved = annotation.clone();
        translate(&mut moved, 7.4, -3.6);
        assert_eq!(translation_between(&annotation, &moved), Some((7.0, -4.0)));

        // Different pixels are not the same image, so the cache must not reuse
        // the old texture.
        let mut replaced = annotation.clone();
        if let Kind::Image(image) = &mut replaced.kind {
            image.image = Arc::new(RgbaImage::from_pixel(5, 4, image::Rgba([1, 2, 3, 255])));
        }
        translate(&mut replaced, 7.0, -4.0);
        assert_eq!(translation_between(&annotation, &replaced), None);
    }

    /// A move applied in several steps must still be recognised: `(p + d1) + d2`
    /// is not always `p + (d1 + d2)` in `f32`, which used to make the sprite
    /// cache re-rasterize at random during a drag.
    #[test]
    fn a_move_applied_in_several_steps_is_still_recognised() {
        let mut stepped = line(&[(100.0, 100.0), (1234.5, 987.25), (3000.75, 40.5)], true);
        let original = stepped.clone();
        let steps = [(3.7f32, -2.3f32), (0.1, 0.4), (11.11, 7.77)];
        for (dx, dy) in steps {
            translate(&mut stepped, dx, dy);
        }

        let (dx, dy) = translation_between(&original, &stepped).expect("a move");
        let expected = (
            steps.iter().map(|(x, _)| x).sum::<f32>(),
            steps.iter().map(|(_, y)| y).sum::<f32>(),
        );
        assert!((dx - expected.0).abs() < 0.01, "{dx} vs {}", expected.0);
        assert!((dy - expected.1).abs() < 0.01, "{dy} vs {}", expected.1);
    }

    #[test]
    fn anything_that_is_not_a_move_is_not_a_translation() {
        let original = rectangle();

        // A shape change.
        let mut resized = original.clone();
        if let Kind::Rect(rect) = &mut resized.kind {
            rect.rect.w += 10.0;
        }
        assert_eq!(translation_between(&original, &resized), None);

        // A style change.
        let mut restyled = original.clone();
        if let Kind::Rect(rect) = &mut restyled.kind
            && let Some(stroke) = &mut rect.stroke
        {
            stroke.width = 9.0;
        }
        assert_eq!(translation_between(&original, &restyled), None);

        // The shadow flag is not part of the pixels' position.
        let mut shadowed = original.clone();
        shadowed.shadow = true;
        assert_eq!(translation_between(&original, &shadowed), None);

        // A different annotation.
        let mut other = original.clone();
        other.id = 99;
        assert_eq!(translation_between(&original, &other), None);

        // Different kinds.
        let mut path = line(&[(100.0, 100.0), (120.0, 120.0)], false);
        path.id = original.id;
        assert_eq!(translation_between(&original, &path), None);

        // Effects read the base image under their rectangle, so a moved one
        // must be re-rendered: never a translation.
        let effect = highlight();
        let mut moved_effect = effect.clone();
        translate(&mut moved_effect, 4.0, 4.0);
        assert_eq!(translation_between(&effect, &moved_effect), None);
    }

    /// NEXT_STEPS step 8: `translation_between` compares Text field by field to
    /// decide "only moved". An outline change must count as a shape change, or
    /// the sprite cache would reuse the stale (unoutlined) sprite.
    #[test]
    fn changing_a_text_outline_is_not_a_translation() {
        let original = Annotation {
            id: 5,
            shadow: false,
            kind: Kind::Text(TextA {
                pos: Point::new(3.0, 4.0),
                text: "hi".to_owned(),
                font_family: "Inter".to_owned(),
                size: 20.0,
                bold: false,
                italic: false,
                color: [0, 0, 0, 255],
                background: None,
                padding: 4.0,
                outline: None,
            }),
        };

        let mut outlined = original.clone();
        if let Kind::Text(text) = &mut outlined.kind {
            text.outline = Some(defaults::stroke());
        }
        assert_eq!(
            translation_between(&original, &outlined),
            None,
            "an outline change was treated as a move"
        );

        // A move of an outlined text is still recognised, so the cache keeps
        // working for the normal case.
        let mut moved = outlined.clone();
        translate(&mut moved, 5.0, -2.0);
        assert_eq!(translation_between(&outlined, &moved), Some((5.0, -2.0)));

        // A changed outline width or colour is a shape change too.
        let mut thicker = outlined.clone();
        if let Kind::Text(text) = &mut thicker.kind
            && let Some(outline) = &mut text.outline
        {
            outline.width = 9.0;
        }
        assert_eq!(translation_between(&outlined, &thicker), None);
    }
}
