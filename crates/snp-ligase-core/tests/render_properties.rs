//! Property tests on rendered output (SPEC section 11.1).
//!
//! These run the real path: build a document, flatten it, look at specific
//! pixels. They exist because nobody can see the screen, and a screenshot tool
//! whose arrows point the wrong way is worse than one that crashes.

use std::sync::Arc;

use image::{Rgba, RgbaImage};
use snp_ligase_core::edit;
use snp_ligase_core::geom::{Point, Rect};
use snp_ligase_core::model::{
    Annotation, FreehandA, ImageA, Kind, PathA, PolygonA, RectA, Rgba as ModelRgba, Stroke, TextA,
    defaults,
};
use snp_ligase_core::render;
use snp_ligase_core::text::Fonts;

const RED: ModelRgba = [255, 0, 0, 255];
const BLUE: ModelRgba = [0, 0, 255, 255];
const YELLOW: ModelRgba = [255, 255, 0, 255];

fn base(width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_pixel(width, height, Rgba([255, 255, 255, 255]))
}

/// Two vertical halves: black on the left, white on the right.
fn two_tone(width: u32, height: u32) -> RgbaImage {
    let mut image = RgbaImage::from_pixel(width, height, Rgba([255, 255, 255, 255]));
    for y in 0..height {
        for x in 0..width / 2 {
            image.put_pixel(x, y, Rgba([0, 0, 0, 255]));
        }
    }
    image
}

fn annotation(kind: Kind) -> Annotation {
    Annotation {
        id: 1,
        shadow: false,
        kind,
    }
}

fn flatten_with(base: RgbaImage, annotations: Vec<Annotation>) -> RgbaImage {
    let mut doc = snp_ligase_core::model::Document::new(base);
    for a in annotations {
        doc.annotations.push(a);
    }
    render::flatten(&doc, &mut Fonts::bundled())
}

#[test]
fn the_centre_of_a_filled_rectangle_is_the_fill_colour() {
    let image = flatten_with(
        base(100, 100),
        vec![annotation(Kind::Rect(RectA {
            rect: Rect::new(20.0, 30.0, 40.0, 20.0),
            stroke: None,
            fill: Some(RED),
            corner_radius: 0.0,
        }))],
    );

    assert_eq!(image.get_pixel(40, 40), &Rgba(RED));
    // Well inside, not just at the centre.
    assert_eq!(image.get_pixel(30, 45), &Rgba(RED));
}

#[test]
fn pixels_outside_the_bounds_are_untouched() {
    let image = flatten_with(
        base(100, 100),
        vec![annotation(Kind::Rect(RectA {
            rect: Rect::new(20.0, 30.0, 40.0, 20.0),
            stroke: Some(defaults::stroke()),
            fill: Some(RED),
            corner_radius: 0.0,
        }))],
    );

    assert_eq!(image.get_pixel(5, 5), &Rgba([255, 255, 255, 255]));
    assert_eq!(image.get_pixel(95, 95), &Rgba([255, 255, 255, 255]));
    // Just outside the rectangle's left edge, past the 4 px stroke: untouched.
    assert_eq!(image.get_pixel(15, 40), &Rgba([255, 255, 255, 255]));
}

#[test]
fn the_pixel_at_an_arrow_tip_is_the_stroke_colour() {
    let tip = Point::new(90.0, 20.0);
    let image = flatten_with(
        base(120, 60),
        vec![annotation(Kind::Path(PathA {
            points: vec![Point::new(10.0, 20.0), tip],
            curved: false,
            stroke: Stroke {
                color: BLUE,
                width: 4.0,
            },
            head_start: false,
            head_end: true,
        }))],
    );

    // The tip is a triangle point, so the pixel exactly on it is mostly but not
    // necessarily completely covered: look for a fully painted pixel within two
    // pixels of it.
    let bluest = |p: Rgba<u8>| i32::from(p[2]) - i32::from(p[0].max(p[1]));
    let mut best = Rgba([0, 0, 0, 0]);
    for dx in 0..5i32 {
        for dy in -1..=1i32 {
            let x = (tip.x as i32 - dx) as u32;
            let y = (tip.y as i32 + dy) as u32;
            let pixel = *image.get_pixel(x, y);
            if bluest(pixel) > bluest(best) {
                best = pixel;
            }
        }
    }
    assert_eq!(best, Rgba(BLUE), "the arrowhead must be solid at its tip");
}

#[test]
fn a_highlight_over_white_gives_the_highlight_colour_and_over_black_gives_black() {
    let image = flatten_with(
        two_tone(40, 10),
        vec![annotation(Kind::Highlight(
            snp_ligase_core::model::HighlightA {
                rect: Rect::new(0.0, 0.0, 40.0, 10.0),
                color: YELLOW,
            },
        ))],
    );

    // Left half was black: multiplying keeps it black.
    assert_eq!(image.get_pixel(5, 5), &Rgba([0, 0, 0, 255]));
    // Right half was white: it takes the highlight colour.
    assert_eq!(image.get_pixel(35, 5), &Rgba([255, 255, 0, 255]));
}

#[test]
fn a_blur_softens_its_own_rectangle_and_leaves_the_rest_alone() {
    let image = flatten_with(
        two_tone(60, 20),
        vec![annotation(Kind::Blur(snp_ligase_core::model::BlurA {
            rect: Rect::new(10.0, 0.0, 20.0, 20.0),
            radius: 8.0,
        }))],
    );

    // Inside the blurred rectangle, near the black/white edge, the values are
    // in between rather than one or the other.
    let inside_left = image.get_pixel(29, 10)[0];
    assert!(
        (20..235).contains(&inside_left),
        "expected a soft edge inside the blur, got {inside_left}"
    );

    // Outside the rectangle nothing changed: still hard black, still hard white.
    assert_eq!(image.get_pixel(2, 10), &Rgba([0, 0, 0, 255]));
    assert_eq!(image.get_pixel(45, 10), &Rgba([255, 255, 255, 255]));
}

#[test]
fn a_shadow_makes_pixels_darker_down_and_right_of_a_shape() {
    let rect = Rect::new(30.0, 30.0, 30.0, 20.0);
    let plain = flatten_with(
        base(100, 100),
        vec![annotation(Kind::Rect(RectA {
            rect,
            stroke: Some(defaults::stroke()),
            fill: Some(BLUE),
            corner_radius: 0.0,
        }))],
    );

    let mut shadowed = annotation(Kind::Rect(RectA {
        rect,
        stroke: Some(defaults::stroke()),
        fill: Some(BLUE),
        corner_radius: 0.0,
    }));
    shadowed.shadow = true;
    let with_shadow = flatten_with(base(100, 100), vec![shadowed]);

    // Down and right of the rectangle's bottom-right corner.
    let (x, y) = (63u32, 53u32);
    let plain_pixel = plain.get_pixel(x, y)[0];
    let shadow_pixel = with_shadow.get_pixel(x, y)[0];
    assert!(
        shadow_pixel < plain_pixel,
        "expected a shadow at ({x}, {y}): {shadow_pixel} is not darker than {plain_pixel}"
    );
    // The shadow is not black: it is black at 45%.
    assert!(shadow_pixel > 100, "the shadow came out at {shadow_pixel}");
}

#[test]
fn an_annotations_bounds_match_the_sprite_it_rasterizes() {
    // The canvas draws selection outlines from bounds(); if that disagreed with
    // the sprite, the outline would sit next to the shape instead of on it.
    let annotations = vec![
        annotation(Kind::Rect(RectA {
            rect: Rect::new(10.3, 20.7, 40.2, 30.9),
            stroke: Some(defaults::stroke()),
            fill: Some(RED),
            corner_radius: 8.0,
        })),
        annotation(Kind::Path(PathA {
            points: vec![
                Point::new(5.5, 5.5),
                Point::new(60.5, 40.5),
                Point::new(90.5, 10.5),
            ],
            curved: true,
            stroke: defaults::stroke(),
            head_start: false,
            head_end: true,
        })),
        annotation(Kind::Polygon(PolygonA {
            points: vec![
                Point::new(10.0, 10.0),
                Point::new(60.0, 10.0),
                Point::new(35.0, 50.0),
            ],
            stroke: Some(defaults::stroke()),
            fill: Some(BLUE),
        })),
    ];

    let image = base(200, 200);
    let mut fonts = Fonts::bundled();
    for a in &annotations {
        let bounds = render::bounds(a, &mut fonts);
        let sprite = render::rasterize(a, &image, &mut fonts)
            .unwrap_or_else(|| panic!("{} should rasterize", a.kind.name()));

        assert_eq!(bounds.round_out().to_rect(), sprite.rect());
    }
}

