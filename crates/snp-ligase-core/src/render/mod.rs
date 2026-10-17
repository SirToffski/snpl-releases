//! Rasterizing annotations into sprites, and compositing them into a flat
//! image (SPEC sections 4.1 and 4.2).
//!
//! There is exactly one render path in this program (SPEC section 2): the
//! canvas shows the same sprites the export writes, so what the user sees is
//! what gets saved. egui only draws the UI chrome around them.
//!
//! Each sub-module owns one annotation kind, and every one of them answers the
//! same two questions: what rectangle does this need (`shape_bounds`), and how
//! do I draw it into that rectangle (`draw`). [`bounds`] and [`rasterize`]
//! combine those answers with the shadow and the two pixels of anti-aliasing
//! slack, so they can never disagree about how big a sprite is.

pub mod effect;
pub mod freehand;
pub mod image;
pub mod number;
pub mod path;
pub mod polygon;
pub mod rect;
pub mod text;

use ::image::RgbaImage;
use std::sync::Arc;
use tiny_skia::{Color, Paint, Pixmap, Transform};

use crate::effects::{SHADOW_GROWTH, premultiply, unpremultiply};
use crate::geom::{Rect, RectI};
use crate::model::{Annotation, Document, Kind, Rgba};
use crate::text::Fonts;

/// Extra room around a shape for anti-aliasing (SPEC section 4.2).
const ANTI_ALIAS_SLACK: f32 = 2.0;

/// The largest expanded export [`flatten_with`] will build, in pixels.
///
/// A safety net, not a feature: an annotation dragged absurdly far outside the
/// image would otherwise ask for an allocation that kills the process. At
/// 16384 × 16384 the buffer is already a gigabyte, which is a failed save
/// either way; the cut-off fallback at least fails loudly in the log instead of
/// aborting.
const MAX_EXPORT_PIXELS: u64 = 16_384 * 16_384;

/// A rasterized annotation: a small premultiplied bitmap and where it sits in
/// the image (SPEC section 4.1).
#[derive(Debug, Clone)]
pub struct Sprite {
    pub pixmap: Pixmap,
    /// Top-left corner in image pixels.
    pub origin: (i32, i32),
}

impl Sprite {
    /// Where this sprite sits in the image, as a rectangle.
    #[must_use]
    pub fn rect(&self) -> Rect {
        Rect::new(
            self.origin.0 as f32,
            self.origin.1 as f32,
            self.pixmap.width() as f32,
            self.pixmap.height() as f32,
        )
    }
}

/// The bounding box of a sprite's **painted pixels** (alpha > 0), in image
/// coordinates, or `None` when it painted nothing (NEXT_STEPS step 11, J2).
///
/// The export follows what was really drawn rather than `bounds()`'s
/// conservative rectangle: `bounds()` carries two pixels of anti-aliasing
/// slack, so an arrow that ends one pixel inside the edge used to save a thin
/// strip of fill colour although nothing was drawn there.
#[must_use]
pub fn painted_bounds(sprite: &Sprite) -> Option<RectI> {
    let width = sprite.pixmap.width();
    let height = sprite.pixmap.height();
    let data = sprite.pixmap.data();
    let (mut left, mut top) = (width, height);
    let (mut right, mut bottom) = (0u32, 0u32);
    let mut painted = false;
    for row in 0..height {
        for column in 0..width {
            let at = (row as usize * width as usize + column as usize) * 4;
            if data[at + 3] == 0 {
                continue;
            }
            painted = true;
            left = left.min(column);
            top = top.min(row);
            right = right.max(column + 1);
            bottom = bottom.max(row + 1);
        }
    }
    painted.then(|| RectI {
        x: sprite.origin.0 + left as i32,
        y: sprite.origin.1 + top as i32,
        right: sprite.origin.0 + right as i32,
        bottom: sprite.origin.1 + bottom as i32,
    })
}

/// A rasterized layer of the document, plus where its top-left pixel sits in
/// image coordinates (NEXT_STEPS step 12).
///
/// Blur and Highlight draw pixels that come from a layer instead of from
/// vector drawing, so they need to know both the pixels and how the layer sits
/// in image coordinates: with "annotations outside the image" set to Expand the
/// layer covers the base image **and** the pasted images beside it, and it does
/// not have to start at (0, 0).
#[derive(Clone)]
pub struct Layer {
    pixels: Arc<RgbaImage>,
    origin: (i32, i32),
    /// The image rectangles that really have pixels: the base image and the
    /// painted pixels of every pasted image. `None` means the whole rectangle,
    /// which is what a base-sized layer has; with images the layer's bounding
    /// rectangle can hold gaps (the space between the base and a pasted image)
    /// and an effect that drew there would paint the fill colour of the added
    /// area (J1).
    covered: Option<Vec<RectI>>,
}

