//! Curve building: Catmull-Rom to cubic Beziers, and the arrowhead geometry
//! that depends on it (SPEC section 4.2).
//!
//! A path is stored as its points plus a `curved` flag, never as curve
//! segments. Everything that needs the curve — rendering, bounds, hit testing,
//! arrowhead directions — asks this module for segments, so there is exactly
//! one place where "what does a curved path mean" is decided.

use crate::geom::Point;

/// One piece of a path: a straight line or a cubic Bezier.
///
/// A cubic carries the start point so a whole path can be walked without
/// remembering where the previous segment ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    /// From `a` to `b`.
    Line { a: Point, b: Point },
    /// From `a` to `b`, bending through the two control points.
    Cubic {
        a: Point,
        c1: Point,
        c2: Point,
        b: Point,
    },
}

impl Segment {
    /// Where the segment starts.
    #[must_use]
    pub fn start(self) -> Point {
        match self {
            Self::Line { a, .. } | Self::Cubic { a, .. } => a,
        }
    }

    /// Where the segment ends.
    #[must_use]
    pub fn end(self) -> Point {
        match self {
            Self::Line { b, .. } | Self::Cubic { b, .. } => b,
        }
    }

    /// The control points to feed a `PathBuilder::cubic_to`, for a segment
    /// that is known to start where the builder already is.
    #[must_use]
    pub fn control_points(self) -> Option<(Point, Point)> {
        match self {
            Self::Line { .. } => None,
            Self::Cubic { c1, c2, .. } => Some((c1, c2)),
        }
    }

    /// The direction the segment leaves its end point in.
    ///
    /// For a line that is simply towards the other point; for a cubic it is
    /// "end point minus the nearest control point" (SPEC section 4.2). A cubic
    /// whose last control point sits on the end point has no direction from the
    /// curve, so the previous point is used instead.
    #[must_use]
    pub fn end_tangent(self) -> Option<(Point, Point)> {
        match self {
            Self::Line { a, b } => Some((a, b)),
            Self::Cubic { a, c2, b, .. } => {
                let direction = b - c2;
                if direction.distance_squared(Point::ZERO) > 1e-6 {
                    Some((b - direction, b))
                } else {
                    Some((a, b))
                }
            }
        }
    }

    /// The same at the start point: the tip is the start of the segment.
    #[must_use]
    pub fn start_tangent(self) -> Option<(Point, Point)> {
        match self {
            Self::Line { a, b } => Some((b, a)),
            Self::Cubic { a, b, c1, .. } => {
                if (c1 - a).distance_squared(Point::ZERO) > 1e-6 {
                    Some((c1, a))
                } else {
                    Some((b, a))
                }
            }
        }
    }

    /// The point part-way through the segment, used to place "add point"
    /// handles and to sample the curve for hit testing.
    #[must_use]
    pub fn point_at(self, t: f32) -> Point {
        match self {
            Self::Line { a, b } => a.lerp(b, t),
            Self::Cubic { a, c1, c2, b } => {
                let u = 1.0 - t;
                let weights = [
                    (u * u * u, a),
                    (3.0 * u * u * t, c1),
                    (3.0 * u * t * t, c2),
                    (t * t * t, b),
                ];
                weights
                    .into_iter()
                    .fold(Point::ZERO, |sum, (weight, point)| sum + point * weight)
            }
        }
    }
}

/// The pieces of a path, in drawing order.
pub type Segments = Vec<Segment>;

/// Builds the segments for a path (SPEC section 4.2).
///
/// Two points are always a straight line, and so is a path with `curved`
/// false. Three or more points with `curved` true become a smooth Catmull-Rom
/// spline.
#[must_use]
pub fn segments(points: &[Point], curved: bool) -> Segments {
    let Some(rest) = points.get(1..) else {
        return Vec::new();
    };
    if rest.is_empty() {
        return Vec::new();
    }
    if !curved || points.len() == 2 {
        return straight(points);
    }
    catmull_rom(points)
}

/// Straight segments through every point.
#[must_use]
pub fn straight(points: &[Point]) -> Segments {
    points
        .windows(2)
        .map(|pair| Segment::Line {
            a: pair[0],
            b: pair[1],
        })
        .collect()
}

