//! Points, rectangles and the small amount of vector maths the rest of the core
//! needs (SPEC section 4).
//!
//! Coordinates are in **image pixels**, `f32`, origin top-left of the base
//! image. Nothing here knows about zoom: a zoomed view maps these coordinates
//! to the screen, it never changes them (SPEC section 4).

use serde::{Deserialize, Serialize};

/// A point in image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    /// The origin.
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    /// A point at `(x, y)`.
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Straight-line distance to `other`.
    #[must_use]
    pub fn distance(self, other: Self) -> f32 {
        self.distance_squared(other).sqrt()
    }

    /// Squared distance to `other`. Cheaper than [`Point::distance`] and
    /// enough for "is this within tolerance" tests.
    #[must_use]
    pub fn distance_squared(self, other: Self) -> f32 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        dx * dx + dy * dy
    }

    /// The point `fraction` of the way from `self` to `other`.
    #[must_use]
    pub fn lerp(self, other: Self, fraction: f32) -> Self {
        Self {
            x: self.x + (other.x - self.x) * fraction,
            y: self.y + (other.y - self.y) * fraction,
        }
    }

    /// True when both coordinates are finite. Zoom and hit testing use this to
    /// ignore degenerate annotations instead of propagating NaN.
    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl std::ops::Add for Point {
    type Output = Point;
    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl std::ops::Sub for Point {
    type Output = Point;
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl std::ops::Mul<Point> for f32 {
    type Output = Point;
    fn mul(self, rhs: Point) -> Point {
        Point::new(self * rhs.x, self * rhs.y)
    }
}

impl std::ops::Mul<f32> for Point {
    type Output = Point;
    fn mul(self, rhs: f32) -> Point {
        Point::new(self.x * rhs, self.y * rhs)
    }
}

impl std::ops::Div<f32> for Point {
    type Output = Point;
    fn div(self, rhs: f32) -> Self {
        Point::new(self.x / rhs, self.y / rhs)
    }
}

/// A rectangle in image pixels.
///
/// `w` and `h` are always `>= 0`: [`Rect::from_ltrb`] normalizes, and the
/// mutating helpers keep it that way, so `x + w` is always the right edge
/// (SPEC section 4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    /// The empty rectangle at the origin.
    pub const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };

    /// A rectangle from its origin and size, normalizing negative sizes.
    #[must_use]
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self::from_ltrb(x, y, x + w, y + h)
    }

    /// A rectangle from its left, top, right and bottom edges, in any order.
    #[must_use]
    pub fn from_ltrb(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            x: left.min(right),
            y: top.min(bottom),
            w: (right - left).abs(),
            h: (bottom - top).abs(),
        }
    }

    /// The smallest rectangle containing all four points.
    #[must_use]
    pub fn from_points(points: &[Point]) -> Self {
        let Some(&first) = points.first() else {
            return Self::ZERO;
        };
        let (mut left, mut top, mut right, mut bottom) = (first.x, first.y, first.x, first.y);
        for point in &points[1..] {
            left = left.min(point.x);
            top = top.min(point.y);
            right = right.max(point.x);
            bottom = bottom.max(point.y);
        }
        Self::from_ltrb(left, top, right, bottom)
    }

    /// The right edge, `x + w`.
    #[must_use]
    pub fn right(self) -> f32 {
        self.x + self.w
    }

    /// The bottom edge, `y + h`.
    #[must_use]
    pub fn bottom(self) -> f32 {
        self.y + self.h
    }

    /// The middle of the rectangle.
    #[must_use]
    pub fn center(self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// True when the rectangle has no area.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// True when every edge is a real number. A rectangle built by dragging
    /// can hold a NaN if the drag maths overflowed; those are ignored.
    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.w.is_finite() && self.h.is_finite()
    }

    /// True when `p` is inside, edges included.
    #[must_use]
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x && p.x <= self.right() && p.y >= self.y && p.y <= self.bottom()
    }

    /// True when the two rectangles share any area.
    #[must_use]
    pub fn intersects(self, other: Self) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    /// The shared area of two rectangles, or an empty rectangle if they do not
    /// overlap. An empty result is at `(0, 0)`, so callers that only care
    /// whether it is empty should test [`Rect::is_empty`] first.
    #[must_use]
    pub fn intersection(self, other: Self) -> Self {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= left || bottom <= top {
            Self::ZERO
        } else {
            Self::from_ltrb(left, top, right, bottom)
        }
    }

    /// The smallest rectangle containing both.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Self::from_ltrb(
            self.x.min(other.x),
            self.y.min(other.y),
            self.right().max(other.right()),
            self.bottom().max(other.bottom()),
        )
    }

    /// This rectangle grown just enough to contain `p`. Never shrinks.
    #[must_use]
    pub fn expand_to_include(self, p: Point) -> Self {
        Self::from_ltrb(
            self.x.min(p.x),
            self.y.min(p.y),
            self.right().max(p.x),
            self.bottom().max(p.y),
        )
    }

    /// The same rectangle moved by `(dx, dy)`.
    #[must_use]
    pub fn translated(self, dx: f32, dy: f32) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
            ..self
        }
    }

    /// The same rectangle with every edge moved `amount` outwards (a negative
    /// amount shrinks it).
    #[must_use]
    pub fn inflated(self, amount: f32) -> Self {
        Self {
            x: self.x - amount,
            y: self.y - amount,
            w: (self.w + amount * 2.0).max(0.0),
            h: (self.h + amount * 2.0).max(0.0),
        }
    }

    /// The integer pixel rectangle that contains this one: the rasterization
    /// bounds for a shape (SPEC section 4.2).
    #[must_use]
    pub fn round_out(self) -> RectI {
        RectI {
            x: self.x.floor() as i32,
            y: self.y.floor() as i32,
            right: self.right().ceil() as i32,
            bottom: self.bottom().ceil() as i32,
        }
    }
}

