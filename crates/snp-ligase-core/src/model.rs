//! The annotation model: `Document`, `Annotation` and every annotation struct
//! (SPEC section 4).
//!
//! The whole model is plain data. It knows nothing about how an annotation is
//! drawn (that is [`crate::render`]) or how it is edited (that is
//! [`crate::edit`]), which is what lets the same model drive the on-screen
//! canvas and the exported image.
//!
//! ## Serialization
//!
//! Every annotation type derives [`serde::Serialize`] and
//! [`serde::Deserialize`], because SPEC section 1 asks for a project format to
//! be *possible* later without changing the model. `Document` implements them
//! by hand, storing the base image as PNG bytes: it is the only place where a
//! lossy-in-the-wrong-way shortcut would silently destroy pixels.

use std::sync::Arc;

use image::RgbaImage;
use serde::de::Error as _;
use serde::ser::SerializeStruct as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::geom::{Point, Rect};

/// Straight (non-premultiplied) sRGB, `[r, g, b, a]`.
pub type Rgba = [u8; 4];

/// A straight sRGB colour from red, green and blue, fully opaque.
///
/// The style defaults in SPEC section 7.7 are written there as hex values.
pub const fn hex(red: u8, green: u8, blue: u8) -> Rgba {
    let channels: Rgba = [red, green, blue, 255];
    channels
}

/// A stroke colour and width. Used by paths, rectangles and polygons.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub color: Rgba,
    pub width: f32,
}

/// A line, an arrow, a curve: any sequence of points drawn as one stroke.
///
/// `curved` is true for new paths (SPEC 7.3): with two points that is still a
/// straight line, and it starts bending as soon as a third point exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathA {
    /// Two or more points.
    pub points: Vec<Point>,
    /// True: a smooth curve through every point. False: straight segments.
    pub curved: bool,
    pub stroke: Stroke,
    /// Arrowhead at the first point.
    pub head_start: bool,
    /// Arrowhead at the last point.
    pub head_end: bool,
}

/// A rectangle, optionally filled, optionally with rounded corners.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RectA {
    pub rect: Rect,
    /// `None` means no outline at all.
    pub stroke: Option<Stroke>,
    /// `None` means no fill.
    pub fill: Option<Rgba>,
    /// 0 means sharp corners.
    pub corner_radius: f32,
}

/// A closed shape: three or more points, straight edges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolygonA {
    pub points: Vec<Point>,
    pub stroke: Option<Stroke>,
    pub fill: Option<Rgba>,
}

/// A text annotation. Rasterized in M4, when `cosmic-text` lands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextA {
    /// Top-left of the text box.
    pub pos: Point,
    /// May contain `\n`.
    pub text: String,
    pub font_family: String,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub color: Rgba,
    pub background: Option<Rgba>,
    pub padding: f32,
    /// The box outline, centred on the box edge. `None` means no outline.
    ///
    /// Defaulted for deserialization so documents and config files written
    /// before the outline existed still load (NEXT_STEPS step 8).
    #[serde(default)]
    pub outline: Option<Stroke>,
}

/// A numbered badge, optionally with a pointer to the thing it points at.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NumberA {
    pub center: Point,
    pub value: u32,
    pub radius: f32,
    pub fill: Rgba,
    /// `Some` draws the "number with arrow" variant.
    pub pointer_tip: Option<Point>,
}

/// A translucent highlighter mark: a multiply blend over the base image.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HighlightA {
    pub rect: Rect,
    pub color: Rgba,
}

/// A blurred rectangle: three box blurs of `radius` over the base image.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BlurA {
    pub rect: Rect,
    pub radius: f32,
}

/// A pasted image: the pixels themselves, drawn 1:1 at `pos` (NEXT_STEPS step
/// 11).
///
/// The pixels sit behind an [`Arc`], so cloning a document for the undo stack
/// stays cheap. `pos` is kept on whole pixels — the app snaps it on create,
/// move and nudge — so the image is never resampled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageA {
    /// Top-left of the image, in image pixels.
    pub pos: Point,
    /// The pasted pixels, straight sRGB.
    ///
    /// Serialized as PNG bytes: `RgbaImage` has no serde of its own, and SPEC
    /// section 1 wants the model ready for a project format one day. Nothing
    /// writes documents today; the (de)serializer is recorded in
    /// `DECISIONS.md`.
    #[serde(with = "image_png")]
    pub image: Arc<RgbaImage>,
}