/// A uniform Catmull-Rom spline as cubic Beziers.
///
/// For the segment from `P1` to `P2` with neighbours `P0` before and `P3`
/// after, the control points are `C1 = P1 + (P2 - P0) / 6` and
/// `C2 = P2 - (P3 - P1) / 6`. At the ends `P0 = P1` and `P3 = P2` (SPEC 4.2).
///
/// Every curve therefore starts and ends exactly on an input point, which is
/// what makes the spline pass through all of them.
#[must_use]
pub fn catmull_rom(points: &[Point]) -> Segments {
    let Some((&first, rest)) = points.split_first() else {
        return Vec::new();
    };
    let Some(&last) = points.last() else {
        return Vec::new();
    };
    if rest.is_empty() {
        return Vec::new();
    }

    let mut out = Segments::with_capacity(points.len() - 1);
    for index in 0..points.len() - 1 {
        let p1 = points[index];
        let p2 = points[index + 1];
        let p0 = if index == 0 { first } else { points[index - 1] };
        let p3 = if index + 2 >= points.len() {
            last
        } else {
            points[index + 2]
        };

        let c1 = p1 + (p2 - p0) / 6.0;
        let c2 = p2 - (p3 - p1) / 6.0;
        out.push(Segment::Cubic {
            a: p1,
            c1,
            c2,
            b: p2,
        });
    }
    out
}

/// How many straight pieces a cubic is sampled as when it has to be treated as
/// a polyline (hit testing, "add point" handles).
const SAMPLES_PER_CUBIC: usize = 16;

/// The path as a polyline: the input points plus curve samples between them.
///
/// Used by hit testing and by the "add point" handle positions, both of which
/// want to treat a curve as a dense straight path (SPEC section 7.2).
#[must_use]
pub fn polyline(points: &[Point], curved: bool) -> Vec<Point> {
    let segments = segments(points, curved);
    if segments.is_empty() {
        return points.to_vec();
    }

    let mut out = Vec::with_capacity(points.len() + segments.len() * SAMPLES_PER_CUBIC);
    out.push(segments[0].start());
    for segment in &segments {
        let pieces = match segment {
            Segment::Line { .. } => 1,
            Segment::Cubic { .. } => SAMPLES_PER_CUBIC,
        };
        for step in 1..=pieces {
            out.push(segment.point_at(step as f32 / pieces as f32));
        }
    }
    out
}

/// Ramer–Douglas–Peucker: drops the samples that stay within `tolerance` of
/// the line through the ends of their piece (NEXT_STEPS step 14a).
///
/// The first and last points are always kept. A freehand drag arrives with one
/// point per pointer event, so this is what turns a scribble into a handful of
/// points before it goes into the model.
///
/// Iterative rather than recursive: a scribble can be thousands of samples
/// long, and the worst case would otherwise be a deep stack.
#[must_use]
pub fn simplify(points: &[Point], tolerance: f32) -> Vec<Point> {
    if points.len() < 3 {
        return points.to_vec();
    }

    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;

    // Distance alone cannot see a stroke that retraces its own line (a
    // back-and-forth scribble crossing something out): every sample lies on
    // the segment between the ends. So every turn sharper than 90° is kept
    // first, and the simplification runs between those anchors (Step 14a
    // review). Repeated samples are skipped when finding the direction.
    let mut previous: Option<Point> = None;
    for index in 1..points.len() {
        let delta = points[index] - points[index - 1];
        if delta.x == 0.0 && delta.y == 0.0 {
            continue;
        }
        if let Some(before) = previous
            && before.x * delta.x + before.y * delta.y < 0.0
        {
            keep[index - 1] = true;
        }
        previous = Some(delta);
    }
    let anchors: Vec<usize> = (0..points.len()).filter(|&index| keep[index]).collect();

    let mut stack: Vec<(usize, usize)> =
        anchors.windows(2).map(|pair| (pair[0], pair[1])).collect();
    while let Some((start, end)) = stack.pop() {
        if end <= start + 1 {
            continue;
        }
        let (a, b) = (points[start], points[end]);
        let mut worst = tolerance;
        let mut worst_index = None;
        for (index, &point) in points.iter().enumerate().take(end).skip(start + 1) {
            // The *segment*, not the infinite line: a scribble that doubles
            // back along one row lies on the line but far from the segment
            // (Step 14a review).
            let distance = crate::geom::distance_to_segment(point, a, b);
            if distance > worst {
                worst = distance;
                worst_index = Some(index);
            }
        }
        if let Some(index) = worst_index {
            keep[index] = true;
            stack.push((start, index));
            stack.push((index, end));
        }
    }

    points
        .iter()
        .zip(keep)
        .filter_map(|(&point, kept)| kept.then_some(point))
        .collect()
}

