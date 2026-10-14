//! The three pixel effects: box blur, shadow and highlight multiply
//! (SPEC section 4.2).
//!
//! All three work on the **base image only** and produce opaque pixels. That
//! is what keeps compositing to a single source-over pass everywhere: an effect
//! never half-covers what is under it, and never hides another annotation
//! (SPEC section 4.2).

use image::RgbaImage;

use crate::geom::{Rect, RectI};

/// How far the shadow is offset down-right, in pixels (SPEC section 4.2).
pub const SHADOW_OFFSET: (i32, i32) = (3, 3);
/// The radius of each of the shadow's two box-blur passes.
pub const SHADOW_BLUR_RADIUS: u32 = 3;
/// How many times the shadow's alpha is box-blurred.
pub const SHADOW_BLUR_PASSES: usize = 2;
/// Black at 45% opacity.
pub const SHADOW_OPACITY: f32 = 0.45;
/// How far a shadow reaches past the shape it belongs to: the offset plus what
/// each blur pass grows it by. Sprite bounds have to allow for this.
pub const SHADOW_GROWTH: f32 =
    (SHADOW_OFFSET.1 + SHADOW_BLUR_PASSES as i32 * SHADOW_BLUR_RADIUS as i32) as f32;
/// How many box-blur passes a Blur annotation runs (SPEC section 4.2).
pub const BLUR_PASSES: usize = 3;
/// Blur reads from this much extra room around the rectangle, so the pixels
/// just inside the edge are blurred by their real neighbours (SPEC 4.2).
pub const BLUR_MARGIN_RATIO: f32 = 3.0;

/// Multiplies straight RGBA by its alpha.
pub(crate) fn premultiply(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        for channel in pixel[..3].iter_mut() {
            *channel = (u16::from(*channel) * alpha / 255) as u8;
        }
    }
}