impl PartialEq for ImageA {
    /// Compares the pixels by **address**.
    ///
    /// Cloning a document for the undo stack shares the buffer, so this is what
    /// keeps `dirty()` and the undo comparisons from walking a multi-megabyte
    /// image every frame. Two separately decoded images with equal pixels are
    /// different images, which is what the model wants: a paste is a new one.
    fn eq(&self, other: &Self) -> bool {
        self.pos == other.pos && Arc::ptr_eq(&self.image, &other.image)
    }
}

/// PNG (de)serialization for an annotation's pixels.
///
/// PNG is lossless, so a round trip through a document file cannot change a
/// single pixel. The base image uses the same format in [`Document`]'s
/// hand-written serializer.
mod image_png {
    use std::sync::Arc;

    use image::RgbaImage;
    use serde::de::Error as _;
    use serde::ser::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        image: &Arc<RgbaImage>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let bytes = crate::codec::encode_png(image).map_err(S::Error::custom)?;
        serializer.serialize_bytes(&bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<RgbaImage>, D::Error> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        crate::codec::decode_png(&bytes)
            .map(Arc::new)
            .map_err(D::Error::custom)
    }
}

/// A freehand stroke: the pointer samples of one drag, already simplified
/// (NEXT_STEPS step 14a).
///
/// The human asked for it against SPEC section 1's non-goal list; the change
/// of scope is recorded in `DECISIONS.md`. There are no handles: like Text and
/// a pasted image, a stroke only moves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FreehandA {
    /// Two or more points in image pixels, in the order they were drawn.
    pub points: Vec<Point>,
    pub stroke: Stroke,
}

/// What an annotation actually is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Kind {
    Path(PathA),
    Rect(RectA),
    Polygon(PolygonA),
    Text(TextA),
    Number(NumberA),
    Highlight(HighlightA),
    Blur(BlurA),
    /// A pasted image (NEXT_STEPS step 11).
    Image(ImageA),
    /// A freehand stroke (NEXT_STEPS step 14a).
    Freehand(FreehandA),
}

impl Kind {
    /// A short name for logs and the properties panel heading.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Path(path) if path.head_start || path.head_end => "Arrow",
            Self::Path(_) => "Line",
            Self::Rect(_) => "Rectangle",
            Self::Polygon(_) => "Shape",
            Self::Text(_) => "Text",
            Self::Number(_) => "Number",
            Self::Highlight(_) => "Highlight",
            Self::Blur(_) => "Blur",
            Self::Image(_) => "Pasted image",
            Self::Freehand(_) => "Freehand",
        }
    }

    /// True for the two kinds whose sprite comes from the base image rather
    /// than from vector drawing (SPEC section 4.2).
    ///
    /// An Image annotation is not an effect: its pixels are its own, they move
    /// with it, and they count towards the export bounds (NEXT_STEPS step 11).
    #[must_use]
    pub fn is_effect(&self) -> bool {
        matches!(self, Self::Highlight(_) | Self::Blur(_))
    }
}

/// One annotation: an id, a shadow flag and the kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    pub id: u64,
    /// Drop shadow under the annotation. Not applicable to Blur and Highlight,
    /// which are opaque and cover their own rectangle.
    pub shadow: bool,
    #[serde(flatten)]
    pub kind: Kind,
}

/// An image and the annotations on it.
///
/// Cheap to clone: the image is behind an [`Arc`] and the annotations are small
/// (SPEC section 4).
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    /// The screenshot or opened image. Behind an `Arc` so cloning a document
    /// for the undo stack does not copy the pixels.
    pub base: Arc<RgbaImage>,
    /// Z-order: the last annotation is on top.
    pub annotations: Vec<Annotation>,
    /// The value the next Number annotation gets (SPEC 7.6).
    pub next_number: u32,
    /// The id the next annotation gets.
    pub next_id: u64,
}

impl Document {
    /// A new document with one annotation's worth of state ready to use.
    #[must_use]
    pub fn new(base: RgbaImage) -> Self {
        Self {
            base: Arc::new(base),
            annotations: Vec::new(),
            next_number: 1,
            next_id: 1,
        }
    }

    /// Hands out the next annotation id and advances the counter.
    pub fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// The base image's size in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.base.width(), self.base.height())
    }

    /// Appends an annotation with a fresh id and returns its id.
    pub fn push(&mut self, kind: Kind) -> u64 {
        let id = self.allocate_id();
        self.annotations.push(Annotation {
            id,
            shadow: false,
            kind,
        });
        id
    }

    /// The annotation with that id.
    #[must_use]
    pub fn find(&self, id: u64) -> Option<&Annotation> {
        self.annotations.iter().find(|a| a.id == id)
    }

    /// The annotation with that id, mutably.
    pub fn find_mut(&mut self, id: u64) -> Option<&mut Annotation> {
        self.annotations.iter_mut().find(|a| a.id == id)
    }
}

