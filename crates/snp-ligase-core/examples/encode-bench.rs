//! Times the 4K encodes for the H6 decision: is saving on the UI thread fast
//! enough, or does encode + write have to move to a background thread?
//!
//! Run it on the release build:
//!
//! ```text
//! cargo run --release -p snp-ligase-core --example encode-bench
//! ```

use std::time::Instant;

use image::RgbaImage;
use snp_ligase_core::codec::{self, EncodeOptions, Format};

/// A 4K image with the kind of content a capture has: flat UI areas, gradients
/// and enough fine variation that the lossless encoders cannot cheat.
fn capture_like(width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, y| {
        let base = (x * 255 / width) as u8;
        let band = if (y / 24) % 2 == 0 { base } else { 255 - base };
        let glyph = if (x / 7 + y / 11) % 13 == 0 { 40 } else { 0 };
        // A little pseudo-random noise, like the anti-aliased text and photos a
        // real capture is full of.
        let noise = ((x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 24) as u8 / 6;
        image::Rgba([
            band.saturating_add(glyph).saturating_add(noise),
            base / 2 + noise,
            (y * 255 / height) as u8,
            255,
        ])
    })
}

fn main() {
    let image = capture_like(3840, 2160);
    let options = EncodeOptions::default();
    println!("4K capture-like image (3840×2160)");
    for format in Format::ALL {
        let started = Instant::now();
        match codec::encode(&image, *format, &options) {
            Ok(bytes) => println!(
                "{:<22} {:>9.1} ms {:>8.1} MiB",
                format.label(),
                started.elapsed().as_secs_f64() * 1000.0,
                bytes.len() as f64 / (1024.0 * 1024.0),
            ),
            Err(err) => println!("{:<22} failed: {err}", format.label()),
        }
    }
}