#[test]
fn an_effects_sprite_is_exactly_its_rectangle() {
    let image = base(100, 100);
    let mut fonts = Fonts::bundled();
    let highlight = annotation(Kind::Highlight(snp_ligase_core::model::HighlightA {
        rect: Rect::new(10.0, 20.0, 30.0, 40.0),
        color: YELLOW,
    }));

    let sprite = render::rasterize(&highlight, &image, &mut fonts).expect("a sprite");
    assert_eq!(sprite.rect(), Rect::new(10.0, 20.0, 30.0, 40.0));
    assert_eq!(sprite.pixmap.width(), 30);
    assert_eq!(sprite.pixmap.height(), 40);
}

/// M7: the sprite cache reuses a moved annotation's texture, so the reuse must
/// be pixel-exact. For an **integer** translation it is: rasterizing the moved
/// annotation gives the same pixels, with the origin shifted by the offset.
///
/// (A fractional offset shifts the same pixels by a fraction, which is the
/// sub-pixel approximation SPEC 12's performance note accepts; the *position*
/// stays exact because the origin is a float.)
#[test]
fn a_translated_sprite_is_the_same_pixels_moved() {
    let annotations = vec![
        annotation(Kind::Rect(RectA {
            rect: Rect::new(10.0, 20.0, 40.0, 30.0),
            stroke: Some(defaults::stroke()),
            fill: Some(RED),
            corner_radius: 4.0,
        })),
        annotation(Kind::Path(PathA {
            points: vec![
                Point::new(5.0, 5.0),
                Point::new(60.0, 40.0),
                Point::new(90.0, 10.0),
            ],
            curved: true,
            stroke: defaults::stroke(),
            head_start: true,
            head_end: true,
        })),
        annotation(Kind::Polygon(PolygonA {
            points: vec![
                Point::new(10.0, 10.0),
                Point::new(60.0, 10.0),
                Point::new(35.0, 50.0),
            ],
            stroke: Some(defaults::stroke()),
            fill: Some(BLUE),
        })),
    ];

    let image = base(200, 200);
    let mut fonts = Fonts::bundled();
    for a in &annotations {
        let (dx, dy) = (7.0, 5.0);
        assert_eq!(
            edit::translation_between(a, &{
                let mut moved = a.clone();
                edit::translate(&mut moved, dx, dy);
                moved
            }),
            Some((dx, dy)),
            "{} was not recognised as moved",
            a.kind.name()
        );

        let before = render::rasterize(a, &image, &mut fonts).expect("a sprite");
        let mut moved = a.clone();
        edit::translate(&mut moved, dx, dy);
        let after = render::rasterize(&moved, &image, &mut fonts).expect("a sprite");

        assert_eq!(
            (
                after.origin.0 - before.origin.0,
                after.origin.1 - before.origin.1
            ),
            (dx as i32, dy as i32),
            "{}: the origin did not move with the shape",
            a.kind.name()
        );
        assert_eq!(
            after.pixmap.data(),
            before.pixmap.data(),
            "{}: the pixels changed, so reusing them would not be exact",
            a.kind.name()
        );
    }
}

#[test]
fn an_empty_annotation_rasterizes_to_nothing() {
    let image = base(100, 100);
    let mut fonts = Fonts::bundled();

    let invisible = annotation(Kind::Rect(RectA {
        rect: Rect::new(10.0, 10.0, 40.0, 20.0),
        stroke: None,
        fill: None,
        corner_radius: 0.0,
    }));
    assert!(render::rasterize(&invisible, &image, &mut fonts).is_none());

    let single_point = annotation(Kind::Path(PathA {
        points: vec![Point::new(10.0, 10.0)],
        curved: false,
        stroke: defaults::stroke(),
        head_start: false,
        head_end: false,
    }));
    assert!(render::rasterize(&single_point, &image, &mut fonts).is_none());

    let two_point_polygon = annotation(Kind::Polygon(PolygonA {
        points: vec![Point::new(0.0, 0.0), Point::new(10.0, 10.0)],
        stroke: None,
        fill: Some(RED),
    }));
    assert!(render::rasterize(&two_point_polygon, &image, &mut fonts).is_none());
}

/// SPEC 4.2's Text: background first, then glyphs, the box being the layout
/// size plus padding.
#[test]
fn text_draws_a_background_box_and_glyphs_inside_it() {
    let text = snp_ligase_core::model::TextA {
        pos: Point::new(20.0, 20.0),
        text: "Hello".to_owned(),
        // The bundled test font, so this test does not depend on the machine.
        font_family: "Inter".to_owned(),
        size: 28.0,
        bold: true,
        italic: false,
        color: [0, 0, 0, 255],
        background: Some(BLUE),
        padding: 6.0,
        outline: None,
    };
    let image = flatten_with(base(200, 100), vec![annotation(Kind::Text(text.clone()))]);

    // Inside the padding, the background is solid.
    assert_eq!(
        image.get_pixel(22, 22),
        &Rgba(BLUE),
        "the background box is not there"
    );

    // Somewhere in the glyph area there are dark pixels (the text), and the
    // background is visible between them.
    let mut dark = 0usize;
    let mut background = 0usize;
    for y in 26..50 {
        for x in 26..120 {
            let pixel = *image.get_pixel(x, y);
            if pixel.0[0] < 60 && pixel.0[1] < 60 && pixel.0[2] < 60 {
                dark += 1;
            }
            if pixel == Rgba(BLUE) {
                background += 1;
            }
        }
    }
    assert!(dark > 20, "only {dark} dark pixels: the glyphs are missing");
    assert!(
        background > 20,
        "only {background} background pixels: the box did not stay"
    );

    // Outside the box, the image is untouched.
    assert_eq!(image.get_pixel(5, 5), &Rgba([255, 255, 255, 255]));
    assert_eq!(image.get_pixel(190, 90), &Rgba([255, 255, 255, 255]));
}

/// The box is the layout size plus padding, and the sprite covers it.
#[test]
fn the_text_box_is_the_layout_plus_padding() {
    let text = snp_ligase_core::model::TextA {
        pos: Point::new(20.0, 20.0),
        text: "Hello".to_owned(),
        font_family: "Inter".to_owned(),
        size: 28.0,
        bold: false,
        italic: false,
        color: [0, 0, 0, 255],
        background: None,
        padding: 6.0,
        outline: None,
    };
    let mut fonts = Fonts::bundled();
    let measured = snp_ligase_core::text::layout(&text, &mut fonts);
    let shape = render::shape_bounds(&annotation(Kind::Text(text)), &mut fonts);

    assert!((shape.x - 20.0).abs() < 0.01);
    assert!((shape.y - 20.0).abs() < 0.01);
    assert!(
        (shape.w - (measured.width + 12.0)).abs() < 0.01,
        "width {} is not the layout {} plus padding",
        shape.w,
        measured.width
    );
    assert!(
        (shape.h - (measured.height + 12.0)).abs() < 0.01,
        "height {} is not the layout {} plus padding",
        shape.h,
        measured.height
    );
}

