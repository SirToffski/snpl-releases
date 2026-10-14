//! The tools and the style each one remembers (SPEC sections 7.3 and 7.7).

use snp_ligase_core::model::{Rgba, Stroke, defaults};

/// One tool. The key is its single-key shortcut, active when no text field has
/// focus (SPEC section 7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tool {
    #[default]
    Select,
    Line,
    Arrow,
    Rectangle,
    Shape,
    Text,
    Number,
    Highlight,
    Blur,
    Crop,
}

impl Tool {
    /// Every tool, in the Tools menu's order (SPEC 8.3).
    pub const ALL: [Self; 10] = [
        Self::Select,
        Self::Line,
        Self::Arrow,
        Self::Rectangle,
        Self::Shape,
        Self::Text,
        Self::Number,
        Self::Highlight,
        Self::Blur,
        Self::Crop,
    ];

    /// The name shown on the tool button and in the Tools menu. Always a word,
    /// never an icon (SPEC 8.1).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Select => "Select",
            Self::Line => "Line",
            Self::Arrow => "Arrow",
            Self::Rectangle => "Rectangle",
            Self::Shape => "Shape",
            Self::Text => "Text",
            Self::Number => "Number",
            Self::Highlight => "Highlight",
            Self::Blur => "Blur",
            Self::Crop => "Crop",
        }
    }

    /// The single-key shortcut (SPEC section 7.3).
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Select => "V",
            Self::Line => "L",
            Self::Arrow => "A",
            Self::Rectangle => "R",
            Self::Shape => "P",
            Self::Text => "T",
            Self::Number => "N",
            Self::Highlight => "H",
            Self::Blur => "B",
            Self::Crop => "C",
        }
    }
}

/// The Tools menu in SPEC 8.3 order: `(name, key, built)`.
///
/// `built` is false for the tools this milestone has not finished yet, so the
/// menu already shows the whole list and those rows say why they cannot be
/// picked rather than doing nothing.
pub const TOOL_MENU: [(&str, &str, bool); 10] = [
    ("Select", "V", true),
    ("Line", "L", true),
    ("Arrow", "A", true),
    ("Rectangle", "R", true),
    ("Shape", "P", true),
    ("Text", "T", true),
    ("Number", "N", true),
    ("Highlight", "H", true),
    ("Blur", "B", true),
    ("Crop", "C", true),
];

/// The tool a row of [`TOOL_MENU`] selects.
#[must_use]
pub fn tool_for_row(index: usize) -> Option<Tool> {
    Tool::ALL.get(index).copied()
}

/// The tool that owns an annotation kind, so a property change knows which
/// tool's defaults to update (SPEC 7.7).
///
/// A `Path` belongs to Arrow when it has any arrowhead and to Line when it does
/// not: that is what stops one tool's style from leaking into the other's.
///
/// A pasted image has no tool: it arrives from the clipboard, not from the
/// toolbar. Its one property, Shadow, still applies — see
/// [`crate::panels::properties::apply_properties`] — it just has no defaults to
/// update (NEXT_STEPS step 11).
#[must_use]
pub fn tool_for_kind(kind: &snp_ligase_core::model::Kind) -> Option<Tool> {
    use snp_ligase_core::model::Kind as K;
    match kind {
        K::Path(path) if path.head_start || path.head_end => Some(Tool::Arrow),
        K::Path(_) => Some(Tool::Line),
        K::Rect(_) => Some(Tool::Rectangle),
        K::Polygon(_) => Some(Tool::Shape),
        K::Text(_) => Some(Tool::Text),
        K::Number(_) => Some(Tool::Number),
        K::Highlight(_) => Some(Tool::Highlight),
        K::Blur(_) => Some(Tool::Blur),
        K::Image(_) => None,
    }
}

