//! Golden-image tests (SPEC section 11.2).
//!
//! These render reference scenes and compare them to PNGs in `tests/golden/`,
//! with a small tolerance: a per-channel difference of at most 2, on at most
//! 0.5% of the pixels. `UPDATE_GOLDEN=1 cargo test` rewrites them.
//!
//! **Goldens only prove that the output has not changed, not that it is right.**
//! Every golden that is created or updated has to be listed in the milestone
//! report so a human can look at it; `render-demo.png` shows the same scenes
//! larger, which is the easier of the two to eyeball.
//!
//! Text and Number use the bundled test font (SPEC 11.3), so these scenes do
//! not depend on what is installed on the machine.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::{Rgba, RgbaImage};
use snp_ligase_core::codec;
use snp_ligase_core::geom::{Point, Rect};
use snp_ligase_core::model::{
    Annotation, BlurA, FreehandA, HighlightA, ImageA, Kind, PathA, PolygonA, RectA,
    Rgba as ModelRgba, Stroke, defaults,
};
use snp_ligase_core::render;
use snp_ligase_core::text::Fonts;

/// How much a pixel may differ from the golden before it counts.
const CHANNEL_TOLERANCE: i32 = 2;
/// How many pixels may differ at all, as a fraction of the image.
const PIXEL_TOLERANCE: f64 = 0.005;

const BLUE: ModelRgba = [0, 120, 255, 255];
const GREEN: ModelRgba = [0, 200, 80, 255];

/// A small stand-in for a screenshot: a gradient with a hard edge in the middle,
/// so blur and highlight have something to work on.
fn scene_base(width: u32, height: u32) -> RgbaImage {
    let mut image = RgbaImage::new(width, height);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let value = if x < width / 2 { 40u8 } else { 220u8 };
        *pixel = Rgba([value / 2 + 40, value, 255 - value / 3, 255]);
        let _ = y;
    }
    image
}

fn arrow() -> Annotation {
    Annotation {
        id: 1,
        shadow: false,
        kind: Kind::Path(PathA {
            points: vec![Point::new(20.0, 120.0), Point::new(200.0, 60.0)],
            curved: false,
            stroke: Stroke {
                color: defaults::STROKE_COLOR,
                width: 6.0,
            },
            head_start: false,
            head_end: true,
        }),
    }
}

fn curved_arrow() -> Annotation {
    Annotation {
        id: 2,
        shadow: true,
        kind: Kind::Path(PathA {
            points: vec![
                Point::new(20.0, 200.0),
                Point::new(90.0, 140.0),
                Point::new(160.0, 210.0),
                Point::new(230.0, 150.0),
            ],
            curved: true,
            stroke: Stroke {
                color: BLUE,
                width: 5.0,
            },
            head_start: true,
            head_end: true,
        }),
    }
}

fn rectangle() -> Annotation {
    Annotation {
        id: 3,
        shadow: true,
        kind: Kind::Rect(RectA {
            rect: Rect::new(30.0, 30.0, 120.0, 70.0),
            stroke: Some(defaults::stroke()),
            fill: Some(GREEN),
            corner_radius: 12.0,
        }),
    }
}

fn polygon() -> Annotation {
    Annotation {
        id: 4,
        shadow: false,
        kind: Kind::Polygon(PolygonA {
            points: vec![
                Point::new(250.0, 40.0),
                Point::new(320.0, 45.0),
                Point::new(300.0, 120.0),
                Point::new(240.0, 110.0),
            ],
            stroke: Some(Stroke {
                color: [255, 255, 255, 255],
                width: 4.0,
            }),
            fill: Some([255, 160, 0, 200]),
        }),
    }
}

fn highlight() -> Annotation {
    Annotation {
        id: 5,
        shadow: false,
        kind: Kind::Highlight(HighlightA {
            rect: Rect::new(20.0, 230.0, 140.0, 40.0),
            color: defaults::HIGHLIGHT_COLOR,
        }),
    }
}

fn blur() -> Annotation {
    Annotation {
        id: 6,
        shadow: false,
        kind: Kind::Blur(BlurA {
            rect: Rect::new(200.0, 230.0, 140.0, 40.0),
            radius: 14.0,
        }),
    }
}