/// Divides out the alpha. Fully transparent pixels become fully transparent
/// black rather than dividing by zero.
pub(crate) fn unpremultiply(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
        } else if alpha != 255 {
            for channel in pixel[..3].iter_mut() {
                let value = u32::from(*channel);
                *channel = ((value * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
}

/// `passes` box blurs over a pixel buffer, `radius` pixels each, running a
/// horizontal and a vertical pass per blur.
///
/// Edges clamp, so the result grows by `radius` on every side: that is what
/// makes the outside of a blurred rectangle soft rather than a hard line.
#[must_use]
pub fn box_blur(src: &[u8], width: u32, height: u32, radius: u32, passes: usize) -> Vec<u8> {
    box_blur_channels(src, width, height, radius, passes, 4)
}

/// The same, for a one-byte-per-pixel buffer such as the shadow mask.
#[must_use]
pub fn box_blur_gray(src: &[u8], width: u32, height: u32, radius: u32, passes: usize) -> Vec<u8> {
    box_blur_channels(src, width, height, radius, passes, 1)
}

/// The box blur, for any channel count.
fn box_blur_channels(
    src: &[u8],
    width: u32,
    height: u32,
    radius: u32,
    passes: usize,
    channels: usize,
) -> Vec<u8> {
    if radius == 0 || passes == 0 || width == 0 || height == 0 {
        return src.to_vec();
    }
    // A short buffer would make the slice indexing below panic, so refuse it.
    if src.len() < width as usize * height as usize * channels {
        return src.to_vec();
    }

    let stride = width as usize * channels;
    let mut current = src.to_vec();
    let mut scratch = vec![0u8; current.len()];

    for _ in 0..passes {
        blur_horizontal(
            &current,
            &mut scratch,
            width,
            height,
            stride,
            radius,
            channels,
        );
        blur_vertical(
            &scratch,
            &mut current,
            width,
            height,
            stride,
            radius,
            channels,
        );
    }

    current
}

/// One horizontal box-blur pass, in and out of separate buffers.
#[allow(clippy::too_many_arguments)]
fn blur_horizontal(
    src: &[u8],
    dst: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    radius: u32,
    channels: usize,
) {
    let window = 2 * radius + 1;
    for y in 0..height as usize {
        let row = y * stride;
        // The first window is [0 - radius, 0 + radius], with indices before the
        // row clamped onto its first pixel: that is what grows the blur out.
        let mut sums = [0u32; 4];
        for offset in -(radius as i64)..=(radius as i64) {
            let at = row + clamp_index(offset, width as i64) as usize * channels;
            add_pixel(&mut sums, src, at, channels);
        }

        for x in 0..width as usize {
            write_pixel(dst, row + x * channels, sums, window, channels);
            // Slide the window: the pixel entering on the right joins, the one
            // leaving on the left drops out.
            let entering =
                row + clamp_index(x as i64 + radius as i64 + 1, width as i64) as usize * channels;
            let leaving =
                row + clamp_index(x as i64 - radius as i64, width as i64) as usize * channels;
            add_pixel(&mut sums, src, entering, channels);
            sub_pixel(&mut sums, src, leaving, channels);
        }
    }
}

/// One vertical box-blur pass, in and out of separate buffers.
#[allow(clippy::too_many_arguments)]
fn blur_vertical(
    src: &[u8],
    dst: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    radius: u32,
    channels: usize,
) {
    let window = 2 * radius + 1;
    for x in 0..width as usize {
        let column = x * channels;
        let mut sums = [0u32; 4];
        for offset in -(radius as i64)..=(radius as i64) {
            let at = clamp_index(offset, height as i64) as usize * stride + column;
            add_pixel(&mut sums, src, at, channels);
        }

        for y in 0..height as usize {
            write_pixel(dst, y * stride + column, sums, window, channels);
            let entering =
                clamp_index(y as i64 + radius as i64 + 1, height as i64) as usize * stride + column;
            let leaving =
                clamp_index(y as i64 - radius as i64, height as i64) as usize * stride + column;
            add_pixel(&mut sums, src, entering, channels);
            sub_pixel(&mut sums, src, leaving, channels);
        }
    }
}
/// Clamps a coordinate into the image, which is what makes a blur grow
/// outwards at the edges.
fn clamp_index(value: i64, length: i64) -> i64 {
    value.clamp(0, length.saturating_sub(1))
}

/// Adds one pixel to the running sums.
fn add_pixel(sums: &mut [u32; 4], src: &[u8], at: usize, channels: usize) {
    let pixel = &src[at..at + channels];
    for (sum, channel) in sums.iter_mut().zip(pixel.iter()) {
        *sum += u32::from(*channel);
    }
}

/// Takes one pixel off the running sums.
fn sub_pixel(sums: &mut [u32; 4], src: &[u8], at: usize, channels: usize) {
    let pixel = &src[at..at + channels];
    for (sum, channel) in sums.iter_mut().zip(pixel.iter()) {
        *sum = sum.saturating_sub(u32::from(*channel));
    }
}

/// Writes the averaged pixel out, rounding halves up.
fn write_pixel(dst: &mut [u8], at: usize, sums: [u32; 4], window: u32, channels: usize) {
    for (offset, sum) in sums.iter().take(channels).enumerate() {
        dst[at + offset] = ((*sum + window / 2) / window).min(255) as u8;
    }
}

/// The sprite for a Blur annotation, and the pixels it actually covers.
///
/// Three box blurs of `radius` (SPEC section 4.2). Source pixels are read from
/// `3 * radius` beyond each edge so the inside of the edge is blurred by its
/// real neighbours, then the result is cropped back to the rectangle.
///
/// The rectangle is clamped to the base image, so a rectangle that starts
/// outside it is clipped rather than shifted. The returned rectangle is the
/// clipped one, which is where the sprite must be placed: the caller must not
/// round it again, or the blurred pixels land one row or column away from the
/// pixels they came from.
///
/// An empty result is a 0x0 image and a zero rectangle.
#[must_use]
pub fn blur_rect(base: &RgbaImage, rect: Rect, radius: f32) -> (RgbaImage, RectI) {
    let target = rounded_clip(rect, base.width(), base.height());
    if target.is_empty() {
        return (RgbaImage::new(0, 0), RectI::default());
    }

    let radius = radius.max(0.0).round() as u32;
    let margin = (radius as f32 * BLUR_MARGIN_RATIO).ceil() as i32;
    let (target_width, target_height) = (target.width() as i32, target.height() as i32);
    let source = clip(
        target.x - margin,
        target.y - margin,
        target_width + margin * 2,
        target_height + margin * 2,
        base.width() as i32,
        base.height() as i32,
    );
    if source.is_empty() {
        return (RgbaImage::new(0, 0), RectI::default());
    }

    // Copy the source region out, premultiply so that blurring does not drag
    // colour out of transparent pixels.
    let (source_width, source_height) = (source.width() as usize, source.height() as usize);
    let source_stride = source_width * 4;
    let mut pixels = vec![0u8; source_stride * source_height];
    for row in 0..source_height {
        let from = ((source.y + row as i32) * base.width() as i32 + source.x) as usize * 4;
        let to = row * source_stride;
        pixels[to..to + source_stride].copy_from_slice(&base.as_raw()[from..from + source_stride]);
    }
    premultiply(&mut pixels);

    let mut blurred = box_blur(
        &pixels,
        source.width(),
        source.height(),
        radius,
        BLUR_PASSES,
    );
    unpremultiply(&mut blurred);

    // Crop back to the annotation's rectangle.
    let (target_width, target_height) = (target.width() as usize, target.height() as usize);
    let inset_x = (target.x - source.x) as usize * 4;
    let inset_y = (target.y - source.y) as usize;
    let stride = target_width * 4;
    let mut out = vec![0u8; stride * target_height];
    for row in 0..target_height {
        let from = (inset_y + row) * source_stride + inset_x;
        out[row * stride..row * stride + stride].copy_from_slice(&blurred[from..from + stride]);
    }

    // Effects are always opaque (SPEC section 4.2).
    for pixel in out.as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }

    RgbaImage::from_raw(target.width(), target.height(), out)
        .map(|image| (image, target))
        .unwrap_or_else(|| {
            (
                RgbaImage::new(0, 0),
                RectI {
                    x: 0,
                    y: 0,
                    right: 0,
                    bottom: 0,
                },
            )
        })
}

/// The sprite for a Highlight annotation, and the pixels it actually covers:
/// `base * colour / 255` per channel.
///
/// A multiply blend, so dark text under the highlight stays dark, the way a
/// real highlighter behaves (SPEC section 4.2). As with [`blur_rect`], the
/// returned rectangle is the clipped one the sprite must be placed at.
#[must_use]
pub fn highlight_rect(base: &RgbaImage, rect: Rect, color: [u8; 4]) -> (RgbaImage, RectI) {
    let target = rounded_clip(rect, base.width(), base.height());
    if target.is_empty() {
        return (RgbaImage::new(0, 0), RectI::default());
    }

    let (width, height) = (target.width() as usize, target.height() as usize);
    let mut out = vec![0u8; width * height * 4];
    for row in 0..height {
        for column in 0..width {
            let from = ((target.y + row as i32) * base.width() as i32 + target.x + column as i32)
                as usize
                * 4;
            let to = (row * width + column) * 4;
            let source = &base.as_raw()[from..from + 3];
            for channel in 0..3usize {
                out[to + channel] =
                    (u32::from(source[channel]) * u32::from(color[channel]) / 255) as u8;
            }
            out[to + 3] = 255;
        }
    }

    RgbaImage::from_raw(target.width(), target.height(), out)
        .map(|image| (image, target))
        .unwrap_or_else(|| {
            (
                RgbaImage::new(0, 0),
                RectI {
                    x: 0,
                    y: 0,
                    right: 0,
                    bottom: 0,
                },
            )
        })
}

/// The shadow to place under a sprite, given the sprite's alpha channel
/// (SPEC section 4.2).
///
/// The alpha is offset by `(3, 3)`, box-blurred twice with radius 3, and
/// scaled to 45% opacity. The result is the same size as the sprite, so the
/// sprite's bounds must already include [`SHADOW_GROWTH`] of room around the
/// shape.
#[must_use]
pub fn shadow_mask(alpha: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut mask = vec![0u8; alpha.len()];
    if width == 0 || height == 0 {
        return mask;
    }

    let (dx, dy) = SHADOW_OFFSET;
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let source_x = x - dx;
            let source_y = y - dy;
            if source_x < 0 || source_y < 0 || source_x >= width as i32 || source_y >= height as i32
            {
                continue;
            }
            let from = (source_y * width as i32 + source_x) as usize;
            mask[(y * width as i32 + x) as usize] = alpha[from];
        }
    }

    // One channel is enough: the shadow is black, only its opacity varies.
    let mut blurred = box_blur_gray(&mask, width, height, SHADOW_BLUR_RADIUS, SHADOW_BLUR_PASSES);
    for value in &mut blurred {
        *value = (f32::from(*value) * SHADOW_OPACITY).round().min(255.0) as u8;
    }
    blurred
}