impl Serialize for Document {
    /// Serializes the annotations and the base image, the image as PNG bytes.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error as _;

        // Encoding the base to PNG can only fail on absurd sizes, but this is
        // a public API and must not panic.
        let base = crate::codec::encode_png(&self.base).map_err(S::Error::custom)?;

        let mut state = serializer.serialize_struct("Document", 4)?;
        state.serialize_field("base_png", &base)?;
        state.serialize_field("annotations", &self.annotations)?;
        state.serialize_field("next_number", &self.next_number)?;
        state.serialize_field("next_id", &self.next_id)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for Document {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Repr {
            base_png: Vec<u8>,
            annotations: Vec<Annotation>,
            next_number: u32,
            next_id: u64,
        }

        let repr = Repr::deserialize(deserializer)?;
        let base = crate::codec::decode_png(&repr.base_png).map_err(D::Error::custom)?;

        Ok(Self {
            base: Arc::new(base),
            annotations: repr.annotations,
            next_number: repr.next_number,
            next_id: repr.next_id,
        })
    }
}

/// The initial style defaults from SPEC section 7.7.
///
/// Each tool remembers these across sessions (SPEC 7.7); the config file
/// arrives in M6, which is where they get persisted.
pub mod defaults {
    use super::{Rgba, Stroke, hex};

    /// `#E81123` — the stroke colour every drawing tool starts with.
    pub const STROKE_COLOR: Rgba = hex(0xE8, 0x11, 0x23);
    /// 4 px.
    pub const STROKE_WIDTH: f32 = 4.0;
    /// The default stroke.
    #[must_use]
    pub fn stroke() -> Stroke {
        Stroke {
            color: STROKE_COLOR,
            width: STROKE_WIDTH,
        }
    }

    /// `#FFFF00`.
    pub const HIGHLIGHT_COLOR: Rgba = hex(0xFF, 0xFF, 0x00);
    /// Blur "Heavy" from the Annotation menu (SPEC 8.3).
    pub const BLUR_RADIUS: f32 = 28.0;
    /// Text defaults; the size is used from M4 on.
    pub const TEXT_SIZE: f32 = 28.0;
    pub const TEXT_COLOR: Rgba = [255, 255, 255, 255];
    /// `#0000FF`.
    pub const TEXT_BACKGROUND: Rgba = hex(0x00, 0x00, 0xFF);
    pub const TEXT_PADDING: f32 = 6.0;
    pub const TEXT_BOLD: bool = true;
    /// `#E81123` — the same red as the stroke (SPEC 7.7).
    pub const NUMBER_FILL: Rgba = hex(0xE8, 0x11, 0x23);
    pub const NUMBER_RADIUS: f32 = 16.0;
    /// Shadow is off for every tool.
    pub const SHADOW: bool = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;

    fn document() -> Document {
        Document::new(RgbaImage::new(64, 48))
    }

    fn rectangle() -> Kind {
        Kind::Rect(RectA {
            rect: Rect::new(4.0, 4.0, 20.0, 10.0),
            stroke: Some(defaults::stroke()),
            fill: None,
            corner_radius: 0.0,
        })
    }

    #[test]
    fn a_new_document_starts_numbering_at_one() {
        let doc = document();
        assert_eq!(doc.next_number, 1);
        assert_eq!(doc.next_id, 1);
        assert_eq!(doc.size(), (64, 48));
    }

    #[test]
    fn ids_are_handed_out_in_order_and_are_unique() {
        let mut doc = document();
        let first = doc.push(rectangle());
        let second = doc.push(rectangle());
        assert_ne!(first, second);
        assert_eq!(doc.next_id, 3);
        assert!(doc.find(first).is_some());
        assert!(doc.find_mut(second).is_some());
    }

    #[test]
    fn annotations_are_cheap_to_clone() {
        // The whole point of the Arc: cloning a document for the undo stack
        // must not copy the pixels.
        let mut doc = document();
        doc.push(rectangle());
        let clone = doc.clone();
        assert_eq!(doc, clone);
        assert!(Arc::ptr_eq(&doc.base, &clone.base));
    }