/// Text uses the bundled font (SPEC 11.3), never a system one.
fn text() -> Annotation {
    Annotation {
        id: 7,
        shadow: false,
        kind: Kind::Text(snp_ligase_core::model::TextA {
            pos: Point::new(30.0, 60.0),
            text: "Hello\nworld!".to_owned(),
            font_family: "Inter".to_owned(),
            size: 32.0,
            bold: true,
            italic: false,
            color: [255, 255, 255, 255],
            background: Some([0, 0, 255, 255]),
            padding: 6.0,
            outline: None,
        }),
    }
}

/// A badge and one with a pointer, in SPEC 7.7's number red.
fn numbers() -> Vec<Annotation> {
    vec![
        Annotation {
            id: 8,
            shadow: false,
            kind: Kind::Number(snp_ligase_core::model::NumberA {
                center: Point::new(70.0, 200.0),
                value: 1,
                radius: 18.0,
                fill: defaults::STROKE_COLOR,
                pointer_tip: None,
            }),
        },
        Annotation {
            id: 9,
            shadow: true,
            kind: Kind::Number(snp_ligase_core::model::NumberA {
                center: Point::new(160.0, 200.0),
                value: 123,
                radius: 18.0,
                fill: defaults::STROKE_COLOR,
                pointer_tip: Some(Point::new(230.0, 150.0)),
            }),
        },
    ]
}

/// Step 8's three text looks side by side: just text, an outlined box with no
/// fill, and an outlined and filled box (NEXT_STEPS section 6e).
fn text_outline_variants() -> Vec<Annotation> {
    let variant = |id: u64,
                   pos: Point,
                   colour: ModelRgba,
                   background: Option<ModelRgba>,
                   outline: Option<Stroke>| Annotation {
        id,
        shadow: false,
        kind: Kind::Text(snp_ligase_core::model::TextA {
            pos,
            text: "Hello".to_owned(),
            font_family: "Inter".to_owned(),
            size: 28.0,
            bold: true,
            italic: false,
            color: colour,
            background,
            padding: 6.0,
            outline,
        }),
    };
    vec![
        // Just text: no background, no outline.
        variant(10, Point::new(20.0, 50.0), [0, 0, 0, 255], None, None),
        // Text in an outlined box with no fill.
        variant(
            11,
            Point::new(160.0, 50.0),
            [0, 0, 0, 255],
            None,
            Some(defaults::stroke()),
        ),
        // Text in an outlined and filled box: today's default, plus an outline.
        variant(
            12,
            Point::new(300.0, 50.0),
            [255, 255, 255, 255],
            Some([0, 0, 255, 255]),
            Some(defaults::stroke()),
        ),
    ]
}

fn flatten_scene(annotations: Vec<Annotation>, width: u32, height: u32) -> RgbaImage {
    let mut doc = snp_ligase_core::model::Document::new(scene_base(width, height));
    for annotation in annotations {
        doc.annotations.push(annotation);
    }
    render::flatten(&doc, &mut Fonts::bundled())
}

/// Compares against the golden, or rewrites it when `UPDATE_GOLDEN=1`.
fn check_golden(name: &str, rendered: &RgbaImage) {
    let path = golden_path(name);
    let updating = std::env::var_os("UPDATE_GOLDEN").is_some();

    if updating || !path.exists() {
        let bytes = codec::encode_png(rendered).expect("the scene encodes");
        std::fs::create_dir_all(path.parent().expect("goldens live in a folder"))
            .expect("the golden folder can be created");
        std::fs::write(&path, bytes).expect("the golden can be written");
        println!("wrote golden {}", path.display());
        if !updating {
            // Creating a golden is not the same as verifying one: the human
            // has to look at it (SPEC section 11.2).
            println!("NEW GOLDEN {} - list it in the milestone report", name);
        }
        return;
    }

    let expected = codec::decode(&std::fs::read(&path).expect("the golden can be read"))
        .expect("the golden decodes");
    assert_eq!(
        (expected.width(), expected.height()),
        (rendered.width(), rendered.height()),
        "{name}: the golden is the wrong size"
    );

    let mut differing = 0usize;
    let mut worst = 0i32;
    for (actual, wanted) in rendered.pixels().zip(expected.pixels()) {
        let channels = [0usize, 1, 2, 3].map(|c| {
            let difference = i32::from(actual[c]) - i32::from(wanted[c]);
            difference.abs()
        });
        let pixel_worst = channels.into_iter().max().unwrap_or(0);
        worst = worst.max(pixel_worst);
        if pixel_worst > CHANNEL_TOLERANCE {
            differing += 1;
        }
    }

    let total = rendered.width() as f64 * rendered.height() as f64;
    let fraction = differing as f64 / total;
    assert!(
        fraction <= PIXEL_TOLERANCE,
        "{name}: {differing} of {:.0} pixels differ by more than {CHANNEL_TOLERANCE} ({:.3}%, worst {worst})",
        total,
        fraction * 100.0
    );
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.png"))
}

