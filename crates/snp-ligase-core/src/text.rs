//! The font system wrapper: family lookup, text layout and glyph
//! rasterization (SPEC sections 4.2 and 7.5).
//!
//! Everything that draws text goes through here: the [`TextA`] annotation and
//! the Number badge. The `Fonts` value owns cosmic-text's font system and glyph
//! cache, and creating it is slow (it scans the installed fonts), so the app
//! creates exactly one on a background thread (SPEC 7.5). Tests build one from
//! the bundled font instead, so they never depend on what is installed
//! (SPEC 11.3).

use std::collections::{BTreeSet, HashSet};

use cosmic_text::fontdb;
use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent, Weight,
    Wrap,
};

use crate::model::{Rgba, TextA};
use crate::render::Canvas;

/// Line height as a multiple of the font size.
///
/// SPEC fixes the font size and says the text box is "the layout size plus
/// padding", but not what the line height is. 1.2 is the usual UI choice and
/// the one the properties panel and the renderer share, so the box the user
/// drags is the box the glyphs occupy.
pub const LINE_HEIGHT_SCALE: f32 = 1.2;

/// The family SPEC 7.5 names as the default.
pub const DEFAULT_FAMILY: &str = "Segoe UI";

/// The font system, owned once per application (SPEC 7.5).
pub struct Fonts {
    system: FontSystem,
    swash: SwashCache,
    /// Every installed family, de-duplicated and sorted, for the font combo
    /// box (SPEC 7.5).
    families: Vec<String>,
    /// The family a missing one falls back to (SPEC 4.2).
    fallback: String,
    /// Families whose "not installed" warning has already been logged, so the
    /// log gets one line per family rather than one per frame.
    warned: HashSet<String>,
}

impl std::fmt::Debug for Fonts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Fonts")
            .field("families", &self.families.len())
            .field("fallback", &self.fallback)
            .finish_non_exhaustive()
    }
}

impl Fonts {
    /// Loads the installed fonts.
    ///
    /// This takes up to a second, so SPEC 7.5 has the app call it on a
    /// background thread and disable the Text tool until it is ready.
    #[must_use]
    pub fn load() -> Self {
        let mut system = FontSystem::new();
        // Make cosmic-text's own fallback family agree with SPEC 7.5's default,
        // so a request for a family nobody installed lands on Segoe UI.
        system.db_mut().set_sans_serif_family(DEFAULT_FAMILY);
        Self::new(system, DEFAULT_FAMILY)
    }

    /// The bundled test font only: Inter Regular (OFL-1.1, in
    /// `tests/fonts/`, SPEC 11.3).
    ///
    /// The database holds nothing else, so even the platform fallback list
    /// (family names like "Segoe UI") resolves to nothing: a test that passes
    /// here passes on any machine.
    #[must_use]
    pub fn bundled() -> Self {
        let mut db = fontdb::Database::new();
        db.load_font_data(include_bytes!("../tests/fonts/Inter-Regular.ttf").to_vec());
        db.set_sans_serif_family("Inter");
        let system = FontSystem::new_with_locale_and_db("en-US".to_owned(), db);
        Self::new(system, "Inter")
    }

    /// A font system with no fonts at all.
    ///
    /// The app starts with this while the real one loads on a background
    /// thread (SPEC 7.5); nothing can be laid out with it, and
    /// [`Fonts::is_loaded`] is false until the real one replaces it.
    #[must_use]
    pub fn empty() -> Self {
        let system =
            FontSystem::new_with_locale_and_db("en-US".to_owned(), fontdb::Database::new());
        Self::new(system, DEFAULT_FAMILY)
    }