impl Layer {
    /// A layer that covers exactly one image, at (0, 0): the base image on its
    /// own, which is what an effect reads in Cut mode.
    #[must_use]
    pub fn whole(pixels: Arc<RgbaImage>) -> Self {
        Self {
            pixels,
            origin: (0, 0),
            covered: None,
        }
    }

    fn new(pixels: RgbaImage, origin: (i32, i32), covered: Option<Vec<RectI>>) -> Self {
        Self {
            pixels: Arc::new(pixels),
            origin,
            covered,
        }
    }

    /// The layer's pixels.
    #[must_use]
    pub fn pixels(&self) -> &RgbaImage {
        &self.pixels
    }

    /// Where the layer's top-left pixel is in image coordinates.
    #[must_use]
    pub fn origin(&self) -> (i32, i32) {
        self.origin
    }

    /// The layer's rectangle in image coordinates.
    #[must_use]
    pub fn rect(&self) -> RectI {
        RectI {
            x: self.origin.0,
            y: self.origin.1,
            right: self.origin.0 + self.pixels.width() as i32,
            bottom: self.origin.1 + self.pixels.height() as i32,
        }
    }

    /// True when both layers are the same buffer at the same place. The sprite
    /// cache uses this for its staleness test: two equal images still rasterize
    /// the same, but only the same buffer can be assumed to.
    #[must_use]
    pub fn shares_pixels(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.pixels, &other.pixels) && self.origin == other.origin
    }

    /// True when the layer is exactly this buffer at (0, 0): the comparison the
    /// sprite cache uses for sprites that are not effects.
    #[must_use]
    pub fn is_buffer(&self, pixels: &Arc<RgbaImage>) -> bool {
        self.origin == (0, 0) && Arc::ptr_eq(&self.pixels, pixels)
    }

    fn source(&self) -> Source<'_> {
        Source {
            pixels: &self.pixels,
            origin: self.origin,
            covered: self.covered.as_deref(),
        }
    }
}

/// What an effect reads: the layer's pixels, where they sit, and which image
/// rectangles really have pixels.
struct Source<'a> {
    pixels: &'a RgbaImage,
    origin: (i32, i32),
    covered: Option<&'a [RectI]>,
}

/// True when `outer` covers every pixel of `inner`.
fn rect_contains(outer: RectI, inner: RectI) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.right <= outer.right
        && inner.bottom <= outer.bottom
}

/// True when the image pixel is inside `rect`.
fn rect_has_pixel(rect: RectI, x: i32, y: i32) -> bool {
    x >= rect.x && x < rect.right && y >= rect.y && y < rect.bottom
}

/// Rasterizes one annotation, shadow included.
///
/// `None` means there is nothing to draw: an annotation with no stroke and no
/// fill, or an empty path.
///
/// An effect reads `base` at its own (0, 0); [`rasterize_layer`] is the variant
/// for a layer with an origin (NEXT_STEPS step 12).
pub fn rasterize(a: &Annotation, base: &RgbaImage, fonts: &mut Fonts) -> Option<Sprite> {
    rasterize_source(
        a,
        &Source {
            pixels: base,
            origin: (0, 0),
            covered: None,
        },
        fonts,
    )
}

/// [`rasterize`] for an effect that reads a [`Layer`]: the annotation's
/// rectangle is mapped onto the layer's coordinates, and the result is placed
/// back in image coordinates.
pub fn rasterize_layer(a: &Annotation, layer: &Layer, fonts: &mut Fonts) -> Option<Sprite> {
    rasterize_source(a, &layer.source(), fonts)
}

fn rasterize_source(a: &Annotation, source: &Source<'_>, fonts: &mut Fonts) -> Option<Sprite> {
    let origin = source.origin;
    // Effects do not use the shape canvas at all: their pixels come from the
    // layer, and their rectangle is exactly the rectangle they were given.
    match &a.kind {
        Kind::Highlight(highlight) => {
            let local = highlight
                .rect
                .translated(-(origin.0 as f32), -(origin.1 as f32));
            let (image, covered) =
                crate::effects::highlight_rect(source.pixels, local, highlight.color);
            return effect_sprite(image, covered.translated(origin.0, origin.1), source);
        }
        Kind::Blur(blur) => {
            let local = blur.rect.translated(-(origin.0 as f32), -(origin.1 as f32));
            let (image, covered) = crate::effects::blur_rect(source.pixels, local, blur.radius);
            return effect_sprite(image, covered.translated(origin.0, origin.1), source);
        }
        _ => {}
    }

    let (mut pixmap, origin) = shape(a, fonts)?;
    // Blur and Highlight are opaque and cover their own rectangle, so a shadow
    // under them would never be seen (SPEC section 4.2).
    if a.shadow {
        pixmap = add_shadow(&pixmap)?;
    }
    Some(Sprite { pixmap, origin })
}