/// One group of controls in a properties panel, and one row group in the
/// Annotation menu (SPEC 8.4).
///
/// Which groups an annotation takes is a fact about its *kind*, not about which
/// tool happens to be active, which is why the panel asks the selection and not
/// the toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleNode {
    Stroke,
    ArrowEnds,
    Curved,
    Fill,
    CornerRadius,
    Shadow,
    /// Text: family, size, bold, italic, colour, background (SPEC 8.4).
    Font,
    TextColour,
    TextBackground,
    /// Text: the box outline, SPEC 8.3's "Text Outline" (NEXT_STEPS step 8).
    TextOutline,
    /// Number: the fill colour and the badge radius.
    NumberFill,
    NumberSize,
    /// Number: the badge's own value, which only makes sense on a selected
    /// badge, not as a tool default (SPEC 7.6).
    NumberValue,
    /// Highlight: the multiply colour.
    HighlightColour,
    /// Blur: the blur radius, written as Light / Medium / Heavy.
    BlurStrength,
}

impl StyleNode {
    /// The groups a kind of annotation takes (SPEC 8.4).
    #[must_use]
    pub fn of_kind(kind: &snp_ligase_core::model::Kind) -> Vec<Self> {
        use snp_ligase_core::model::Kind;
        match kind {
            Kind::Path(_) => vec![Self::Stroke, Self::ArrowEnds, Self::Curved, Self::Shadow],
            Kind::Rect(_) => vec![Self::Stroke, Self::Fill, Self::CornerRadius, Self::Shadow],
            Kind::Polygon(_) => vec![Self::Stroke, Self::Fill, Self::Shadow],
            Kind::Text(_) => vec![
                Self::Font,
                Self::TextColour,
                Self::TextBackground,
                Self::TextOutline,
                Self::Shadow,
            ],
            Kind::Number(_) => vec![
                Self::NumberValue,
                Self::NumberFill,
                Self::NumberSize,
                Self::Shadow,
            ],
            Kind::Highlight(_) => vec![Self::HighlightColour],
            // SPEC 4.2: a shadow under an opaque effect would never be seen.
            Kind::Blur(_) => vec![Self::BlurStrength],
            // A pasted image takes a shadow and nothing else; resizing is out
            // of scope (NEXT_STEPS step 11).
            Kind::Image(_) => vec![Self::Shadow],
        }
    }

    /// The groups a single tool takes, for the panel with nothing selected.
    #[must_use]
    pub fn of_tool(tool: Tool) -> Vec<Self> {
        match tool {
            Tool::Line | Tool::Arrow => {
                vec![Self::Stroke, Self::ArrowEnds, Self::Curved, Self::Shadow]
            }
            Tool::Rectangle => vec![Self::Stroke, Self::Fill, Self::CornerRadius, Self::Shadow],
            Tool::Shape => vec![Self::Stroke, Self::Fill, Self::Shadow],
            Tool::Text => vec![
                Self::Font,
                Self::TextColour,
                Self::TextBackground,
                Self::TextOutline,
                Self::Shadow,
            ],
            Tool::Number => vec![Self::NumberFill, Self::NumberSize, Self::Shadow],
            Tool::Highlight => vec![Self::HighlightColour],
            Tool::Blur => vec![Self::BlurStrength],
            // Select has no style; Crop uses the rectangle it is dragging.
            Tool::Select | Tool::Crop => Vec::new(),
        }
    }

    /// The groups a set of annotations have in common (SPEC 8.4: with several
    /// selected, show the controls common to all of them).
    #[must_use]
    pub fn common_to(kinds: &[&snp_ligase_core::model::Kind]) -> Vec<Self> {
        let Some((first, rest)) = kinds.split_first() else {
            return Vec::new();
        };
        let mut common = Self::of_kind(first);
        for kind in rest {
            common.retain(|node| Self::of_kind(kind).contains(node));
        }
        common
    }
}