/// The integer form of [`Rect`], as returned by [`Rect::round_out`].
///
/// `right` and `bottom` are edges, not sizes: that is what rounding a
/// fractional rectangle needs, and converting to a size is one subtraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RectI {
    pub x: i32,
    pub y: i32,
    pub right: i32,
    pub bottom: i32,
}

impl RectI {
    /// True when the rectangle has no area.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.width() == 0 || self.height() == 0
    }

    /// The width in pixels, never negative.
    #[must_use]
    pub fn width(self) -> u32 {
        (self.right - self.x).max(0) as u32
    }

    /// The height in pixels, never negative.
    #[must_use]
    pub fn height(self) -> u32 {
        (self.bottom - self.y).max(0) as u32
    }

    /// Back to floating point, for building a sprite canvas.
    #[must_use]
    pub fn to_rect(self) -> Rect {
        Rect::from_ltrb(
            self.x as f32,
            self.y as f32,
            self.right as f32,
            self.bottom as f32,
        )
    }

    /// The smallest rectangle containing both, ignoring an empty one.
    ///
    /// The export bounds are built with this: the base image united with the
    /// painted pixels of every sprite (NEXT_STEPS step 11, J2).
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Self {
            x: self.x.min(other.x),
            y: self.y.min(other.y),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// The same rectangle moved by `(dx, dy)`.
    ///
    /// The sprite cache shifts a painted rectangle by the same offset a moved
    /// annotation's sprite is drawn at (NEXT_STEPS step 11, J3).
    #[must_use]
    pub fn translated(self, dx: i32, dy: i32) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
            right: self.right + dx,
            bottom: self.bottom + dy,
        }
    }
}