/// The sprite rectangle of an annotation: the shape, plus the room its shadow
/// and anti-aliasing need (SPEC section 4.1).
///
/// This is exactly the rectangle [`rasterize`] allocates, which is why selection
/// outlines line up with what is drawn.
#[must_use]
pub fn bounds(a: &Annotation, fonts: &mut Fonts) -> Rect {
    let shape = shape_bounds(a, fonts);
    if a.kind.is_effect() {
        // An effect covers its rectangle and nothing more: no shadow room, no
        // anti-aliasing slack.
        return shape;
    }
    // A text box's outline is centred on the box edge, so half of it lies
    // outside `shape_bounds` — which stays the box itself, because hit testing
    // covers the box (NEXT_STEPS step 8). Everything else already includes its
    // stroke in `shape_bounds`.
    let shape = match &a.kind {
        Kind::Text(text) => {
            let half = text
                .outline
                .map_or(0.0, |stroke| stroke.width.max(0.0) / 2.0);
            shape.inflated(half)
        }
        _ => shape,
    };
    let with_shadow = if a.shadow {
        shape.inflated(SHADOW_GROWTH)
    } else {
        shape
    };
    with_shadow.inflated(ANTI_ALIAS_SLACK)
}

/// The order annotations are composited in, on screen and in `flatten` alike
/// (SPEC section 4.2, extended by NEXT_STEPS step 11):
///
/// 1. the base image,
/// 2. **Image** annotations, in list order — the pasted images are part of the
///    "base layer" that Blur and Highlight read,
/// 3. every Blur, computed from the base plus the images,
/// 4. every Highlight, computed from the base plus the images plus the blurs,
/// 5. everything else, in list order.
///
/// Both the editor and the export go through this one function, which is what
/// makes "what the user sees is exactly what is saved" true by construction
/// rather than by two matching lists.
#[must_use]
pub fn compositing_order(doc: &Document) -> Vec<u64> {
    doc.annotations
        .iter()
        .filter(|a| matches!(a.kind, Kind::Image(_)))
        .chain(
            doc.annotations
                .iter()
                .filter(|a| matches!(a.kind, Kind::Blur(_))),
        )
        .chain(
            doc.annotations
                .iter()
                .filter(|a| matches!(a.kind, Kind::Highlight(_))),
        )
        .chain(
            doc.annotations
                .iter()
                .filter(|a| !a.kind.is_effect() && !matches!(a.kind, Kind::Image(_))),
        )
        .map(|a| a.id)
        .collect()
}

/// The base every pasted image draws on: `doc.base` with every Image
/// annotation composited, in list order (NEXT_STEPS step 11), as a [`Layer`]
/// (step 12).
///
/// This is the layer Blur sprites read, so a blur over a pasted image really
/// blurs the pasted pixels rather than the base image behind them.
///
/// With `expand` the layer covers the base image **and** the painted pixels of
/// the pasted images, so a blur or highlight outside the original screenshot
/// has pixels to read. Without it the layer is exactly the base image, which
/// keeps Cut mode byte-for-byte what it always was.
#[must_use]
pub fn images_base(doc: &Document, fonts: &mut Fonts, expand: bool) -> Layer {
    let mut sprites = Vec::new();
    for annotation in doc
        .annotations
        .iter()
        .filter(|a| matches!(a.kind, Kind::Image(_)))
    {
        if let Some(sprite) = rasterize(annotation, &doc.base, fonts) {
            sprites.push(sprite);
        }
    }
    if sprites.is_empty() {
        return Layer::whole(Arc::clone(&doc.base));
    }

    let base_rect = RectI {
        x: 0,
        y: 0,
        right: doc.base.width() as i32,
        bottom: doc.base.height() as i32,
    };
    let mut area = base_rect;
    let mut covered = vec![base_rect];
    for sprite in &sprites {
        if let Some(painted) = painted_bounds(sprite) {
            area = area.union(painted);
            covered.push(painted);
        }
    }

    if !expand || area == base_rect || too_large(area) {
        // Cut mode, or a layer the export would refuse to build anyway: the
        // base-sized layer, exactly as before step 12.
        let mut pixels = doc.base.as_ref().clone();
        for sprite in &sprites {
            composite_onto(&mut pixels, sprite, (0, 0));
        }
        return Layer::new(pixels, (0, 0), None);
    }

    let offset = (-area.x, -area.y);
    let mut pixels = RgbaImage::new(area.width(), area.height());
    paste_region(&mut pixels, &doc.base, offset);
    // `composite_onto` takes the layer's *origin* (where image (0, 0) sits is
    // its negation): passing `offset` here put a pasted image left of or above
    // the base in the wrong place, and a Blur over it read empty pixels.
    for sprite in &sprites {
        composite_onto(&mut pixels, sprite, (area.x, area.y));
    }
    Layer::new(pixels, (area.x, area.y), Some(covered))
}