/// The style one tool starts with, and remembers (SPEC section 7.7).
///
/// One struct covers every tool: each tool only edits the fields its own
/// controls name (SPEC 8.4), so a text style can carry an unused stroke colour
/// without ever showing it. It is serialized into the config file (SPEC
/// section 10's "per-tool style defaults"), so a field that is missing from an
/// old file falls back to the SPEC default.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ToolStyle {
    pub stroke_color: Rgba,
    pub stroke_width: f32,
    /// `None` is "Fill: None".
    pub fill: Option<Rgba>,
    pub corner_radius: f32,
    pub shadow: bool,
    /// Where an arrow's heads go (SPEC 8.3 "Arrow Ends").
    pub arrow_ends: ArrowEnds,
    /// Curved or straight segments (SPEC 8.3 "Line Style").
    pub curved: bool,
    /// Text: the glyph colour, the box background and the font.
    pub text_color: Rgba,
    pub text_background: Option<Rgba>,
    /// Text: the box outline, `None` for "None" (NEXT_STEPS step 8). Solid
    /// starts from SPEC 7.7's stroke, `#E81123` at 4 px.
    pub text_outline: Option<Stroke>,
    pub font_family: String,
    pub font_size: f32,
    pub bold: bool,
    pub italic: bool,
    pub padding: f32,
    /// Number: the badge fill and radius.
    pub number_fill: Rgba,
    pub number_radius: f32,
    /// Number: the badge's own value. Only meaningful on a selected badge; the
    /// *next* number lives in the document (SPEC 7.6).
    pub number_value: u32,
    /// Highlight: the multiply colour (SPEC 7.7: `#FFFF00`).
    pub highlight_color: Rgba,
    /// Blur: the radius in pixels (SPEC 7.7: 28, "Heavy").
    pub blur_radius: f32,
}

impl Default for ToolStyle {
    /// SPEC 7.7's initial defaults: stroke `#E81123` width 4, no fill, curved,
    /// no shadow; text white on `#0000FF`, Segoe UI 28 px bold, padding 6;
    /// number fill `#E81123`, radius 16; highlight `#FFFF00`; blur 28.
    fn default() -> Self {
        Self {
            stroke_color: defaults::STROKE_COLOR,
            stroke_width: defaults::STROKE_WIDTH,
            fill: None,
            corner_radius: 0.0,
            shadow: defaults::SHADOW,
            arrow_ends: ArrowEnds::End,
            curved: true,
            text_color: defaults::TEXT_COLOR,
            text_background: Some(defaults::TEXT_BACKGROUND),
            text_outline: None,
            font_family: snp_ligase_core::text::DEFAULT_FAMILY.to_owned(),
            font_size: defaults::TEXT_SIZE,
            bold: defaults::TEXT_BOLD,
            italic: false,
            padding: defaults::TEXT_PADDING,
            number_fill: defaults::NUMBER_FILL,
            number_radius: defaults::NUMBER_RADIUS,
            number_value: 1,
            highlight_color: defaults::HIGHLIGHT_COLOR,
            blur_radius: defaults::BLUR_RADIUS,
        }
    }
}

/// The style of every tool, so each tool remembers its own (SPEC 7.7).
///
/// Persisted in the config file by [`crate::settings`] (SPEC section 10);
/// [`crate::tool::ToolStyles::default`] is the SPEC 7.7 initial style for every
/// tool, which is what a missing field falls back to.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ToolStyles {
    line: ToolStyle,
    arrow: ToolStyle,
    rectangle: ToolStyle,
    shape: ToolStyle,
    text: ToolStyle,
    number: ToolStyle,
    highlight: ToolStyle,
    blur: ToolStyle,
}

impl ToolStyles {
    /// The style a drawing tool uses.
    #[must_use]
    pub fn get(&self, tool: Tool) -> Option<&ToolStyle> {
        match tool {
            Tool::Line => Some(&self.line),
            Tool::Arrow => Some(&self.arrow),
            Tool::Rectangle => Some(&self.rectangle),
            Tool::Shape => Some(&self.shape),
            Tool::Text => Some(&self.text),
            Tool::Number => Some(&self.number),
            Tool::Highlight => Some(&self.highlight),
            Tool::Blur => Some(&self.blur),
            Tool::Select | Tool::Crop => None,
        }
    }

