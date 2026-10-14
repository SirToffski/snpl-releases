//! Renders one of every annotation kind built so far into a single PNG
//! (SPEC section 12, milestone M2).
//!
//! ```text
//! cargo run -p snp-ligase-core --example render-demo
//! cargo run -p snp-ligase-core --example render-demo -- out.png
//! ```
//!
//! This is the picture a human should look at after M2: shapes are
//! anti-aliased, arrowheads point the right way, the curve is smooth, the blur
//! makes text unreadable and the highlight leaves dark text readable. It writes
//! to `target/render-demo.png` unless given another path.

use std::path::PathBuf;

use image::{Rgba, RgbaImage};
use snp_ligase_core::codec;
use snp_ligase_core::geom::{Point, Rect};
use snp_ligase_core::model::{
    BlurA, Document, HighlightA, Kind, NumberA, PathA, PolygonA, RectA, Rgba as ModelRgba, Stroke,
    TextA, defaults,
};
use snp_ligase_core::render;
use snp_ligase_core::text::Fonts;

const WIDTH: u32 = 960;
const HEIGHT: u32 = 640;

const BLUE: ModelRgba = [0, 120, 255, 255];
const GREEN: ModelRgba = [0, 200, 80, 255];

/// A stand-in for a screenshot: the kind of content the effects have to cope
/// with, a light background, hard black-on-white text, and a hard vertical
/// edge for the blur to smear.
fn base() -> RgbaImage {
    let mut image = RgbaImage::from_pixel(WIDTH, HEIGHT, Rgba([245, 245, 248, 255]));

    // A panel on the left, white with a dark border: something to highlight,
    // blur and outline.
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        if (20..=420).contains(&x) && (20..=300).contains(&y) {
            *pixel = Rgba([255, 255, 255, 255]);
        }
        if (x == 20 || x == 420) && (20..=300).contains(&y) {
            *pixel = Rgba([40, 40, 46, 255]);
        }
        if (y == 20 || y == 300) && (20..=420).contains(&x) {
            *pixel = Rgba([40, 40, 46, 255]);
        }
        // A hard vertical edge at the middle, for the blur to soften.
        if x == 700 {
            *pixel = Rgba([20, 20, 20, 255]);
        }
    }

    // Something that reads as text: rows of dark dashes. The blur has to make
    // these unreadable and the highlight has to keep them readable.
    for row in 0..6 {
        let y = 70 + row * 32;
        let width = 300 - row * 18;
        for x in 50..(50 + width) {
            for dy in 0..10 {
                image.put_pixel(x, y + dy, Rgba([30, 30, 36, 255]));
            }
        }
    }

    image
}

fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    let output: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/render-demo.png"));

    match run(&output) {
        Ok(()) => {
            println!("wrote {}", output.display());
            std::process::ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("render-demo: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Builds the scene, flattens it and writes the PNG.
fn run(output: &PathBuf) -> Result<(), String> {
    let mut doc = Document::new(base());

    // Highlight over the text, then a blur over the panel's other half: both
    // are effects, so both read the base image, not each other.
    doc.push(Kind::Highlight(HighlightA {
        rect: Rect::new(50.0, 60.0, 340.0, 60.0),
        color: defaults::HIGHLIGHT_COLOR,
    }));
    doc.push(Kind::Blur(BlurA {
        rect: Rect::new(50.0, 170.0, 340.0, 110.0),
        radius: 14.0,
    }));

    // A rectangle with a fill, a corner radius and a shadow.
    doc.push(Kind::Rect(RectA {
        rect: Rect::new(470.0, 40.0, 200.0, 130.0),
        stroke: Some(defaults::stroke()),
        fill: Some(GREEN),
        corner_radius: 16.0,
    }));

    // A straight line, an arrow at the end, and a double-headed curved one.
    doc.push(Kind::Path(PathA {
        points: vec![Point::new(470.0, 250.0), Point::new(700.0, 250.0)],
        curved: false,
        stroke: defaults::stroke(),
        head_start: false,
        head_end: false,
    }));
    doc.push(Kind::Path(PathA {
        points: vec![Point::new(470.0, 320.0), Point::new(880.0, 320.0)],
        curved: false,
        stroke: Stroke {
            color: BLUE,
            width: 6.0,
        },
        head_start: false,
        head_end: true,
    }));

    let curved = doc.push(Kind::Path(PathA {
        points: vec![
            Point::new(470.0, 430.0),
            Point::new(580.0, 370.0),
            Point::new(700.0, 470.0),
            Point::new(830.0, 400.0),
        ],
        curved: true,
        stroke: Stroke {
            color: defaults::STROKE_COLOR,
            width: 6.0,
        },
        head_start: true,
        head_end: true,
    }));
    // A shadow on the curved arrow, so the demo shows one.
    doc.find_mut(curved).expect("just pushed").shadow = true;

    // A polygon with a translucent fill.
    doc.push(Kind::Polygon(PolygonA {
        points: vec![
            Point::new(60.0, 360.0),
            Point::new(200.0, 340.0),
            Point::new(260.0, 450.0),
            Point::new(150.0, 520.0),
            Point::new(50.0, 450.0),
        ],
        stroke: Some(Stroke {
            color: [255, 255, 255, 255],
            width: 5.0,
        }),
        fill: Some([255, 160, 0, 200]),
    }));

    // A blurred strip across the hard edge in the background.
    doc.push(Kind::Blur(BlurA {
        rect: Rect::new(620.0, 520.0, 160.0, 90.0),
        radius: 28.0,
    }));

    // A rounded rectangle outline with no fill, to check anti-aliasing on the
    // stroke alone.
    doc.push(Kind::Rect(RectA {
        rect: Rect::new(300.0, 540.0, 260.0, 70.0),
        stroke: Some(Stroke {
            color: BLUE,
            width: 4.0,
        }),
        fill: None,
        corner_radius: 24.0,
    }));

    // Text with a background: two lines, white on blue, the SPEC 7.7 default.
    doc.push(Kind::Text(TextA {
        pos: Point::new(700.0, 50.0),
        text: "Text with\na background".to_owned(),
        font_family: "Segoe UI".to_owned(),
        size: 28.0,
        bold: true,
        italic: false,
        color: [255, 255, 255, 255],
        background: Some([0, 0, 255, 255]),
        padding: 6.0,
        outline: None,
    }));

    // Number badges: one plain, one with a pointer and three digits, which has
    // to shrink to stay inside the circle (SPEC 4.2).
    doc.push(Kind::Number(NumberA {
        center: Point::new(730.0, 220.0),
        value: 1,
        radius: 18.0,
        fill: defaults::STROKE_COLOR,
        pointer_tip: None,
    }));
    doc.push(Kind::Number(NumberA {
        center: Point::new(830.0, 250.0),
        value: 123,
        radius: 18.0,
        fill: defaults::STROKE_COLOR,
        pointer_tip: Some(Point::new(900.0, 310.0)),
    }));

    let flattened = render::flatten(&doc, &mut Fonts::load());
    let bytes = codec::encode_png(&flattened).map_err(|e| e.to_string())?;

    if let Some(folder) = output.parent()
        && !folder.as_os_str().is_empty()
    {
        std::fs::create_dir_all(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    }
    std::fs::write(output, bytes).map_err(|e| format!("{}: {e}", output.display()))?;

    let kinds: Vec<&str> = doc.annotations.iter().map(|a| a.kind.name()).collect();
    println!("annotations: {}", kinds.join(", "));
    Ok(())
}
