//! The properties panel (SPEC section 8.4).
//!
//! Every control has a visible text label (SPEC 8.1, the rule that matters most
//! to the user). The panel shows the selected annotation's own properties, or
//! the active tool's style when nothing is selected. Changing a property with an
//! annotation selected changes that annotation **and** the style for its tool
//! (SPEC 7.7).
//!
//! Three rules keep this honest, all of them learned the hard way:
//!
//! - The controls shown are chosen by the **selected annotation's kind**, not by
//!   which tool is active. A selected Arrow shows "Arrow ends" whether the
//!   active tool is Rectangle or Select (SPEC 8.4).
//! - Defaults are written to the tool that *owns the annotation*, so changing a
//!   Line's stroke width no longer rewrites the Arrow tool's defaults.
//! - A change is one undo step, and nothing is written when nothing changed:
//!   the edited style is compared against the one it was read from, and the
//!   snapshot is staged for the whole interaction rather than pushed per frame
//!   (SPEC 7.3).

use egui::Ui;
use snp_ligase_core::model::{Kind, Rgba, Stroke};

use crate::panels::CustomField;
use crate::panels::StyleProperty;
use crate::panels::colour;
use crate::tab::Tab;
use crate::tool::{
    ArrowEnds, BLUR_STRENGTHS, CORNER_RADII, FONT_SIZES, NUMBER_RADII, STROKE_WIDTHS, StyleNode,
    Tool, ToolStyle, ToolStyles, tool_for_kind,
};

/// What the properties panel needs for one frame.
///
/// A struct rather than six arguments: the app's fields are disjoint borrows
/// (the tab, the styles, the recent colours), and naming them keeps the call
/// site readable.
pub struct PanelContext<'a> {
    /// The active tab, or `None` when no image is open.
    pub tab: Option<&'a mut Tab>,
    /// The active tool, for the "tool defaults" heading and controls.
    pub tool: Tool,
    /// Each tool's remembered style (SPEC 7.7).
    pub styles: &'a mut ToolStyles,
    /// The last colours used (SPEC 8.4).
    pub recent: &'a mut Vec<Rgba>,
    /// The installed font families, for the font combo box (SPEC 7.5).
    pub families: &'a [String],
    /// True while a text editing session is open on this tab: the change is
    /// then part of the session's one undo step, so no snapshot is staged here
    /// (M7).
    pub fold_edits: bool,
}

/// Draws the properties panel.
pub fn show(ui: &mut Ui, context: PanelContext<'_>, actions: &mut Vec<crate::panels::Action>) {
    let PanelContext {
        tab,
        tool,
        styles,
        recent,
        families,
        fold_edits,
    } = context;
    ui.heading(heading(tab.as_deref(), tool));
    ui.separator();

    let Some(tab) = tab else {
        ui.label("Open or capture an image to edit it.");
        return;
    };

    if tab.selection().is_empty() {
        // Nothing selected: the panel edits the active tool's defaults.
        let Some(style) = styles.get_mut(tool) else {
            ui.label("The Select tool has no settings of its own.");
            ui.add_space(4.0);
            ui.label("Pick a tool, or select an annotation to change what it draws.");
            return;
        };
        // SPEC 7.6: with the Number tool active, the panel shows the document's
        // next number and a way to start over. Setting it back to 1 is how a
        // second "1" is placed on the same image.
        if tool == Tool::Number {
            number_counter(ui, tab);
            ui.separator();
        }
        // SPEC 7.3: the Crop tool applies with Enter or this button.
        if tool == Tool::Crop {
            crop_controls(ui, actions);
            ui.separator();
        }
        ui.label(format!("New {} annotations use these:", tool.name()));
        ui.add_space(4.0);
        style_controls(ui, StyleNode::of_tool(tool), style, recent, families);
        return;
    }

    // One set of controls for the whole selection, chosen by the kinds it holds
    // (SPEC 8.4). Editing a copy and comparing it afterwards is what stops an
    // unchanged panel from pushing an undo step every frame.
    let ids = tab.selection().to_vec();
    let Some(mut style) = style_of_selection(tab, &ids) else {
        ui.label("Nothing here has properties this milestone can edit yet.");
        return;
    };
    let before = style.clone();
    let kinds: Vec<Kind> = ids
        .iter()
        .filter_map(|id| tab.document.find(*id))
        .map(|annotation| annotation.kind.clone())
        .collect();
    let nodes = StyleNode::common_to(&kinds.iter().collect::<Vec<_>>());

    style_controls(ui, nodes, &mut style, recent, families);
    if style != before {
        // One interaction, one undo step: the snapshot is staged on the first
        // change and only pushed when the interaction ends. Inside a text
        // session the session's own snapshot is the step (M7).
        if !fold_edits {
            tab.stage_edit();
        }
        apply(tab, styles, &ids, &style);
    }

    if tab.selection().len() > 1 {
        ui.add_space(4.0);
        ui.label("A change applies to every selected annotation.");
    }
}

/// The panel's heading names what is being edited (SPEC 8.2).
fn heading(tab: Option<&Tab>, tool: Tool) -> String {
    let Some(tab) = tab else {
        return "Properties".to_owned();
    };
    match tab.selected() {
        Some(annotation) if tab.selection().len() == 1 => annotation.kind.name().to_owned(),
        Some(_) => format!("{} annotations selected", tab.selection().len()),
        None if tool == Tool::Select => "Select an annotation or pick a tool.".to_owned(),
        None => format!("{} tool defaults", tool.name()),
    }
}

/// The style shared by every selected annotation, or `None` when they do not
/// share one.
fn style_of_selection(tab: &Tab, ids: &[u64]) -> Option<ToolStyle> {
    let mut styles = Vec::new();
    for id in ids {
        let annotation = tab.document.find(*id)?;
        styles.push(style_of(annotation)?);
    }
    // The values the controls start from are the first annotation's, and a
    // change is then written to all of them.
    styles.into_iter().next()
}

