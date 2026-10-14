//! snp-ligase core: the annotation model, rendering and codecs.
//!
//! This crate is pure Rust. It contains no GUI, windowing or OS code, so all of
//! its behaviour is unit-testable on any machine (SPEC section 2). The OS
//! boundary lives in `snp-ligase-platform`, the user interface in
//! `snp-ligase-app`.
//!
//! The shape of the crate follows SPEC section 4:
//!
//! - `geom` — points, rectangles and small vector maths.
//! - `model` — `Document`, `Annotation` and every annotation struct.
//! - `path` — Catmull-Rom to Bezier curves and arrowhead geometry.
//! - `render` — one rasterizer per annotation kind, sprites, and `flatten`.
//! - `effects` — box blur, shadow, highlight multiply.
//! - `edit` — hit testing, handles, drag, translate, crop.
//! - `color` — the HDR to SDR conversion the capture path needs (M1).
//! - `codec` — the format table of SPEC section 9: PNG, JPEG, WebP, JPEG XL
//!   (lossless), AVIF behind its feature, and BMP/GIF decoding.
//! - `text` — the font system wrapper; M2 has no fonts yet, see its docs.
#![forbid(unsafe_code)]

pub mod codec;
pub mod color;
pub mod edit;
pub mod effects;
pub mod geom;
pub mod model;
pub mod path;
pub mod render;
pub mod text;

/// Errors returned by this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A frame buffer did not hold the pixels its width, height and pitch
    /// promise. This is what a truncated or corrupt capture looks like.
    #[error(
        "frame buffer too small: {width}x{height} at pitch {pitch} needs {need} bytes, buffer has {have}"
    )]
    FrameTooSmall {
        width: u32,
        height: u32,
        pitch: usize,
        need: usize,
        have: usize,
    },

    /// An image could not be encoded or decoded.
    #[error("image codec error: {0}")]
    Codec(#[from] image::ImageError),

    /// The JPEG XL encoder or decoder failed (SPEC section 9).
    #[error("JPEG XL codec error: {0}")]
    Jxl(String),

    /// Reading image bytes off a reader failed.
    #[error("could not read image data: {0}")]
    Io(#[from] std::io::Error),
}

/// Result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;
