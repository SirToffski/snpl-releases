//! The root window's pixels, in the form the rest of the program wants them.
//!
//! `GetImage` and its MIT-SHM twin hand over the server's own pixel format:
//! the byte order of the machine, channel widths and positions that the visual
//! decides, and rows padded to the pixmap format's scanline pad. None of that
//! may leak into the capture: everything above this module gets 8-bit straight
//! sRGB with alpha 255, exactly like the Windows capture does (SPEC section
//! 5.4). This is the one place that knows otherwise, and it is pure, so every
//! layout is a test on a byte slice (LINUX_SUPPORT.md section 4).

use image::RgbaImage;
use x11rb::protocol::xproto::{ImageOrder, VisualClass};

/// The root window's pixel format, as the X server describes it: the visual's
/// masks and class, the pixmap format's bits per pixel and row padding, and
/// the server's byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelLayout {
    /// The root window's depth, e.g. 24.
    pub(crate) depth: u8,
    /// Bits per pixel in the image data, e.g. 32.
    pub(crate) bits_per_pixel: u8,
    /// Every row is padded to a multiple of this many bits.
    pub(crate) scanline_pad: u8,
    pub(crate) byte_order: ImageOrder,
    pub(crate) class: VisualClass,
    pub(crate) red_mask: u32,
    pub(crate) green_mask: u32,
    pub(crate) blue_mask: u32,
}

impl PixelLayout {
    /// Bytes one pixel takes in the server's image data (4 for the usual
    /// 24-bit TrueColor root window).
    pub(crate) fn bytes_per_pixel(&self) -> usize {
        usize::from(self.bits_per_pixel).div_ceil(8)
    }

    /// Bytes from the start of one row to the next: the pixels plus whatever
    /// the scanline pad adds (SPEC section 5.4's `row_pitch`).
    pub(crate) fn pitch(&self, width: u32) -> usize {
        let bits = u64::from(width) * u64::from(self.bits_per_pixel);
        let pad = u64::from(self.scanline_pad.max(8));
        let row_bits = (bits.div_ceil(pad)) * pad;
        usize::try_from(row_bits.div_ceil(8)).unwrap_or(usize::MAX)
    }

    /// The layout's name for `capture.json` and the log, in the same style as
    /// the Windows formats (`Bgra8`): `Bgrx8` for the usual 32-bit 888 root
    /// window, `Bgr8` for a 24-bit one, and the masks in words otherwise.
    #[must_use]
    pub(crate) fn name(&self) -> String {
        let bgr =
            self.red_mask == 0xff_0000 && self.green_mask == 0xff_00 && self.blue_mask == 0xff;
        let lsb = self.byte_order != ImageOrder::MSB_FIRST;
        if self.class == VisualClass::TRUE_COLOR && bgr && lsb {
            match self.bits_per_pixel {
                32 => return "Bgrx8".to_owned(),
                24 => return "Bgr8".to_owned(),
                _ => {}
            }
        }
        format!(
            "{}bpp depth {} masks {:06x}/{:06x}/{:06x}",
            self.bits_per_pixel, self.depth, self.red_mask, self.green_mask, self.blue_mask
        )
    }

    /// One pixel's raw value: the bytes as one integer in the server's byte
    /// order.
    fn value(&self, bytes: &[u8]) -> u32 {
        let mut value = 0u32;
        if self.byte_order == ImageOrder::MSB_FIRST {
            for byte in bytes {
                value = (value << 8) | u32::from(*byte);
            }
        } else {
            for (index, byte) in bytes.iter().enumerate() {
                value |= u32::from(*byte) << (8 * index);
            }
        }
        value
    }
}

/// The server's image data as an 8-bit sRGB image, plus the highest channel
/// value seen (`FrameDiagnostics::max_channel_value`: an all-black or washed-out
/// capture is what that number is read for).
///
/// A buffer shorter than the image leaves the missing pixels transparent
/// instead of panicking, the way the Windows capture degrades (SPEC 5.4).
#[must_use]
pub(crate) fn to_rgba(
    data: &[u8],
    layout: &PixelLayout,
    width: u32,
    height: u32,
) -> (RgbaImage, f32) {
    let bytes = layout.bytes_per_pixel();
    let pitch = layout.pitch(width);
    let true_color = layout.class == VisualClass::TRUE_COLOR;

    let mut image = RgbaImage::new(width, height);
    let mut peak = 0u8;

    for row in 0..height {
        let row_start = usize::try_from(u64::from(row) * pitch as u64).unwrap_or(usize::MAX);
        for column in 0..width {
            let start = row_start.saturating_add(usize::try_from(column).unwrap_or(0) * bytes);
            let Some(pixel) = data.get(start..start + bytes) else {
                continue; // shorter than promised: stay transparent
            };
            let value = layout.value(pixel);
            let (red, green, blue) = if true_color {
                (
                    channel(value, layout.red_mask),
                    channel(value, layout.green_mask),
                    channel(value, layout.blue_mask),
                )
            } else {
                // No colormap is read: a PseudoColor/GrayScale screen is
                // reported as the greys its indices are, which the caller
                // warns about.
                let grey = pixel[0];
                (grey, grey, grey)
            };
            peak = peak.max(red).max(green).max(blue);

            let index = (usize::try_from(row).unwrap_or(0) * width as usize
                + usize::try_from(column).unwrap_or(0))
                * 4;
            if let Some(target) = image.as_mut().get_mut(index..index + 4) {
                target.copy_from_slice(&[red, green, blue, 255]);
            }
        }
    }

    (image, f32::from(peak) / 255.0)
}