/// One annotation's style.
///
/// Fields the kind does not have come from [`ToolStyle::default`], so a
/// comparison against another annotation of the same kind is still meaningful.
pub fn style_of(annotation: &snp_ligase_core::model::Annotation) -> Option<ToolStyle> {
    let stroke_default = snp_ligase_core::model::defaults::stroke();
    let fallback = ToolStyle::default();
    Some(match &annotation.kind {
        Kind::Path(path) => ToolStyle {
            stroke_color: path.stroke.color,
            stroke_width: path.stroke.width,
            fill: None,
            corner_radius: 0.0,
            shadow: annotation.shadow,
            arrow_ends: ArrowEnds::from_flags(path.head_start, path.head_end),
            curved: path.curved,
            ..fallback
        },
        Kind::Rect(rect) => ToolStyle {
            stroke_color: rect
                .stroke
                .as_ref()
                .map_or(stroke_default.color, |s| s.color),
            stroke_width: rect
                .stroke
                .as_ref()
                .map_or(stroke_default.width, |s| s.width),
            fill: rect.fill,
            corner_radius: rect.corner_radius,
            shadow: annotation.shadow,
            ..fallback
        },
        Kind::Polygon(polygon) => ToolStyle {
            stroke_color: polygon
                .stroke
                .as_ref()
                .map_or(stroke_default.color, |s| s.color),
            stroke_width: polygon
                .stroke
                .as_ref()
                .map_or(stroke_default.width, |s| s.width),
            fill: polygon.fill,
            shadow: annotation.shadow,
            ..fallback
        },
        Kind::Text(text) => ToolStyle {
            shadow: annotation.shadow,
            text_color: text.color,
            text_background: text.background,
            text_outline: text.outline,
            font_family: text.font_family.clone(),
            font_size: text.size,
            bold: text.bold,
            italic: text.italic,
            padding: text.padding,
            ..fallback
        },
        Kind::Number(number) => ToolStyle {
            shadow: annotation.shadow,
            number_fill: number.fill,
            number_radius: number.radius,
            number_value: number.value,
            ..fallback
        },
        Kind::Highlight(highlight) => ToolStyle {
            highlight_color: highlight.color,
            ..fallback
        },
        Kind::Blur(blur) => ToolStyle {
            blur_radius: blur.radius,
            ..fallback
        },
        // A pasted image takes a shadow and nothing else (NEXT_STEPS step 11).
        Kind::Image(_) => ToolStyle {
            shadow: annotation.shadow,
            ..fallback
        },
    })
}

/// Writes an edited style into every selected annotation, and into the style of
/// every tool that owns one of them (SPEC 7.7).
pub fn apply(tab: &mut Tab, styles: &mut ToolStyles, ids: &[u64], style: &ToolStyle) {
    let properties = difference(&style_shared_default(ids, tab), style);
    apply_properties(tab, styles, ids, &properties);
}

/// The changes that turn `before` into `after`, as menu-style properties, so the
/// panel and the Annotation menu share one write path.
fn difference(before: &ToolStyle, after: &ToolStyle) -> Vec<StyleProperty> {
    let mut changes = Vec::new();
    if before.stroke_color != after.stroke_color {
        changes.push(StyleProperty::StrokeColour(after.stroke_color));
    }
    if before.stroke_width != after.stroke_width {
        changes.push(StyleProperty::StrokeWidth(after.stroke_width));
    }
    if before.fill != after.fill {
        changes.push(StyleProperty::Fill(after.fill));
    }
    if before.corner_radius != after.corner_radius {
        changes.push(StyleProperty::CornerRadius(after.corner_radius));
    }
    if before.shadow != after.shadow {
        changes.push(StyleProperty::Shadow(after.shadow));
    }
    if before.arrow_ends != after.arrow_ends {
        changes.push(StyleProperty::ArrowEnds(after.arrow_ends));
    }
    if before.curved != after.curved {
        changes.push(StyleProperty::Curved(after.curved));
    }
    if before.text_color != after.text_color {
        changes.push(StyleProperty::TextColour(after.text_color));
    }
    if before.text_background != after.text_background {
        changes.push(StyleProperty::TextBackground(after.text_background));
    }
    if before.text_outline != after.text_outline {
        changes.push(StyleProperty::TextOutline(after.text_outline));
    }
    if before.font_family != after.font_family {
        changes.push(StyleProperty::FontFamily(after.font_family.clone()));
    }
    if before.font_size != after.font_size {
        changes.push(StyleProperty::FontSize(after.font_size));
    }
    if before.bold != after.bold {
        changes.push(StyleProperty::Bold(after.bold));
    }
    if before.italic != after.italic {
        changes.push(StyleProperty::Italic(after.italic));
    }
    if before.number_fill != after.number_fill {
        changes.push(StyleProperty::NumberFill(after.number_fill));
    }
    if before.number_radius != after.number_radius {
        changes.push(StyleProperty::NumberSize(after.number_radius));
    }
    if before.number_value != after.number_value {
        changes.push(StyleProperty::NumberValue(after.number_value));
    }
    if before.highlight_color != after.highlight_color {
        changes.push(StyleProperty::HighlightColour(after.highlight_color));
    }
    if before.blur_radius != after.blur_radius {
        changes.push(StyleProperty::BlurRadius(after.blur_radius));
    }
    changes
}

/// The style the controls started from, for a whole selection.
fn style_shared_default(ids: &[u64], tab: &Tab) -> ToolStyle {
    style_of_selection(tab, ids).unwrap_or_default()
}

