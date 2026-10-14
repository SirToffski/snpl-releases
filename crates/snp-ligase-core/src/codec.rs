//! Image encoding and decoding (SPEC section 9).
//!
//! The table this module implements:
//!
//! | Format | Encode | Decode |
//! |---|---|---|
//! | PNG | `image` | `image` |
//! | JPEG | `image`, quality 1–100, transparency flattened onto white | `image` |
//! | WebP | `image`, lossless only | `image` |
//! | JPEG XL | `zune-jpegxl`, lossless | `jxl-oxide` |
//! | JPEG XL lossy | `jxl-encoder`, behind the `jxl-lossy` feature (OFF) | — |
//! | AVIF | `image`'s pure-Rust encoder, behind the `avif` feature | no (P2) |
//! | BMP, GIF | — | `image` (GIF: first frame) |
//!
//! Formats whose feature is disabled are not in [`Format::ALL`], so they do not
//! appear in the Save dialog either (SPEC section 9).
//!
//! Every lossless encoder round-trips a test image exactly; the lossy ones
//! produce a decodable file with the right dimensions (SPEC section 9's test
//! list).

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::{
    DynamicImage, ExtendedColorType, ImageEncoder as _, ImageFormat, ImageReader, RgbImage,
    RgbaImage,
};

/// An output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Lossless, the default (SPEC section 9).
    Png,
    /// Lossy, quality 1–100.
    Jpeg,
    /// Lossless only: SPEC section 9 does not ask for lossy WebP.
    WebP,
    /// Lossless JPEG XL (`zune-jpegxl`), the default JXL encoder.
    Jxl,
    /// AVIF (`image`'s pure-Rust encoder). Compiled in unless the `avif`
    /// feature is off.
    #[cfg(feature = "avif")]
    Avif,
    /// Lossy JPEG XL (`jxl-encoder`, AGPL-3.0). Only exists behind the
    /// `jxl-lossy` feature, which is OFF by default (SPEC section 3).
    #[cfg(feature = "jxl-lossy")]
    JxlLossy,
}

impl Format {
    /// Every format this build can write, in Save As dialog order (SPEC
    /// section 9: "formats whose feature is disabled do not appear").
    pub const ALL: &'static [Self] = &[
        Self::Png,
        Self::Jpeg,
        Self::WebP,
        Self::Jxl,
        #[cfg(feature = "avif")]
        Self::Avif,
        #[cfg(feature = "jxl-lossy")]
        Self::JxlLossy,
    ];

    /// The usual file extension, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::WebP => "webp",
            Self::Jxl => "jxl",
            #[cfg(feature = "avif")]
            Self::Avif => "avif",
            #[cfg(feature = "jxl-lossy")]
            Self::JxlLossy => "jxl",
        }
    }

    /// The name the Save As dialog's filter uses.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::WebP => "WebP (lossless)",
            Self::Jxl => "JPEG XL (lossless)",
            #[cfg(feature = "avif")]
            Self::Avif => "AVIF",
            #[cfg(feature = "jxl-lossy")]
            Self::JxlLossy => "JPEG XL (lossy)",
        }
    }

    /// Every extension this format is saved under, for the dialog filter.
    #[must_use]
    pub const fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Jpeg => &["jpg", "jpeg"],
            Self::Png => &["png"],
            Self::WebP => &["webp"],
            Self::Jxl => &["jxl"],
            #[cfg(feature = "avif")]
            Self::Avif => &["avif"],
            #[cfg(feature = "jxl-lossy")]
            Self::JxlLossy => &["jxl"],
        }
    }

    /// The format named by a file extension, if this build knows it.
    ///
    /// Matching is case-insensitive, because Explorer lets people type `PNG`.
    /// `.jxl` always means the lossless encoder: the lossy one is only ever
    /// chosen explicitly, so Ctrl+S on an existing `.jxl` cannot silently lose
    /// quality (SPEC section 9 calls the lossless encoder "the default JXL
    /// encoder").
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "webp" => Some(Self::WebP),
            "jxl" => Some(Self::Jxl),
            #[cfg(feature = "avif")]
            "avif" => Some(Self::Avif),
            _ => None,
        }
    }
}