#[test]
fn golden_an_arrow() {
    check_golden("arrow", &flatten_scene(vec![arrow()], 360, 280));
}

#[test]
fn golden_a_curved_double_arrow_with_a_shadow() {
    check_golden(
        "curved-arrow",
        &flatten_scene(vec![curved_arrow()], 360, 280),
    );
}

#[test]
fn golden_a_rounded_filled_rectangle_with_a_shadow() {
    check_golden("rectangle", &flatten_scene(vec![rectangle()], 360, 280));
}

#[test]
fn golden_a_filled_polygon() {
    check_golden("polygon", &flatten_scene(vec![polygon()], 360, 280));
}

#[test]
fn golden_highlight_over_a_hard_edge() {
    check_golden("highlight", &flatten_scene(vec![highlight()], 360, 280));
}

#[test]
fn golden_blur_over_a_hard_edge() {
    check_golden("blur", &flatten_scene(vec![blur()], 360, 280));
}

#[test]
fn golden_text_with_a_background() {
    check_golden("text", &flatten_scene(vec![text()], 360, 280));
}

/// NEXT_STEPS step 8: the three text looks (plain, outline-only, outline and
/// fill) side by side, on a light background so the black text reads.
#[test]
fn golden_text_outline_variants() {
    let mut doc = snp_ligase_core::model::Document::new(RgbaImage::from_pixel(
        460,
        160,
        Rgba([240, 240, 240, 255]),
    ));
    for annotation in text_outline_variants() {
        doc.annotations.push(annotation);
    }
    let rendered = render::flatten(&doc, &mut Fonts::bundled());
    check_golden("text-outline", &rendered);
}

#[test]
fn golden_number_badges() {
    check_golden("number", &flatten_scene(numbers(), 360, 280));
}

/// NEXT_STEPS step 10: the export grown to keep a text label above the image
/// and a shadowed rectangle sticking out past the right edge, on the default
/// white fill.
#[test]
fn golden_expanded_export() {
    let mut doc = snp_ligase_core::model::Document::new(scene_base(360, 280));
    doc.annotations.push(Annotation {
        id: 20,
        shadow: false,
        kind: Kind::Text(snp_ligase_core::model::TextA {
            pos: Point::new(40.0, -64.0),
            text: "Above the image".to_owned(),
            font_family: "Inter".to_owned(),
            size: 28.0,
            bold: true,
            italic: false,
            color: [255, 255, 255, 255],
            background: Some(BLUE),
            padding: 6.0,
            outline: None,
        }),
    });
    doc.annotations.push(Annotation {
        id: 21,
        shadow: true,
        kind: Kind::Rect(RectA {
            rect: Rect::new(320.0, 150.0, 80.0, 60.0),
            stroke: Some(defaults::stroke()),
            fill: Some(GREEN),
            corner_radius: 8.0,
        }),
    });

    let rendered = render::flatten_with(
        &doc,
        &mut Fonts::bundled(),
        &render::FlattenOptions {
            expand: true,
            fill: Some([255, 255, 255, 255]),
        },
    );
    check_golden("expanded", &rendered);
}

