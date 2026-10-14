//! The HDR (scRGB) to 8-bit sRGB conversion, SPEC section 5.5.
//!
//! On an HDR monitor Windows composes the desktop in scRGB: linear light,
//! BT.709 primaries, where `1.0` means 80 nits. Ordinary SDR content is placed
//! at the monitor's "SDR white level" brightness, so SDR white appears as
//! `sdr_white_nits / 80` — 2.5 on a 200 nit monitor. Capturing that naively
//! either clips it to white or washes it out.
//!
//! The fix is a divide, a highlight policy, a clamp and an sRGB encode:
//!
//! ```text
//! scale = sdr_white_nits / 80.0
//! v = c / scale                      // SDR white becomes 1.0
//! v = highlight(v)                   // Clip or RollOff, per pixel
//! v = clamp(v, 0.0, 1.0)
//! s = if v <= 0.0031308 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
//! out = round(s * 255.0)
//! ```
//!
//! Everything here is pure maths, so it is fully unit tested: a screenshot that
//! comes out washed out or dark is the one bug that would make this whole
//! project pointless.
//!
//! R26: the sRGB encode's `powf` is the expensive part, and it is a pure
//! function of (f16 input, frame's SDR white level). [`SdrEncoder`] tabulates
//! all 65536 f16 inputs once per capture, and the table is checked against the
//! direct path for every one of them.

use half::f16;

/// What to do with values above the SDR white level (SPEC section 5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightMode {
    /// Everything above SDR white clips to white. SDR content comes out exactly
    /// as it would on an SDR monitor, which is what a screenshot of a UI needs.
    Clip,
    /// Compress the highlights instead, per pixel, so that detail above SDR
    /// white survives as near-white instead of flat white (SPEC 5.5's formula).
    RollOff,
}

/// Used when the OS cannot tell us the monitor's SDR white level
/// (SPEC section 5.5): 200 nits is the Windows default.
pub const DEFAULT_SDR_WHITE_NITS: f32 = 200.0;

/// The brightness, in nits, of one unit of linear scRGB.
const NITS_AT_UNITY: f32 = 80.0;

/// Where SPEC 5.5's roll-off starts compressing, in SDR-white units.
pub const ROLL_OFF_KNEE: f32 = 0.8;

/// How many f16 bit patterns there are; one table entry each.
const F16_INPUTS: usize = 1 << 16;

/// The linear scRGB level that Windows treats as one unit of SDR white.
///
/// Dividing an FP16 channel by this value puts SDR white at 1.0.
#[must_use]
pub fn sdr_white_scale(sdr_white_nits: f32) -> f32 {
    // A zero or negative reading is nonsense; fall back rather than divide by it.
    let nits = if sdr_white_nits > 0.0 {
        sdr_white_nits
    } else {
        DEFAULT_SDR_WHITE_NITS
    };
    nits / NITS_AT_UNITY
}