    /// True once the system fonts have been loaded (SPEC 7.5).
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        !self.families.is_empty()
    }

    fn new(system: FontSystem, fallback: &str) -> Self {
        let families: Vec<String> = system
            .db()
            .faces()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Self {
            system,
            swash: SwashCache::new(),
            families,
            fallback: fallback.to_owned(),
            warned: HashSet::new(),
        }
    }

    /// The installed families, de-duplicated and sorted (SPEC 7.5).
    #[must_use]
    pub fn families(&self) -> &[String] {
        &self.families
    }

    /// The family a missing one falls back to (SPEC 4.2 and 7.5).
    #[must_use]
    pub fn fallback_family(&self) -> &str {
        &self.fallback
    }

    /// The family to actually shape with: the one asked for when it is
    /// installed, the fallback otherwise, with one warning per family
    /// (SPEC 4.2).
    fn resolve_family(&mut self, family: &str) -> String {
        let wanted = if family.is_empty() {
            self.fallback.clone()
        } else {
            family.to_owned()
        };
        // Nothing is installed yet (the fonts are still loading): do not warn
        // about a family that cannot be found, because nothing can be found.
        if self.families.is_empty() {
            return wanted;
        }
        if self
            .families
            .iter()
            .any(|installed| installed.eq_ignore_ascii_case(&wanted))
        {
            return wanted;
        }
        if self.warned.insert(wanted.clone()) {
            tracing::warn!(
                family = %wanted,
                fallback = %self.fallback,
                "font family is not installed; using the fallback"
            );
        }
        self.fallback.clone()
    }
}

/// What a laid-out run of text measures, without any padding (SPEC 4.2).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TextLayout {
    pub width: f32,
    pub height: f32,
}

impl TextLayout {
    pub const ZERO: Self = Self {
        width: 0.0,
        height: 0.0,
    };
}

/// Lays out a [`TextA`]'s text: no wrapping, lines break only at `\n`
/// (SPEC 4.2).
#[must_use]
pub fn layout(text: &TextA, fonts: &mut Fonts) -> TextLayout {
    layout_run(
        &text.text,
        &text.font_family,
        text.size,
        text.bold,
        text.italic,
        fonts,
    )
}

/// Lays out one run of text with an explicit style, for callers that do not
/// have a `TextA` (the Number badge).
#[must_use]
pub fn layout_run(
    text: &str,
    family: &str,
    size: f32,
    bold: bool,
    italic: bool,
    fonts: &mut Fonts,
) -> TextLayout {
    if text.is_empty() || size <= 0.0 || !fonts.is_loaded() {
        // While the fonts are still loading there is nothing to measure with,
        // and cosmic-text panics rather than returning an empty layout when the
        // font database holds nothing at all (SPEC 7.5 disables the Text tool
        // for exactly this window).
        return TextLayout::ZERO;
    }
    let buffer = buffer_for(text, family, size, bold, italic, fonts);
    let mut layout = TextLayout::ZERO;
    for run in buffer.layout_runs() {
        layout.width = layout.width.max(run.line_w);
        layout.height = layout.height.max(run.line_top + run.line_height);
    }
    layout
}

/// Builds a shaped buffer for one run of text.
fn buffer_for(
    text: &str,
    family: &str,
    size: f32,
    bold: bool,
    italic: bool,
    fonts: &mut Fonts,
) -> Buffer {
    let family = fonts.resolve_family(family);
    let metrics = Metrics::new(size, size * LINE_HEIGHT_SCALE);
    let mut buffer = Buffer::new(&mut fonts.system, metrics);
    // SPEC 4.2: no wrapping; only `\n` starts a new line.
    buffer.set_wrap(Wrap::None);
    buffer.set_size(None, None);
    let attrs = Attrs::new()
        .family(Family::Name(&family))
        .weight(if bold { Weight::BOLD } else { Weight::NORMAL })
        .style(if italic { Style::Italic } else { Style::Normal });
    buffer.set_text(text, &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(&mut fonts.system, false);
    buffer
}

/// One run of text to draw: everything the rasterizer needs except where it
/// goes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Run<'a> {
    pub text: &'a str,
    pub family: &'a str,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub colour: Rgba,
}

