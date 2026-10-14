//! Number badges: a filled circle with the value, and an optional tapered
//! pointer to a spot on the image (SPEC section 4.2).

use tiny_skia::{FillRule, PathBuilder};

use super::Canvas;
use crate::geom::{Point, Rect};
use crate::model::{NumberA, Rgba};
use crate::text::{self, Fonts};

/// The font size the value starts at: about `1.1 * radius` (SPEC 4.2).
const VALUE_SIZE_FACTOR: f32 = 1.1;

/// How wide the value may get before it is scaled down: a bit inside the
/// circle's diameter, so three or more digits stay inside the badge (SPEC 4.2).
const VALUE_MAX_WIDTH_FACTOR: f32 = 1.5;

/// The circle, plus the pointer when there is one (SPEC 4.2).
#[must_use]
pub fn shape_bounds(number: &NumberA) -> Rect {
    let circle = Rect::new(
        number.center.x - number.radius,
        number.center.y - number.radius,
        number.radius * 2.0,
        number.radius * 2.0,
    );
    match number.pointer_tip {
        Some(tip) => circle.expand_to_include(tip),
        None => circle,
    }
}

/// Draws the pointer (underneath), the circle, and the value (SPEC 4.2).
pub(crate) fn draw(canvas: &mut Canvas, number: &NumberA, fonts: &mut Fonts) {
    if let Some(tip) = number.pointer_tip {
        draw_pointer(canvas, number, tip);
    }
    draw_circle(canvas, number.center, number.radius, number.fill);

    // The value is bold, in the contrasting colour, centred on the badge.
    let family = fonts.fallback_family().to_owned();
    let value = number.value.to_string();
    let size = value_size(number, &family, fonts);
    let layout = text::layout_run(&value, &family, size, true, false, fonts);
    let at = (
        number.center.x - layout.width / 2.0,
        number.center.y - layout.height / 2.0,
    );
    text::draw_run(
        canvas,
        &text::Run {
            text: &value,
            family: &family,
            size,
            bold: true,
            italic: false,
            colour: text::contrasting_colour(number.fill),
        },
        at,
        fonts,
    );
}

/// The font size the value is drawn at: `1.1 * radius`, scaled down when the
/// text would be wider than the badge (SPEC 4.2).
fn value_size(number: &NumberA, family: &str, fonts: &mut Fonts) -> f32 {
    let full = number.radius * VALUE_SIZE_FACTOR;
    let measured = text::layout_run(&number.value.to_string(), family, full, true, false, fonts);
    let max_width = number.radius * VALUE_MAX_WIDTH_FACTOR;
    if measured.width > max_width && measured.width > 0.0 {
        full * (max_width / measured.width)
    } else {
        full
    }
}

/// The tapered pointer: a triangle from a chord of the circle to the tip.
///
/// SPEC 4.2: the base is a chord about `1.2 * radius` wide — half-width
/// `0.6 * radius`, which sits `0.8 * radius` from the centre — and the apex is
/// the tip. It is drawn in the badge's fill colour and then covered by the
/// circle.
fn draw_pointer(canvas: &mut Canvas, number: &NumberA, tip: Point) {
    let centre = number.center;
    let direction = Point::new(tip.x - centre.x, tip.y - centre.y);
    let length = direction.distance(Point::ZERO);
    if length < 1e-3 || number.radius <= 0.0 {
        return;
    }
    let unit = Point::new(direction.x / length, direction.y / length);
    // Perpendicular, for the two base corners.
    let side = Point::new(-unit.y, unit.x);
    let base = Point::new(
        centre.x + unit.x * number.radius * 0.8,
        centre.y + unit.y * number.radius * 0.8,
    );
    let half = number.radius * 0.6;
    let left = Point::new(base.x + side.x * half, base.y + side.y * half);
    let right = Point::new(base.x - side.x * half, base.y - side.y * half);

    let mut builder = PathBuilder::new();
    builder.move_to(left.x, left.y);
    builder.line_to(right.x, right.y);
    builder.line_to(tip.x, tip.y);
    builder.close();
    let Some(path) = builder.finish() else {
        return;
    };
    canvas.pixmap.fill_path(
        &path,
        &Canvas::paint(number.fill),
        FillRule::Winding,
        canvas.transform,
        None,
    );
}

/// The filled circle of the badge.
fn draw_circle(canvas: &mut Canvas, centre: Point, radius: f32, fill: Rgba) {
    let Some(path) = PathBuilder::from_circle(centre.x, centre.y, radius) else {
        return;
    };
    canvas.pixmap.fill_path(
        &path,
        &Canvas::paint(fill),
        FillRule::Winding,
        canvas.transform,
        None,
    );
}