/// The controls for every kind (SPEC 8.4): stroke colour and width, arrow ends,
/// line style, fill, corner radius, font, text colour and background, number
/// fill and size, highlight colour, blur strength, and shadow.
fn style_controls(
    ui: &mut Ui,
    nodes: Vec<StyleNode>,
    style: &mut ToolStyle,
    recent: &mut Vec<Rgba>,
    families: &[String],
) {
    if nodes.contains(&StyleNode::Stroke) {
        stroke_controls(ui, &mut style.stroke_color, &mut style.stroke_width, recent);
    }
    if nodes.contains(&StyleNode::ArrowEnds) {
        arrow_ends(ui, &mut style.arrow_ends);
    }
    if nodes.contains(&StyleNode::Curved) {
        line_style(ui, &mut style.curved);
    }
    if nodes.contains(&StyleNode::Fill) {
        fill_controls(ui, &mut style.fill, recent);
    }
    if nodes.contains(&StyleNode::CornerRadius) {
        corner_radius(ui, &mut style.corner_radius);
    }
    if nodes.contains(&StyleNode::Font) {
        font_controls(
            ui,
            &mut style.font_family,
            &mut style.font_size,
            &mut style.bold,
            &mut style.italic,
            families,
        );
    }
    if nodes.contains(&StyleNode::TextColour) {
        colour::button(ui, "Text colour", &mut style.text_color, recent);
    }
    if nodes.contains(&StyleNode::TextBackground) {
        text_background(ui, &mut style.text_background, recent);
    }
    if nodes.contains(&StyleNode::TextOutline) {
        text_outline_controls(ui, &mut style.text_outline, recent);
    }
    if nodes.contains(&StyleNode::NumberValue) {
        ui.horizontal(|ui| {
            ui.label("Value");
            ui.add(egui::DragValue::new(&mut style.number_value).range(0..=9999));
        });
    }
    if nodes.contains(&StyleNode::NumberFill) {
        colour::button(ui, "Colour", &mut style.number_fill, recent);
    }
    if nodes.contains(&StyleNode::NumberSize) {
        number_size(ui, &mut style.number_radius);
    }
    if nodes.contains(&StyleNode::HighlightColour) {
        colour::button(ui, "Colour", &mut style.highlight_color, recent);
    }
    if nodes.contains(&StyleNode::BlurStrength) {
        blur_strength(ui, &mut style.blur_radius);
    }
    if nodes.contains(&StyleNode::Shadow) {
        ui.checkbox(&mut style.shadow, "Shadow");
    }
}

/// The document's next number, and the way back to 1 (SPEC 7.6).
///
/// Both writes go through the tab's staging, so a whole drag on the field is
/// one undo step and the counter is part of the document's history.
fn number_counter(ui: &mut Ui, tab: &mut Tab) {
    ui.label("Next number");
    ui.horizontal(|ui| {
        let mut value = tab.document.next_number;
        let original = value;
        ui.add(egui::DragValue::new(&mut value).range(1..=9999));
        if value != original {
            tab.stage_edit();
            tab.document.next_number = value;
        }
        if ui
            .button("Reset to 1")
            .on_hover_text("Place the next badge as number 1 again")
            .clicked()
            && tab.document.next_number != 1
        {
            tab.stage_edit();
            tab.document.next_number = 1;
        }
    });
}

/// The Crop tool's controls: a hint and the button that applies the rectangle
/// (SPEC 7.3). The rectangle itself is drawn on the canvas.
fn crop_controls(ui: &mut Ui, actions: &mut Vec<crate::panels::Action>) {
    ui.label("Crop");
    ui.label("Drag a rectangle on the image, then adjust its handles.");
    ui.label("Enter applies the crop; Esc cancels it.");
    if ui
        .button("Apply crop")
        .on_hover_text("Crop the image to the rectangle (Enter)")
        .clicked()
    {
        actions.push(crate::panels::Action::ApplyCrop);
    }
}

/// Stroke colour and width (SPEC 8.4).
fn stroke_controls(ui: &mut Ui, colour: &mut Rgba, width: &mut f32, recent: &mut Vec<Rgba>) {
    colour::button(ui, "Stroke colour", colour, recent);
    ui.horizontal(|ui| {
        ui.label("Stroke width");
        // `from_id_salt`, not `from_label`: the label is already drawn above,
        // and `from_label` would draw it a second time beside the box (R07).
        egui::ComboBox::from_id_salt("stroke_width")
            .selected_text(format!("{width} px"))
            .show_ui(ui, |ui| {
                for candidate in STROKE_WIDTHS {
                    ui.selectable_value(width, candidate, format!("{candidate} px"));
                }
            });
        custom_value(ui, "Stroke width", width, 0.1..=200.0);
    });
}

/// Fill: None or Solid, with its colour (SPEC 8.4, written as words per SPEC
/// 8.1 rule 3).
fn fill_controls(ui: &mut Ui, fill: &mut Option<Rgba>, recent: &mut Vec<Rgba>) {
    ui.horizontal(|ui| {
        ui.label("Fill");
        let chosen = fill.is_some();
        let mut want_solid = chosen;
        if ui
            .selectable_label(chosen, "Solid")
            .on_hover_text("Fill the shape with a colour")
            .clicked()
        {
            want_solid = true;
        }
        if ui
            .selectable_label(!chosen, "None")
            .on_hover_text("No fill, only the outline")
            .clicked()
        {
            want_solid = false;
        }
        if want_solid != chosen {
            *fill = want_solid.then_some([255, 255, 255, 255]);
        }
    });
    if let Some(current) = fill.as_mut() {
        colour::button(ui, "Fill colour", current, recent);
    }
}

/// Corner radius (SPEC 8.4).
fn corner_radius(ui: &mut Ui, radius: &mut f32) {
    ui.horizontal(|ui| {
        ui.label("Corner radius");
        egui::ComboBox::from_id_salt("corner_radius")
            .selected_text(format!("{radius} px"))
            .show_ui(ui, |ui| {
                for candidate in CORNER_RADII {
                    ui.selectable_value(radius, candidate, format!("{candidate} px"));
                }
            });
        custom_value(ui, "Corner radius", radius, 0.0..=500.0);
    });
}

/// A "Custom…" button with a small numeric field, for values the list does not
/// offer (SPEC 8.3). The field writes into the style copy the panel is already
/// editing, so it goes through the same one-change-one-undo-step path as every
/// other control.
///
/// `CloseOnClickOutside` keeps the field usable: the `menu` default closed the
/// popup on the click that focuses the `DragValue`, so a typed value was
/// impossible (C1).
fn custom_value(ui: &mut Ui, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>) {
    let response = ui
        .button("Custom…")
        .on_hover_text(format!("Type any {label}"));
    egui::Popup::menu(&response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(200.0);
            ui.label(format!("{label} in image pixels"));
            ui.add(
                egui::DragValue::new(value)
                    .range(range)
                    .speed(0.25)
                    .suffix(" px"),
            );
        });
}

