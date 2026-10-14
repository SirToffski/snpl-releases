//! The Number tool: a badge with the document's next number (SPEC sections 7.3
//! and 7.6).
//!
//! A click places a plain badge; a press-and-drag places the badge where the
//! press was and points its arrow at the release point. The badge is created on
//! the release either way, so the whole gesture is one undo step and a click
//! that turns out to be a drag never leaves a stray badge behind.

use snp_ligase_core::geom::Point;
use snp_ligase_core::model::{Kind, NumberA};

use crate::canvas::CanvasInput;
use crate::tab::Tab;
use crate::tool::{Tool, ToolStyles};
use crate::tools::{self, Gesture};

/// How far the pointer has to move, in screen points, before the release adds a
/// pointer instead of a plain badge (SPEC 7.3: "more than 8 screen px").
pub const POINTER_DRAG_POINTS: f32 = 8.0;

/// Runs the Number tool for one frame.
pub fn handle(
    tab: &mut Tab,
    styles: &ToolStyles,
    input: &CanvasInput,
    gesture: &mut Gesture,
) -> Option<String> {
    if let Gesture::NumberDrag { center } = *gesture {
        // The release ends the gesture, and it is checked first because the
        // release frame can have no pointer position at all (R16).
        if !input.primary_released {
            return None;
        }
        *gesture = Gesture::None;
        let Some(release) = input.image_pointer else {
            return Some("The number was not placed.".to_owned());
        };
        // SPEC 7.3: a drag of more than 8 screen points makes it a "number with
        // arrow"; anything shorter is a plain badge.
        let tip = tools::dragged_far_enough_by(tab, center, release, POINTER_DRAG_POINTS)
            .then_some(release);
        return Some(place(tab, styles, center, tip));
    }

    if !input.primary_pressed {
        return None;
    }
    let center = input.image_pointer?;
    *gesture = Gesture::NumberDrag { center };
    None
}

/// Adds the badge and moves the document's counter on.
fn place(tab: &mut Tab, styles: &ToolStyles, center: Point, pointer_tip: Option<Point>) -> String {
    let style = styles.get(Tool::Number).cloned().unwrap_or_default();
    let value = tab.document.next_number;

    // One undo step for the whole gesture, and the counter moves with the badge
    // so undo puts both back (SPEC 7.3 and 7.6).
    tab.begin_edit();
    let id = tab.document.push(Kind::Number(NumberA {
        center,
        value,
        radius: style.number_radius,
        fill: style.number_fill,
        pointer_tip,
    }));
    if style.shadow
        && let Some(annotation) = tab.document.find_mut(id)
    {
        annotation.shadow = true;
    }
    tab.document.next_number = value.saturating_add(1);
    tab.select_only(Some(id));
    match pointer_tip {
        Some(_) => format!("Number {value} placed with a pointer."),
        None => format!("Number {value} placed. The next one is {}.", value + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            Tool::Number,
            styles,
            input,
            &mut fonts,
            gesture,
            &mut active_point,
        );
    }

    /// Press, drag, release: the three frames of one gesture.
    fn drag(tab: &mut Tab, from: Point, to: Point) {
        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;
        run(
            tab,
            &styles,
            &mut gesture,
            &CanvasInput {
                primary_pressed: true,
                ..at(from)
            },
        );
        run(tab, &styles, &mut gesture, &at(to));
        run(
            tab,
            &styles,
            &mut gesture,
            &CanvasInput {
                primary_released: true,
                ..at(to)
            },
        );
        tab.commit_edit();
    }

    #[test]
    fn a_click_places_the_next_number_and_counts_up() {
        let mut tab = tab();
        drag(&mut tab, Point::new(50.0, 50.0), Point::new(50.0, 50.0));

        assert_eq!(tab.document.annotations.len(), 1);
        let Kind::Number(number) = &tab.document.annotations[0].kind else {
            panic!("expected a number");
        };
        assert_eq!(number.value, 1, "the first badge is not 1");
        assert_eq!(number.center, Point::new(50.0, 50.0));
        assert_eq!(number.pointer_tip, None, "a click has no pointer");
        assert_eq!(tab.document.next_number, 2, "the counter did not move on");
        assert_eq!(tab.undo_depth_for_test(), 1, "not exactly one undo step");

        // Undo puts the counter back too, so the next badge is 1 again.
        assert!(tab.undo());
        assert_eq!(tab.document.next_number, 1);
    }

    #[test]
    fn a_drag_adds_a_pointer_from_the_press_to_the_release() {
        let mut tab = tab();
        drag(&mut tab, Point::new(50.0, 50.0), Point::new(90.0, 80.0));

        let Kind::Number(number) = &tab.document.annotations[0].kind else {
            panic!("expected a number");
        };
        assert_eq!(
            number.center,
            Point::new(50.0, 50.0),
            "the badge went to the release point"
        );
        assert_eq!(number.pointer_tip, Some(Point::new(90.0, 80.0)));
    }

    #[test]
    fn a_short_drag_stays_a_plain_badge() {
        let mut tab = tab();
        // 5 image pixels at 100% zoom is under the 8-point threshold.
        drag(&mut tab, Point::new(50.0, 50.0), Point::new(53.0, 54.0));
        let Kind::Number(number) = &tab.document.annotations[0].kind else {
            panic!("expected a number");
        };
        assert_eq!(number.pointer_tip, None);
    }

    #[test]
    fn numbers_count_up_and_a_reset_starts_again() {
        let mut tab = tab();
        drag(&mut tab, Point::new(20.0, 20.0), Point::new(20.0, 20.0));
        drag(&mut tab, Point::new(60.0, 20.0), Point::new(60.0, 20.0));
        assert_eq!(tab.document.next_number, 3);
        let values: Vec<u32> = tab
            .document
            .annotations
            .iter()
            .filter_map(|annotation| match &annotation.kind {
                Kind::Number(number) => Some(number.value),
                _ => None,
            })
            .collect();
        assert_eq!(values, [1, 2]);

        // SPEC 7.6: setting it back to 1 is how a second "1" is placed.
        tab.document.next_number = 1;
        drag(&mut tab, Point::new(100.0, 20.0), Point::new(100.0, 20.0));
        let last = tab.document.annotations.last().expect("the third badge");
        let Kind::Number(number) = &last.kind else {
            panic!("expected a number");
        };
        assert_eq!(number.value, 1);
    }

    #[test]
    fn a_release_outside_the_canvas_places_nothing() {
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
        run(
            &mut tab,
            &styles,
            &mut gesture,
            &CanvasInput {
                primary_released: true,
                ..CanvasInput::default()
            },
        );
        assert!(tab.document.annotations.is_empty());
        assert!(!gesture.is_active());
        assert_eq!(tab.document.next_number, 1, "the counter moved anyway");
    }
}