/// True when a rectangle is past the size [`flatten_with`] will build.
fn too_large(area: RectI) -> bool {
    let (width, height) = (area.width(), area.height());
    width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_EXPORT_PIXELS
}

/// [`images_base`] with every Blur already composited; Highlights read this,
/// so a highlight can never reveal a blurred password (R18, and `DECISIONS.md`
/// for why that differs from SPEC 4.2).
///
/// The canvas calls this too, so the screen and the saved file agree.
#[must_use]
pub fn effects_base(doc: &Document, fonts: &mut Fonts, expand: bool) -> Layer {
    let images = images_base(doc, fonts, expand);
    effects_base_from(doc, &images, fonts)
}

/// [`effects_base`] for an images layer that is already built.
///
/// The app and `flatten_with` need both layers, and building the images layer
/// twice would composite a large pasted image twice for nothing. The result
/// keeps the images layer's origin and coverage.
#[must_use]
pub fn effects_base_from(doc: &Document, images: &Layer, fonts: &mut Fonts) -> Layer {
    let any_blurs = doc
        .annotations
        .iter()
        .any(|a| matches!(a.kind, Kind::Blur(_)));
    if !any_blurs {
        return images.clone();
    }

    // Every Blur reads the base with the images but *without* the other blurs,
    // so two overlapping blurs each blur the same sharp pixels.
    let mut pixels = images.pixels().clone();
    for annotation in doc
        .annotations
        .iter()
        .filter(|a| matches!(a.kind, Kind::Blur(_)))
    {
        if let Some(sprite) = rasterize_layer(annotation, images, fonts) {
            composite_onto(&mut pixels, &sprite, images.origin());
        }
    }
    Layer::new(pixels, images.origin(), images.covered.clone())
}

/// Rasterizes every annotation onto the base image (SPEC section 4.1).
///
/// Compositing order: the base image, the pasted images, then every Blur, then
/// every Highlight, then everything else in list order — in exactly the order
/// [`compositing_order`] gives, so the canvas and this function cannot drift.
///
/// Everything outside the base image is cut off: see [`flatten_with`] for the
/// option that keeps it (NEXT_STEPS step 10).
#[must_use]
pub fn flatten(doc: &Document, fonts: &mut Fonts) -> RgbaImage {
    // The base plus the pasted images plus the blurs; Highlights read this.
    let base = effects_base(doc, fonts, false);
    let mut canvas = base.pixels().clone();

    // A highlight over a blur multiplies the blurred pixels rather than the
    // sharp ones.
    for annotation in doc
        .annotations
        .iter()
        .filter(|a| matches!(a.kind, Kind::Highlight(_)))
    {
        if let Some(sprite) = rasterize_layer(annotation, &base, fonts) {
            composite_onto(&mut canvas, &sprite, base.origin());
        }
    }

    // Everything else, in list order. The images are already in the canvas
    // (they are part of the base layer), so they are not drawn twice.
    for annotation in doc
        .annotations
        .iter()
        .filter(|a| !a.kind.is_effect() && !matches!(a.kind, Kind::Image(_)))
    {
        if let Some(sprite) = rasterize(annotation, &doc.base, fonts) {
            composite_onto(&mut canvas, &sprite, base.origin());
        }
    }

    canvas
}

/// What [`flatten_with`] does with annotations that stick out past the base
/// image (NEXT_STEPS step 10).
///
/// The app fills this from the "Annotations outside the image" and "Fill for
/// the added area" settings; the core only implements the two behaviours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlattenOptions {
    /// Grow the output to hold every annotation. Off cuts them off at the
    /// image edge — what this program did before step 10, and what saved files
    /// have always looked like.
    pub expand: bool,
    /// The colour the added area is filled with; `None` leaves it transparent.
    /// Only meaningful with `expand`.
    pub fill: Option<Rgba>,
}