    /// The style a drawing tool uses, mutably.
    pub fn get_mut(&mut self, tool: Tool) -> Option<&mut ToolStyle> {
        match tool {
            Tool::Line => Some(&mut self.line),
            Tool::Arrow => Some(&mut self.arrow),
            Tool::Rectangle => Some(&mut self.rectangle),
            Tool::Shape => Some(&mut self.shape),
            Tool::Text => Some(&mut self.text),
            Tool::Number => Some(&mut self.number),
            Tool::Highlight => Some(&mut self.highlight),
            Tool::Blur => Some(&mut self.blur),
            Tool::Select | Tool::Crop => None,
        }
    }
}

/// Where an arrow's heads go (SPEC section 8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ArrowEnds {
    None,
    #[default]
    End,
    Both,
    Start,
}

impl ArrowEnds {
    /// Every choice, in menu order.
    pub const ALL: [Self; 4] = [Self::None, Self::End, Self::Both, Self::Start];

    /// The words the menu and the properties panel use. SPEC 8.1 rule 3: the
    /// enumerated options are written out, not abbreviated to icons.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::End => "Arrow at End",
            Self::Both => "Arrows at Both Ends",
            Self::Start => "Arrow at Start",
        }
    }

    /// What this choice means for a path's two head flags.
    #[must_use]
    pub fn flags(self) -> (bool, bool) {
        match self {
            Self::None => (false, false),
            Self::End => (false, true),
            Self::Both => (true, true),
            Self::Start => (true, false),
        }
    }

    /// The choice a path's two head flags mean.
    #[must_use]
    pub fn from_flags(head_start: bool, head_end: bool) -> Self {
        match (head_start, head_end) {
            (false, false) => Self::None,
            (false, true) => Self::End,
            (true, true) => Self::Both,
            (true, false) => Self::Start,
        }
    }
}

/// The stroke widths the menu and the panel offer, before "Custom…" (SPEC 8.3:
/// 1 / 2 / 3 / 4 / 6 / 8 / 12 px).
pub const STROKE_WIDTHS: [f32; 7] = [1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0];

/// The corner radii the menu and the panel offer, before "Custom…" (SPEC 8.3:
/// 0 / 4 / 8 / 16 px).
pub const CORNER_RADII: [f32; 4] = [0.0, 4.0, 8.0, 16.0];

/// The text sizes the menu and the panel offer, before "Custom…" (SPEC 8.3's
/// "Text Size ▸"; the SPEC leaves the entries open, so these sit around the
/// 28 px default).
pub const FONT_SIZES: [f32; 8] = [12.0, 16.0, 20.0, 24.0, 28.0, 36.0, 48.0, 72.0];

/// The number badge radii the menu and the panel offer, before "Custom…".
pub const NUMBER_RADII: [f32; 5] = [8.0, 12.0, 16.0, 24.0, 32.0];

/// Blur strengths (SPEC 8.3: Light 6 / Medium 14 / Heavy 28).
pub const BLUR_STRENGTHS: [(&str, f32); 3] = [("Light", 6.0), ("Medium", 14.0), ("Heavy", 28.0)];

#[cfg(test)]
mod tests {
    use super::*;
    use snp_ligase_core::geom::{Point, Rect};
    use snp_ligase_core::model::{HighlightA, PathA, RectA, defaults};

    #[test]
    fn arrow_ends_round_trip_through_flags() {
        for ends in ArrowEnds::ALL {
            let (start, end) = ends.flags();
            assert_eq!(ArrowEnds::from_flags(start, end), ends);
        }
    }