/// Draws one run of text with its top-left at `at` in image pixels.
///
/// The caller owns the background: this only puts glyphs down, blended
/// source-over into the canvas's premultiplied pixmap.
pub(crate) fn draw_run(canvas: &mut Canvas, run: &Run<'_>, at: (f32, f32), fonts: &mut Fonts) {
    let Run {
        text,
        family,
        size,
        bold,
        italic,
        colour,
    } = *run;
    if text.is_empty() || size <= 0.0 || colour[3] == 0 || !fonts.is_loaded() {
        return;
    }
    let buffer = buffer_for(text, family, size, bold, italic, fonts);

    // The buffer's top-left in the canvas's own coordinates.
    let base_x = at.0 - canvas.origin.0 as f32;
    let base_y = at.1 - canvas.origin.1 as f32;

    // Collect the placements first, so the buffer and font-system borrows end
    // before the pixmap is written.
    let mut placed: Vec<(cosmic_text::CacheKey, i32, i32)> = Vec::new();
    for run in buffer.layout_runs() {
        for glyph in run.glyphs.iter() {
            let physical = glyph.physical((base_x, base_y + run.line_y), 1.0);
            placed.push((physical.cache_key, physical.x, physical.y));
        }
    }

    for (cache_key, x, y) in placed {
        let Some(image) = fonts.swash.get_image(&mut fonts.system, cache_key) else {
            continue;
        };
        let left = x + image.placement.left;
        let top = y - image.placement.top;
        match image.content {
            SwashContent::Mask => {
                let mut index = 0;
                for row in 0..image.placement.height as i32 {
                    for column in 0..image.placement.width as i32 {
                        let coverage = image.data[index];
                        index += 1;
                        blend_mask(canvas, left + column, top + row, colour, coverage);
                    }
                }
            }
            SwashContent::Color => {
                // Colour glyphs (emoji) carry their own premultiplied RGBA.
                let mut index = 0;
                for row in 0..image.placement.height as i32 {
                    for column in 0..image.placement.width as i32 {
                        let pixel = &image.data[index..index + 4];
                        index += 4;
                        blend_premultiplied(
                            canvas,
                            left + column,
                            top + row,
                            [pixel[0], pixel[1], pixel[2], pixel[3]],
                        );
                    }
                }
            }
            SwashContent::SubpixelMask => {
                // Subpixel rendering is off (the mask is alpha coverage), but
                // if a font ever produces one, average the three channels.
                let mut index = 0;
                for row in 0..image.placement.height as i32 {
                    for column in 0..image.placement.width as i32 {
                        let coverage = (u16::from(image.data[index])
                            + u16::from(image.data[index + 1])
                            + u16::from(image.data[index + 2]))
                            / 3;
                        index += 3;
                        blend_mask(canvas, left + column, top + row, colour, coverage as u8);
                    }
                }
            }
        }
    }
}

/// Blends one coverage-masked glyph pixel of `colour` into the canvas.
fn blend_mask(canvas: &mut Canvas, x: i32, y: i32, colour: Rgba, coverage: u8) {
    if coverage == 0 {
        return;
    }
    let alpha = u32::from(coverage) * u32::from(colour[3]) / 255;
    if alpha == 0 {
        return;
    }
    // Premultiply the colour by the effective coverage.
    let premultiplied = [
        (u32::from(colour[0]) * alpha / 255) as u8,
        (u32::from(colour[1]) * alpha / 255) as u8,
        (u32::from(colour[2]) * alpha / 255) as u8,
        alpha as u8,
    ];
    blend_premultiplied(canvas, x, y, premultiplied);
}

/// Source-over composite of one premultiplied pixel into the canvas.
fn blend_premultiplied(canvas: &mut Canvas, x: i32, y: i32, source: [u8; 4]) {
    let width = canvas.pixmap.width() as i32;
    let height = canvas.pixmap.height() as i32;
    if x < 0 || y < 0 || x >= width || y >= height || source[3] == 0 {
        return;
    }
    let at = (y as usize * width as usize + x as usize) * 4;
    let destination = &mut canvas.pixmap.data_mut()[at..at + 4];
    let alpha = u16::from(source[3]);
    if alpha == 255 {
        destination.copy_from_slice(&source);
        return;
    }
    let inverse = u32::from(255 - source[3]);
    for channel in 0..3 {
        let under = (u32::from(destination[channel]) * inverse + 127) / 255;
        destination[channel] = (u32::from(source[channel]) + under).min(255) as u8;
    }
    let under = (u32::from(destination[3]) * inverse + 127) / 255;
    destination[3] = (u32::from(source[3]) + under).min(255) as u8;
}