/// What the lossy encoders are asked for (SPEC section 9's Options column).
///
/// The defaults are SPEC's: JPEG quality 90, AVIF quality 80 at speed 6, JPEG
/// XL lossy distance 1.0. They are settings (SPEC section 10's "default format
/// and per-format quality"), so they travel with every encode call instead of
/// living in a global.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeOptions {
    /// JPEG quality, 1–100.
    pub jpeg_quality: u8,
    /// AVIF quality, 1–100.
    pub avif_quality: u8,
    /// AVIF speed, 1–10; slower is better compression.
    pub avif_speed: u8,
    /// JPEG XL lossy distance (butteraugli), 1.0 is high quality.
    pub jxl_distance: f32,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            jpeg_quality: 90,
            avif_quality: 80,
            avif_speed: 6,
            jxl_distance: 1.0,
        }
    }
}

/// Encodes an image in the given format.
pub fn encode(
    image: &RgbaImage,
    format: Format,
    options: &EncodeOptions,
) -> crate::Result<Vec<u8>> {
    match format {
        Format::Png => encode_png(image),
        Format::Jpeg => encode_jpeg(image, options.jpeg_quality),
        Format::WebP => encode_webp(image),
        Format::Jxl => encode_jxl(image),
        #[cfg(feature = "avif")]
        Format::Avif => encode_avif(image, options),
        #[cfg(feature = "jxl-lossy")]
        Format::JxlLossy => encode_jxl_lossy(image, options.jxl_distance),
    }
}

/// Encodes an image as PNG.
pub fn encode_png(image: &RgbaImage) -> crate::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(crate::Error::Codec)?;
    Ok(bytes)
}

/// Encodes an image as JPEG.
///
/// JPEG has no alpha channel, so transparency is flattened onto **white**
/// (SPEC section 9). A screenshot with a transparent area is meant to look like
/// paper, not like whatever the decoder decides a missing alpha means.
pub fn encode_jpeg(image: &RgbaImage, quality: u8) -> crate::Result<Vec<u8>> {
    let flattened = flatten_onto_white(image);
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, quality)
        .write_image(
            flattened.as_raw(),
            flattened.width(),
            flattened.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(crate::Error::Codec)?;
    Ok(bytes)
}

/// Composites an image over white and drops the alpha channel.
#[must_use]
pub fn flatten_onto_white(image: &RgbaImage) -> RgbImage {
    let mut flattened = RgbImage::new(image.width(), image.height());
    for (source, target) in image.pixels().zip(flattened.pixels_mut()) {
        let alpha = u32::from(source.0[3]);
        let blend = |channel: u8| -> u8 {
            ((u32::from(channel) * alpha + 255 * (255 - alpha)) / 255) as u8
        };
        target.0 = [blend(source.0[0]), blend(source.0[1]), blend(source.0[2])];
    }
    flattened
}

/// Encodes an image as lossless WebP.
pub fn encode_webp(image: &RgbaImage) -> crate::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    WebPEncoder::new_lossless(&mut bytes)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(crate::Error::Codec)?;
    Ok(bytes)
}

/// Encodes an image as lossless JPEG XL.
///
/// `zune-jpegxl` refuses images whose width or height is under 2 px, so that is
/// reported as a clear error instead of a panic or a corrupt file.
pub fn encode_jxl(image: &RgbaImage) -> crate::Result<Vec<u8>> {
    use zune_core::bit_depth::BitDepth;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::EncoderOptions;
    use zune_jpegxl::JxlSimpleEncoder;

    let (width, height) = image.dimensions();
    if width < 2 || height < 2 {
        return Err(crate::Error::Jxl(format!(
            "JPEG XL needs an image of at least 2 × 2 px; this one is {width} × {height}."
        )));
    }
    let options = EncoderOptions::new(
        width as usize,
        height as usize,
        ColorSpace::RGBA,
        BitDepth::Eight,
    );
    let mut bytes = Vec::new();
    JxlSimpleEncoder::new(image.as_raw(), options)
        .encode(&mut bytes)
        .map_err(|err| crate::Error::Jxl(err.to_string()))?;
    Ok(bytes)
}