/// NEXT_STEPS step 8, outline-only: a pixel on the box edge is the outline
/// colour, the padding just inside is untouched base, and the glyphs are still
/// drawn.
#[test]
fn text_with_only_an_outline_draws_the_edge_and_keeps_the_padding_clear() {
    let text = snp_ligase_core::model::TextA {
        pos: Point::new(20.0, 20.0),
        text: "Hello".to_owned(),
        font_family: "Inter".to_owned(),
        size: 28.0,
        bold: true,
        italic: false,
        color: [0, 0, 0, 255],
        background: None,
        padding: 6.0,
        outline: Some(Stroke {
            color: RED,
            width: 4.0,
        }),
    };
    let a = annotation(Kind::Text(text));
    let image = flatten_with(base(220, 100), vec![a.clone()]);

    let mut fonts = Fonts::bundled();
    let box_rect = render::shape_bounds(&a, &mut fonts);
    let middle = (box_rect.y + box_rect.h / 2.0).floor() as u32;

    // The outline is centred on the edge: the edge itself is solid.
    assert_eq!(
        image.get_pixel(box_rect.x as u32, middle),
        &Rgba(RED),
        "the outline is missing on the box edge"
    );
    // Just inside (still in the padding) the base is untouched.
    assert_eq!(
        image.get_pixel((box_rect.x + 3.0) as u32, middle),
        &Rgba([255, 255, 255, 255]),
        "the outline bled into the padding"
    );
    // And outside the half-width the base is untouched too.
    assert_eq!(
        image.get_pixel((box_rect.x - 4.0) as u32, middle),
        &Rgba([255, 255, 255, 255]),
        "the outline bled outside the box"
    );

    // The glyphs are still drawn inside the box.
    let mut dark = 0usize;
    let (x0, y0) = ((box_rect.x + 6.0) as u32, (box_rect.y + 6.0) as u32);
    let (x1, y1) = (
        (box_rect.right() - 6.0) as u32,
        (box_rect.bottom() - 6.0) as u32,
    );
    for y in y0..y1 {
        for x in x0..x1 {
            let pixel = *image.get_pixel(x, y);
            if pixel.0[0] < 60 && pixel.0[1] < 60 && pixel.0[2] < 60 {
                dark += 1;
            }
        }
    }
    assert!(dark > 20, "only {dark} dark pixels: the glyphs are missing");
}

/// NEXT_STEPS step 8, outline + fill: the padding area is the fill colour and
/// the edge is the outline colour.
#[test]
fn text_with_an_outline_and_a_background_shows_both() {
    let text = snp_ligase_core::model::TextA {
        pos: Point::new(20.0, 20.0),
        text: "Hello".to_owned(),
        font_family: "Inter".to_owned(),
        size: 28.0,
        bold: true,
        italic: false,
        color: [255, 255, 255, 255],
        background: Some(BLUE),
        padding: 6.0,
        outline: Some(Stroke {
            color: RED,
            width: 4.0,
        }),
    };
    let a = annotation(Kind::Text(text));
    let image = flatten_with(base(220, 100), vec![a.clone()]);

    let mut fonts = Fonts::bundled();
    let box_rect = render::shape_bounds(&a, &mut fonts);
    let middle = (box_rect.y + box_rect.h / 2.0).floor() as u32;

    assert_eq!(
        image.get_pixel(box_rect.x as u32, middle),
        &Rgba(RED),
        "the outline is missing on the box edge"
    );
    assert_eq!(
        image.get_pixel((box_rect.x + 3.0) as u32, middle),
        &Rgba(BLUE),
        "the fill is missing just inside the outline"
    );
    assert_eq!(
        image.get_pixel((box_rect.x - 4.0) as u32, middle),
        &Rgba([255, 255, 255, 255]),
        "the fill or outline bled outside the box"
    );
}

/// NEXT_STEPS step 8: `bounds()` grows by half the outline width on every side
/// (the sprite must be big enough for the stroke).
#[test]
fn text_bounds_include_half_the_outline_width() {
    let text = |outline: Option<Stroke>| snp_ligase_core::model::TextA {
        pos: Point::new(20.0, 20.0),
        text: "Hello".to_owned(),
        font_family: "Inter".to_owned(),
        size: 28.0,
        bold: false,
        italic: false,
        color: [0, 0, 0, 255],
        background: None,
        padding: 6.0,
        outline,
    };
    let mut fonts = Fonts::bundled();
    let plain = render::bounds(&annotation(Kind::Text(text(None))), &mut fonts);
    let outlined = render::bounds(
        &annotation(Kind::Text(text(Some(Stroke {
            color: RED,
            width: 4.0,
        })))),
        &mut fonts,
    );

    assert!(
        (plain.x - outlined.x - 2.0).abs() < 0.01,
        "left edge: {plain:?} vs {outlined:?}"
    );
    assert!(
        (plain.y - outlined.y - 2.0).abs() < 0.01,
        "top edge: {plain:?} vs {outlined:?}"
    );
    assert!(
        (outlined.right() - plain.right() - 2.0).abs() < 0.01,
        "right edge: {plain:?} vs {outlined:?}"
    );
    assert!(
        (outlined.bottom() - plain.bottom() - 2.0).abs() < 0.01,
        "bottom edge: {plain:?} vs {outlined:?}"
    );
}

/// SPEC 11.1's Number checks: the badge is a filled circle, its value is drawn
/// in the contrasting colour, and the pointer reaches the tip.
#[test]
fn a_number_badge_is_a_filled_circle_with_a_contrasting_value() {
    let number = snp_ligase_core::model::NumberA {
        center: Point::new(50.0, 50.0),
        value: 1,
        radius: 16.0,
        fill: RED,
        pointer_tip: None,
    };
    let image = flatten_with(base(100, 100), vec![annotation(Kind::Number(number))]);

    // A point inside the circle but clear of the glyph: up and left of the
    // centre, on the diagonal (the "1" is a narrow glyph in the middle).
    assert_eq!(
        image.get_pixel(41, 41),
        &Rgba(RED),
        "the circle is not filled at 45 degrees"
    );
    // The value is white on this red (SPEC 4.2's contrast rule). The glyph is
    // anti-aliased, so look for near-white pixels rather than exact ones.
    let mut white = 0usize;
    for y in 40..60 {
        for x in 40..60 {
            let pixel = *image.get_pixel(x, y);
            if pixel.0[0] > 200 && pixel.0[1] > 200 && pixel.0[2] > 200 {
                white += 1;
            }
        }
    }
    assert!(white > 5, "the value is not drawn in white: {white} pixels");
    // Outside the circle: untouched.
    assert_eq!(image.get_pixel(10, 50), &Rgba([255, 255, 255, 255]));
    assert_eq!(image.get_pixel(50, 10), &Rgba([255, 255, 255, 255]));
}

/// The pointer triangle reaches the tip, in the badge's colour.
#[test]
fn the_pixel_at_a_number_pointers_tip_is_the_fill_colour() {
    let tip = Point::new(90.0, 80.0);
    let number = snp_ligase_core::model::NumberA {
        center: Point::new(50.0, 50.0),
        value: 2,
        radius: 16.0,
        fill: RED,
        pointer_tip: Some(tip),
    };
    let image = flatten_with(base(120, 120), vec![annotation(Kind::Number(number))]);

    // The apex itself is thinner than a pixel, so the test looks for a fully
    // painted pixel a few pixels back along the pointer's axis, the same way
    // the arrowhead test does.
    let direction = Point::new(tip.x - 50.0, tip.y - 50.0);
    let length = direction.distance(Point::ZERO);
    let unit = Point::new(direction.x / length, direction.y / length);
    let mut solid = false;
    for back in 2..12 {
        let x = (tip.x - unit.x * back as f32).round() as u32;
        let y = (tip.y - unit.y * back as f32).round() as u32;
        if *image.get_pixel(x, y) == Rgba(RED) {
            solid = true;
            break;
        }
    }
    assert!(solid, "the pointer is not solid anywhere near its tip");

    // The bounds contain the tip.
    let mut fonts = Fonts::bundled();
    let bounds = render::bounds(&annotation(Kind::Number(number)), &mut fonts);
    assert!(
        bounds.contains(tip),
        "bounds {bounds:?} do not contain the tip {tip:?}"
    );
}