/// White or black, whichever contrasts more with `fill` (SPEC 4.2, relative
/// luminance with a 0.5 threshold).
#[must_use]
pub fn contrasting_colour(fill: Rgba) -> Rgba {
    // sRGB relative luminance (IEC 61966-2-1), the same weights egui uses.
    let linear = |channel: u8| {
        let value = f32::from(channel) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * linear(fill[0]) + 0.7152 * linear(fill[1]) + 0.0722 * linear(fill[2]);
    if luminance > 0.5 {
        [0, 0, 0, 255]
    } else {
        [255, 255, 255, 255]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> Fonts {
        Fonts::bundled()
    }

    #[test]
    fn the_bundled_font_is_the_only_one() {
        let fonts = fonts();
        assert_eq!(fonts.families(), ["Inter"], "the test font is not alone");
        assert_eq!(fonts.fallback_family(), "Inter");
    }

    #[test]
    fn layout_measures_a_single_line() {
        let mut fonts = fonts();
        let layout = layout_run("Hello", "Inter", 20.0, false, false, &mut fonts);
        assert!(layout.width > 20.0, "width {} is too small", layout.width);
        assert!(
            layout.height > 20.0,
            "height {} is too small",
            layout.height
        );
        assert!(
            (layout.height - 20.0 * LINE_HEIGHT_SCALE).abs() < 0.01,
            "height {} is not one line",
            layout.height
        );
    }

    #[test]
    fn layout_grows_with_more_lines_and_wider_text() {
        let mut fonts = fonts();
        let one = layout_run("One", "Inter", 20.0, false, false, &mut fonts);
        let wide = layout_run(
            "One long line of text",
            "Inter",
            20.0,
            false,
            false,
            &mut fonts,
        );
        // The same words on two lines, so only the height can change.
        let two = layout_run("One\nOne", "Inter", 20.0, false, false, &mut fonts);

        assert!(wide.width > one.width, "a longer line is not wider");
        assert!(two.height > one.height, "a second line is not taller");
        assert!(
            (two.height - 2.0 * one.height).abs() < 0.01,
            "two lines are not twice one line: {} vs {}",
            two.height,
            one.height
        );
        // The widest line decides the width, not the sum.
        assert!(
            (two.width - one.width).abs() < 0.01,
            "the box is not the widest line"
        );
    }

    #[test]
    fn empty_text_measures_nothing() {
        let mut fonts = fonts();
        assert_eq!(
            layout_run("", "Inter", 20.0, false, false, &mut fonts),
            TextLayout::ZERO
        );
    }

    #[test]
    fn empty_fonts_are_not_loaded_and_warn_about_nothing() {
        let mut fonts = Fonts::empty();
        assert!(!fonts.is_loaded());
        // A family asked for before the fonts arrive must not warn: nothing is
        // installed yet, so "not installed" would be noise.
        let layout = layout_run("Hello", "Segoe UI", 20.0, false, false, &mut fonts);
        assert_eq!(layout, TextLayout::ZERO, "an empty font system drew text");
        assert!(fonts.warned.is_empty(), "it warned about a missing family");
    }

    #[test]
    fn a_missing_family_falls_back_and_warns_once() {
        let mut fonts = fonts();
        let layout = layout_run("Hello", "No Such Font", 20.0, false, false, &mut fonts);
        assert!(layout.width > 0.0, "the fallback did not lay anything out");
        // The warning is per family, so the second call does not add another.
        let again = layout_run("Hello", "No Such Font", 20.0, false, false, &mut fonts);
        assert!(again.width > 0.0);
        assert!(fonts.warned.contains("No Such Font"));
    }

    #[test]
    fn contrast_picks_the_readable_colour() {
        assert_eq!(contrasting_colour([255, 255, 255, 255]), [0, 0, 0, 255]);
        assert_eq!(contrasting_colour([0, 0, 0, 255]), [255, 255, 255, 255]);
        // SPEC 7.7's number red is dark enough for white text.
        assert_eq!(contrasting_colour([232, 17, 35, 255]), [255, 255, 255, 255]);
        // Yellow is bright: black text.
        assert_eq!(contrasting_colour([255, 255, 0, 255]), [0, 0, 0, 255]);
    }
}