/// Where arrowheads go, in words (SPEC 8.1 rule 3).
fn arrow_ends(ui: &mut Ui, value: &mut ArrowEnds) {
    ui.horizontal(|ui| {
        ui.label("Arrow ends");
        egui::ComboBox::from_id_salt("arrow_ends")
            .selected_text(value.label())
            .show_ui(ui, |ui| {
                for candidate in ArrowEnds::ALL {
                    ui.selectable_value(value, candidate, candidate.label());
                }
            });
    });
}

/// The font controls (SPEC 8.4): family, size, bold and italic.
///
/// The family list is the installed families from the font system, in a
/// searchable combo box (SPEC 7.5): a filter field above the list, so a long
/// list of installed fonts is still usable.
fn font_controls(
    ui: &mut Ui,
    family: &mut String,
    size: &mut f32,
    bold: &mut bool,
    italic: &mut bool,
    families: &[String],
) {
    ui.horizontal(|ui| {
        ui.label("Font");
        let mut filter = ui.ctx().data_mut(|data| {
            data.get_temp::<String>(egui::Id::new("font_filter"))
                .unwrap_or_default()
        });
        egui::ComboBox::from_id_salt("font_family")
            .selected_text(family.clone())
            .width(160.0)
            .show_ui(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut filter)
                        .hint_text("Search fonts")
                        .desired_width(150.0),
                );
                let needle = filter.to_lowercase();
                let mut shown = 0usize;
                for candidate in families {
                    if !needle.is_empty() && !candidate.to_lowercase().contains(&needle) {
                        continue;
                    }
                    shown += 1;
                    if ui
                        .selectable_label(candidate == family, candidate)
                        .clicked()
                    {
                        *family = candidate.clone();
                    }
                }
                if shown == 0 {
                    ui.label("No font matches.");
                }
            });
        ui.ctx()
            .data_mut(|data| data.insert_temp(egui::Id::new("font_filter"), filter));
    });

    ui.horizontal(|ui| {
        ui.label("Text size");
        egui::ComboBox::from_id_salt("font_size")
            .selected_text(format!("{size} px"))
            .show_ui(ui, |ui| {
                for candidate in FONT_SIZES {
                    ui.selectable_value(size, candidate, format!("{candidate} px"));
                }
            });
        custom_value(ui, "Text size", size, CustomField::FontSize.range());
    });

    ui.horizontal(|ui| {
        ui.checkbox(bold, "Bold");
        ui.checkbox(italic, "Italic");
    });
}

/// Text background: None or Solid, with its colour (SPEC 8.4).
///
/// The "None" label is only highlighted when there is no background — the
/// `chosen` flag was passed to both buttons, which highlighted "None" while a
/// background existed (NEXT_STEPS step 8's fix while there).
fn text_background(ui: &mut Ui, background: &mut Option<Rgba>, recent: &mut Vec<Rgba>) {
    ui.horizontal(|ui| {
        ui.label("Background");
        let chosen = background.is_some();
        let mut want_solid = chosen;
        if ui
            .selectable_label(chosen, "Solid")
            .on_hover_text("Draw a box behind the text")
            .clicked()
        {
            want_solid = true;
        }
        if ui
            .selectable_label(!chosen, "None")
            .on_hover_text("Text with no background box")
            .clicked()
        {
            want_solid = false;
        }
        if want_solid != chosen {
            *background = want_solid.then_some(snp_ligase_core::model::defaults::TEXT_BACKGROUND);
        }
    });
    if let Some(current) = background.as_mut() {
        colour::button(ui, "Background colour", current, recent);
    }
}

/// Text outline: None or Solid, then its colour and width (SPEC 8.4,
/// NEXT_STEPS step 8). Same shape as the rectangle's stroke controls.
fn text_outline_controls(ui: &mut Ui, outline: &mut Option<Stroke>, recent: &mut Vec<Rgba>) {
    ui.horizontal(|ui| {
        ui.label("Outline");
        let chosen = outline.is_some();
        let mut want_solid = chosen;
        if ui
            .selectable_label(chosen, "Solid")
            .on_hover_text("Draw an outline around the text box")
            .clicked()
        {
            want_solid = true;
        }
        if ui
            .selectable_label(!chosen, "None")
            .on_hover_text("Text with no outline")
            .clicked()
        {
            want_solid = false;
        }
        if want_solid != chosen {
            *outline = want_solid.then(snp_ligase_core::model::defaults::stroke);
        }
    });
    if let Some(current) = outline.as_mut() {
        colour::button(ui, "Outline colour", &mut current.color, recent);
        ui.horizontal(|ui| {
            ui.label("Outline width");
            egui::ComboBox::from_id_salt("text_outline_width")
                .selected_text(format!("{} px", current.width))
                .show_ui(ui, |ui| {
                    for candidate in STROKE_WIDTHS {
                        ui.selectable_value(
                            &mut current.width,
                            candidate,
                            format!("{candidate} px"),
                        );
                    }
                });
            custom_value(ui, "Outline width", &mut current.width, 0.1..=200.0);
        });
    }
}

/// The number badge size, in the same shape as the stroke width (SPEC 8.4).
fn number_size(ui: &mut Ui, radius: &mut f32) {
    ui.horizontal(|ui| {
        ui.label("Size");
        egui::ComboBox::from_id_salt("number_size")
            .selected_text(format!("{radius} px"))
            .show_ui(ui, |ui| {
                for candidate in NUMBER_RADII {
                    ui.selectable_value(radius, candidate, format!("{candidate} px"));
                }
            });
        custom_value(ui, "Number size", radius, CustomField::NumberSize.range());
    });
}