/// SPEC 4.2: "reduced for 3+ digits so the text fits" — every pixel of a
/// three-digit badge stays inside the circle.
#[test]
fn a_three_digit_value_stays_inside_its_circle() {
    let centre = Point::new(50.0, 50.0);
    let radius = 16.0;
    let number = snp_ligase_core::model::NumberA {
        center: centre,
        value: 123,
        radius,
        fill: RED,
        pointer_tip: None,
    };
    let image = flatten_with(base(100, 100), vec![annotation(Kind::Number(number))]);

    // Every pixel that is not the base white must be within the circle (plus
    // one pixel of anti-aliasing slack).
    for (x, y, pixel) in image.enumerate_pixels() {
        if *pixel == Rgba([255, 255, 255, 255]) {
            continue;
        }
        let distance =
            (((x as f32 + 0.5) - centre.x).powi(2) + ((y as f32 + 0.5) - centre.y).powi(2)).sqrt();
        assert!(
            distance <= radius + 1.0,
            "pixel ({x}, {y}) at {distance:.1} px is outside the badge"
        );
    }
}

#[test]
fn effects_are_composited_in_order_blur_then_highlight_then_the_rest() {
    // SPEC section 4.2: base, then every Blur, then every Highlight, then
    // everything else in list order. A highlight added after a blur must win.
    let image = two_tone(60, 20);
    let mut blur = annotation(Kind::Blur(snp_ligase_core::model::BlurA {
        rect: Rect::new(0.0, 0.0, 30.0, 20.0),
        radius: 8.0,
    }));
    blur.id = 1;
    let highlight = annotation(Kind::Highlight(snp_ligase_core::model::HighlightA {
        rect: Rect::new(0.0, 0.0, 30.0, 20.0),
        color: YELLOW,
    }));

    // Listed blur-first: the same result either way, because the compositor
    // always draws effects first regardless of list order.
    let blur_first = flatten_with(image.clone(), vec![blur.clone(), highlight.clone()]);
    let highlight_first = flatten_with(image, vec![highlight, blur]);

    assert_eq!(blur_first.as_raw(), highlight_first.as_raw());
    // The highlight multiply ran last, so the left half is black-on-black.
    assert_eq!(blur_first.get_pixel(5, 10), &Rgba([0, 0, 0, 255]));
}

#[test]
fn a_highlight_over_a_blur_multiplies_the_blurred_pixels() {
    // A Highlight is opaque. If it read the *sharp* base it would reveal
    // whatever the Blur was hiding, so it reads the blurred base instead
    // (DECISIONS.md records this as a deviation from SPEC 4.2).
    let image = two_tone(40, 20);
    let blur = Kind::Blur(snp_ligase_core::model::BlurA {
        rect: Rect::new(0.0, 0.0, 40.0, 20.0),
        radius: 10.0,
    });
    let highlight = Kind::Highlight(snp_ligase_core::model::HighlightA {
        rect: Rect::new(0.0, 0.0, 40.0, 20.0),
        color: YELLOW,
    });

    let flattened = flatten_with(
        image.clone(),
        vec![annotation(blur.clone()), annotation(highlight)],
    );

    // The blur on its own: somewhere inside the blur the hard black/white edge
    // has become a ramp.
    let blurred_only = flatten_with(image, vec![annotation(blur)]);
    let ramped = (0..40u32)
        .find(|&x| blurred_only.get_pixel(x, 10)[0] > 0 && blurred_only.get_pixel(x, 10)[0] < 255)
        .expect("the blur has to soften the edge somewhere");

    // The highlight over it multiplies that same softened pixel by yellow, not
    // the sharp 0 or 255 the base had there.
    let blurred = blurred_only.get_pixel(ramped, 10)[0];
    let highlighted = flattened.get_pixel(ramped, 10)[0];
    assert!(
        highlighted > 0 && highlighted < 255,
        "the highlight un-blurred the pixel: {blurred} became {highlighted}"
    );
    assert_eq!(
        highlighted,
        (u32::from(blurred) * 255 / 255) as u8,
        "the highlight should be a multiply of the blurred value"
    );
}

#[test]
fn two_overlapping_blurs_match_compositing_their_sprites() {
    // SPEC 4.2, and the reason the canvas and the export share one function:
    // a Blur reads the base image only, so two of them cannot compound.
    let image = two_tone(40, 20);
    let first = annotation(Kind::Blur(snp_ligase_core::model::BlurA {
        rect: Rect::new(0.0, 0.0, 20.0, 20.0),
        radius: 6.0,
    }));
    let mut second = annotation(Kind::Blur(snp_ligase_core::model::BlurA {
        rect: Rect::new(10.0, 0.0, 20.0, 20.0),
        radius: 6.0,
    }));
    second.id = 2;

    let flattened = flatten_with(image.clone(), vec![first.clone(), second.clone()]);

    // Each blur reads the original base, so the overlap must equal the pixels
    // of the second blur on its own — not a blur of an already-blurred image.
    let sprite = render::rasterize(&second, &image, &mut Fonts::bundled()).expect("a sprite");
    for column in 0..sprite.pixmap.width() {
        let at = (10 + column as i32) as u32;
        let expected = sprite
            .pixmap
            .pixel(column, sprite.pixmap.height() / 2)
            .expect("in");
        let got = flattened.get_pixel(at, 10);
        assert_eq!(
            &[expected.red(), expected.green(), expected.blue(), 255],
            &got.0,
            "column {at} of the overlap is not the second blur's own pixels"
        );
    }
    let _ = first;
}