impl FlattenOptions {
    /// Today's behaviour and the default: anything outside the image is cut
    /// off. `flatten_with` with these options is byte-identical to
    /// [`flatten`].
    pub const CUT: Self = Self {
        expand: false,
        fill: None,
    };
}

/// The rectangle the exported image would cover: the base image united with
/// the **painted pixels** of every annotation that draws (NEXT_STEPS step 11,
/// J1 and J2).
///
/// Blur and Highlight are left out. Their pixels come from the layer with the
/// pasted images, which never reaches past the pasted pixels themselves
/// (NEXT_STEPS step 12), so they can never draw anywhere the pasted images do
/// not already reach; unioning in the rectangle they were given made a blur
/// dragged past the top edge grow the saved image by a strip of fill colour
/// (J1).
///
/// Everything else contributes its painted pixels rather than its
/// [`bounds`]: the two pixels of anti-aliasing slack are not something the
/// user drew (J2).
#[must_use]
pub fn export_bounds(doc: &Document, fonts: &mut Fonts) -> RectI {
    let sprites: Vec<Sprite> = doc
        .annotations
        .iter()
        .filter(|annotation| !annotation.kind.is_effect())
        .filter_map(|annotation| rasterize(annotation, &doc.base, fonts))
        .collect();
    export_bounds_from_sprites(doc.base.width(), doc.base.height(), sprites.iter())
}

/// [`export_bounds`] for sprites that are already rasterized.
///
/// [`flatten_with`] draws these sprites anyway, so it hands its own list over
/// instead of rasterizing every annotation twice.
fn export_bounds_from_sprites<'a>(
    base_width: u32,
    base_height: u32,
    sprites: impl IntoIterator<Item = &'a Sprite>,
) -> RectI {
    let mut area = RectI {
        x: 0,
        y: 0,
        right: base_width as i32,
        bottom: base_height as i32,
    };
    for sprite in sprites {
        if let Some(painted) = painted_bounds(sprite) {
            area = area.union(painted);
        }
    }
    area
}

/// [`flatten`] with options: with `expand` the output is [`export_bounds`]
/// sized, filled first, and every annotation is composited at its offset from
/// the export's corner; with [`FlattenOptions::CUT`] the result is exactly
/// [`flatten`] (NEXT_STEPS step 10).
///
/// The compositing order is the same as [`flatten`]'s, so the two modes and the
/// canvas agree about what is on top. Blur and Highlight sprites are clipped to
/// the [`Layer`] they read — the base image and the pasted images (NEXT_STEPS
/// step 12) — so they can never grow the export, and the part of such an
/// annotation that sticks out of that layer draws nothing (J1).
///
/// Every annotation is rasterized once, up front: the painted pixels of the
/// non-effect sprites decide how big the export is (J2), and the same sprites
/// are composited below.
#[must_use]
pub fn flatten_with(doc: &Document, fonts: &mut Fonts, options: &FlattenOptions) -> RgbaImage {
    if !options.expand {
        return flatten(doc, fonts);
    }

    // The two layers Blur and Highlight read (NEXT_STEPS steps 11 and 12): the
    // base plus the pasted images, and that plus the blurs. With Expand the
    // layers reach the pasted images, so an effect outside the original
    // screenshot has pixels to read.
    let images = images_base(doc, fonts, true);
    let effects = effects_base_from(doc, &images, fonts);

    let mut image_sprites: Vec<Sprite> = Vec::new();
    let mut blur_sprites: Vec<Sprite> = Vec::new();
    let mut highlight_sprites: Vec<Sprite> = Vec::new();
    let mut other_sprites: Vec<Sprite> = Vec::new();
    for annotation in &doc.annotations {
        let sprite = match &annotation.kind {
            Kind::Image(_) => rasterize(annotation, &doc.base, fonts),
            Kind::Blur(_) => rasterize_layer(annotation, &images, fonts),
            Kind::Highlight(_) => rasterize_layer(annotation, &effects, fonts),
            _ => rasterize(annotation, &doc.base, fonts),
        };
        let Some(sprite) = sprite else {
            continue;
        };
        match &annotation.kind {
            Kind::Image(_) => image_sprites.push(sprite),
            Kind::Blur(_) => blur_sprites.push(sprite),
            Kind::Highlight(_) => highlight_sprites.push(sprite),
            _ => other_sprites.push(sprite),
        }
    }

    // J1 + J2: the base united with the painted pixels of every sprite that is
    // not an effect. Blur and Highlight are clipped to the layer with the
    // pasted images, so they cannot grow it past what the pasted images
    // painted; the pasted images are part of the export.
    let export = export_bounds_from_sprites(
        doc.base.width(),
        doc.base.height(),
        image_sprites.iter().chain(other_sprites.iter()),
    );
    let (width, height) = (export.width(), export.height());
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_EXPORT_PIXELS {
        tracing::error!(
            width,
            height,
            "the expanded export is too large to build; cutting the annotations off instead"
        );
        return flatten(doc, fonts);
    }

    // The canvas is **premultiplied** while it is built, so the whole thing is
    // one source-over formula; it is converted back to the straight alpha an
    // `RgbaImage` promises at the end.
    let mut canvas = vec![0u8; width as usize * height as usize * 4];
    if let Some(fill) = options.fill {
        let mut pixel = fill;
        premultiply(&mut pixel);
        for destination in canvas.as_chunks_mut::<4>().0 {
            destination.copy_from_slice(&pixel);
        }
    }

    // The export's top-left corner is where the base image's (0, 0) lands.
    let offset = (-export.x, -export.y);
    let mut base = doc.base.as_raw().clone();
    premultiply(&mut base);
    over_region(
        &mut canvas,
        width,
        height,
        &base,
        doc.base.width(),
        doc.base.height(),
        offset,
    );

    // The compositing order of NEXT_STEPS step 11: the pasted images first,
    // then the blurs, then the highlights, then everything else.
    for sprite in &image_sprites {
        over_sprite(&mut canvas, width, height, sprite, offset);
    }
    for sprite in &blur_sprites {
        over_sprite(&mut canvas, width, height, sprite, offset);
    }
    for sprite in &highlight_sprites {
        over_sprite(&mut canvas, width, height, sprite, offset);
    }
    for sprite in &other_sprites {
        over_sprite(&mut canvas, width, height, sprite, offset);
    }

    unpremultiply(&mut canvas);
    RgbaImage::from_raw(width, height, canvas).unwrap_or_else(|| flatten(doc, fonts))
}