/// Blur strength, in words (SPEC 8.3: Light 6 / Medium 14 / Heavy 28).
fn blur_strength(ui: &mut Ui, radius: &mut f32) {
    ui.horizontal(|ui| {
        ui.label("Strength");
        egui::ComboBox::from_id_salt("blur_strength")
            .selected_text(blur_strength_label(*radius))
            .show_ui(ui, |ui| {
                for (name, value) in BLUR_STRENGTHS {
                    ui.selectable_value(radius, value, format!("{name} ({value})"));
                }
            });
        custom_value(
            ui,
            "Blur strength",
            radius,
            CustomField::BlurStrength.range(),
        );
    });
}

/// The word for a blur radius: one of the three named strengths, or the number.
#[must_use]
pub fn blur_strength_label(radius: f32) -> String {
    for (name, value) in BLUR_STRENGTHS {
        if (radius - value).abs() < 0.01 {
            return format!("{name} ({value})");
        }
    }
    format!("{radius} px")
}

/// Curved or straight segments (SPEC 7.3).
fn line_style(ui: &mut Ui, curved: &mut bool) {
    ui.horizontal(|ui| {
        ui.label("Line style");
        egui::ComboBox::from_id_salt("line_style")
            .selected_text(if *curved {
                "Curved"
            } else {
                "Straight Segments"
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(curved, true, "Curved");
                ui.selectable_value(curved, false, "Straight Segments");
            });
    });
}

/// The one write path for a style change, shared by the properties panel and the
/// Annotation menu (SPEC 8.1 rule 2).
///
/// Returns the number of annotations it changed.
pub fn apply_properties(
    tab: &mut Tab,
    styles: &mut ToolStyles,
    ids: &[u64],
    properties: &[StyleProperty],
) -> usize {
    if properties.is_empty() {
        // Nothing was asked for, so nothing changed: this must not be counted
        // as an edit, or a panel that wrote nothing would still take an undo
        // step.
        return 0;
    }

    let mut changed = 0;
    let mut owned_tools: Vec<Tool> = Vec::new();

    for id in ids {
        let Some(annotation) = tab.document.find_mut(*id) else {
            continue;
        };
        // The property is written whatever the kind is. A pasted image has no
        // tool (NEXT_STEPS step 11), but its Shadow still has to land; only the
        // tool-defaults propagation needs a tool to know where to go.
        for property in properties {
            write_into(annotation, property);
        }
        if let Some(tool) = tool_for_kind(&annotation.kind)
            && !owned_tools.contains(&tool)
        {
            owned_tools.push(tool);
        }
        changed += 1;
    }

    // SPEC 7.7: the style becomes the default for the tool that owns the
    // annotation, so the next one drawn starts from it.
    if changed > 0
        && let Some(first) = style_of_selection(tab, ids)
    {
        for tool in owned_tools {
            if let Some(stored) = styles.get_mut(tool) {
                *stored = first.clone();
            }
        }
    }
    changed
}

/// Applies one property change to a style struct, which is how the tool
/// defaults are updated as well as the annotations.
pub fn write_into_style(style: &mut ToolStyle, property: &StyleProperty) {
    match property {
        StyleProperty::StrokeColour(colour) => style.stroke_color = *colour,
        StyleProperty::StrokeWidth(width) => style.stroke_width = *width,
        StyleProperty::Fill(fill) => style.fill = *fill,
        StyleProperty::CornerRadius(radius) => style.corner_radius = *radius,
        StyleProperty::Shadow(shadow) => style.shadow = *shadow,
        StyleProperty::ArrowEnds(ends) => style.arrow_ends = *ends,
        StyleProperty::Curved(curved) => style.curved = *curved,
        StyleProperty::TextColour(colour) => style.text_color = *colour,
        StyleProperty::TextBackground(background) => style.text_background = *background,
        StyleProperty::TextOutline(outline) => style.text_outline = *outline,
        StyleProperty::TextOutlineColour(colour) => {
            set_text_outline_style(style, |outline| outline.color = *colour);
        }
        StyleProperty::TextOutlineWidth(width) => {
            set_text_outline_style(style, |outline| outline.width = *width);
        }
        StyleProperty::FontFamily(family) => style.font_family = family.clone(),
        StyleProperty::FontSize(size) => style.font_size = *size,
        StyleProperty::Bold(bold) => style.bold = *bold,
        StyleProperty::Italic(italic) => style.italic = *italic,
        StyleProperty::NumberFill(colour) => style.number_fill = *colour,
        StyleProperty::NumberSize(radius) => style.number_radius = *radius,
        StyleProperty::NumberValue(value) => style.number_value = *value,
        StyleProperty::HighlightColour(colour) => style.highlight_color = *colour,
        StyleProperty::BlurRadius(radius) => style.blur_radius = *radius,
    }
}

