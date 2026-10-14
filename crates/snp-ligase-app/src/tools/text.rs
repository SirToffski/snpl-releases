//! The Text tool: click to place a text annotation and open the editor
//! (SPEC section 7.5).
//!
//! The tool only places the annotation; the floating editor and the undo
//! session live in the app, because they are UI. What the tool does contribute
//! is the [`Gesture::TextEdit`] the app watches for, carrying the document as it
//! was *before* the annotation existed: that is the snapshot a whole editing
//! session undoes to (SPEC 7.5: one session is one undo step).

use snp_ligase_core::model::{Kind, TextA};
use snp_ligase_core::text::Fonts;

use crate::canvas::CanvasInput;
use crate::tab::Tab;
use crate::tool::{Tool, ToolStyles};
use crate::tools::{self, Gesture};

/// Runs the Text tool for one frame: a click places an empty annotation and
/// asks for the editor.
///
/// The annotation is placed on the *release*, not the press: the click's
/// release is what takes focus away from a widget the click landed outside of
/// (egui's `SurrenderFocusOn::Clicks`), so an editor opened on the press would
/// lose its focus on the very next frame and commit itself shut.
pub fn handle(
    tab: &mut Tab,
    styles: &ToolStyles,
    input: &CanvasInput,
    gesture: &mut Gesture,
) -> Option<String> {
    if let Gesture::TextPlace { pos } = *gesture {
        if !input.primary_released {
            return None;
        }
        *gesture = Gesture::None;
        let style = styles.get(Tool::Text)?.clone();
        let snapshot = Box::new(tab.document.clone());
        let kind = Kind::Text(TextA {
            pos,
            text: String::new(),
            font_family: style.font_family,
            size: style.font_size,
            bold: style.bold,
            italic: style.italic,
            color: style.text_color,
            background: style.text_background,
            padding: style.padding,
            outline: style.text_outline,
        });
        let id = tab.document.push(kind);
        tab.select_only(Some(id));
        *gesture = Gesture::TextEdit {
            id,
            created: true,
            snapshot,
        };
        return Some("Type below; Ctrl+Enter finishes, Esc cancels.".to_owned());
    }

    if !input.primary_pressed {
        return None;
    }
    let pos = input.image_pointer?;
    *gesture = Gesture::TextPlace { pos };
    None
}

/// A double-click on an existing text annotation opens the editor for it
/// (SPEC 7.5), with Select or Text active.
pub fn double_click(
    tab: &mut Tab,
    input: &CanvasInput,
    fonts: &mut Fonts,
    gesture: &mut Gesture,
) -> Option<String> {
    let point = input.image_pointer?;
    let tolerance = tools::tolerance_in_pixels(tools::HIT_TOLERANCE_POINTS, tab.viewport.zoom);
    let id = tools::annotation_at(tab, point, tolerance, fonts)?;
    if !matches!(tab.document.find(id)?.kind, Kind::Text(_)) {
        return None;
    }
    let snapshot = Box::new(tab.document.clone());
    tab.select_only(Some(id));
    *gesture = Gesture::TextEdit {
        id,
        created: false,
        snapshot,
    };
    Some("Editing text. Ctrl+Enter finishes, Esc cancels.".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use snp_ligase_core::geom::Point;
    use snp_ligase_core::model::Document;
    use snp_ligase_core::text::Fonts;

    fn tab() -> Tab {
        Tab::new(
            Document::new(image::RgbaImage::new(400, 400)),
            "test".to_owned(),
        )
    }

    fn at(point: Point) -> CanvasInput {
        CanvasInput {
            pointer: Some(egui::pos2(point.x, point.y)),
            image_pointer: Some(point),
            primary_down: true,
            ..CanvasInput::default()
        }
    }

    fn run(tab: &mut Tab, styles: &ToolStyles, gesture: &mut Gesture, input: &CanvasInput) {
        let mut fonts = Fonts::bundled();
        let mut active_point = None;
        crate::tools::handle(
            tab,
            Tool::Text,
            styles,
            input,
            &mut fonts,
            gesture,
            &mut active_point,
        );
    }

    #[test]
    fn the_annotation_is_placed_on_the_release_not_the_press() {
        // Found by driving the real window: opening the editor on the press
        // meant the click's own release surrendered the editor's focus
        // (egui's `SurrenderFocusOn::Clicks`) and the session committed itself
        // shut before a single key could be typed.
        let mut tab = tab();
        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;

        run(
            &mut tab,
            &styles,
            &mut gesture,
            &CanvasInput {
                primary_pressed: true,
                ..at(Point::new(50.0, 50.0))
            },
        );
        assert!(
            tab.document.annotations.is_empty(),
            "the press already placed the annotation"
        );
        assert!(
            matches!(gesture, Gesture::TextPlace { .. }),
            "the press did not remember where the text goes: {gesture:?}"
        );

        run(
            &mut tab,
            &styles,
            &mut gesture,
            &CanvasInput {
                primary_released: true,
                ..at(Point::new(50.0, 50.0))
            },
        );
        assert_eq!(tab.document.annotations.len(), 1, "the release placed it");
        let Gesture::TextEdit { id, created, .. } = &gesture else {
            panic!("the release did not ask for the editor: {gesture:?}");
        };
        assert!(*created, "a new annotation is not marked as created");
        assert_eq!(tab.document.annotations[0].id, *id);
    }
}