/// One premultiplied 4-byte pixel over another, in place.
///
/// `source` is premultiplied and `destination` is premultiplied too, so this is
/// the whole blend: `out = src + dst * (1 - src_alpha)`.
fn over_pixel(destination: &mut [u8], source: &[u8]) {
    let alpha = u16::from(source[3]);
    match alpha {
        255 => destination.copy_from_slice(source),
        0 => {}
        _ => {
            let inverse = u32::from(255 - alpha);
            for channel in 0..3 {
                let under = (u32::from(destination[channel]) * inverse + 127) / 255;
                destination[channel] = (u32::from(source[channel]) + under).min(255) as u8;
            }
            let under = (u32::from(destination[3]) * inverse + 127) / 255;
            destination[3] = (u32::from(alpha) + under).min(255) as u8;
        }
    }
}

/// Composites a premultiplied buffer onto a premultiplied canvas at `origin`,
/// clipping to the canvas (NEXT_STEPS step 10's expansion).
fn over_region(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    origin: (i32, i32),
) {
    for row in 0..source_height as i32 {
        let y = origin.1 + row;
        if y < 0 || y >= canvas_height as i32 {
            continue;
        }
        for column in 0..source_width as i32 {
            let x = origin.0 + column;
            if x < 0 || x >= canvas_width as i32 {
                continue;
            }
            let source_at = (row as usize * source_width as usize + column as usize) * 4;
            let canvas_at = (y as usize * canvas_width as usize + x as usize) * 4;
            over_pixel(
                &mut canvas[canvas_at..canvas_at + 4],
                &source[source_at..source_at + 4],
            );
        }
    }
}

/// [`over_region`] for a sprite, which is already premultiplied. `offset` is
/// the export's own origin, so sprite and base coordinates stay in image space.
fn over_sprite(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    sprite: &Sprite,
    offset: (i32, i32),
) {
    over_region(
        canvas,
        canvas_width,
        canvas_height,
        sprite.pixmap.data(),
        sprite.pixmap.width(),
        sprite.pixmap.height(),
        (sprite.origin.0 + offset.0, sprite.origin.1 + offset.1),
    );
}