    #[test]
    fn kinds_report_their_names() {
        assert_eq!(rectangle().name(), "Rectangle");

        let arrow = Kind::Path(PathA {
            points: vec![Point::new(0.0, 0.0), Point::new(10.0, 10.0)],
            curved: false,
            stroke: defaults::stroke(),
            head_start: false,
            head_end: true,
        });
        assert_eq!(arrow.name(), "Arrow");

        let line = Kind::Path(PathA {
            points: vec![Point::new(0.0, 0.0), Point::new(10.0, 10.0)],
            curved: false,
            stroke: defaults::stroke(),
            head_start: false,
            head_end: false,
        });
        assert_eq!(line.name(), "Line");

        let image = Kind::Image(ImageA {
            pos: Point::new(0.0, 0.0),
            image: Arc::new(RgbaImage::new(1, 1)),
        });
        assert_eq!(image.name(), "Pasted image");

        let freehand = Kind::Freehand(FreehandA {
            points: vec![Point::new(0.0, 0.0), Point::new(10.0, 10.0)],
            stroke: defaults::stroke(),
        });
        assert_eq!(freehand.name(), "Freehand");
    }

    #[test]
    fn effects_are_recognized_as_such() {
        assert!(
            Kind::Blur(BlurA {
                rect: Rect::ZERO,
                radius: 4.0
            })
            .is_effect()
        );
        assert!(
            Kind::Highlight(HighlightA {
                rect: Rect::ZERO,
                color: defaults::HIGHLIGHT_COLOR
            })
            .is_effect()
        );
        assert!(!rectangle().is_effect());
        // A pasted image's pixels are its own, not read from the base image.
        assert!(
            !Kind::Image(ImageA {
                pos: Point::new(0.0, 0.0),
                image: Arc::new(RgbaImage::new(1, 1)),
            })
            .is_effect()
        );
        // A freehand stroke is a plain annotation: its pixels are its own and
        // it draws above the effects (NEXT_STEPS step 14a).
        assert!(
            !Kind::Freehand(FreehandA {
                points: vec![Point::new(0.0, 0.0), Point::new(4.0, 4.0)],
                stroke: defaults::stroke(),
            })
            .is_effect()
        );
    }

    #[test]
    fn a_document_round_trips_through_serde() {
        let mut doc = document();
        doc.push(rectangle());
        doc.next_number = 7;

        let json = serde_json::to_string(&doc).expect("document serialises");
        let restored: Document = serde_json::from_str(&json).expect("document deserialises");

        assert_eq!(restored, doc);
        assert_eq!(restored.base.dimensions(), (64, 48));
        assert_eq!(restored.next_number, 7);
    }

    #[test]
    fn annotations_round_trip_through_serde_on_their_own() {
        let kinds = vec![
            rectangle(),
            Kind::Path(PathA {
                points: vec![Point::new(1.0, 2.0), Point::new(3.0, 4.0)],
                curved: true,
                stroke: Stroke {
                    color: [1, 2, 3, 4],
                    width: 2.5,
                },
                head_start: true,
                head_end: false,
            }),
            Kind::Polygon(PolygonA {
                points: vec![
                    Point::new(0.0, 0.0),
                    Point::new(10.0, 0.0),
                    Point::new(5.0, 9.0),
                ],
                stroke: None,
                fill: Some([9, 8, 7, 6]),
            }),
            Kind::Text(TextA {
                pos: Point::new(5.0, 5.0),
                text: "hello\nworld".to_owned(),
                font_family: "Segoe UI".to_owned(),
                size: 28.0,
                bold: true,
                italic: false,
                color: defaults::TEXT_COLOR,
                background: Some(defaults::TEXT_BACKGROUND),
                padding: 6.0,
                outline: Some(Stroke {
                    color: [7, 8, 9, 255],
                    width: 3.0,
                }),
            }),
            Kind::Number(NumberA {
                center: Point::new(20.0, 20.0),
                value: 3,
                radius: 16.0,
                fill: defaults::STROKE_COLOR,
                pointer_tip: Some(Point::new(90.0, 80.0)),
            }),
            Kind::Highlight(HighlightA {
                rect: Rect::new(1.0, 1.0, 2.0, 2.0),
                color: defaults::HIGHLIGHT_COLOR,
            }),
            Kind::Blur(BlurA {
                rect: Rect::new(1.0, 1.0, 2.0, 2.0),
                radius: 28.0,
            }),
            Kind::Image(ImageA {
                pos: Point::new(3.0, 4.0),
                image: Arc::new(RgbaImage::from_pixel(2, 3, image::Rgba([9, 8, 7, 255]))),
            }),
            // NEXT_STEPS step 14a: a freehand stroke round-trips like every
            // other kind.
            Kind::Freehand(FreehandA {
                points: vec![
                    Point::new(1.0, 2.0),
                    Point::new(3.5, 4.25),
                    Point::new(8.0, 1.5),
                ],
                stroke: Stroke {
                    color: [3, 2, 1, 128],
                    width: 6.5,
                },
            }),
        ];

        for kind in kinds {
            let annotation = Annotation {
                id: 7,
                shadow: true,
                kind,
            };
            let json = serde_json::to_string(&annotation).expect("annotation serialises");
            let restored: Annotation =
                serde_json::from_str(&json).expect("annotation deserialises");
            match (&annotation.kind, &restored.kind) {
                // `ImageA` compares its pixels by address, so the decoded
                // buffer is a different one: the pixels themselves are what the
                // round trip has to preserve.
                (Kind::Image(before), Kind::Image(after)) => {
                    assert_eq!(before.pos, after.pos);
                    assert_eq!(
                        before.image.as_ref(),
                        after.image.as_ref(),
                        "the pasted pixels changed in the round trip"
                    );
                    assert_eq!(restored.id, annotation.id);
                    assert_eq!(restored.shadow, annotation.shadow);
                }
                _ => assert_eq!(restored, annotation),
            }
        }
    }