/// The linear-light value of an 8-bit sRGB sample.
///
/// Used when building test data: to pretend an SDR colour was captured on an
/// HDR monitor, multiply the result of this by [`sdr_white_scale`].
#[must_use]
pub fn linear_from_srgb8(value: u8) -> f32 {
    let v = f32::from(value) / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Encodes one linear-light value as an 8-bit sRGB sample.
#[must_use]
pub fn encode_srgb8(linear: f32) -> u8 {
    let v = linear.clamp(0.0, 1.0);
    let srgb = if v <= 0.0031308 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (srgb * 255.0).round().clamp(0.0, 255.0) as u8
}

/// The roll-off curve of SPEC 5.5, applied to one pixel's three channels.
///
/// `m` is the brightest channel in SDR-white units. Below the knee nothing
/// changes; above it the whole pixel is scaled by `m' / m`, where
/// `m' = 0.8 + 0.2 * tanh((m - 0.8) / 0.2)`, so hue and saturation are kept
/// and the result approaches — but never passes — 1.0.
#[must_use]
fn roll_off(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let m = r.max(g).max(b);
    if m <= ROLL_OFF_KNEE {
        return (r, g, b);
    }
    let headroom = 1.0 - ROLL_OFF_KNEE;
    let rolled = ROLL_OFF_KNEE + headroom * ((m - ROLL_OFF_KNEE) / headroom).tanh();
    let factor = rolled / m;
    (r * factor, g * factor, b * factor)
}

/// Converts one pixel of linear scRGB to 8-bit sRGB through the full pipeline.
///
/// The three channels are converted together because the highlight policy is
/// defined per pixel, in terms of the brightest channel.
#[must_use]
pub fn convert_pixel(r: f32, g: f32, b: f32, sdr_white_nits: f32, mode: HighlightMode) -> [u8; 3] {
    let scale = sdr_white_scale(sdr_white_nits);
    let (r, g, b) = (r / scale, g / scale, b / scale);
    let (r, g, b) = match mode {
        HighlightMode::Clip => (r, g, b),
        HighlightMode::RollOff => roll_off(r, g, b),
    };
    [encode_srgb8(r), encode_srgb8(g), encode_srgb8(b)]
}

/// The per-capture conversion table from f16 bits to 8-bit sRGB (R26).
///
/// One `powf` per channel per pixel is 25 million of them on a 4K monitor; the
/// result only depends on the input bits and the frame's SDR white level, so it
/// is tabulated once per capture (65536 entries, 64 KB) and every pixel becomes
/// three byte lookups. The highlight policy is **not** in the table: `Clip` is
/// the table, `RollOff` needs the pixel's other channels and takes the direct
/// path.
#[derive(Debug, Clone)]
pub struct SdrEncoder {
    /// One byte per f16 bit pattern, `value / scale` clamped and encoded.
    table: Box<[u8]>,
    /// The scale the table was built for, for the direct path.
    scale: f32,
}

impl SdrEncoder {
    /// Builds the table for a monitor's SDR white level.
    #[must_use]
    pub fn new(sdr_white_nits: f32) -> Self {
        let scale = sdr_white_scale(sdr_white_nits);
        let mut table = vec![0u8; F16_INPUTS].into_boxed_slice();
        for (bits, entry) in table.iter_mut().enumerate() {
            let value = f16::from_bits(bits as u16).to_f32();
            *entry = encode_srgb8(value / scale);
        }
        Self { table, scale }
    }

    /// The 8-bit sRGB value for one f16 sample, by lookup.
    #[must_use]
    pub fn encode_f16(&self, bits: u16) -> u8 {
        self.table[usize::from(bits)]
    }

    /// The direct (non-tabulated) conversion, for the roll-off path and for the
    /// test that checks every table entry against it.
    #[must_use]
    pub fn encode_linear(&self, value: f32) -> u8 {
        encode_srgb8(value / self.scale)
    }
}

/// Converts a whole FP16 RGBA frame to tightly packed straight 8-bit sRGB RGBA.
///
/// `src` holds `width * height` pixels of 16-bit float channels with `row_pitch`
/// bytes between rows, because capture APIs pad rows. The result has no padding
/// and the alpha channel is forced to 255 (SPEC section 5.4).
///
/// # Errors
///
/// [`crate::Error::FrameTooSmall`] when `src` cannot hold the promised pixels.
pub fn rgba16f_to_rgba8(
    src: &[u8],
    width: u32,
    height: u32,
    row_pitch: usize,
    sdr_white_nits: f32,
    mode: HighlightMode,
) -> crate::Result<Vec<u8>> {
    const BYTES_PER_PIXEL: usize = 8; // four f16 channels
    let row = width as usize * BYTES_PER_PIXEL;
    let need = (height as usize)
        .checked_sub(1)
        .map_or(0, |rows| rows * row_pitch)
        + row;
    if src.len() < need {
        return Err(crate::Error::FrameTooSmall {
            width,
            height,
            pitch: row_pitch,
            need,
            have: src.len(),
        });
    }

    let encoder = SdrEncoder::new(sdr_white_nits);
    let mut out = vec![0u8; width as usize * height as usize * 4];
    let dst_row_bytes = width as usize * 4;
    for y in 0..height as usize {
        let src_row = &src[y * row_pitch..y * row_pitch + row];
        let dst_row = &mut out[y * dst_row_bytes..y * dst_row_bytes + dst_row_bytes];
        for x in 0..width as usize {
            let px = &src_row[x * BYTES_PER_PIXEL..x * BYTES_PER_PIXEL + BYTES_PER_PIXEL];
            let half_at = |i: usize| f16::from_le_bytes([px[i], px[i + 1]]).to_f32();
            let [r, g, b] = match mode {
                HighlightMode::Clip => [
                    encoder.encode_f16(u16::from_le_bytes([px[0], px[1]])),
                    encoder.encode_f16(u16::from_le_bytes([px[2], px[3]])),
                    encoder.encode_f16(u16::from_le_bytes([px[4], px[5]])),
                ],
                HighlightMode::RollOff => {
                    convert_pixel(half_at(0), half_at(2), half_at(4), sdr_white_nits, mode)
                }
            };
            let dst = &mut dst_row[x * 4..x * 4 + 4];
            dst[0] = r;
            dst[1] = g;
            dst[2] = b;
            dst[3] = 255; // alpha is meaningless in a desktop capture
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scRGB levels an SDR colour would have on a monitor whose SDR white
    /// level is `nits`.
    fn sc_rgb(srgb: u8, nits: f32) -> (f32, f32, f32) {
        let v = linear_from_srgb8(srgb);
        let scale = sdr_white_scale(nits);
        (v * scale, v * scale, v * scale)
    }

    #[test]
    fn sdr_white_maps_to_255() {
        // SDR white in scRGB is exactly `scale` by definition.
        let nits = 200.0;
        let (r, g, b) = sc_rgb(255, nits);
        assert_eq!(convert_pixel(r, g, b, nits, HighlightMode::Clip), [255; 3]);
    }

    #[test]
    fn black_maps_to_zero() {
        assert_eq!(
            convert_pixel(0.0, 0.0, 0.0, 200.0, HighlightMode::Clip),
            [0; 3]
        );
    }

    #[test]
    fn mid_grey_round_trips_within_one() {
        // SPEC 5.5: sRGB 128 encoded to scRGB at 200 nits must come back within ±1.
        let nits = 200.0;
        let (r, g, b) = sc_rgb(128, nits);
        let out = convert_pixel(r, g, b, nits, HighlightMode::Clip);
        for channel in out {
            assert!(
                (i32::from(channel) - 128).abs() <= 1,
                "channel {channel} is not within ±1 of 128"
            );
        }
    }

    #[test]
    fn values_above_white_clip_to_255() {
        assert_eq!(
            convert_pixel(4.0, 4.0, 4.0, 200.0, HighlightMode::Clip),
            [255; 3]
        );
    }

    #[test]
    fn negative_values_clamp_to_zero() {
        assert_eq!(
            convert_pixel(-1.0, -0.25, 0.0, 200.0, HighlightMode::Clip),
            [0; 3]
        );
    }

    #[test]
    fn a_darker_sdr_white_level_lifts_sdr_content() {
        // The same desktop content on a 400 nit monitor must not come out
        // darker: scRGB scales with the white level, and so does the divide.
        let (r, g, b) = sc_rgb(200, 400.0);
        let out = convert_pixel(r, g, b, 400.0, HighlightMode::Clip);
        assert_eq!(out, [200, 200, 200]);
    }

    // ---------------------------------------------------------------------
    // M7: the roll-off policy (SPEC 5.5) and the lookup table (R26)
    // ---------------------------------------------------------------------

    #[test]
    fn roll_off_leaves_content_at_or_below_the_knee_alone() {
        // SPEC 5.5 compresses only above 0.8 of SDR white.
        let nits = 200.0;
        let scale = sdr_white_scale(nits);
        for (linear, expected) in [(0.0f32, 0u8), (0.25, 137), (0.8, 231)] {
            let out = convert_pixel(
                linear * scale,
                linear * scale,
                linear * scale,
                nits,
                HighlightMode::RollOff,
            );
            let clip = convert_pixel(
                linear * scale,
                linear * scale,
                linear * scale,
                nits,
                HighlightMode::Clip,
            );
            assert_eq!(out, clip, "roll-off changed a value at {linear}");
            let _ = expected;
        }
        // And the knee itself is untouched.
        let out = convert_pixel(
            0.8 * scale,
            0.8 * scale,
            0.8 * scale,
            nits,
            HighlightMode::RollOff,
        );
        let clip = convert_pixel(
            0.8 * scale,
            0.8 * scale,
            0.8 * scale,
            nits,
            HighlightMode::Clip,
        );
        assert_eq!(out, clip);
    }

    #[test]
    fn roll_off_compresses_sdr_white_instead_of_clipping_it() {
        // SDR white (1.0) must come out near white, not flat white: the whole
        // point of the policy is keeping detail above the knee.
        let nits = 200.0;
        let scale = sdr_white_scale(nits);
        let out = convert_pixel(scale, scale, scale, nits, HighlightMode::RollOff);
        assert!(
            out[0] < 255 && out[0] > 240,
            "SDR white rolled off to {out:?}, which is not 'near white'"
        );
        // 1.0 of SDR white is `m = 1.0`, so m' = 0.8 + 0.2*tanh(1) ≈ 0.9523.
        let expected = encode_srgb8(0.8 + 0.2 * 1.0f32.tanh());
        assert_eq!(out, [expected; 3]);
    }

    #[test]
    fn roll_off_keeps_the_hue() {
        // The scale factor is the same for all three channels, so the ratios
        // between them survive (that is why SPEC applies it to the max).
        let nits = 200.0;
        let scale = sdr_white_scale(nits);
        let (r, g, b) = (1.2 * scale, 0.6 * scale, 0.3 * scale);
        let out = convert_pixel(r, g, b, nits, HighlightMode::RollOff);
        let clip = convert_pixel(r, g, b, nits, HighlightMode::Clip);
        // Every channel is darker than it would have clipped to...
        for channel in 0..3 {
            assert!(
                out[channel] < clip[channel],
                "channel {channel} was not compressed: {out:?} vs {clip:?}"
            );
        }
        // ...and the order (and so the hue) is unchanged.
        assert!(
            out[0] > out[1] && out[1] > out[2],
            "the channel order flipped: {out:?}"
        );
    }

    #[test]
    fn roll_off_is_monotonic_and_never_passes_white() {
        let nits = 200.0;
        let scale = sdr_white_scale(nits);
        let mut previous = 0u8;
        for step in 0..=400 {
            let m = step as f32 / 40.0; // 0.0 .. 10.0
            let out = convert_pixel(
                m * scale,
                m * scale,
                m * scale,
                nits,
                HighlightMode::RollOff,
            );
            assert!(
                out[0] >= previous,
                "roll-off went backwards at m = {m}: {previous} then {}",
                out[0]
            );
            previous = out[0];
        }
        // 10× SDR white is 255 by then; the curve's asymptote is 1.0.
        assert_eq!(previous, 255);
    }

    /// R26: the table must agree with the direct path for **every** f16 input.
    #[test]
    fn the_lookup_table_matches_the_direct_path_for_every_input() {
        for nits in [80.0, 200.0, 240.0, 300.0, 1000.0] {
            let encoder = SdrEncoder::new(nits);
            for bits in 0..=u16::MAX {
                let value = f16::from_bits(bits).to_f32();
                assert_eq!(
                    encoder.encode_f16(bits),
                    encoder.encode_linear(value),
                    "table and direct path disagree for 0x{bits:04X} ({value}) at {nits} nits"
                );
            }
        }
    }

    /// The table is the one the frame converter uses, so a whole frame must
    /// come out exactly as the per-pixel path would produce it.
    #[test]
    fn the_frame_converter_uses_the_table() {
        let width = 8u32;
        let height = 4u32;
        let row_pitch = width as usize * 8;
        // One value per pixel; the same value in all three channels.
        let inputs: Vec<f32> = (0..(width * height) as usize)
            .map(|index| match index % 8 {
                0 => 0.0,
                1 => 0.5,
                2 => 1.0,
                3 => 3.0,
                4 => -1.0,
                5 => f32::NAN,
                6 => 100.0,
                _ => index as f32 / 512.0,
            })
            .collect();

        let mut src = vec![0u8; row_pitch * height as usize];
        for (index, value) in inputs.iter().enumerate() {
            let offset = (index / width as usize) * row_pitch + (index % width as usize) * 8;
            for channel in 0..3 {
                let bytes = f16::from_f32(*value).to_le_bytes();
                src[offset + channel * 2] = bytes[0];
                src[offset + channel * 2 + 1] = bytes[1];
            }
        }

        let out = rgba16f_to_rgba8(&src, width, height, row_pitch, 200.0, HighlightMode::Clip)
            .expect("converts");
        let scale = sdr_white_scale(200.0);
        for (index, value) in inputs.iter().enumerate() {
            let expected = encode_srgb8(f16::from_f32(*value).to_f32() / scale);
            assert_eq!(
                out[index * 4],
                expected,
                "pixel {index} (input {value}) came out {} not {expected}",
                out[index * 4]
            );
        }
    }

    #[test]
    fn the_roll_off_frame_path_agrees_with_the_per_pixel_path() {
        let width = 4u32;
        let height = 2u32;
        let row_pitch = width as usize * 8;
        let scale = sdr_white_scale(200.0);
        let pixels: [[f32; 3]; 8] = [
            [0.0, 0.0, 0.0],
            [scale, scale, scale],
            [3.0 * scale, 1.0 * scale, 0.5 * scale],
            [0.1 * scale, 0.9 * scale, 0.4 * scale],
            [10.0 * scale, 10.0 * scale, 10.0 * scale],
            [-1.0, 0.5, 2.0],
            [0.7 * scale, 0.7 * scale, 0.7 * scale],
            [f32::NAN, 0.0, 0.0],
        ];
        let mut src = vec![0u8; row_pitch * height as usize];
        for (index, pixel) in pixels.iter().enumerate() {
            let offset = (index / 4) * row_pitch + (index % 4) * 8;
            for (channel, value) in pixel.iter().enumerate() {
                let bytes = f16::from_f32(*value).to_le_bytes();
                src[offset + channel * 2] = bytes[0];
                src[offset + channel * 2 + 1] = bytes[1];
            }
        }

        let out = rgba16f_to_rgba8(
            &src,
            width,
            height,
            row_pitch,
            200.0,
            HighlightMode::RollOff,
        )
        .expect("converts");
        for (index, pixel) in pixels.iter().enumerate() {
            let expected =
                convert_pixel(pixel[0], pixel[1], pixel[2], 200.0, HighlightMode::RollOff);
            assert_eq!(
                &out[index * 4..index * 4 + 3],
                &expected,
                "pixel {index} differs"
            );
        }
    }

    #[test]
    fn frame_conversion_respects_row_pitch() {
        // A 2x2 image whose rows are padded to 16 bytes, as capture APIs do.
        let width = 2u32;
        let height = 2u32;
        let row_pitch = 16usize;
        let mut src = vec![0u8; row_pitch * height as usize];
        let put = |buf: &mut [u8], index: usize, r: f32, g: f32, b: f32, a: f32| {
            let mut write = |offset: usize, value: f32| {
                let bytes = f16::from_f32(value).to_le_bytes();
                buf[index + offset] = bytes[0];
                buf[index + offset + 1] = bytes[1];
            };
            write(0, r);
            write(2, g);
            write(4, b);
            write(6, a);
        };

        let scale = sdr_white_scale(200.0);
        // White and black on the top row, mid grey and white on the bottom.
        put(&mut src, 0, scale, scale, scale, 0.0);
        put(&mut src, 8, 0.0, 0.0, 0.0, 0.0);
        put(
            &mut src,
            row_pitch,
            sc_rgb(128, 200.0).0,
            sc_rgb(128, 200.0).1,
            scale,
            0.0,
        );
        put(&mut src, row_pitch + 8, scale, scale, scale, 0.0);

        let out =
            rgba16f_to_rgba8(&src, width, height, row_pitch, 200.0, HighlightMode::Clip).unwrap();

        assert_eq!(out.len(), 16, "output must be tightly packed");
        assert_eq!(&out[0..4], &[255, 255, 255, 255]);
        assert_eq!(&out[4..8], &[0, 0, 0, 255]);
        let mid = i32::from(out[8]);
        assert!((mid - 128).abs() <= 1, "mid grey came out as {mid}");
        assert_eq!(&out[12..16], &[255, 255, 255, 255]);
    }

    #[test]
    fn truncated_frames_are_reported_not_panicked_on() {
        let src = vec![0u8; 8]; // not even one pixel of a 4x4 frame
        let err = rgba16f_to_rgba8(&src, 4, 4, 32, 200.0, HighlightMode::Clip).unwrap_err();
        match err {
            crate::Error::FrameTooSmall { width, height, .. } => {
                assert_eq!(width, 4);
                assert_eq!(height, 4);
            }
            other => panic!("expected FrameTooSmall, got {other:?}"),
        }
    }

    #[test]
    fn an_unusual_white_level_still_lands_on_the_right_numbers() {
        // 203 nits: an odd but real reading from QueryDisplayConfig.
        let (r, g, b) = sc_rgb(96, 203.0);
        let out = convert_pixel(r, g, b, 203.0, HighlightMode::Clip);
        assert_eq!(out, [96, 96, 96]);
    }
}