/// Rasterizes one annotation without its shadow, into a canvas exactly
/// `bounds()` sized. The shadow is added afterwards, over the finished shape.
fn shape(a: &Annotation, fonts: &mut Fonts) -> Option<(Pixmap, (i32, i32))> {
    // A shape with no extent (no fill and no stroke, one path point, fewer than
    // three polygon points) has nothing to draw.
    if shape_bounds(a, fonts).is_empty() {
        return None;
    }
    let area = bounds(a, fonts);
    let rounded = area.round_out();
    if rounded.width() == 0 || rounded.height() == 0 {
        return None;
    }
    let mut canvas = Canvas::new(rounded)?;

    match &a.kind {
        Kind::Rect(r) => rect::draw(&mut canvas, r),
        Kind::Path(p) => path::draw(&mut canvas, p),
        Kind::Polygon(p) => polygon::draw(&mut canvas, p),
        Kind::Text(t) => text::draw(&mut canvas, t, fonts),
        Kind::Number(n) => number::draw(&mut canvas, n, fonts),
        Kind::Image(pasted) => image::draw(&mut canvas, pasted),
        Kind::Freehand(stroke) => freehand::draw(&mut canvas, stroke),
        // Effects never reach here: they are rasterized from the base image.
        Kind::Highlight(_) | Kind::Blur(_) => return None,
    }

    Some((canvas.pixmap, canvas.origin))
}

/// The shape's own rectangle, before shadow and slack.
///
/// Effects are exact rectangles: their pixels stop at the rectangle edge, so
/// they get no slack either. The rectangle is rounded out to whole pixels,
/// because that is what the sprite itself covers (SPEC section 4.2).
#[must_use]
pub fn shape_bounds(a: &Annotation, fonts: &mut Fonts) -> Rect {
    match &a.kind {
        Kind::Rect(r) => rect::shape_bounds(r),
        Kind::Path(p) => path::shape_bounds(p),
        Kind::Polygon(p) => polygon::shape_bounds(p),
        Kind::Highlight(highlight) => highlight.rect.round_out().to_rect(),
        Kind::Blur(blur) => blur.rect.round_out().to_rect(),
        Kind::Text(text) => text::shape_bounds(text, fonts),
        Kind::Number(number) => number::shape_bounds(number),
        Kind::Image(pasted) => image::shape_bounds(pasted),
        Kind::Freehand(stroke) => freehand::shape_bounds(stroke),
    }
}

/// A pixmap plus the image-space origin its drawing coordinates are relative
/// to.
pub(crate) struct Canvas {
    pub pixmap: Pixmap,
    /// Top-left in image pixels.
    pub origin: (i32, i32),
    /// Maps image coordinates onto the pixmap.
    pub transform: Transform,
}

impl Canvas {
    /// An all-transparent pixmap covering `rounded`, positioned at its origin.
    pub fn new(rounded: crate::geom::RectI) -> Option<Self> {
        let pixmap = Pixmap::new(rounded.width(), rounded.height())?;
        let transform = Transform::from_translate(-(rounded.x as f32), -(rounded.y as f32));
        Some(Self {
            pixmap,
            origin: (rounded.x, rounded.y),
            transform,
        })
    }

    /// A paint with anti-aliasing on, as SPEC section 4.2 requires for shapes.
    pub fn paint(color: Rgba) -> Paint<'static> {
        let mut paint = Paint::default();
        paint.set_color(Color::from_rgba8(color[0], color[1], color[2], color[3]));
        paint.anti_alias = true;
        paint
    }
}

/// Puts the black, blurred, offset shadow under an already drawn shape.
fn add_shadow(shape: &Pixmap) -> Option<Pixmap> {
    let alpha: Vec<u8> = shape
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| p[3])
        .collect();
    let mask = crate::effects::shadow_mask(&alpha, shape.width(), shape.height());

    let mut result = Pixmap::new(shape.width(), shape.height())?;
    for (pixel, &opacity) in result
        .data_mut()
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(mask.iter())
    {
        // Black at the blurred alpha, written straight into premultiplied
        // storage: black premultiplied by any alpha is still black.
        pixel[0] = 0;
        pixel[1] = 0;
        pixel[2] = 0;
        pixel[3] = opacity;
    }
    source_over(result.data_mut(), shape.data());
    Some(result)
}

/// Source-over composite of two premultiplied buffers of the same size.
fn source_over(dst: &mut [u8], src: &[u8]) {
    for (destination, source) in dst
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(src.as_chunks::<4>().0.iter())
    {
        let alpha = u16::from(source[3]);
        match alpha {
            255 => destination.copy_from_slice(source),
            0 => {}
            _ => {
                let inverse = 255 - alpha;
                for channel in 0..3 {
                    let under = (u32::from(destination[channel]) * u32::from(inverse) + 127) / 255;
                    destination[channel] = (u32::from(source[channel]) + under).min(255) as u8;
                }
                let under = (u32::from(destination[3]) * u32::from(inverse) + 127) / 255;
                destination[3] = (u32::from(alpha) + under).min(255) as u8;
            }
        }
    }
}