/// Encodes an image as AVIF (SPEC section 9: quality default 80, speed 6).
#[cfg(feature = "avif")]
pub fn encode_avif(image: &RgbaImage, options: &EncodeOptions) -> crate::Result<Vec<u8>> {
    use image::codecs::avif::AvifEncoder;

    let mut bytes = Vec::new();
    AvifEncoder::new_with_speed_quality(
        &mut bytes,
        options.avif_speed.clamp(1, 10),
        options.avif_quality.clamp(1, 100),
    )
    .write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgba8,
    )
    .map_err(crate::Error::Codec)?;
    Ok(bytes)
}

/// Encodes an image as lossy JPEG XL.
///
/// This is the AGPL-3.0 path, behind the `jxl-lossy` feature (SPEC section 3).
/// It must not be enabled without the human's approval.
#[cfg(feature = "jxl-lossy")]
pub fn encode_jxl_lossy(image: &RgbaImage, distance: f32) -> crate::Result<Vec<u8>> {
    use jxl_encoder::ResultAtExt as _;

    jxl_encoder::LossyConfig::new(distance)
        .encode(
            image.as_raw(),
            image.width(),
            image.height(),
            jxl_encoder::PixelLayout::Rgba8,
        )
        .map_err(|err| crate::Error::Jxl(err.decompose().0.to_string()))
}

/// Decodes a PNG.
pub fn decode_png(bytes: &[u8]) -> crate::Result<RgbaImage> {
    decode_as(bytes, ImageFormat::Png)
}

/// Decodes any image this build supports, working the format out from the
/// file's magic bytes rather than from its name.
///
/// Every format in SPEC section 9 is lossless-decodable except AVIF, so
/// opening a file never depends on its extension being right. JPEG XL is not
/// in `image`'s magic-byte table, so it is checked first.
pub fn decode(bytes: &[u8]) -> crate::Result<RgbaImage> {
    if is_jxl(bytes) {
        return decode_jxl(bytes);
    }
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(crate::Error::Io)?;
    let image = reader.decode().map_err(crate::Error::Codec)?;
    Ok(image.to_rgba8())
}

/// True when the bytes start with a JPEG XL signature.
///
/// JPEG XL has two shapes: the bare codestream (`FF 0A`) and the container
/// box (`00 00 00 0C "JXL " 0D 0A 87 0A`). Both are recognised.
#[must_use]
pub fn is_jxl(bytes: &[u8]) -> bool {
    const CODESTREAM: [u8; 2] = [0xFF, 0x0A];
    const CONTAINER: [u8; 12] = [
        0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A,
    ];
    bytes.starts_with(&CODESTREAM) || bytes.starts_with(&CONTAINER)
}

/// Decodes a JPEG XL file through `jxl-oxide`'s `image` integration.
fn decode_jxl(bytes: &[u8]) -> crate::Result<RgbaImage> {
    let decoder =
        jxl_oxide::integration::JxlDecoder::new(Cursor::new(bytes)).map_err(crate::Error::Codec)?;
    let image = DynamicImage::from_decoder(decoder).map_err(crate::Error::Codec)?;
    Ok(image.to_rgba8())
}

/// Decodes a specific format, for callers that already know it.
fn decode_as(bytes: &[u8], format: ImageFormat) -> crate::Result<RgbaImage> {
    let image = ImageReader::with_format(Cursor::new(bytes), format)
        .decode()
        .map_err(crate::Error::Codec)?;
    Ok(to_rgba(image))
}