    #[test]
    fn every_tool_has_a_name_and_a_one_letter_key() {
        for (index, (name, key, _)) in TOOL_MENU.iter().enumerate() {
            let Some(tool) = tool_for_row(index) else {
                continue;
            };
            assert!(!tool.name().is_empty(), "{tool:?} has no name");
            assert_eq!(tool.key().len(), 1, "{tool:?} key is not one letter");
            assert_eq!(tool.name(), *name, "{tool:?} name");
            assert_eq!(tool.key(), *key, "{tool:?} key");
        }
    }

    #[test]
    fn each_tool_keeps_its_own_style() {
        let mut styles = ToolStyles::default();
        if let Some(arrow) = styles.get_mut(Tool::Arrow) {
            arrow.stroke_width = 8.0;
        }
        if let Some(line) = styles.get(Tool::Line) {
            assert!(
                (line.stroke_width - 4.0).abs() < 0.001,
                "changing Arrow changed Line too"
            );
        }
    }

    #[test]
    fn the_defaults_are_the_spec_defaults() {
        let style = ToolStyle::default();
        assert_eq!(style.stroke_color, [232, 17, 35, 255], "SPEC 7.7: #E81123");
        assert!(
            (style.stroke_width - 4.0).abs() < 0.001,
            "SPEC 7.7: width 4"
        );
        assert_eq!(style.fill, None, "SPEC 7.7: rectangle no fill");
        assert!(style.curved, "SPEC 7.3: new paths are curved");
        assert!(!style.shadow, "SPEC 7.7: shadow off");
    }

    #[test]
    fn the_tools_menu_lists_all_ten_rows_with_a_key() {
        assert_eq!(TOOL_MENU.len(), 10);
        assert_eq!(Tool::ALL.len(), TOOL_MENU.len());
        for (index, (name, key, _built)) in TOOL_MENU.iter().enumerate() {
            assert!(!name.is_empty());
            assert_eq!(key.len(), 1);
            // Every row maps to a real tool variant with that name and key.
            let tool = tool_for_row(index).expect("every row has a tool");
            assert_eq!(tool.name(), *name, "row {index}");
            assert_eq!(tool.key(), *key, "row {index}");
        }
    }

    #[test]
    fn a_kind_belongs_to_the_tool_that_owns_its_style() {
        // R07: the properties panel must write the defaults of the annotation's
        // *own* tool, not of whatever tool happens to be active.
        fn path(heads: bool) -> snp_ligase_core::model::Kind {
            snp_ligase_core::model::Kind::Path(PathA {
                points: vec![Point::new(0.0, 0.0), Point::new(10.0, 10.0)],
                curved: true,
                stroke: defaults::stroke(),
                head_start: heads,
                head_end: heads,
            })
        }

        assert_eq!(tool_for_kind(&path(true)), Some(Tool::Arrow));
        assert_eq!(tool_for_kind(&path(false)), Some(Tool::Line));
        assert_eq!(
            tool_for_kind(&snp_ligase_core::model::Kind::Rect(RectA {
                rect: Rect::new(0.0, 0.0, 1.0, 1.0),
                stroke: None,
                fill: None,
                corner_radius: 0.0,
            })),
            Some(Tool::Rectangle)
        );
        // Every M4 kind belongs to its own tool.
        assert_eq!(
            tool_for_kind(&snp_ligase_core::model::Kind::Highlight(HighlightA {
                rect: Rect::new(0.0, 0.0, 1.0, 1.0),
                color: defaults::HIGHLIGHT_COLOR,
            })),
            Some(Tool::Highlight)
        );
        assert_eq!(
            tool_for_kind(&snp_ligase_core::model::Kind::Blur(
                snp_ligase_core::model::BlurA {
                    rect: Rect::new(0.0, 0.0, 1.0, 1.0),
                    radius: 14.0,
                }
            )),
            Some(Tool::Blur)
        );
        assert_eq!(
            tool_for_kind(&snp_ligase_core::model::Kind::Text(
                snp_ligase_core::model::TextA {
                    pos: Point::new(0.0, 0.0),
                    text: "x".to_owned(),
                    font_family: "Inter".to_owned(),
                    size: 28.0,
                    bold: false,
                    italic: false,
                    color: [0, 0, 0, 255],
                    background: None,
                    padding: 6.0,
                    outline: None,
                }
            )),
            Some(Tool::Text)
        );
        assert_eq!(
            tool_for_kind(&snp_ligase_core::model::Kind::Number(
                snp_ligase_core::model::NumberA {
                    center: Point::new(0.0, 0.0),
                    value: 1,
                    radius: 16.0,
                    fill: defaults::NUMBER_FILL,
                    pointer_tip: None,
                }
            )),
            Some(Tool::Number)
        );
        assert_eq!(
            tool_for_kind(&snp_ligase_core::model::Kind::Polygon(
                snp_ligase_core::model::PolygonA {
                    points: vec![
                        Point::new(0.0, 0.0),
                        Point::new(1.0, 0.0),
                        Point::new(1.0, 1.0)
                    ],
                    stroke: None,
                    fill: None,
                }
            )),
            Some(Tool::Shape)
        );

        // A pasted image has no tool of its own; its one property is Shadow
        // (NEXT_STEPS step 11).
        let image = snp_ligase_core::model::Kind::Image(snp_ligase_core::model::ImageA {
            pos: Point::new(0.0, 0.0),
            image: std::sync::Arc::new(image::RgbaImage::new(2, 2)),
        });
        assert_eq!(tool_for_kind(&image), None);
        assert_eq!(StyleNode::of_kind(&image), vec![StyleNode::Shadow]);
    }