/// Applies one property change to an annotation.
fn write_into(annotation: &mut snp_ligase_core::model::Annotation, property: &StyleProperty) {
    match &mut annotation.kind {
        Kind::Path(path) => match property {
            StyleProperty::StrokeColour(colour) => path.stroke.color = *colour,
            StyleProperty::StrokeWidth(width) => path.stroke.width = *width,
            StyleProperty::ArrowEnds(ends) => {
                let (head_start, head_end) = ends.flags();
                path.head_start = head_start;
                path.head_end = head_end;
            }
            StyleProperty::Curved(curved) => path.curved = *curved,
            StyleProperty::Shadow(shadow) => annotation.shadow = *shadow,
            _ => {}
        },
        Kind::Rect(rect) => match property {
            StyleProperty::StrokeColour(colour) => {
                set_rect_stroke(rect, |stroke| stroke.color = *colour)
            }
            StyleProperty::StrokeWidth(width) => {
                set_rect_stroke(rect, |stroke| stroke.width = *width)
            }
            StyleProperty::Fill(fill) => rect.fill = *fill,
            StyleProperty::CornerRadius(radius) => rect.corner_radius = *radius,
            StyleProperty::Shadow(shadow) => annotation.shadow = *shadow,
            _ => {}
        },
        Kind::Polygon(polygon) => match property {
            StyleProperty::StrokeColour(colour) => {
                set_polygon_stroke(polygon, |stroke| stroke.color = *colour)
            }
            StyleProperty::StrokeWidth(width) => {
                set_polygon_stroke(polygon, |stroke| stroke.width = *width)
            }
            StyleProperty::Fill(fill) => polygon.fill = *fill,
            StyleProperty::Shadow(shadow) => annotation.shadow = *shadow,
            _ => {}
        },
        Kind::Text(text) => match property {
            StyleProperty::TextColour(colour) => text.color = *colour,
            StyleProperty::TextBackground(background) => text.background = *background,
            StyleProperty::TextOutline(outline) => text.outline = *outline,
            StyleProperty::TextOutlineColour(colour) => {
                set_text_outline(text, |outline| outline.color = *colour);
            }
            StyleProperty::TextOutlineWidth(width) => {
                set_text_outline(text, |outline| outline.width = *width);
            }
            StyleProperty::FontFamily(family) => text.font_family = family.clone(),
            StyleProperty::FontSize(size) => text.size = *size,
            StyleProperty::Bold(bold) => text.bold = *bold,
            StyleProperty::Italic(italic) => text.italic = *italic,
            StyleProperty::Shadow(shadow) => annotation.shadow = *shadow,
            _ => {}
        },
        Kind::Number(number) => match property {
            StyleProperty::NumberFill(colour) => number.fill = *colour,
            StyleProperty::NumberSize(radius) => number.radius = *radius,
            StyleProperty::NumberValue(value) => number.value = *value,
            StyleProperty::Shadow(shadow) => annotation.shadow = *shadow,
            _ => {}
        },
        Kind::Highlight(highlight) => {
            if let StyleProperty::HighlightColour(colour) = property {
                highlight.color = *colour;
            }
        }
        Kind::Blur(blur) => {
            if let StyleProperty::BlurRadius(radius) = property {
                blur.radius = *radius;
            }
        }
        // A pasted image takes a shadow and nothing else (NEXT_STEPS step 11).
        Kind::Image(_) => {
            if let StyleProperty::Shadow(shadow) = property {
                annotation.shadow = *shadow;
            }
        }
    }
}

/// A rectangle without a stroke still needs one before a colour can be set.
fn set_rect_stroke(
    rect: &mut snp_ligase_core::model::RectA,
    change: impl FnOnce(&mut snp_ligase_core::model::Stroke),
) {
    let default = snp_ligase_core::model::defaults::stroke();
    let stroke = rect.stroke.get_or_insert(default);
    change(stroke);
}

/// The same for a shape.
fn set_polygon_stroke(
    polygon: &mut snp_ligase_core::model::PolygonA,
    change: impl FnOnce(&mut snp_ligase_core::model::Stroke),
) {
    let default = snp_ligase_core::model::defaults::stroke();
    let stroke = polygon.stroke.get_or_insert(default);
    change(stroke);
}

/// A text annotation without an outline still needs one before its colour or
/// width can be set (the menu rows are disabled while it is None, but the write
/// path must not depend on that).
fn set_text_outline(text: &mut snp_ligase_core::model::TextA, change: impl FnOnce(&mut Stroke)) {
    let default = snp_ligase_core::model::defaults::stroke();
    let outline = text.outline.get_or_insert(default);
    change(outline);
}

/// The same for a tool style's remembered outline (NEXT_STEPS step 8).
fn set_text_outline_style(style: &mut ToolStyle, change: impl FnOnce(&mut Stroke)) {
    let default = snp_ligase_core::model::defaults::stroke();
    let outline = style.text_outline.get_or_insert(default);
    change(outline);
}

#[cfg(test)]
mod tests {
    use super::*;
    use snp_ligase_core::geom::{Point, Rect};
    use snp_ligase_core::model::PathA;

    fn tab() -> Tab {
        Tab::new(
            snp_ligase_core::model::Document::new(image::RgbaImage::new(100, 100)),
            "test".to_owned(),
        )
    }

    fn line(tab: &mut Tab) -> u64 {
        tab.document.push(Kind::Path(PathA {
            points: vec![Point::new(0.0, 0.0), Point::new(50.0, 50.0)],
            curved: false,
            stroke: snp_ligase_core::model::defaults::stroke(),
            head_start: false,
            head_end: false,
        }))
    }

    fn rectangle(tab: &mut Tab) -> u64 {
        tab.document.push(Kind::Rect(snp_ligase_core::model::RectA {
            rect: Rect::new(0.0, 0.0, 20.0, 20.0),
            stroke: Some(snp_ligase_core::model::defaults::stroke()),
            fill: None,
            corner_radius: 0.0,
        }))
    }

    #[test]
    fn an_unchanged_style_changes_nothing() {
        let mut tab = tab();
        let id = line(&mut tab);
        tab.select_only(Some(id));
        let mut styles = ToolStyles::default();
        let before = style_of(tab.selected().expect("selected")).expect("a style");

        assert_eq!(apply_properties(&mut tab, &mut styles, &[id], &[]), 0);
        assert_eq!(
            style_of(tab.selected().expect("selected")).expect("a style"),
            before,
            "nothing was written"
        );
        assert_eq!(
            tab.undo_depth_for_test(),
            0,
            "an unchanged style took an undo step"
        );
    }

    #[test]
    fn one_change_is_written_once() {
        let mut tab = tab();
        let id = line(&mut tab);
        tab.select_only(Some(id));
        let mut styles = ToolStyles::default();

        let mut edited = style_of(tab.selected().expect("selected")).expect("a style");
        edited.stroke_width = 9.0;
        apply(&mut tab, &mut styles, &[id], &edited);

        let Kind::Path(path) = &tab.selected().expect("still there").kind else {
            panic!("expected a path");
        };
        assert!((path.stroke.width - 9.0).abs() < 0.001);
    }

    #[test]
    fn one_change_is_exactly_one_undo_step() {
        // R07: the panel used to write every frame without a snapshot, so a
        // property change was not its own undo step.
        let mut tab = tab();
        let id = line(&mut tab);
        tab.select_only(Some(id));
        let before = tab.document.clone();
        let mut styles = ToolStyles::default();

        tab.stage_edit();
        let mut edited = style_of(tab.selected().expect("selected")).expect("a style");
        edited.stroke_width = 9.0;
        apply(&mut tab, &mut styles, &[id], &edited);
        tab.commit_edit();

        assert_eq!(tab.undo_depth_for_test(), 1, "not exactly one undo step");
        assert!(tab.undo(), "there was nothing to undo");
        assert_eq!(tab.document, before, "undo did not restore the style");
    }