/// Distance from `p` to the segment `a`-`b`, used by hit testing.
#[must_use]
pub fn distance_to_segment(p: Point, a: Point, b: Point) -> f32 {
    let segment = b - a;
    let length_squared = segment.x * segment.x + segment.y * segment.y;
    if length_squared <= f32::EPSILON {
        // Degenerate segment: it is a point.
        return p.distance(a);
    }
    // Project p onto the segment, clamped to its ends.
    let t = ((p.x - a.x) * segment.x + (p.y - a.y) * segment.y) / length_squared;
    let t = t.clamp(0.0, 1.0);
    let closest = a + segment * t;
    p.distance(closest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_normalize_their_edges() {
        let dragged_up_left = Rect::new(100.0, 80.0, -40.0, -30.0);
        assert_eq!(
            dragged_up_left,
            Rect {
                x: 60.0,
                y: 50.0,
                w: 40.0,
                h: 30.0
            }
        );
        assert_eq!(dragged_up_left.right(), 100.0);
        assert_eq!(dragged_up_left.bottom(), 80.0);
    }

    #[test]
    fn from_points_covers_every_point() {
        let bounds = Rect::from_points(&[
            Point::new(10.0, 40.0),
            Point::new(-5.0, 3.0),
            Point::new(7.0, 20.0),
        ]);
        assert_eq!(bounds, Rect::new(-5.0, 3.0, 15.0, 37.0));
    }

    #[test]
    fn from_points_of_nothing_is_the_empty_rectangle() {
        assert_eq!(Rect::from_points(&[]), Rect::ZERO);
        assert!(Rect::from_points(&[]).is_empty());
    }

    #[test]
    fn intersection_of_overlapping_rectangles() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert!(a.intersects(b));
        assert_eq!(a.intersection(b), Rect::new(5.0, 5.0, 5.0, 5.0));
    }

    #[test]
    fn intersection_of_disjoint_rectangles_is_empty() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(20.0, 0.0, 10.0, 10.0);
        assert!(!a.intersects(b));
        assert!(a.intersection(b).is_empty());

        // Touching edges do not count as overlapping.
        let touching = Rect::new(10.0, 0.0, 5.0, 5.0);
        assert!(!a.intersects(touching));
    }

    #[test]
    fn union_covers_both() {
        let a = Rect::new(-100.0, 0.0, 100.0, 50.0);
        let b = Rect::new(0.0, 100.0, 300.0, 200.0);
        assert_eq!(a.union(b), Rect::new(-100.0, 0.0, 400.0, 300.0));
    }

    #[test]
    fn union_with_an_empty_rectangle_is_the_other_one() {
        let real = Rect::new(3.0, 4.0, 5.0, 6.0);
        assert_eq!(real.union(Rect::ZERO), real);
        assert_eq!(Rect::ZERO.union(real), real);
    }

    #[test]
    fn rounding_out_contains_the_fractional_rectangle() {
        let rounded = Rect::new(10.4, 20.6, 5.3, 4.2).round_out();
        assert_eq!(rounded.x, 10);
        assert_eq!(rounded.y, 20);
        assert_eq!(rounded.right, 16);
        assert_eq!(rounded.bottom, 25);
        assert_eq!((rounded.width(), rounded.height()), (6, 5));

        let original = Rect::new(10.4, 20.6, 5.3, 4.2);
        let as_int = rounded.to_rect();
        assert!(as_int.contains(original.center()));
        assert!(as_int.contains(Point::new(original.x, original.y)));
        assert!(as_int.contains(Point::new(original.right(), original.bottom())));
    }

    #[test]
    fn inflating_never_produces_a_negative_size() {
        let tiny = Rect::new(0.0, 0.0, 2.0, 2.0).inflated(-10.0);
        assert_eq!((tiny.w, tiny.h), (0.0, 0.0));
    }

    #[test]
    fn contains_includes_the_edges() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(Point::new(0.0, 0.0)));
        assert!(r.contains(Point::new(10.0, 10.0)));
        assert!(!r.contains(Point::new(10.1, 5.0)));
    }

    #[test]
    fn distance_to_a_segment() {
        let a = Point::new(0.0, 0.0);
        let b = Point::new(10.0, 0.0);

        assert_eq!(distance_to_segment(Point::new(5.0, 3.0), a, b), 3.0);
        // Beyond the ends, the distance is to the end point.
        assert_eq!(distance_to_segment(Point::new(-4.0, 0.0), a, b), 4.0);
        // A zero-length segment behaves like a point.
        assert_eq!(distance_to_segment(Point::new(3.0, 4.0), a, a), 5.0);
    }

    #[test]
    fn lerp_and_arithmetic() {
        let a = Point::new(0.0, 0.0);
        let b = Point::new(10.0, 20.0);
        assert_eq!(a.lerp(b, 0.5), Point::new(5.0, 10.0));
        assert_eq!(a + b, Point::new(10.0, 20.0));
        assert_eq!(b - a, Point::new(10.0, 20.0));
        assert_eq!(b * 0.5, Point::new(5.0, 10.0));
        assert_eq!(b / 2.0, Point::new(5.0, 10.0));
    }

    #[test]
    fn rect_i_union_covers_both_and_ignores_the_empty_one() {
        let a = RectI {
            x: -5,
            y: 3,
            right: 10,
            bottom: 8,
        };
        let b = RectI {
            x: 8,
            y: -2,
            right: 20,
            bottom: 4,
        };
        assert_eq!(
            a.union(b),
            RectI {
                x: -5,
                y: -2,
                right: 20,
                bottom: 8
            }
        );

        let empty = RectI::default();
        assert_eq!(a.union(empty), a);
        assert_eq!(empty.union(a), a);
        assert_eq!(empty.union(empty), empty);
    }

    #[test]
    fn rect_i_translated_moves_both_edges() {
        let rect = RectI {
            x: 2,
            y: -3,
            right: 7,
            bottom: 5,
        };
        assert_eq!(
            rect.translated(-4, 10),
            RectI {
                x: -2,
                y: 7,
                right: 3,
                bottom: 15
            }
        );
        assert_eq!(
            (
                rect.translated(0, 0).width(),
                rect.translated(0, 0).height()
            ),
            (5, 8)
        );
    }
}