#[test]
fn the_canvas_draw_order_matches_the_exports() {
    // The editor draws `compositing_order`; `flatten` uses the same function,
    // so what is on screen is what gets saved (SPEC section 2). NEXT_STEPS
    // step 11 puts the pasted images between the base and the effects.
    let image = two_tone(20, 20);
    let mut doc = snp_ligase_core::model::Document::new(image);
    doc.push(Kind::Rect(RectA {
        rect: Rect::new(1.0, 1.0, 5.0, 5.0),
        stroke: None,
        fill: Some(RED),
        corner_radius: 0.0,
    }));
    doc.push(Kind::Highlight(snp_ligase_core::model::HighlightA {
        rect: Rect::new(0.0, 0.0, 5.0, 5.0),
        color: YELLOW,
    }));
    doc.push(Kind::Blur(snp_ligase_core::model::BlurA {
        rect: Rect::new(0.0, 0.0, 5.0, 5.0),
        radius: 4.0,
    }));
    doc.push(Kind::Image(ImageA {
        pos: Point::new(8.0, 8.0),
        image: Arc::new(RgbaImage::from_pixel(4, 4, Rgba([1, 2, 3, 255]))),
    }));

    let order = render::compositing_order(&doc);
    let kinds: Vec<&str> = order
        .iter()
        .filter_map(|id| doc.find(*id))
        .map(|a| match &a.kind {
            Kind::Image(_) => "image",
            Kind::Blur(_) => "blur",
            Kind::Highlight(_) => "highlight",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["image", "blur", "highlight", "other"]);
}

#[test]
fn a_pasted_image_draws_its_pixels_at_its_position() {
    // NEXT_STEPS step 11: the pixels themselves, 1:1 at `pos`.
    let pixels = RgbaImage::from_fn(4, 3, |x, y| Rgba([x as u8 * 60, y as u8 * 80, 7, 255]));
    let mut doc = snp_ligase_core::model::Document::new(base(20, 10));
    doc.push(Kind::Image(ImageA {
        pos: Point::new(5.0, 2.0),
        image: Arc::new(pixels.clone()),
    }));

    let flattened = render::flatten(&doc, &mut Fonts::bundled());
    for y in 0..3 {
        for x in 0..4 {
            assert_eq!(
                flattened.get_pixel(5 + x, 2 + y),
                pixels.get_pixel(x, y),
                "pixel ({x}, {y}) of the pasted image"
            );
        }
    }
    // One pixel outside each side is still the base.
    assert_eq!(flattened.get_pixel(4, 2), &Rgba([255, 255, 255, 255]));
    assert_eq!(flattened.get_pixel(9, 2), &Rgba([255, 255, 255, 255]));
    assert_eq!(flattened.get_pixel(5, 1), &Rgba([255, 255, 255, 255]));
    assert_eq!(flattened.get_pixel(5, 5), &Rgba([255, 255, 255, 255]));

    // The bounds are the image's own rectangle plus the usual slack.
    let annotation = &doc.annotations[0];
    let bounds = render::bounds(annotation, &mut Fonts::bundled());
    assert_eq!(
        (bounds.x, bounds.y, bounds.w, bounds.h),
        (3.0, 0.0, 8.0, 7.0)
    );
}

#[test]
fn a_blur_over_a_pasted_image_blurs_the_pasted_pixels() {
    // NEXT_STEPS step 11's key decision: a pasted image is part of the base
    // layer the Blur reads, so a password in the pasted screenshot can be
    // blurred. The pasted image has a hard black/white edge at x = 30.
    let mut pasted = RgbaImage::new(40, 20);
    for y in 0..20 {
        for x in 0..40 {
            let value = if x < 20 { 0 } else { 255 };
            pasted.put_pixel(x, y, Rgba([value, value, value, 255]));
        }
    }

    let mut doc = snp_ligase_core::model::Document::new(base(60, 20));
    doc.push(Kind::Image(ImageA {
        pos: Point::new(10.0, 0.0),
        image: Arc::new(pasted.clone()),
    }));
    doc.push(Kind::Blur(snp_ligase_core::model::BlurA {
        rect: Rect::new(10.0, 0.0, 40.0, 20.0),
        radius: 6.0,
    }));

    let flattened = render::flatten(&doc, &mut Fonts::bundled());
    let edge = flattened.get_pixel(30, 10)[0];
    assert!(
        edge > 0 && edge < 255,
        "the blur did not soften the pasted image's edge: {edge}"
    );

    // The control: without the blur the same pixel is the image's sharp white.
    let mut sharp_doc = snp_ligase_core::model::Document::new(base(60, 20));
    sharp_doc.push(Kind::Image(ImageA {
        pos: Point::new(10.0, 0.0),
        image: Arc::new(pasted),
    }));
    let sharp = render::flatten(&sharp_doc, &mut Fonts::bundled());
    assert_eq!(sharp.get_pixel(30, 10), &Rgba([255, 255, 255, 255]));
}

#[test]
fn a_highlight_over_a_pasted_image_multiplies_its_pixels() {
    // Blue pasted image, yellow highlight: blue × yellow = black. The
    // highlight covers only the right half, so the left half stays blue.
    let mut doc = snp_ligase_core::model::Document::new(base(40, 20));
    doc.push(Kind::Image(ImageA {
        pos: Point::new(10.0, 0.0),
        image: Arc::new(RgbaImage::from_pixel(20, 20, Rgba(BLUE))),
    }));
    doc.push(Kind::Highlight(snp_ligase_core::model::HighlightA {
        rect: Rect::new(20.0, 0.0, 10.0, 20.0),
        color: YELLOW,
    }));

    let flattened = render::flatten(&doc, &mut Fonts::bundled());
    assert_eq!(
        flattened.get_pixel(25, 10),
        &Rgba([0, 0, 0, 255]),
        "the highlight did not multiply the pasted pixels"
    );
    assert_eq!(flattened.get_pixel(15, 10), &Rgba(BLUE));
}

/// A stand-in for a pasted screenshot with a hard edge every six pixels: a blur
/// is easy to see on it and a highlight's multiply is easy to check (NEXT_STEPS
/// step 12).
fn stripes(width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, _y| {
        if (x / 6) % 2 == 0 {
            Rgba([0, 0, 0, 255])
        } else {
            Rgba([255, 255, 255, 255])
        }
    })
}

/// A base image with `stripes` pasted `gap` pixels to the right of it, plus
/// `annotation` on top — the layout step 12 is about (NEXT_STEPS K1).
fn stitched(gap: u32, annotation: Annotation) -> snp_ligase_core::model::Document {
    let mut doc = snp_ligase_core::model::Document::new(base(80, 40));
    doc.push(Kind::Image(ImageA {
        pos: Point::new(80.0 + gap as f32, 0.0),
        image: Arc::new(stripes(60, 40)),
    }));
    doc.annotations.push(annotation);
    doc
}

#[test]
fn a_blur_over_a_pasted_image_beside_the_base_blurs_its_pixels() {
    // NEXT_STEPS step 12, K1: the normal stitching case is a screenshot pasted
    // *next to* the original. A Blur dragged over the pasted pixels used to do
    // nothing there, because the layer it reads stopped at the original.
    let mut doc = stitched(
        20,
        Annotation {
            id: 2,
            shadow: false,
            kind: Kind::Blur(snp_ligase_core::model::BlurA {
                rect: Rect::new(100.0, 0.0, 60.0, 40.0),
                radius: 8.0,
            }),
        },
    );

    let expanded = expand(&doc, Some(WHITE));
    // Local x = 5 of the pasted image is the last column of a black stripe:
    // sharp black before the fix, a black-to-white ramp with it.
    let value = expanded.get_pixel(105, 20)[0];
    assert!(
        value > 0 && value < 255,
        "the blur did not touch the pasted image: {value}"
    );

    // The control: without the blur the same pixel is the sharp black.
    doc.annotations.pop();
    let sharp = expand(&doc, Some(WHITE));
    assert_eq!(sharp.get_pixel(105, 20), &Rgba([0, 0, 0, 255]));
}

#[test]
fn a_blur_over_a_pasted_image_left_of_and_above_the_base_blurs_its_pixels() {
    // Step 12 review: the layer's origin is negative when a screenshot is
    // pasted to the left of or above the original. The pasted pixels must land
    // at the right place in the layer the Blur reads, or the blur reads empty
    // pixels and the pasted content stays readable.
    let mut doc = snp_ligase_core::model::Document::new(base(80, 40));
    doc.push(Kind::Image(ImageA {
        pos: Point::new(-80.0, -10.0),
        image: Arc::new(stripes(60, 40)),
    }));
    doc.push(Kind::Blur(snp_ligase_core::model::BlurA {
        rect: Rect::new(-75.0, -5.0, 50.0, 30.0),
        radius: 8.0,
    }));

    let expanded = expand(&doc, Some(WHITE));
    // The export starts at the pasted image's corner (-80, -10). Image point
    // (-75, 10) is local column 5 of the stripes, the last column of a black
    // stripe: sharp black when the blur misses, a black-to-white ramp when it
    // works.
    let value = expanded.get_pixel(5, 20)[0];
    assert!(
        value > 0 && value < 255,
        "the blur did not touch the pasted image left of the base: {value}"
    );
}

#[test]
fn a_highlight_over_a_pasted_image_beside_the_base_multiplies_its_pixels() {
    // NEXT_STEPS step 12: the same case for a Highlight, and an exact value:
    // black × yellow stays black, white × yellow is yellow.
    let mut doc = snp_ligase_core::model::Document::new(base(80, 40));
    doc.push(Kind::Image(ImageA {
        pos: Point::new(100.0, 0.0),
        image: Arc::new(RgbaImage::from_pixel(60, 40, Rgba([255, 255, 255, 255]))),
    }));
    doc.push(Kind::Highlight(snp_ligase_core::model::HighlightA {
        rect: Rect::new(100.0, 0.0, 60.0, 40.0),
        color: YELLOW,
    }));

    let expanded = expand(&doc, Some(WHITE));
    assert_eq!(
        expanded.get_pixel(130, 20),
        &Rgba([255, 255, 0, 255]),
        "the highlight did not multiply the pasted pixels"
    );
}