    #[test]
    fn a_line_change_does_not_touch_the_arrow_defaults() {
        // R07: the old code wrote the *active* tool's defaults, so drawing a
        // line and then an arrow produced an arrow with no head.
        let mut tab = tab();
        let id = line(&mut tab);
        tab.select_only(Some(id));
        let mut styles = ToolStyles::default();

        let mut edited = style_of(tab.selected().expect("selected")).expect("a style");
        edited.arrow_ends = ArrowEnds::None;
        edited.stroke_width = 12.0;
        apply(&mut tab, &mut styles, &[id], &edited);

        let line_tool = styles.get(Tool::Line).expect("a line style");
        assert!((line_tool.stroke_width - 12.0).abs() < 0.001);
        let arrow_tool = styles.get(Tool::Arrow).expect("an arrow style");
        assert!(
            (arrow_tool.stroke_width - 4.0).abs() < 0.001,
            "the Arrow defaults were rewritten by a Line"
        );
        assert_eq!(arrow_tool.arrow_ends, ArrowEnds::End, "the head went away");
    }

    #[test]
    fn an_arrow_change_updates_the_arrow_defaults() {
        let mut tab = tab();
        let id = tab.document.push(Kind::Path(PathA {
            points: vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)],
            curved: true,
            stroke: snp_ligase_core::model::defaults::stroke(),
            head_start: false,
            head_end: true,
        }));
        tab.select_only(Some(id));
        let mut styles = ToolStyles::default();

        let mut edited = style_of(tab.selected().expect("selected")).expect("a style");
        edited.stroke_color = [1, 2, 3, 255];
        apply(&mut tab, &mut styles, &[id], &edited);

        assert_eq!(
            styles
                .get(Tool::Arrow)
                .expect("an arrow style")
                .stroke_color,
            [1, 2, 3, 255],
            "the Arrow tool did not learn the new colour"
        );
    }

    #[test]
    fn a_change_applies_to_every_selected_annotation() {
        let mut tab = tab();
        let first = rectangle(&mut tab);
        let second = rectangle(&mut tab);
        tab.selection = vec![first, second];
        let mut styles = ToolStyles::default();

        assert_eq!(
            apply_properties(
                &mut tab,
                &mut styles,
                &[first, second],
                &[StyleProperty::Fill(Some([9, 9, 9, 255]))],
            ),
            2
        );

        for id in [first, second] {
            let Kind::Rect(rect) = &tab.document.find(id).expect("there").kind else {
                panic!("expected a rectangle");
            };
            assert_eq!(rect.fill, Some([9, 9, 9, 255]));
        }
    }

    #[test]
    fn the_controls_are_chosen_by_kind_not_by_the_active_tool() {
        // A selected Arrow keeps its "Arrow ends" even with Rectangle active.
        let arrow = snp_ligase_core::model::Kind::Path(PathA {
            points: vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)],
            curved: true,
            stroke: snp_ligase_core::model::defaults::stroke(),
            head_start: false,
            head_end: true,
        });
        let nodes = StyleNode::of_kind(&arrow);
        assert!(nodes.contains(&StyleNode::ArrowEnds));
        assert!(nodes.contains(&StyleNode::Stroke));
        assert!(!nodes.contains(&StyleNode::Fill));
        assert!(!nodes.contains(&StyleNode::CornerRadius));

        let rectangle = snp_ligase_core::model::Kind::Rect(snp_ligase_core::model::RectA {
            rect: Rect::new(0.0, 0.0, 5.0, 5.0),
            stroke: None,
            fill: None,
            corner_radius: 0.0,
        });
        let nodes = StyleNode::of_kind(&rectangle);
        assert!(nodes.contains(&StyleNode::CornerRadius));
        assert!(nodes.contains(&StyleNode::Fill));
        assert!(!nodes.contains(&StyleNode::ArrowEnds));
    }

    #[test]
    fn several_selections_show_only_what_they_have_in_common() {
        let arrow = snp_ligase_core::model::Kind::Path(PathA {
            points: vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)],
            curved: true,
            stroke: snp_ligase_core::model::defaults::stroke(),
            head_start: true,
            head_end: true,
        });
        let rectangle = snp_ligase_core::model::Kind::Rect(snp_ligase_core::model::RectA {
            rect: Rect::new(0.0, 0.0, 5.0, 5.0),
            stroke: None,
            fill: None,
            corner_radius: 0.0,
        });
        let common = StyleNode::common_to(&[&arrow, &rectangle]);
        assert_eq!(common, vec![StyleNode::Stroke, StyleNode::Shadow]);
        assert!(!common.contains(&StyleNode::Fill));
        assert!(!common.contains(&StyleNode::ArrowEnds));
    }

    #[test]
    fn write_into_a_style_round_trips_through_the_difference() {
        let before = ToolStyle::default();
        let after = ToolStyle {
            stroke_width: 7.0,
            shadow: true,
            ..ToolStyle::default()
        };

        let changes = difference(&before, &after);
        let mut rebuilt = ToolStyle::default();
        for change in changes {
            write_into_style(&mut rebuilt, &change);
        }
        assert_eq!(rebuilt, after);
    }

    /// NEXT_STEPS step 8's selectable-label fix: the Background control passed
    /// `chosen` to both buttons, so "None" was highlighted while a background
    /// was set. The Outline control uses the corrected pattern.
    #[test]
    fn the_background_choice_labels_show_the_current_choice() {
        use egui::accesskit::Toggled;
        use egui_kittest::kittest::{NodeT as _, Queryable as _};

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, background: &mut Option<Rgba>| {
                let mut recent = Vec::new();
                text_background(ui, background, &mut recent);
            },
            Some::<Rgba>([0, 0, 255, 255]),
        );
        harness.run();

        assert_eq!(
            harness.get_by_label("Solid").accesskit_node().toggled(),
            Some(Toggled::True),
            "Solid is not highlighted while a background is set"
        );
        assert_eq!(
            harness.get_by_label("None").accesskit_node().toggled(),
            Some(Toggled::False),
            "None is highlighted while a background is set"
        );
    }

    /// The same fix on the Rectangle's Fill control, which had the identical
    /// `selectable_label(chosen, "None")` pattern.
    #[test]
    fn the_fill_choice_labels_show_the_current_choice() {
        use egui::accesskit::Toggled;
        use egui_kittest::kittest::{NodeT as _, Queryable as _};

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, fill: &mut Option<Rgba>| {
                let mut recent = Vec::new();
                fill_controls(ui, fill, &mut recent);
            },
            Some::<Rgba>([255, 255, 255, 255]),
        );
        harness.run();

        assert_eq!(
            harness.get_by_label("Solid").accesskit_node().toggled(),
            Some(Toggled::True),
            "Solid is not highlighted while a fill is set"
        );
        assert_eq!(
            harness.get_by_label("None").accesskit_node().toggled(),
            Some(Toggled::False),
            "None is highlighted while a fill is set"
        );
    }

    /// NEXT_STEPS step 8: the Outline choice shows its current value, and the
    /// colour and width controls only appear once it is Solid.
    #[test]
    fn the_outline_controls_follow_the_outline_choice() {
        use egui::accesskit::Toggled;
        use egui_kittest::kittest::{NodeT as _, Queryable as _};

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, outline: &mut Option<Stroke>| {
                let mut recent = Vec::new();
                text_outline_controls(ui, outline, &mut recent);
            },
            None::<Stroke>,
        );
        harness.run();
        assert_eq!(
            harness.get_by_label("None").accesskit_node().toggled(),
            Some(Toggled::True),
            "None is not highlighted while the outline is off"
        );
        assert_eq!(
            harness.get_by_label("Solid").accesskit_node().toggled(),
            Some(Toggled::False),
        );
        assert!(
            harness.query_by_label_contains("Outline width").is_none(),
            "the width control is shown with no outline"
        );

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, outline: &mut Option<Stroke>| {
                let mut recent = Vec::new();
                text_outline_controls(ui, outline, &mut recent);
            },
            Some(Stroke {
                color: [232, 17, 35, 255],
                width: 4.0,
            }),
        );
        harness.run();
        assert_eq!(
            harness.get_by_label("Solid").accesskit_node().toggled(),
            Some(Toggled::True),
            "Solid is not highlighted while the outline is on"
        );
        assert_eq!(
            harness.get_by_label("None").accesskit_node().toggled(),
            Some(Toggled::False),
        );
        assert!(
            harness.query_by_label_contains("Outline width").is_some(),
            "the width control is missing with a solid outline"
        );
    }

    /// NEXT_STEPS step 8: the outline is part of the one style-difference path,
    /// so the panel, the menu and the tool defaults cannot drift apart.
    #[test]
    fn a_text_outline_change_round_trips_through_the_difference() {
        let before = ToolStyle::default();
        let after = ToolStyle {
            text_outline: Some(Stroke {
                color: [9, 8, 7, 255],
                width: 7.0,
            }),
            ..ToolStyle::default()
        };

        let changes = difference(&before, &after);
        assert!(
            changes
                .iter()
                .any(|change| matches!(change, StyleProperty::TextOutline(Some(_)))),
            "the outline change produced no property"
        );
        let mut rebuilt = ToolStyle::default();
        for change in changes {
            write_into_style(&mut rebuilt, &change);
        }
        assert_eq!(rebuilt, after);
    }

    /// NEXT_STEPS step 8: the colour and width properties write into a text
    /// annotation and into the Text tool's defaults (SPEC 7.7).
    #[test]
    fn outline_colour_and_width_write_into_a_text() {
        let mut tab = tab();
        let id = tab.document.push(Kind::Text(snp_ligase_core::model::TextA {
            pos: Point::new(0.0, 0.0),
            text: "x".to_owned(),
            font_family: "Inter".to_owned(),
            size: 20.0,
            bold: false,
            italic: false,
            color: [0, 0, 0, 255],
            background: None,
            padding: 4.0,
            outline: Some(snp_ligase_core::model::defaults::stroke()),
        }));
        tab.select_only(Some(id));
        let mut styles = ToolStyles::default();

        apply_properties(
            &mut tab,
            &mut styles,
            &[id],
            &[
                StyleProperty::TextOutlineColour([1, 2, 3, 255]),
                StyleProperty::TextOutlineWidth(9.0),
            ],
        );

        let Kind::Text(text) = &tab.document.find(id).expect("the text").kind else {
            panic!("expected text");
        };
        assert_eq!(
            text.outline,
            Some(Stroke {
                color: [1, 2, 3, 255],
                width: 9.0
            })
        );
        let style = styles.get(Tool::Text).expect("a text style");
        assert_eq!(
            style.text_outline,
            Some(Stroke {
                color: [1, 2, 3, 255],
                width: 9.0
            }),
            "the Text tool did not learn the outline"
        );
    }

    /// C1, the panel half: clicking the `DragValue` inside "Custom…" must not
    /// close the popup, or a typed value was impossible — only dragging worked.
    #[test]
    fn the_custom_value_popup_survives_a_click_inside_it() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, width: &mut f32| {
                custom_value(ui, "Stroke width", width, 0.1..=200.0);
            },
            4.0_f32,
        );

        harness.get_by_label("Custom…").click();
        harness.run();
        assert!(
            harness
                .query_by_label_contains("Stroke width in image pixels")
                .is_some(),
            "the custom popup did not open"
        );

        // Click the number field inside it.
        let field = harness
            .query_all_by_role(egui::accesskit::Role::SpinButton)
            .next()
            .expect("the popup has no numeric field");
        field.click();
        harness.run();

        assert!(
            harness
                .query_by_label_contains("Stroke width in image pixels")
                .is_some(),
            "clicking the numeric field closed the popup"
        );
    }
}