    #[test]
    fn every_tool_has_its_own_remembered_style() {
        // SPEC 7.7: each tool remembers the style last used with it, and no
        // tool's style leaks into another's.
        let mut styles = ToolStyles::default();
        for (index, tool) in Tool::ALL.iter().enumerate() {
            let Some(style) = styles.get_mut(*tool) else {
                // Select and Crop have no style of their own.
                assert!(matches!(tool, Tool::Select | Tool::Crop));
                continue;
            };
            style.stroke_width = index as f32 + 1.0;
        }
        for (index, tool) in Tool::ALL.iter().enumerate() {
            let Some(style) = styles.get(*tool) else {
                continue;
            };
            assert!(
                (style.stroke_width - (index as f32 + 1.0)).abs() < 0.001,
                "{tool:?} did not keep its own style"
            );
        }
    }

    #[test]
    fn the_m4_tools_start_from_the_spec_defaults() {
        // SPEC 7.7's initial defaults for the kinds M4 brings.
        let styles = ToolStyles::default();
        let text = styles.get(Tool::Text).expect("a text style");
        assert_eq!(text.text_color, [255, 255, 255, 255]);
        assert_eq!(text.text_background, Some([0, 0, 255, 255]));
        assert_eq!(text.font_family, "Segoe UI");
        assert!((text.font_size - 28.0).abs() < 0.001);
        assert!(text.bold);
        assert!((text.padding - 6.0).abs() < 0.001);

        let number = styles.get(Tool::Number).expect("a number style");
        assert_eq!(number.number_fill, [232, 17, 35, 255]);
        assert!((number.number_radius - 16.0).abs() < 0.001);

        let highlight = styles.get(Tool::Highlight).expect("a highlight style");
        assert_eq!(highlight.highlight_color, [255, 255, 0, 255]);

        let blur = styles.get(Tool::Blur).expect("a blur style");
        assert!((blur.blur_radius - 28.0).abs() < 0.001);
    }

    #[test]
    fn the_menu_lists_the_spec_widths_and_radii() {
        assert_eq!(STROKE_WIDTHS, [1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0]);
        assert_eq!(CORNER_RADII, [0.0, 4.0, 8.0, 16.0]);
    }
}