/// Rounds a floating-point rectangle to whole pixels and clips it to the image.
fn rounded_clip(rect: Rect, width: u32, height: u32) -> RectI {
    let rounded = rect.round_out();
    clip(
        rounded.x,
        rounded.y,
        rounded.right - rounded.x,
        rounded.bottom - rounded.y,
        width as i32,
        height as i32,
    )
}

/// Clips an integer rectangle to the image, keeping its origin.
fn clip(x: i32, y: i32, w: i32, h: i32, width: i32, height: i32) -> RectI {
    let left = x.clamp(0, width.max(0));
    let top = y.clamp(0, height.max(0));
    let right = (x + w).clamp(left, width.max(0));
    let bottom = (y + h).clamp(top, height.max(0));
    RectI {
        x: left,
        y: top,
        right,
        bottom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// A `size`-sized image with a hard vertical edge down the middle: the left
    /// half black, the right half white. Blurring it must produce a ramp.
    fn two_tone(size: u32) -> RgbaImage {
        let mut image = RgbaImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                let value = if x < size / 2 { 0u8 } else { 255u8 };
                image.put_pixel(x, y, Rgba([value, value, value, 255]));
            }
        }
        image
    }

    #[test]
    fn a_blur_ramps_the_edge_of_a_two_tone_image() {
        // SPEC 11.1: a blurred region has intermediate values at the boundary.
        let base = two_tone(64);
        let (blurred, covered) = blur_rect(&base, Rect::new(0.0, 0.0, 64.0, 64.0), 6.0);

        assert_eq!((blurred.width(), blurred.height()), (64, 64));
        assert_eq!(covered.width(), 64);
        assert_eq!(covered.height(), 64);

        let row = blurred.height() / 2;
        // Far from the edge the pixels keep their value...
        assert_eq!(blurred.get_pixel(2, row)[0], 0);
        assert_eq!(blurred.get_pixel(61, row)[0], 255);
        // ...and just either side of the middle they are in between.
        let left = blurred.get_pixel(29, row)[0];
        let right = blurred.get_pixel(34, row)[0];
        assert!(left > 0 && left < 255, "left of the edge was {left}");
        assert!(right > 0 && right < 255, "right of the edge was {right}");
        assert!(left < right, "the ramp has to get lighter, not darker");
    }

    #[test]
    fn blur_does_nothing_outside_its_rectangle() {
        // SPEC 11.1: the area outside the rect is unchanged.
        let base = two_tone(64);
        let (only_left, covered) = blur_rect(&base, Rect::new(0.0, 0.0, 32.0, 64.0), 8.0);

        assert_eq!((only_left.width(), only_left.height()), (32, 64));
        assert_eq!((covered.x, covered.y), (0, 0));
        // The rectangle's own pixels are still opaque, even though the region
        // it covers was uniform.
        assert_eq!(only_left.get_pixel(4, 32)[3], 255);
        assert_eq!(only_left.get_pixel(4, 32)[0], 0);
    }

    #[test]
    fn a_zero_radius_blur_returns_the_pixels_unchanged() {
        let base = two_tone(32);
        let (unchanged, covered) = blur_rect(&base, Rect::new(0.0, 0.0, 32.0, 32.0), 0.0);
        assert_eq!(unchanged.as_raw(), base.as_raw());
        assert_eq!((covered.x, covered.y), (0, 0));
    }

    #[test]
    fn a_rectangle_outside_the_image_blurs_to_nothing() {
        let base = two_tone(32);
        let (outside, covered) = blur_rect(&base, Rect::new(100.0, 100.0, 10.0, 10.0), 6.0);
        assert_eq!((outside.width(), outside.height()), (0, 0));
        assert!(
            covered.is_empty(),
            "a zero rectangle, not a stale one: {covered:?}"
        );

        // Half outside: the pixels come from x = 0, so that is where the sprite
        // must be placed. Rounding the rectangle again would shift them.
        let (straddling, at) = blur_rect(&base, Rect::new(-8.0, -8.0, 16.0, 16.0), 6.0);
        assert_eq!((straddling.width(), straddling.height()), (8, 8));
        assert_eq!((at.x, at.y), (0, 0));
    }

    #[test]
    fn the_covered_rectangle_tells_the_caller_where_the_pixels_are() {
        // Rounding a rectangle *out* means "cover every pixel it touches", so
        // 10.6..15.6 becomes the six pixels 10..16. What the caller must not do
        // is round it again: the sprite is placed exactly where the pixels are.
        let base = two_tone(64);
        let (blurred, at) = blur_rect(&base, Rect::new(10.6, 4.2, 20.0, 20.0), 6.0);
        assert_eq!((blurred.width(), blurred.height()), (21, 21));
        assert_eq!(at.x, 10, "floor of 10.6");
        assert_eq!(at.y, 4, "floor of 4.2");
        assert_eq!((at.right, at.bottom), (31, 25));
    }

    #[test]
    fn highlight_multipplies_like_a_real_highlighter() {
        // SPEC 11.1: over white it gives the highlight colour, over black black.
        let mut base = RgbaImage::new(4, 1);
        base.put_pixel(0, 0, Rgba([255, 255, 255, 255]));
        base.put_pixel(1, 0, Rgba([0, 0, 0, 255]));
        base.put_pixel(2, 0, Rgba([128, 64, 32, 255]));
        base.put_pixel(3, 0, Rgba([10, 20, 30, 255]));

        let yellow = [255u8, 255, 0, 255];
        let (marked, at) = highlight_rect(&base, Rect::new(0.0, 0.0, 4.0, 1.0), yellow);
        assert_eq!((at.x, at.y), (0, 0));

        assert_eq!(marked.get_pixel(0, 0), &Rgba([255, 255, 0, 255]), "white");
        assert_eq!(marked.get_pixel(1, 0), &Rgba([0, 0, 0, 255]), "black");
        assert_eq!(
            marked.get_pixel(2, 0),
            &Rgba([128, 64, 0, 255]),
            "128,64,32"
        );
        assert_eq!(marked.get_pixel(3, 0), &Rgba([10, 20, 0, 255]), "10,20,30");
    }

    #[test]
    fn highlight_is_always_opaque() {
        let base = RgbaImage::from_pixel(4, 4, Rgba([1, 2, 3, 0]));
        let (marked, _) = highlight_rect(&base, Rect::new(0.0, 0.0, 4.0, 4.0), [255, 0, 0, 255]);
        for pixel in marked.pixels() {
            assert_eq!(pixel.0[3], 255);
        }
    }

    #[test]
    fn a_highlight_reports_the_rectangle_it_really_covers() {
        let mut base = RgbaImage::new(16, 16);
        for (x, y, pixel) in base.enumerate_pixels_mut() {
            *pixel = Rgba([x as u8, y as u8, 9, 255]);
        }
        // Fractional and negative origins: the pixels are taken from whole
        // pixels, and the sprite says which ones.
        let (_, at) = highlight_rect(&base, Rect::new(10.6, 3.0, 5.0, 5.0), [255, 255, 0, 255]);
        assert_eq!(at.x, 10, "floor of 10.6, not round");
        assert_eq!((at.right, at.bottom), (16, 8));

        // A rectangle that starts off the image is clipped, not shifted.
        let (_, at) = highlight_rect(&base, Rect::new(-8.0, 0.0, 16.0, 4.0), [255, 255, 0, 255]);
        assert_eq!((at.x, at.y), (0, 0));
    }

    #[test]
    fn the_shadow_is_offset_down_and_right() {
        // A single opaque pixel, blurred into a small blob.
        let mut alpha = vec![0u8; 32 * 32];
        alpha[(10 * 32 + 10) as usize] = 255;
        let mask = shadow_mask(&alpha, 32, 32);

        let mut peak = (0usize, 0usize, 0u8);
        for y in 0..32usize {
            for x in 0..32usize {
                let value = mask[y * 32 + x];
                if value > peak.2 {
                    peak = (x, y, value);
                }
            }
        }

        // Offset by 3, then blurred with radius 3 twice, so the peak sits
        // further out than the original pixel but never up or left of it.
        assert!(peak.0 > 10, "shadow x was {}", peak.0);
        assert!(peak.1 > 10, "shadow y was {}", peak.1);
        assert!(peak.2 < 255, "the shadow must not be solid black");
        assert!(
            mask[(10 * 32 + 10) as usize] < peak.2,
            "the source pixel is not the peak"
        );
    }

    #[test]
    fn an_empty_sprite_has_an_empty_shadow() {
        let mask = shadow_mask(&[], 0, 0);
        assert!(mask.is_empty());
    }

    #[test]
    fn a_box_blur_of_a_uniform_region_changes_nothing() {
        let pixels = vec![40u8; 16 * 16 * 4];
        let blurred = box_blur(&pixels, 16, 16, 3, 3);
        assert_eq!(blurred, pixels);
    }

    #[test]
    fn premultiply_and_unpremultiply_round_trip_opaque_pixels() {
        let mut pixels = vec![10u8, 20, 30, 255, 1, 2, 3, 128];
        let original = pixels.clone();
        premultiply(&mut pixels);
        assert_eq!(pixels[4], 0, "a half-transparent red is nearly nothing");
        unpremultiply(&mut pixels);
        // Fully opaque pixels survive exactly; translucent ones cannot, which
        // is why effects work premultiplied and only un-premultiply once.
        assert_eq!(&pixels[..4], &original[..4]);
        assert_eq!(pixels[7], 128);
    }

    #[test]
    fn unpremultiplying_transparent_pixels_does_not_divide_by_zero() {
        let mut pixels = vec![255u8, 255, 255, 0];
        unpremultiply(&mut pixels);
        assert_eq!(pixels, vec![0, 0, 0, 0]);
    }
}