/// An arrowhead triangle, as three points (SPEC section 4.2).
///
/// `tip` is exactly the path's end point. The triangle's **base** sits `L` back
/// from the tip, and [`ArrowPlacement::pulled_back`] says where the *stroke*
/// stops, which is `0.8 * L` back: the two overlap on purpose, so there is no
/// hairline seam between the line and the head.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArrowHead {
    pub tip: Point,
    pub base_centre: Point,
    pub base_left: Point,
    pub base_right: Point,
}

impl ArrowHead {
    /// Builds an arrowhead at `tip`, pointing away from `towards`.
    ///
    /// Length is `max(12, 4 * width)`, half-width `0.4 * length`, and the
    /// stroke ends `0.8 * length` back from the tip along the tangent.
    #[must_use]
    pub fn new(tip: Point, towards: Point, width: f32) -> Option<Self> {
        let direction = tip - towards;
        let length = direction.distance(Point::ZERO);
        if length <= f32::EPSILON {
            // No direction: the tip sits on the previous point.
            return None;
        }
        let unit = direction / length;
        // A perpendicular to the direction, for the base corners.
        let normal = Point::new(-unit.y, unit.x);

        let size = (4.0 * width).max(12.0);
        let half_width = 0.4 * size;
        // The triangle is `size` long, from base to tip.
        let base_centre = tip - unit * size;

        Some(Self {
            tip,
            base_centre,
            base_left: base_centre + normal * half_width,
            base_right: base_centre - normal * half_width,
        })
    }

    /// How far the stroke has to stop short of the tip: `0.8 * L`, which is
    /// inside the triangle rather than at its base.
    #[must_use]
    pub fn pull_back_distance(&self) -> f32 {
        0.8 * self.length()
    }

    /// The triangle's length, from base to tip: `max(12, 4 * width)`.
    #[must_use]
    pub fn length(&self) -> f32 {
        (self.tip - self.base_centre).distance(Point::ZERO)
    }
}

/// Where an arrowhead sits and how far the stroke has to stop short of it.
///
/// The stroke's last point becomes `pulled_back`, which is what keeps the line
/// from poking through the tip (SPEC section 4.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArrowPlacement {
    pub head: ArrowHead,
    /// The point the stroked path must end at instead of the tip.
    pub pulled_back: Point,
}

/// Works out the arrowhead at the end of a path.
///
/// Returns `None` when the tangent is unusable, which happens when every point
/// of the path is in the same place.
#[must_use]
pub fn arrow_at_end(path: &crate::model::PathA) -> Option<ArrowPlacement> {
    let segments = segments(&path.points, path.curved);
    let last = segments.last().copied()?;
    let (towards, tip) = last.end_tangent()?;
    let head = ArrowHead::new(tip, towards, path.stroke.width)?;
    let unit = unit_towards(tip, towards);
    // SPEC 4.2: the stroke stops `0.8 L` back, inside the triangle, so the line
    // cannot poke through the tip and there is no seam at the base.
    let pulled_back = tip - unit * head.pull_back_distance();
    Some(ArrowPlacement { head, pulled_back })
}

/// The same, at the start of the path, for a double arrow.
#[must_use]
pub fn arrow_at_start(path: &crate::model::PathA) -> Option<ArrowPlacement> {
    let segments = segments(&path.points, path.curved);
    let first = segments.first().copied()?;
    let (towards, tip) = first.start_tangent()?;
    let head = ArrowHead::new(tip, towards, path.stroke.width)?;
    let unit = unit_towards(tip, towards);
    let pulled_back = tip - unit * head.pull_back_distance();
    Some(ArrowPlacement { head, pulled_back })
}