#[test]
fn a_blur_across_the_seam_of_base_and_pasted_image_is_continuous() {
    // NEXT_STEPS step 12: the two images are one layer, so a blur that reaches
    // over the seam must not show it. Before the fix the pasted side of the
    // seam was not blurred at all, which is a 255-step jump on this scene.
    let mut doc = snp_ligase_core::model::Document::new(base(80, 40));
    doc.push(Kind::Image(ImageA {
        pos: Point::new(80.0, 0.0),
        image: Arc::new(RgbaImage::from_pixel(40, 40, Rgba([0, 0, 0, 255]))),
    }));
    doc.push(Kind::Blur(snp_ligase_core::model::BlurA {
        rect: Rect::new(70.0, 0.0, 30.0, 40.0),
        radius: 8.0,
    }));

    let expanded = expand(&doc, Some(WHITE));
    // The seam sits at x = 80: the pasted image starts there. A blurred
    // neighbourhood changes gently from column to column; the sharp pasted
    // pixels change by 255. 40 is far from both.
    for y in 0..40 {
        let left = i32::from(expanded.get_pixel(79, y)[0]);
        let right = i32::from(expanded.get_pixel(80, y)[0]);
        assert!(
            (left - right).abs() < 40,
            "a hard edge at the seam on row {y}: {left} then {right}"
        );
    }
    // And the seam is really inside the blur: black and white meet in a ramp.
    let seam = expanded.get_pixel(80, 20)[0];
    assert!(
        seam > 0 && seam < 255,
        "the seam was not blurred at all: {seam}"
    );
}

#[test]
fn a_blur_over_the_gap_between_the_base_and_a_pasted_image_leaves_the_fill_alone() {
    // NEXT_STEPS step 12, J1: the layer covers the base and the pasted images,
    // and nothing in between. A blur that reaches over the gap must blur what
    // it touches and leave the fill colour of the empty part alone.
    let doc = stitched(
        40,
        Annotation {
            id: 2,
            shadow: false,
            kind: Kind::Blur(snp_ligase_core::model::BlurA {
                rect: Rect::new(90.0, 0.0, 60.0, 40.0),
                radius: 8.0,
            }),
        },
    );

    let expanded = expand(&doc, Some(WHITE));
    for y in 0..40 {
        for x in 90..120 {
            assert_eq!(
                expanded.get_pixel(x, y),
                &Rgba(WHITE),
                "the blur painted the added area at ({x}, {y})"
            );
        }
    }
    // The pasted pixels the same blur reaches are blurred (local x = 5).
    let value = expanded.get_pixel(125, 20)[0];
    assert!(
        value > 0 && value < 255,
        "the blur did not reach the pasted image: {value}"
    );
}

#[test]
fn the_layer_a_sprite_is_made_from_gives_the_same_pixels_as_the_export() {
    // NEXT_STEPS step 12: the canvas rasterizes a Blur against the same layer
    // `flatten_with` composites, so the screen and the saved file agree. One
    // sprite's pixels are exactly the export's pixels where it is opaque.
    let doc = stitched(
        20,
        Annotation {
            id: 2,
            shadow: false,
            kind: Kind::Blur(snp_ligase_core::model::BlurA {
                rect: Rect::new(100.0, 0.0, 60.0, 40.0),
                radius: 8.0,
            }),
        },
    );

    let mut fonts = Fonts::bundled();
    let layer = render::images_base(&doc, &mut fonts, true);
    let sprite = render::rasterize_layer(&doc.annotations[1], &layer, &mut fonts)
        .expect("the pasted image has a blur sprite");
    assert_eq!(sprite.origin, (100, 0));

    let exported = expand(&doc, Some(WHITE));
    for row in 0..sprite.pixmap.height() {
        for column in 0..sprite.pixmap.width() {
            let at = (row as usize * sprite.pixmap.width() as usize + column as usize) * 4;
            let pixel = &sprite.pixmap.data()[at..at + 4];
            assert_eq!(pixel[3], 255, "an effect sprite is opaque");
            let (x, y) = (100 + column, row);
            assert_eq!(
                &exported.get_pixel(x, y).0[..3],
                &pixel[..3],
                "the canvas sprite and the export disagree at ({x}, {y})"
            );
        }
    }
}

#[test]
fn cut_mode_still_stops_the_layer_at_the_base_image() {
    // NEXT_STEPS step 12: all the old goldens stay byte-identical because Cut
    // mode keeps the base-sized layer. A blur over a pasted image outside the
    // base does nothing there, which is what "cut off at the image edge" means.
    let doc = stitched(
        20,
        Annotation {
            id: 2,
            shadow: false,
            kind: Kind::Blur(snp_ligase_core::model::BlurA {
                rect: Rect::new(100.0, 0.0, 60.0, 40.0),
                radius: 8.0,
            }),
        },
    );

    let cut = render::flatten(&doc, &mut Fonts::bundled());
    assert_eq!((cut.width(), cut.height()), (80, 40));
    assert!(
        cut.pixels().all(|pixel| pixel.0 == WHITE),
        "the pasted image or its blur leaked into the Cut export"
    );
}

#[test]
fn a_pasted_image_beside_the_base_grows_the_export() {
    // Step 11's expansion: pasting to the right of the image grows the saved
    // image (through step 10's Expand), with the fill colour where nothing
    // covers it.
    let mut doc = snp_ligase_core::model::Document::new(base(20, 20));
    doc.push(Kind::Image(ImageA {
        pos: Point::new(25.0, 5.0),
        image: Arc::new(RgbaImage::from_pixel(10, 10, Rgba(BLUE))),
    }));

    let export = render::export_bounds(&doc, &mut Fonts::bundled());
    assert_eq!(
        (export.x, export.y, export.right, export.bottom),
        (0, 0, 35, 20)
    );

    let expanded = expand(&doc, Some(WHITE));
    assert_eq!((expanded.width(), expanded.height()), (35, 20));
    assert_eq!(
        expanded.get_pixel(30, 10),
        &Rgba(BLUE),
        "the pasted image is missing from the export"
    );
    assert_eq!(
        expanded.get_pixel(33, 2),
        &Rgba(WHITE),
        "the fill is missing where nothing covers it"
    );
}

#[test]
fn effects_are_placed_on_the_pixels_they_came_from() {
    // Fractional and partly-off-image rectangles must not shift the effect.
    let mut image = RgbaImage::new(24, 24);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = Rgba([x as u8, y as u8, 9, 255]);
    }

    for rect in [
        Rect::new(10.6, 4.0, 6.0, 6.0),
        Rect::new(-8.0, 0.0, 16.0, 4.0),
    ] {
        let highlight = annotation(Kind::Highlight(snp_ligase_core::model::HighlightA {
            rect,
            color: YELLOW,
        }));
        let flattened = flatten_with(image.clone(), vec![highlight.clone()]);
        let mut fonts = Fonts::bundled();
        let sprite = render::rasterize(&highlight, &image, &mut fonts).expect("a sprite");
        // `bounds()` is the annotation's own rectangle and cannot know how much
        // of it is off the image; the sprite is the clipped part. For a
        // rectangle that is inside the image the two are the same thing.
        if rect.x >= 0.0 && rect.right() <= 24.0 {
            assert_eq!(
                render::bounds(&highlight, &mut fonts).round_out().to_rect(),
                sprite.rect(),
                "bounds() and the sprite disagree about where the effect is"
            );
        }

        for row in 0..sprite.pixmap.height() {
            for column in 0..sprite.pixmap.width() {
                let x = sprite.origin.0 + column as i32;
                let y = sprite.origin.1 + row as i32;
                if x < 0 || y < 0 || x >= 24 || y >= 24 {
                    continue;
                }
                let expected = [
                    (u32::from(image.get_pixel(x as u32, y as u32)[0]) * 255 / 255) as u8,
                    (u32::from(image.get_pixel(x as u32, y as u32)[1]) * 255 / 255) as u8,
                    0,
                ];
                let got = flattened.get_pixel(x as u32, y as u32);
                assert_eq!(
                    &expected[..],
                    &got.0[..3],
                    "pixel ({x}, {y}) is not its own base pixel multiplied"
                );
            }
        }
    }
}