/// Composites a sprite onto an image, which is how `flatten` builds its result.
///
/// `offset` is where the image's own (0, 0) sits inside the sprite's
/// coordinate space: (0, 0) for the base image, the layer's origin when the
/// image is a [`Layer`] that reaches past the base (NEXT_STEPS step 12).
///
/// The sprite is premultiplied and the image is straight, so the blend is the
/// premultiplied source-over: `out = src + dst * (1 - src_alpha)`. That is
/// [`over_pixel`]'s formula; the image is opaque in the normal case, where the
/// two agree exactly, and this is the M2 behaviour the goldens pin down.
fn composite_onto(image: &mut RgbaImage, sprite: &Sprite, offset: (i32, i32)) {
    let (width, height) = (sprite.pixmap.width(), sprite.pixmap.height());
    let (origin_x, origin_y) = (sprite.origin.0 - offset.0, sprite.origin.1 - offset.1);
    let data = sprite.pixmap.data();

    for row in 0..height as i32 {
        let destination_y = origin_y + row;
        if destination_y < 0 || destination_y >= image.height() as i32 {
            continue;
        }
        for column in 0..width as i32 {
            let destination_x = origin_x + column;
            if destination_x < 0 || destination_x >= image.width() as i32 {
                continue;
            }

            let source = &data[(row as usize * width as usize + column as usize) * 4..][..4];
            let at = (destination_y as usize * image.width() as usize + destination_x as usize) * 4;
            over_pixel(&mut image.as_mut()[at..at + 4], source);
        }
    }
}

/// Copies an image into a bigger canvas at `origin`, clipped to the canvas.
///
/// The bytes are copied, not blended: this is the first thing drawn on a layer
/// (the base image), so there is nothing under it. Like [`composite_onto`], the
/// straight-alpha bytes are treated as premultiplied while the layer is built.
fn paste_region(canvas: &mut RgbaImage, source: &RgbaImage, origin: (i32, i32)) {
    let rows = source.height() as i32;
    let columns = source.width() as i32;
    for row in 0..rows {
        let y = origin.1 + row;
        if y < 0 || y >= canvas.height() as i32 {
            continue;
        }
        let first = 0.max(-origin.0);
        let last = columns.min(canvas.width() as i32 - origin.0);
        if first >= last {
            continue;
        }
        let from = (row as usize * columns as usize + first as usize) * 4;
        let to = (y as usize * canvas.width() as usize + (origin.0 + first) as usize) * 4;
        let length = (last - first) as usize * 4;
        canvas.as_mut()[to..to + length].copy_from_slice(&source.as_raw()[from..from + length]);
    }
}

/// Wraps an effect's pixels as a sprite, dropping the parts of the layer's
/// bounding rectangle that have no pixels under them (NEXT_STEPS step 12).
///
/// The layer can be bigger than what it is made of: with the base image and a
/// pasted one beside it, the space between them is inside the layer's
/// rectangle but there is nothing there. A Blur or Highlight that drew the
/// filled pixels it computed for such a gap would paint over the fill colour of
/// the added area, so those pixels are dropped (J1).
fn effect_sprite(mut image: RgbaImage, covered: RectI, source: &Source<'_>) -> Option<Sprite> {
    let Some(rects) = source.covered else {
        return sprite_from_image(image, covered);
    };
    if rects.iter().any(|rect| rect_contains(*rect, covered)) {
        return sprite_from_image(image, covered);
    }

    let mut painted = false;
    for (column, row, pixel) in image.enumerate_pixels_mut() {
        let (x, y) = (covered.x + column as i32, covered.y + row as i32);
        if rects.iter().any(|rect| rect_has_pixel(*rect, x, y)) {
            painted = true;
        } else {
            // Zeroed in place: transparent premultiplied pixels have no colour.
            pixel.0 = [0, 0, 0, 0];
        }
    }
    painted.then(|| sprite_from_image(image, covered))?
}

/// Wraps an opaque image region as a sprite positioned at `covered`'s origin.
///
/// `covered` is the rectangle the effect really wrote, which is not always the
/// rectangle the annotation was given: a fractional rectangle is rounded out to
/// whole pixels, and a rectangle that starts outside the image is clipped.
fn sprite_from_image(image: RgbaImage, covered: RectI) -> Option<Sprite> {
    if image.width() == 0 || image.height() == 0 {
        return None;
    }
    let mut pixmap = Pixmap::new(image.width(), image.height())?;
    let mut pixels = image.into_raw();
    // Effects are opaque, but go through the conversion anyway so a future
    // translucent effect cannot produce broken premultiplied pixels.
    premultiply(&mut pixels);
    pixmap.data_mut().copy_from_slice(&pixels);

    Some(Sprite {
        pixmap,
        origin: (covered.x, covered.y),
    })
}