/// The unit vector from `towards` to `tip`, or `None`'s stand-in when they
/// coincide.
fn unit_towards(tip: Point, towards: Point) -> Point {
    let direction = tip - towards;
    let length = direction.distance(Point::ZERO);
    if length <= f32::EPSILON {
        return Point::new(1.0, 0.0);
    }
    direction / length
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(values: &[(f32, f32)]) -> Vec<Point> {
        values.iter().map(|&(x, y)| Point::new(x, y)).collect()
    }

    #[test]
    fn a_curve_passes_through_every_point() {
        // SPEC 11.4: the Catmull-Rom curve must pass through every input point.
        let input = points(&[(0.0, 0.0), (10.0, 40.0), (40.0, 10.0), (80.0, 30.0)]);
        let segments = catmull_rom(&input);

        assert_eq!(segments.len(), input.len() - 1);
        for (index, segment) in segments.iter().enumerate() {
            assert_eq!(segment.start(), input[index], "segment {index} start");
            assert_eq!(segment.end(), input[index + 1], "segment {index} end");
        }
    }

    #[test]
    fn control_points_follow_the_formula() {
        let input = points(&[(0.0, 0.0), (30.0, 30.0), (60.0, 0.0), (90.0, 30.0)]);
        let segments = catmull_rom(&input);

        let Segment::Cubic { a, c1, c2, b } = segments[1] else {
            panic!("a curved path must produce cubics");
        };
        let p0 = input[0];
        let p1 = input[1];
        let p2 = input[2];
        let p3 = input[3];

        assert!((c1.x - (p1.x + (p2.x - p0.x) / 6.0)).abs() < 1e-5);
        assert!((c1.y - (p1.y + (p2.y - p0.y) / 6.0)).abs() < 1e-5);
        assert!((c2.x - (p2.x - (p3.x - p1.x) / 6.0)).abs() < 1e-5);
        assert!((c2.y - (p2.y - (p3.y - p1.y) / 6.0)).abs() < 1e-5);
        assert_eq!(a, p1);
        assert_eq!(b, p2);
    }

    #[test]
    fn the_ends_use_the_endpoint_as_their_neighbour() {
        let input = points(&[(0.0, 0.0), (10.0, 10.0), (20.0, 0.0)]);
        let segments = catmull_rom(&input);

        let Segment::Cubic { a, c1, .. } = segments[0] else {
            panic!("expected a cubic");
        };
        // P0 = P1 at the start, so C1 = P1 + (P2 - P1) / 6.
        let expected = a + (input[1] - input[0]) / 6.0;
        assert!((c1.x - expected.x).abs() < 1e-5);
        assert!((c1.y - expected.y).abs() < 1e-5);
    }

    #[test]
    fn two_point_paths_are_straight_even_when_curved() {
        // SPEC 11.4: a 2-point curved path equals a straight line.
        let input = points(&[(3.0, 7.0), (40.0, 12.0)]);
        let segments = segments(&input, true);

        assert_eq!(segments, straight(&input));
        assert!(matches!(segments[0], Segment::Line { .. }));
    }

    #[test]
    fn straight_segments_when_not_curved() {
        let input = points(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]);
        let segments = segments(&input, false);
        assert_eq!(segments.len(), 2);
        assert!(segments.iter().all(|s| matches!(s, Segment::Line { .. })));
    }

    #[test]
    fn degenerate_input_produces_no_segments() {
        assert!(segments(&[], true).is_empty());
        assert!(segments(&[Point::new(1.0, 1.0)], true).is_empty());
    }

    #[test]
    fn a_cubic_midpoint_sits_between_its_endpoints() {
        let input = points(&[(0.0, 0.0), (10.0, 100.0), (20.0, 0.0)]);
        let segments = catmull_rom(&input);
        let middle = segments[0].point_at(0.5);
        let last = segments[0].end();

        // A Bezier stays inside the convex hull of its control points, so the
        // midpoint cannot be left of the segment's own x range or above its
        // peak.
        assert!(middle.x > 0.0 && middle.x < last.x, "x was {}", middle.x);
        assert!(middle.y <= 100.0, "y was {}", middle.y);
    }

    #[test]
    fn the_polyline_starts_and_ends_on_the_real_points() {
        let input = points(&[(0.0, 0.0), (20.0, 30.0), (60.0, 10.0)]);
        let line = polyline(&input, true);
        assert_eq!(line.first(), Some(&input[0]));
        assert_eq!(line.last(), Some(&input[input.len() - 1]));
        assert!(line.len() > input.len(), "a curve needs samples");
    }

    #[test]
    fn an_arrowhead_size_follows_the_spec() {
        let tip = Point::new(100.0, 0.0);
        let head = ArrowHead::new(tip, Point::new(0.0, 0.0), 4.0).expect("a direction exists");

        // L = max(12, 4 * 4) = 16, half-width 0.4 * 16 = 6.4. The triangle runs
        // from its base (at 100 - 16) to the tip (at 100).
        assert!((head.tip - tip).distance(Point::ZERO) < 1e-5);
        assert!((head.base_centre.x - (100.0 - 16.0)).abs() < 1e-4);
        assert!((head.base_centre.y).abs() < 1e-5);
        assert!((head.base_left.y - 6.4).abs() < 1e-4);
        assert!((head.base_right.y + 6.4).abs() < 1e-4);

        // The *stroke* stops 0.8 L back, inside the triangle.
        assert!((head.pull_back_distance() - 0.8 * 16.0).abs() < 1e-4);
    }

    #[test]
    fn the_stroke_stops_short_of_the_arrowheads_base() {
        let tip = Point::new(100.0, 0.0);
        let towards = Point::new(0.0, 0.0);
        let head = ArrowHead::new(tip, towards, 4.0).expect("a direction exists");
        let unit = unit_towards(tip, towards);
        let pulled_back = tip - unit * head.pull_back_distance();

        // 0.8 L < L, so the stroke ends inside the triangle rather than exactly
        // on its anti-aliased base edge.
        assert!(
            (pulled_back.x - 100.0).abs() < (head.base_centre.x - 100.0).abs(),
            "the stroke should stop inside the triangle"
        );
        assert!((pulled_back.x - (100.0 - 0.8 * 16.0)).abs() < 1e-4);
    }

    #[test]
    fn a_thin_arrowhead_still_gets_the_minimum_length() {
        let head = ArrowHead::new(Point::new(100.0, 0.0), Point::new(0.0, 0.0), 1.0)
            .expect("a direction exists");
        // L = max(12, 4) = 12, and the stroke stops 0.8 * 12 back from the tip.
        assert!((head.base_centre.x - (100.0 - 12.0)).abs() < 1e-4);
        assert!((head.pull_back_distance() - 9.6).abs() < 1e-4);
    }

    #[test]
    fn an_arrowhead_needs_a_direction() {
        assert!(ArrowHead::new(Point::new(5.0, 5.0), Point::new(5.0, 5.0), 4.0).is_none());
    }

    #[test]
    fn simplifying_keeps_the_end_points_and_the_corners() {
        // NEXT_STEPS step 14a: RDP at 0.5 px turns a densely sampled
        // right-angle into its three corners.
        let mut input = Vec::new();
        for step in 0..=10 {
            input.push(Point::new(step as f32 * 10.0, 0.0));
        }
        for step in 0..=10 {
            input.push(Point::new(100.0, step as f32 * 10.0));
        }

        let simplified = simplify(&input, 0.5);

        assert_eq!(simplified.first(), input.first());
        assert_eq!(simplified.last(), input.last());
        assert_eq!(
            simplified,
            points(&[(0.0, 0.0), (100.0, 0.0), (100.0, 100.0)])
        );
    }

    #[test]
    fn simplifying_drops_points_that_stay_near_the_line() {
        let input = points(&[
            (0.0, 0.0),
            (10.0, 0.2),
            (20.0, -0.15),
            (30.0, 0.1),
            (40.0, 0.0),
        ]);
        assert_eq!(simplify(&input, 0.5), points(&[(0.0, 0.0), (40.0, 0.0)]));

        // A corner is not noise: a tolerance below its height keeps it.
        let corner = points(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]);
        assert_eq!(simplify(&corner, 0.5), corner);
    }

    /// Step 14a review: a back-and-forth scribble along one row (crossing
    /// something out) lies on the *line* through its ends but not on the
    /// *segment*. Measuring against the infinite line collapsed it into one
    /// straight stroke.
    #[test]
    fn simplifying_keeps_a_back_and_forth_scribble() {
        let mut points = Vec::new();
        for pass in 0..4 {
            for step in 0..=20 {
                let x = if pass % 2 == 0 {
                    step as f32 * 5.0
                } else {
                    100.0 - step as f32 * 5.0
                };
                points.push(Point::new(x, 10.0));
            }
        }
        let simplified = simplify(&points, 0.5);
        // Three turns (at x = 100, 0, 100) plus the two ends.
        assert!(
            simplified.len() >= 5,
            "the scribble collapsed to {} points: {simplified:?}",
            simplified.len()
        );
        let turns = simplified
            .windows(3)
            .filter(|w| (w[1].x - w[0].x) * (w[2].x - w[1].x) < 0.0)
            .count();
        assert_eq!(
            turns, 3,
            "the back-and-forth turns are gone: {simplified:?}"
        );
    }

    #[test]
    fn simplifying_needs_three_points_and_survives_repeats() {
        let two = points(&[(1.0, 1.0), (5.0, 5.0)]);
        assert_eq!(simplify(&two, 0.5), two);

        let one = points(&[(1.0, 1.0)]);
        assert_eq!(simplify(&one, 0.5), one);

        // Every sample in the same place: the line through the ends has zero
        // length, which must not divide by zero or loop.
        let repeated = points(&[(3.0, 3.0), (3.0, 3.0), (3.0, 3.0)]);
        assert_eq!(simplify(&repeated, 0.5), points(&[(3.0, 3.0), (3.0, 3.0)]));
    }
}