#[test]
fn hit_testing_agrees_with_what_was_drawn() {
    // A click must land on the pixels the sprite painted, not near them.
    let rect = Kind::Rect(RectA {
        rect: Rect::new(40.0, 40.0, 60.0, 40.0),
        stroke: Some(Stroke {
            color: RED,
            width: 6.0,
        }),
        fill: None,
        corner_radius: 0.0,
    });
    let a = annotation(rect);

    let mut fonts = Fonts::bundled();
    let on_the_stroke = Point::new(70.0, 40.0);
    assert!(edit::hit_test(&a, on_the_stroke, 0.0, &mut fonts));
    assert!(!edit::hit_test(&a, Point::new(70.0, 65.0), 0.0, &mut fonts));
}

// -------------------------------------------------------------------------
// NEXT_STEPS step 10: annotations outside the image grow the saved image
// -------------------------------------------------------------------------

const WHITE: ModelRgba = [255, 255, 255, 255];
const BLACK: ModelRgba = [0, 0, 0, 255];

/// The document the step-10 tests share: a black base with one filled red
/// rectangle 50 px to the left of it, 40 × 40, no stroke.
fn document_with_an_outside_rectangle() -> snp_ligase_core::model::Document {
    let mut doc = snp_ligase_core::model::Document::new(RgbaImage::from_pixel(60, 60, Rgba(BLACK)));
    doc.push(Kind::Rect(RectA {
        rect: Rect::new(-50.0, 20.0, 40.0, 40.0),
        stroke: None,
        fill: Some(RED),
        corner_radius: 0.0,
    }));
    doc
}

fn expand(doc: &snp_ligase_core::model::Document, fill: Option<ModelRgba>) -> RgbaImage {
    render::flatten_with(
        doc,
        &mut Fonts::bundled(),
        &render::FlattenOptions { expand: true, fill },
    )
}

#[test]
fn a_rectangle_to_the_left_grows_the_export_and_the_base_keeps_its_offset() {
    let doc = document_with_an_outside_rectangle();
    let export = render::export_bounds(&doc, &mut Fonts::bundled());
    // The rectangle paints x = -50..-10, y = 20..60; the base is 60 × 60.
    // The export is their exact union: no anti-aliasing slack (J2).
    assert_eq!(
        (export.x, export.y, export.right, export.bottom),
        (-50, 0, 60, 60)
    );

    let expanded = expand(&doc, Some(WHITE));
    assert_eq!((expanded.width(), expanded.height()), (110, 60));

    // The far corner of the added area is the fill colour.
    assert_eq!(expanded.get_pixel(0, 0), &Rgba(WHITE));
    assert_eq!(expanded.get_pixel(49, 0), &Rgba(WHITE));
    // The base image sits at the offset: black at its own top-left corner.
    assert_eq!(expanded.get_pixel(50, 0), &Rgba(BLACK));
    assert_eq!(expanded.get_pixel(80, 30), &Rgba(BLACK));
    // The rectangle is drawn at its image position plus the offset.
    assert_eq!(expanded.get_pixel(0, 40), &Rgba(RED));
    assert_eq!(expanded.get_pixel(20, 40), &Rgba(RED));
}

#[test]
fn a_transparent_expansion_leaves_the_added_area_clear() {
    let doc = document_with_an_outside_rectangle();
    let expanded = expand(&doc, None);

    assert_eq!(expanded.get_pixel(0, 0), &Rgba([0, 0, 0, 0]));
    // The base and the rectangle are unaffected by the transparency around
    // them.
    assert_eq!(expanded.get_pixel(82, 30), &Rgba(BLACK));
    assert_eq!(expanded.get_pixel(20, 40), &Rgba(RED));
}

#[test]
fn a_text_label_above_the_image_grows_the_output_height() {
    // The human's own example: a label placed above a screenshot (NEXT_STEPS
    // step 10).
    let mut doc = snp_ligase_core::model::Document::new(base(120, 80));
    doc.push(Kind::Text(TextA {
        pos: Point::new(10.0, -60.0),
        text: "Label".to_owned(),
        font_family: "Inter".to_owned(),
        size: 28.0,
        bold: true,
        italic: false,
        color: WHITE,
        background: Some(BLUE),
        padding: 6.0,
        outline: None,
    }));

    let export = render::export_bounds(&doc, &mut Fonts::bundled());
    assert!(
        export.y < 0,
        "the label did not grow the export: {export:?}"
    );
    assert_eq!((export.x, export.right), (0, 120), "only the top grew");

    let expanded = expand(&doc, Some(WHITE));
    assert_eq!(
        expanded.height(),
        export.height(),
        "the output height does not include the label"
    );
    // Inside the label's box, above the image: the text background, shifted by
    // the export offset.
    let x = 12u32;
    let y = (-58 - export.y) as u32;
    assert_eq!(expanded.get_pixel(x, y), &Rgba(BLUE));
    // The base image still starts one export-height minus its own height down.
    assert_eq!(expanded.get_pixel(60, export.height() - 40), &Rgba(WHITE));
}

#[test]
fn cut_mode_is_byte_for_byte_the_old_flatten() {
    // The option that keeps everything must not change what cutting off
    // produces: every existing golden depends on it (NEXT_STEPS step 10).
    let mut doc = document_with_an_outside_rectangle();
    doc.push(Kind::Highlight(snp_ligase_core::model::HighlightA {
        rect: Rect::new(0.0, 0.0, 60.0, 10.0),
        color: YELLOW,
    }));
    doc.push(Kind::Blur(snp_ligase_core::model::BlurA {
        rect: Rect::new(30.0, 30.0, 20.0, 20.0),
        radius: 4.0,
    }));
    doc.push(Kind::Text(TextA {
        pos: Point::new(5.0, 5.0),
        text: "In".to_owned(),
        font_family: "Inter".to_owned(),
        size: 20.0,
        bold: false,
        italic: false,
        color: BLACK,
        background: None,
        padding: 4.0,
        outline: None,
    }));

    let old = render::flatten(&doc, &mut Fonts::bundled());
    let cut = render::flatten_with(&doc, &mut Fonts::bundled(), &render::FlattenOptions::CUT);
    assert_eq!(old, cut, "cut mode is not the old flatten");
}

#[test]
fn a_blur_that_sticks_out_of_the_image_does_not_grow_the_export() {
    // J1: dragging a blur past the top edge is the natural way to cover a band
    // along the edge. Blur and Highlight only ever draw inside the base image,
    // so the export must stay exactly the size of the base.
    let mut doc = snp_ligase_core::model::Document::new(two_tone(60, 20));
    doc.push(Kind::Blur(snp_ligase_core::model::BlurA {
        // 30 px above the image, 20 px below it, 20 px past the left edge.
        rect: Rect::new(-20.0, -30.0, 100.0, 70.0),
        radius: 4.0,
    }));

    let export = render::export_bounds(&doc, &mut Fonts::bundled());
    assert_eq!(
        (export.x, export.y, export.right, export.bottom),
        (0, 0, 60, 20),
        "a blur grew the export"
    );

    let expanded = expand(&doc, Some(WHITE));
    assert_eq!((expanded.width(), expanded.height()), (60, 20));
    // Inside the image the blur really ran: the hard edge at x = 30 is soft.
    let soft = expanded.get_pixel(30, 10)[0];
    assert!(
        (20..235).contains(&soft),
        "expected a soft edge, got {soft}"
    );
}

#[test]
fn a_highlight_that_sticks_out_of_the_image_does_not_grow_the_export() {
    // J1 for the other effect.
    let mut doc = snp_ligase_core::model::Document::new(two_tone(60, 20));
    doc.push(Kind::Highlight(snp_ligase_core::model::HighlightA {
        rect: Rect::new(-10.0, -10.0, 80.0, 40.0),
        color: YELLOW,
    }));

    let export = render::export_bounds(&doc, &mut Fonts::bundled());
    assert_eq!(
        (export.x, export.y, export.right, export.bottom),
        (0, 0, 60, 20),
        "a highlight grew the export"
    );
}