/// One channel of one pixel as 8 bits: the mask says where it sits and how
/// wide it is, so a 5-6-5 or 10-10-10 screen converts like an 8-bit one.
#[must_use]
fn channel(value: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let raw = (value & mask) >> shift;
    // Rounded to nearest, so a 5-bit 1 is 8 and not 16.
    u8::try_from((raw * 255 + max / 2) / max).unwrap_or(255)
}

/// `Xft.dpi` as a scale factor, 1.0 when the session does not name one or
/// names something unusable (LINUX_SUPPORT.md section 4: for logs only, X11
/// has one global scale and physical pixels are the only real space).
#[must_use]
pub(crate) fn scale_from_dpi(dpi: Option<f32>) -> f32 {
    dpi.filter(|dpi| *dpi > 0.0)
        .filter(|dpi| dpi.is_finite())
        .map(|dpi| dpi / 96.0)
        .unwrap_or(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The usual Xorg root window: depth 24 in 32-bit pixels, masks 888.
    fn bgrx_layout() -> PixelLayout {
        PixelLayout {
            depth: 24,
            bits_per_pixel: 32,
            scanline_pad: 32,
            byte_order: ImageOrder::LSB_FIRST,
            class: VisualClass::TRUE_COLOR,
            red_mask: 0x00ff_0000,
            green_mask: 0x0000_ff00,
            blue_mask: 0x0000_00ff,
        }
    }

    fn pixel(image: &RgbaImage, x: u32, y: u32) -> [u8; 4] {
        image.get_pixel(x, y).0
    }

    #[test]
    fn a_32_bit_bgrx_pixel_becomes_rgba_in_the_right_order() {
        let layout = bgrx_layout();
        // One pixel: B 0x10, G 0x20, R 0x30, pad 0x00 (little endian).
        let data = [0x10, 0x20, 0x30, 0x00];
        let (image, peak) = to_rgba(&data, &layout, 1, 1);

        assert_eq!(image.width(), 1);
        assert_eq!(image.height(), 1);
        assert_eq!(pixel(&image, 0, 0), [0x30, 0x20, 0x10, 255]);
        assert_eq!(peak, 0x30 as f32 / 255.0);
    }

    #[test]
    fn every_row_starts_at_the_pitched_offset() {
        // Two 24-bit pixels are 48 bits, padded to the 32-bit scanline pad:
        // 8 bytes per row, so the second row starts at byte 8, not byte 6.
        let mut layout = bgrx_layout();
        layout.depth = 24;
        layout.bits_per_pixel = 24;
        assert_eq!(layout.pitch(2), 8, "the row pitch");

        let data = [
            0xff, 0x00, 0x00, // pixel 0: B
            0x00, 0xff, 0x00, // pixel 1: G
            0x00, 0x00, // pad
            0x00, 0x00, 0xff, // row 1, pixel 0: R
            0xff, 0x00, 0x00, // row 1, pixel 1: B
            0x00, 0x00, // pad
        ];
        let (image, _) = to_rgba(&data, &layout, 2, 2);

        assert_eq!(pixel(&image, 0, 0), [0x00, 0x00, 0xff, 255]);
        assert_eq!(pixel(&image, 1, 0), [0x00, 0xff, 0x00, 255]);
        assert_eq!(pixel(&image, 0, 1), [0xff, 0x00, 0x00, 255]);
        assert_eq!(pixel(&image, 1, 1), [0x00, 0x00, 0xff, 255]);
    }

    #[test]
    fn a_16_bit_five_six_five_screen_scales_its_channels() {
        let layout = PixelLayout {
            depth: 16,
            bits_per_pixel: 16,
            scanline_pad: 16,
            byte_order: ImageOrder::LSB_FIRST,
            class: VisualClass::TRUE_COLOR,
            red_mask: 0xf800,
            green_mask: 0x07e0,
            blue_mask: 0x001f,
        };

        let cases: [(u16, [u8; 4]); 5] = [
            (0xf800, [255, 0, 0, 255]),
            (0x07e0, [0, 255, 0, 255]),
            (0x001f, [0, 0, 255, 255]),
            (0xffff, [255, 255, 255, 255]),
            // 1 of 31, rounded: 255/31 = 8.2 -> 8, not 16.
            (0x0001, [0, 0, 8, 255]),
        ];
        for (value, expected) in cases {
            let data = value.to_le_bytes();
            let (image, _) = to_rgba(&data, &layout, 1, 1);
            assert_eq!(pixel(&image, 0, 0), expected, "{value:#06x}");
        }
        assert_eq!(layout.pitch(1), 2);
    }

    #[test]
    fn a_ten_bit_channel_scales_to_eight_bits() {
        // XRGB2101010, the 30-bit deep-colour layout.
        let layout = PixelLayout {
            depth: 30,
            bits_per_pixel: 32,
            scanline_pad: 32,
            byte_order: ImageOrder::MSB_FIRST,
            class: VisualClass::TRUE_COLOR,
            red_mask: 0x3ff0_0000,
            green_mask: 0x000f_fc00,
            blue_mask: 0x0000_03ff,
        };

        let white = 0x3fff_ffffu32.to_be_bytes();
        let (image, peak) = to_rgba(&white, &layout, 1, 1);
        assert_eq!(pixel(&image, 0, 0), [255, 255, 255, 255]);
        assert_eq!(peak, 1.0);

        let red = 0x3ff0_0000u32.to_be_bytes();
        let (image, _) = to_rgba(&red, &layout, 1, 1);
        assert_eq!(pixel(&image, 0, 0), [255, 0, 0, 255]);
    }

    #[test]
    fn a_visual_without_masks_is_read_as_greys() {
        // A PseudoColor screen (an 8-bit VNC or Xnest session): the indices are
        // not colours, but the grey levels are what the bytes are.
        let layout = PixelLayout {
            depth: 8,
            bits_per_pixel: 8,
            scanline_pad: 8,
            byte_order: ImageOrder::LSB_FIRST,
            class: VisualClass::PSEUDO_COLOR,
            red_mask: 0,
            green_mask: 0,
            blue_mask: 0,
        };
        let data = [0x40, 0x80];
        let (image, peak) = to_rgba(&data, &layout, 2, 1);

        assert_eq!(pixel(&image, 0, 0), [0x40, 0x40, 0x40, 255]);
        assert_eq!(pixel(&image, 1, 0), [0x80, 0x80, 0x80, 255]);
        assert_eq!(peak, 0x80 as f32 / 255.0);
    }

    #[test]
    fn a_short_buffer_leaves_the_missing_pixels_transparent() {
        // What a mismatched or truncated reply produces (SPEC 5.4): the frame
        // is used as-is, never a panic.
        let layout = bgrx_layout();
        let data = [0x10, 0x20, 0x30, 0x00]; // one pixel of a 2x2 image
        let (image, _) = to_rgba(&data, &layout, 2, 2);

        assert_eq!(pixel(&image, 0, 0), [0x30, 0x20, 0x10, 255]);
        assert_eq!(pixel(&image, 1, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(&image, 1, 1), [0, 0, 0, 0]);
    }

    #[test]
    fn the_layout_names_what_capture_json_reports() {
        assert_eq!(bgrx_layout().name(), "Bgrx8");

        let mut packed = bgrx_layout();
        packed.bits_per_pixel = 24;
        assert_eq!(packed.name(), "Bgr8");

        // Anything else says what it is in words, masks included.
        let mut deep = bgrx_layout();
        deep.bits_per_pixel = 16;
        deep.red_mask = 0xf800;
        deep.green_mask = 0x07e0;
        deep.blue_mask = 0x001f;
        let name = deep.name();
        assert!(name.contains("16bpp"), "{name}");
        assert!(name.contains("f800"), "{name}");
    }

    #[test]
    fn the_scale_factor_comes_from_xft_dpi() {
        assert_eq!(scale_from_dpi(Some(96.0)), 1.0);
        assert_eq!(scale_from_dpi(Some(144.0)), 1.5);
        assert_eq!(scale_from_dpi(Some(120.0)), 1.25);
        assert_eq!(scale_from_dpi(None), 1.0);
        // A nonsense value is not a scale factor.
        assert_eq!(scale_from_dpi(Some(0.0)), 1.0);
        assert_eq!(scale_from_dpi(Some(-96.0)), 1.0);
        assert_eq!(scale_from_dpi(Some(f32::NAN)), 1.0);
    }
}