    /// NEXT_STEPS step 11: an image's pixels are compared by address, so
    /// `dirty()` and the undo comparisons do not walk a multi-megabyte buffer
    /// every frame. Two documents that share a paste compare equal; a
    /// separately decoded copy with the same pixels does not.
    #[test]
    fn image_annotations_compare_their_pixel_buffers_by_address() {
        let pixels = RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
        let shared = Arc::new(pixels.clone());
        let same = Kind::Image(ImageA {
            pos: Point::new(5.0, 5.0),
            image: Arc::clone(&shared),
        });
        let also_same = Kind::Image(ImageA {
            pos: Point::new(5.0, 5.0),
            image: shared,
        });
        assert_eq!(same, also_same, "a shared buffer is the same image");

        let copy = Kind::Image(ImageA {
            pos: Point::new(5.0, 5.0),
            image: Arc::new(pixels),
        });
        assert_ne!(same, copy, "a separately decoded copy is a different image");

        // A move still shows up: the position is compared.
        let buffer = match &same {
            Kind::Image(image) => Arc::clone(&image.image),
            _ => unreachable!(),
        };
        let moved = Kind::Image(ImageA {
            pos: Point::new(6.0, 5.0),
            image: buffer,
        });
        assert_ne!(same, moved, "a moved image compares equal");
    }

    /// NEXT_STEPS step 11: a pasted image's pixels survive a document round
    /// trip through the custom PNG (de)serializer, pixel for pixel.
    #[test]
    fn a_pasted_image_round_trips_through_serde_with_its_pixels() {
        let pixels = RgbaImage::from_fn(3, 2, |x, y| {
            image::Rgba([x as u8 * 10, y as u8 * 20, 7, 255 - x as u8])
        });
        let kind = Kind::Image(ImageA {
            pos: Point::new(-5.0, 12.0),
            image: Arc::new(pixels.clone()),
        });

        let json = serde_json::to_string(&kind).expect("the image serialises");
        let restored: Kind = serde_json::from_str(&json).expect("the image deserialises");
        let Kind::Image(restored) = restored else {
            panic!("expected an image annotation");
        };
        assert_eq!(restored.pos, Point::new(-5.0, 12.0));
        assert_eq!(
            restored.image.as_ref(),
            &pixels,
            "the pixels changed in the round trip"
        );
    }

    #[test]
    fn a_text_without_an_outline_still_loads() {
        // NEXT_STEPS step 8: `outline` is serde-defaulted, so a document (or
        // config) written before the outline existed still loads.
        let json = r#"{
            "kind": "text",
            "pos": { "x": 1.0, "y": 2.0 },
            "text": "old",
            "font_family": "Segoe UI",
            "size": 28.0,
            "bold": true,
            "italic": false,
            "color": [255, 255, 255, 255],
            "background": null,
            "padding": 6.0
        }"#;
        let kind: Kind = serde_json::from_str(json).expect("an old text annotation loads");
        let Kind::Text(text) = kind else {
            panic!("expected text");
        };
        assert_eq!(text.outline, None);
    }
}