#[test]
fn a_stroke_one_pixel_inside_the_edge_leaves_the_export_at_the_base_size() {
    // J2: `bounds()` adds two pixels of anti-aliasing slack, so the old export
    // grew even though every painted pixel was inside the image.
    let mut doc = snp_ligase_core::model::Document::new(base(60, 60));
    doc.push(Kind::Rect(RectA {
        // A 4 px stroke centred on the path covers x = 1..5: fully inside,
        // one pixel from the edge.
        rect: Rect::new(3.0, 3.0, 40.0, 40.0),
        stroke: Some(Stroke {
            color: BLACK,
            width: 4.0,
        }),
        fill: None,
        corner_radius: 0.0,
    }));

    let export = render::export_bounds(&doc, &mut Fonts::bundled());
    assert_eq!(
        (export.x, export.y, export.right, export.bottom),
        (0, 0, 60, 60),
        "the anti-aliasing slack grew the export"
    );
}

#[test]
fn a_stroke_centred_on_the_edge_grows_the_export_by_exactly_the_painted_part() {
    // J2: a 4 px stroke centred on x = 0 paints x = -2..2, so the export grows
    // by exactly those two pixels — not by the four the old `bounds()` did.
    let mut doc = snp_ligase_core::model::Document::new(base(60, 60));
    doc.push(Kind::Rect(RectA {
        rect: Rect::new(0.0, 0.0, 40.0, 40.0),
        stroke: Some(Stroke {
            color: BLACK,
            width: 4.0,
        }),
        fill: None,
        corner_radius: 0.0,
    }));

    let export = render::export_bounds(&doc, &mut Fonts::bundled());
    assert_eq!(
        (export.x, export.y, export.right, export.bottom),
        (-2, -2, 60, 60),
        "the export did not follow the painted stroke"
    );
    // The painted strip really is there: a pixel of the solid part of the
    // stroke (image x = -1, y = 20) is the stroke colour, not the fill.
    let expanded = expand(&doc, Some(WHITE));
    assert_eq!(
        expanded.get_pixel(1, 22),
        &Rgba(BLACK),
        "the painted stroke is missing from the expanded export"
    );
}

#[test]
fn painted_bounds_follow_the_pixels_not_the_sprite_canvas() {
    // J2: the sprite canvas comes from `bounds()` and carries two pixels of
    // anti-aliasing slack; `painted_bounds` reports only what was drawn.
    let mut doc = snp_ligase_core::model::Document::new(base(60, 60));
    doc.push(Kind::Rect(RectA {
        rect: Rect::new(3.0, 3.0, 40.0, 40.0),
        stroke: Some(Stroke {
            color: BLACK,
            width: 4.0,
        }),
        fill: None,
        corner_radius: 0.0,
    }));

    let annotation = &doc.annotations[0];
    let sprite = render::rasterize(annotation, &doc.base, &mut Fonts::bundled()).expect("a sprite");
    let painted = render::painted_bounds(&sprite).expect("painted pixels");
    assert_eq!(
        (painted.x, painted.y, painted.right, painted.bottom),
        (1, 1, 45, 45)
    );
    assert!(
        sprite.rect().x < painted.x as f32,
        "the sprite canvas should have slack around the painted pixels"
    );

    // A sprite that draws nothing has no painted bounds at all, and cannot
    // grow the export.
    let mut empty = snp_ligase_core::model::Document::new(base(60, 60));
    empty.push(Kind::Text(TextA {
        pos: Point::new(10.0, 10.0),
        text: String::new(),
        font_family: "Inter".to_owned(),
        size: 20.0,
        bold: false,
        italic: false,
        color: BLACK,
        background: None,
        padding: 6.0,
        outline: None,
    }));
    let annotation = &empty.annotations[0];
    let sprite =
        render::rasterize(annotation, &empty.base, &mut Fonts::bundled()).expect("a sprite");
    assert_eq!(render::painted_bounds(&sprite), None);
    assert_eq!(
        render::export_bounds(&empty, &mut Fonts::bundled()),
        snp_ligase_core::geom::RectI {
            x: 0,
            y: 0,
            right: 60,
            bottom: 60
        }
    );
}

// ---------------------------------------------------------------------------
// Freehand strokes (NEXT_STEPS step 14a).

/// One freehand stroke: the points from (x, y) pairs, one stroke style.
fn freehand(points: &[(f32, f32)], width: f32, colour: ModelRgba) -> Kind {
    Kind::Freehand(FreehandA {
        points: points.iter().map(|&(x, y)| Point::new(x, y)).collect(),
        stroke: Stroke {
            color: colour,
            width,
        },
    })
}

#[test]
fn a_freehand_stroke_paints_along_its_points() {
    let image = flatten_with(
        base(60, 60),
        vec![annotation(freehand(
            &[(10.0, 20.0), (50.0, 20.0)],
            8.0,
            RED,
        ))],
    );

    assert_eq!(image.get_pixel(30, 20), &Rgba(RED), "on the stroke");
    assert_eq!(image.get_pixel(20, 20), &Rgba(RED), "on the stroke");
    // Well away from the line: the base is untouched.
    assert_eq!(image.get_pixel(30, 5), &Rgba([255, 255, 255, 255]));
    assert_eq!(image.get_pixel(30, 40), &Rgba([255, 255, 255, 255]));
}

#[test]
fn a_freehand_stroke_that_overlaps_itself_is_painted_once() {
    // NEXT_STEPS step 14a: the stroke is **one** stroked path, so a
    // translucent stroke must not darken where it crosses itself. Going out
    // and back over the same pixels is the worst case for blending twice.
    let translucent = |points: &[(f32, f32)]| annotation(freehand(points, 12.0, [255, 0, 0, 128]));

    let once = flatten_with(
        base(60, 30),
        vec![translucent(&[(10.0, 15.0), (50.0, 15.0)])],
    );
    let twice = flatten_with(
        base(60, 30),
        vec![translucent(&[(10.0, 15.0), (50.0, 15.0), (10.0, 15.0)])],
    );

    assert_eq!(
        twice.get_pixel(30, 15),
        once.get_pixel(30, 15),
        "the overlap was blended twice"
    );
}

#[test]
fn a_freehand_stroke_covers_half_its_width_on_each_side_of_the_line() {
    // NEXT_STEPS step 14a: the shape's rectangle is the polyline grown by half
    // the stroke width, and `bounds` adds the anti-aliasing slack on top.
    let mut fonts = Fonts::bundled();
    let stroke = annotation(freehand(&[(10.0, 20.0), (50.0, 20.0)], 8.0, RED));

    let shape = render::shape_bounds(&stroke, &mut fonts);
    assert_eq!(shape, Rect::new(6.0, 16.0, 48.0, 8.0));
    assert_eq!(render::bounds(&stroke, &mut fonts), shape.inflated(2.0));
}

#[test]
fn a_freehand_stroke_outside_the_image_grows_the_export() {
    // NEXT_STEPS step 14a: a stroke is an ordinary annotation, so its painted
    // pixels grow the export like any other (J2).
    let mut doc = snp_ligase_core::model::Document::new(RgbaImage::from_pixel(60, 60, Rgba(BLACK)));
    doc.push(freehand(&[(-40.0, 30.0), (-5.0, 30.0)], 8.0, RED));

    let export = render::export_bounds(&doc, &mut Fonts::bundled());
    assert!(
        export.x < 0,
        "a stroke left of the image must grow the export: {export:?}"
    );

    let expanded = expand(&doc, Some(WHITE));
    assert_eq!(
        (expanded.width(), expanded.height()),
        (export.width(), export.height())
    );
    // The middle of the stroke, at its image position plus the export offset.
    let offset = (-export.x) as u32;
    assert_eq!(expanded.get_pixel(offset - 22, 30), &Rgba(RED));
    // In the area the stroke added, but away from it, it is the bare fill: a
    // stroke is not part of the base layer the effects read.
    assert_eq!(expanded.get_pixel(offset - 30, 5), &Rgba(WHITE));
}