/// Everything decodes to straight RGBA, whatever it was on disk.
fn to_rgba(image: DynamicImage) -> RgbaImage {
    image.to_rgba8()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// SPEC section 9: every lossless encoder round-trips exactly.
    fn test_image() -> RgbaImage {
        let mut image = RgbaImage::new(17, 9);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgba([
                (x * 13 % 256) as u8,
                (y * 29 % 256) as u8,
                ((x + y) * 7 % 256) as u8,
                255,
            ]);
        }
        image
    }

    /// An image with real transparency, which JPEG has to flatten.
    fn translucent_image() -> RgbaImage {
        let mut image = RgbaImage::new(8, 8);
        for (_, _, pixel) in image.enumerate_pixels_mut() {
            // Fully transparent red: over white this is white.
            *pixel = Rgba([255, 0, 0, 0]);
        }
        image
    }

    fn encoded(image: &RgbaImage, format: Format) -> Vec<u8> {
        encode(image, format, &EncodeOptions::default()).expect("encodes")
    }

    #[test]
    fn png_round_trips_exactly() {
        let original = test_image();
        let bytes = encoded(&original, Format::Png);
        let restored = decode(&bytes).expect("decodes");

        assert_eq!(restored.dimensions(), original.dimensions());
        assert_eq!(restored.as_raw(), original.as_raw());
    }

    #[test]
    fn transparency_survives_the_round_trip() {
        let mut original = RgbaImage::new(4, 1);
        original.put_pixel(0, 0, Rgba([1, 2, 3, 0]));
        original.put_pixel(1, 0, Rgba([4, 5, 6, 128]));
        original.put_pixel(2, 0, Rgba([7, 8, 9, 255]));
        original.put_pixel(3, 0, Rgba([0, 0, 0, 0]));

        let restored = decode_png(&encode_png(&original).expect("encodes")).expect("decodes");
        assert_eq!(restored.as_raw(), original.as_raw());
    }

    #[test]
    fn the_format_is_guessed_from_the_bytes_not_the_name() {
        let bytes = encoded(&test_image(), Format::Png);
        // The PNG magic number, which is what guess_format looks at.
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(decode(&bytes).expect("decodes").dimensions(), (17, 9));
    }

    #[test]
    fn rubbish_is_an_error_rather_than_a_panic() {
        let err = decode(b"this is not an image").unwrap_err();
        assert!(matches!(err, crate::Error::Codec(_)), "got {err:?}");

        // A valid PNG header with the rest chopped off.
        let truncated = &encode_png(&test_image()).expect("encodes")[..20];
        assert!(decode_png(truncated).is_err());
    }

    #[test]
    fn extensions_map_to_formats_case_insensitively() {
        assert_eq!(Format::from_extension("png"), Some(Format::Png));
        assert_eq!(Format::from_extension("PNG"), Some(Format::Png));
        assert_eq!(Format::from_extension("jpg"), Some(Format::Jpeg));
        assert_eq!(Format::from_extension("JPG"), Some(Format::Jpeg));
        assert_eq!(Format::from_extension("jpeg"), Some(Format::Jpeg));
        assert_eq!(Format::from_extension("webp"), Some(Format::WebP));
        assert_eq!(Format::from_extension("jxl"), Some(Format::Jxl));
        assert_eq!(Format::from_extension("bmp"), None, "BMP is decode-only");
        assert_eq!(Format::from_extension("gif"), None, "GIF is decode-only");
        assert_eq!(Format::Png.extension(), "png");
        assert_eq!(Format::Jpeg.extension(), "jpg");
    }

    /// SPEC section 9: "formats whose feature is disabled do not appear in the
    /// Save dialog", which is `Format::ALL`.
    #[test]
    fn the_format_list_matches_the_compiled_in_features() {
        let names: Vec<&str> = Format::ALL.iter().map(|format| format.label()).collect();
        assert!(names.contains(&"PNG"), "{names:?}");
        assert!(names.contains(&"JPEG"), "{names:?}");
        assert!(names.contains(&"WebP (lossless)"), "{names:?}");
        assert!(names.contains(&"JPEG XL (lossless)"), "{names:?}");

        #[cfg(feature = "avif")]
        assert!(names.contains(&"AVIF"), "{names:?}");
        #[cfg(not(feature = "avif"))]
        assert!(!names.contains(&"AVIF"), "{names:?}");

        // The AGPL-3.0 lossy encoder is never in the default build.
        #[cfg(not(feature = "jxl-lossy"))]
        assert!(
            !names.iter().any(|name| name.contains("lossy")),
            "the jxl-lossy feature is on: {names:?}"
        );

        // Every format has a usable extension list for the dialog filter.
        for format in Format::ALL {
            assert!(!format.extensions().is_empty(), "{format:?}");
            assert!(
                format.extensions().contains(&format.extension()),
                "{format:?} does not list its own extension"
            );
        }
    }

    #[test]
    fn jpeg_produces_a_decodable_file_with_the_right_size() {
        let original = test_image();
        let bytes = encoded(&original, Format::Jpeg);
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "not a JPEG: {:?}", &bytes[..2]);

        let restored = decode(&bytes).expect("decodes");
        assert_eq!(restored.dimensions(), original.dimensions());
        assert_eq!(restored.as_raw().len(), original.as_raw().len());
    }

    /// SPEC section 9: "JPEG: transparency flattened onto white".
    #[test]
    fn jpeg_flattens_transparency_onto_white() {
        let bytes = encoded(&translucent_image(), Format::Jpeg);
        let restored = decode(&bytes).expect("decodes");

        // JPEG is lossy, so the check is "white to within a couple of steps".
        for (x, y, pixel) in restored.enumerate_pixels() {
            for channel in 0..3 {
                assert!(
                    pixel.0[channel] >= 250,
                    "pixel {x},{y} channel {channel} is {} not white",
                    pixel.0[channel]
                );
            }
        }
    }

    /// The blend itself is exact and has its own test, because the JPEG check
    /// above can only be tolerant.
    #[test]
    fn the_white_flattening_blends_the_alpha_channel() {
        let mut image = RgbaImage::new(4, 1);
        image.put_pixel(0, 0, Rgba([255, 0, 0, 0])); // invisible: white
        image.put_pixel(1, 0, Rgba([0, 0, 0, 255])); // opaque black
        image.put_pixel(2, 0, Rgba([0, 0, 0, 128])); // half black
        image.put_pixel(3, 0, Rgba([0, 0, 255, 255])); // opaque blue

        let flattened = flatten_onto_white(&image);
        assert_eq!(flattened.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(flattened.get_pixel(1, 0).0, [0, 0, 0]);
        assert_eq!(flattened.get_pixel(3, 0).0, [0, 0, 255]);
        let grey = flattened.get_pixel(2, 0).0;
        assert!(
            grey[0].abs_diff(127) <= 1 && grey[0] == grey[1] && grey[1] == grey[2],
            "half-transparent black became {grey:?}"
        );
    }

    #[test]
    fn webp_round_trips_exactly() {
        let original = test_image();
        let bytes = encoded(&original, Format::WebP);
        assert_eq!(&bytes[..4], b"RIFF", "not a WebP container");
        assert_eq!(&bytes[8..12], b"WEBP", "not a WebP container");

        let restored = decode(&bytes).expect("decodes");
        assert_eq!(restored.dimensions(), original.dimensions());
        assert_eq!(restored.as_raw(), original.as_raw(), "WebP lost pixels");
    }

    #[test]
    fn webp_transparency_survives() {
        let original = translucent_image();
        let restored = decode(&encoded(&original, Format::WebP)).expect("decodes");
        assert_eq!(restored.as_raw(), original.as_raw());
    }

    #[test]
    fn jxl_round_trips_exactly() {
        let original = test_image();
        let bytes = encoded(&original, Format::Jxl);
        assert!(is_jxl(&bytes), "the encoder wrote no JXL signature");

        let restored = decode(&bytes).expect("decodes");
        assert_eq!(restored.dimensions(), original.dimensions());
        assert_eq!(restored.as_raw(), original.as_raw(), "JXL lost pixels");
    }

    #[test]
    fn jxl_transparency_survives() {
        let original = translucent_image();
        let restored = decode(&encoded(&original, Format::Jxl)).expect("decodes");
        assert_eq!(restored.as_raw(), original.as_raw());
    }

    #[test]
    fn a_tiny_jxl_is_a_clear_error() {
        // zune-jpegxl refuses anything under 2 px per side; the message says so
        // instead of the encoder's "ZeroDimension".
        let one_pixel = RgbaImage::new(1, 1);
        let err = encode_jxl(&one_pixel).unwrap_err();
        assert!(matches!(err, crate::Error::Jxl(_)), "got {err:?}");
        assert!(err.to_string().contains("2 × 2"), "{err}");
    }

    #[test]
    fn the_jxl_signature_is_recognised_in_both_shapes() {
        assert!(is_jxl(&[0xFF, 0x0A, 0x00]));
        assert!(is_jxl(&[
            0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A
        ]));
        assert!(!is_jxl(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_jxl(&[0xFF, 0x0B]));
        assert!(!is_jxl(&[]));
    }

    /// SPEC section 9: BMP and GIF decode (GIF: first frame).
    #[test]
    fn bmp_and_gif_decode() {
        let original = RgbaImage::new(16, 8);
        let png = encoded(&original, Format::Png);

        // BMP through the `image` crate, then back through our decoder.
        let decoded = decode(&png).expect("decodes");
        let mut bmp = Vec::new();
        image::codecs::bmp::BmpEncoder::new(&mut bmp)
            .write_image(
                decoded.as_raw(),
                decoded.width(),
                decoded.height(),
                ExtendedColorType::Rgba8,
            )
            .expect("BMP encodes");
        let restored = decode(&bmp).expect("BMP decodes");
        assert_eq!(restored.dimensions(), original.dimensions());

        // GIF is palettized, so only the size and the colour of a known pixel
        // are checked: a solid image survives any palette.
        let mut gif = Vec::new();
        let solid = RgbaImage::from_pixel(16, 8, Rgba([0, 0, 255, 255]));
        image::codecs::gif::GifEncoder::new(&mut gif)
            .encode(
                solid.as_raw(),
                solid.width(),
                solid.height(),
                ExtendedColorType::Rgba8,
            )
            .expect("GIF encodes");
        let restored = decode(&gif).expect("GIF decodes");
        assert_eq!(restored.dimensions(), (16, 8));
        assert_eq!(restored.get_pixel(3, 3).0, [0, 0, 255, 255]);
    }

    #[cfg(feature = "avif")]
    #[test]
    fn avif_produces_a_decodable_container_with_the_right_size() {
        // This build has no AVIF decoder (SPEC section 9 marks it P2), so the
        // file is checked by its container: the `ftyp` brand and the `ispe`
        // box, which carries the width and height.
        let original = RgbaImage::from_pixel(21, 13, Rgba([10, 200, 30, 255]));
        let bytes = encoded(&original, Format::Avif);

        assert!(&bytes[4..8] == b"ftyp", "not an ISO BMFF container");
        assert!(
            bytes.windows(4).any(|window| window == b"avif"),
            "no avif brand"
        );

        let ispe = bytes
            .windows(4)
            .position(|window| window == b"ispe")
            .expect("no ispe box");
        let width = u32::from_be_bytes(
            bytes[ispe + 8..ispe + 12]
                .try_into()
                .expect("ispe width bytes"),
        );
        let height = u32::from_be_bytes(
            bytes[ispe + 12..ispe + 16]
                .try_into()
                .expect("ispe height bytes"),
        );
        assert_eq!((width, height), (21, 13));
    }

    #[test]
    fn the_lossy_jpeg_quality_changes_the_file_size() {
        // The quality setting is wired through, not ignored: a low quality
        // encodes a photograph-like image to fewer bytes.
        let mut photo = RgbaImage::new(64, 64);
        for (x, y, pixel) in photo.enumerate_pixels_mut() {
            *pixel = Rgba([
                ((x * 7 + y * 3) % 256) as u8,
                ((x * 3 + y * 11) % 256) as u8,
                ((x * 13 + y * 5) % 256) as u8,
                255,
            ]);
        }
        let low = encode(
            &photo,
            Format::Jpeg,
            &EncodeOptions {
                jpeg_quality: 10,
                ..EncodeOptions::default()
            },
        )
        .expect("encodes");
        let high = encode(
            &photo,
            Format::Jpeg,
            &EncodeOptions {
                jpeg_quality: 95,
                ..EncodeOptions::default()
            },
        )
        .expect("encodes");
        assert!(
            low.len() < high.len(),
            "quality 10 gave {} bytes, quality 95 gave {}",
            low.len(),
            high.len()
        );
    }
}