/// NEXT_STEPS step 11: two pasted images stitched onto the base, with a blur
/// across the first one — the pasted pixels are what gets blurred, which is
/// the key compositing decision of the step. NEXT_STEPS step 12 adds a second
/// blur wholly outside the original image, which needs the layer to reach the
/// pasted screenshot beside it.
#[test]
fn golden_a_stitched_image() {
    /// A small stand-in for a pasted screenshot: red with white stripes.
    fn striped_patch() -> RgbaImage {
        let mut image = RgbaImage::new(90, 60);
        for (x, _y, pixel) in image.enumerate_pixels_mut() {
            *pixel = if (x / 6) % 2 == 0 {
                Rgba([220, 40, 40, 255])
            } else {
                Rgba([255, 255, 255, 255])
            };
        }
        image
    }

    /// A green tile with a darker border.
    fn green_tile() -> RgbaImage {
        let mut image = RgbaImage::new(50, 50);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = if x == 0 || y == 0 || x == 49 || y == 49 {
                Rgba([0, 90, 40, 255])
            } else {
                Rgba([0, 200, 80, 255])
            };
        }
        image
    }

    let mut doc = snp_ligase_core::model::Document::new(scene_base(240, 160));
    doc.annotations.push(Annotation {
        id: 30,
        shadow: true,
        kind: Kind::Image(ImageA {
            pos: Point::new(180.0, 20.0),
            image: Arc::new(striped_patch()),
        }),
    });
    doc.annotations.push(Annotation {
        id: 31,
        shadow: false,
        kind: Kind::Image(ImageA {
            pos: Point::new(200.0, 90.0),
            image: Arc::new(green_tile()),
        }),
    });
    doc.annotations.push(Annotation {
        id: 32,
        shadow: false,
        kind: Kind::Blur(BlurA {
            rect: Rect::new(190.0, 40.0, 60.0, 40.0),
            radius: 8.0,
        }),
    });
    // NEXT_STEPS step 12: a second blur, entirely outside the original image
    // (which ends at x = 240) — the case that used to do nothing at all.
    doc.annotations.push(Annotation {
        id: 33,
        shadow: false,
        kind: Kind::Blur(BlurA {
            rect: Rect::new(250.0, 24.0, 16.0, 30.0),
            radius: 6.0,
        }),
    });

    let rendered = render::flatten_with(
        &doc,
        &mut Fonts::bundled(),
        &render::FlattenOptions {
            expand: true,
            fill: Some([255, 255, 255, 255]),
        },
    );
    check_golden("stitched", &rendered);
}

/// NEXT_STEPS step 14a: an opaque freehand scribble with a shadow, and a
/// translucent marker that crosses itself — the crossing must stay uniform.
#[test]
fn golden_freehand_scribbles() {
    let mut doc = snp_ligase_core::model::Document::new(scene_base(360, 280));
    doc.annotations.push(Annotation {
        id: 40,
        shadow: true,
        kind: Kind::Freehand(FreehandA {
            points: vec![
                Point::new(30.0, 200.0),
                Point::new(65.0, 160.0),
                Point::new(100.0, 215.0),
                Point::new(135.0, 165.0),
                Point::new(170.0, 205.0),
            ],
            stroke: Stroke {
                color: defaults::STROKE_COLOR,
                width: 4.0,
            },
        }),
    });
    doc.annotations.push(Annotation {
        id: 41,
        shadow: false,
        kind: Kind::Freehand(FreehandA {
            points: vec![
                Point::new(190.0, 230.0),
                Point::new(260.0, 170.0),
                Point::new(200.0, 170.0),
                Point::new(270.0, 230.0),
            ],
            stroke: Stroke {
                color: [255, 255, 0, 128],
                width: 16.0,
            },
        }),
    });

    let rendered = render::flatten(&doc, &mut Fonts::bundled());
    check_golden("freehand", &rendered);
}

#[test]
fn golden_everything_at_once() {
    let mut annotations = vec![
        highlight(),
        blur(),
        arrow(),
        curved_arrow(),
        rectangle(),
        polygon(),
        text(),
    ];
    annotations.extend(numbers());
    check_golden("all", &flatten_scene(annotations, 360, 280));
}

#[test]
fn goldens_are_not_empty_files() {
    // A golden that is all one colour means the scene stopped drawing, which is
    // the failure a pure "did it change" test cannot see.
    let scene = flatten_scene(vec![arrow(), rectangle()], 360, 280);
    let distinct: std::collections::HashSet<[u8; 4]> = scene.pixels().map(|p| p.0).collect();
    assert!(
        distinct.len() > 20,
        "the scene only has {} distinct colours",
        distinct.len()
    );
}
