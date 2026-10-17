//! The Freehand tool: press, drag, release draws one stroke (NEXT_STEPS step
//! 14b; the kind and its rendering are step 14a).
//!
//! SPEC 7.3's rules for every drawing tool apply: the new annotation becomes
//! the selection, a drag shorter than three screen px creates nothing, and the
//! whole gesture is one undo step. What is special here is the *capture*: egui
//! can report several pointer positions in one frame, and keeping only the
//! latest would turn a fast scribble into a polygon at 60 fps. So every
//! `Event::PointerMoved` of the frame arrives in [`CanvasInput::pointer_samples`]
//! and all of them go into the stroke.

use snp_ligase_core::geom::Point;
use snp_ligase_core::model::{FreehandA, Kind, Stroke};
use snp_ligase_core::path;

use crate::canvas::CanvasInput;
use crate::tab::Tab;
use crate::tool::{Tool, ToolStyles};
use crate::tools::{self, Gesture};

/// A sample closer than this to the previous one is dropped, in **screen**
/// points, so the sampling density does not change with the zoom.
pub const SAMPLE_GAP_POINTS: f32 = 1.0;

/// The Ramerâ€“Douglasâ€“Peucker tolerance the release simplifies with, in image
/// pixels (NEXT_STEPS step 14b).
pub const SIMPLIFY_TOLERANCE_PX: f32 = 0.5;

/// Runs the Freehand tool for one frame.
pub fn handle(
    tab: &mut Tab,
    styles: &ToolStyles,
    input: &CanvasInput,
    gesture: &mut Gesture,
) -> Option<String> {
    // The press starts the stroke at the press position.
    if !matches!(gesture, Gesture::Freehand { .. }) {
        let start = input.image_pointer?;
        if !input.primary_pressed {
            return None;
        }
        *gesture = Gesture::Freehand { points: Vec::new() };
        // Not `return`: the press frame can already carry moves, and they are
        // part of the stroke.
        let Gesture::Freehand { points } = gesture else {
            return None;
        };
        points.push(start);
    }

    let Gesture::Freehand { points } = gesture else {
        return None;
    };
    // Every position egui reported this frame, and the latest one â€” a frame
    // with no move event still has a pointer position, and the release point
    // is the last sample of the stroke.
    for sample in input
        .pointer_samples
        .iter()
        .copied()
        .chain(input.image_pointer)
    {
        push_sample(tab, points, sample);
    }

    if !input.primary_released {
        return None;
    }
    let points = std::mem::take(points);
    *gesture = Gesture::None;
    finish(tab, styles, &points)
}

/// Adds one sample unless it is too close to the previous one.
///
/// The distance is measured in screen points: at 25% zoom a one-image-pixel
/// step is a quarter of a point on screen and must not become a point of the
/// model, and at 400% a half-pixel step is two points and must stay.
fn push_sample(tab: &Tab, points: &mut Vec<Point>, sample: Point) {
    if let Some(last) = points.last()
        && (tab.viewport.to_screen(*last) - tab.viewport.to_screen(sample)).length()
            < SAMPLE_GAP_POINTS
    {
        return;
    }
    points.push(sample);
}

/// Ends the stroke: simplifies it, and adds it as one undo step.
fn finish(tab: &mut Tab, styles: &ToolStyles, points: &[Point]) -> Option<String> {
    // SPEC 7.3's short-drag rule, measured along the stroke: a scribble that
    // wanders more than three screen px is a stroke even if its ends are close
    // together.
    if stroke_length_points(tab, points) < tools::MIN_DRAG_POINTS {
        return None;
    }

    let style = styles.get(Tool::Freehand)?;
    let simplified = path::simplify(points, SIMPLIFY_TOLERANCE_PX);
    if simplified.len() < 2 {
        return None;
    }

    // One undo step for the whole stroke, pushed before the annotation exists
    // so undo takes it away again (SPEC 7.3).
    tab.begin_edit();
    let stroke = Stroke {
        color: style.stroke_color,
        width: style.stroke_width,
    };
    let shadow = style.shadow;
    let id = tab.document.push(Kind::Freehand(FreehandA {
        points: simplified,
        stroke,
    }));
    if shadow && let Some(annotation) = tab.document.find_mut(id) {
        annotation.shadow = true;
    }
    tab.select_only(Some(id));
    Some("Freehand stroke added.".to_owned())
}

/// How long the stroke is, in screen points.
///
/// `pub(crate)` for the tests: it is the screen-space measure SPEC 7.3's rule
/// is written in.
pub(crate) fn stroke_length_points(tab: &Tab, points: &[Point]) -> f32 {
    points
        .windows(2)
        .map(|pair| (tab.viewport.to_screen(pair[0]) - tab.viewport.to_screen(pair[1])).length())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use snp_ligase_core::model::Document;

    /// A tab at 100% with no pan, so image and screen coordinates are the same
    /// numbers.
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

    /// The press frame: the button goes down on this frame only.
    fn pressed(point: Point) -> CanvasInput {
        CanvasInput {
            primary_pressed: true,
            ..at(point)
        }
    }

    /// A move frame that reports several pointer events at once, the way egui
    /// does during a fast stroke.
    fn moved(samples: &[Point]) -> CanvasInput {
        let latest = *samples.last().expect("a move frame has a sample");
        CanvasInput {
            pointer_samples: samples.to_vec(),
            ..at(latest)
        }
    }

    fn released(point: Point) -> CanvasInput {
        CanvasInput {
            primary_released: true,
            ..at(point)
        }
    }

    /// The frame sequence of one stroke: press at `start`, then one move frame
    /// per slice, then the release.
    fn draw_stroke(
        tab: &mut Tab,
        styles: &ToolStyles,
        start: Point,
        slices: &[&[Point]],
        release: Point,
    ) -> (Option<String>, Gesture) {
        let mut gesture = Gesture::None;
        let _ = handle(tab, styles, &pressed(start), &mut gesture);
        for slice in slices {
            let _ = handle(tab, styles, &moved(slice), &mut gesture);
        }
        let message = handle(tab, styles, &released(release), &mut gesture);
        (message, gesture)
    }

    fn points_of(tab: &Tab) -> Vec<Point> {
        let annotation = tab.document.annotations.first().expect("a stroke");
        let Kind::Freehand(stroke) = &annotation.kind else {
            panic!("expected a freehand stroke, got {:?}", annotation.kind);
        };
        stroke.points.clone()
    }

    #[test]
    fn a_stroke_with_several_moves_in_one_frame_is_one_annotation_and_one_undo_step() {
        let mut tab = tab();
        let styles = ToolStyles::default();
        let (message, gesture) = draw_stroke(
            &mut tab,
            &styles,
            Point::new(10.0, 10.0),
            &[&[
                Point::new(120.0, 10.0),
                Point::new(220.0, 10.0),
                Point::new(320.0, 10.0),
            ]],
            Point::new(320.0, 10.0),
        );

        assert_eq!(tab.document.annotations.len(), 1, "expected one stroke");
        let annotation = &tab.document.annotations[0];
        let Kind::Freehand(stroke) = &annotation.kind else {
            panic!("expected a freehand stroke, got {:?}", annotation.kind);
        };
        // Every sample of the frame is in the stroke, and the release simplifies
        // the three collinear ones away.
        assert_eq!(
            stroke.points,
            vec![Point::new(10.0, 10.0), Point::new(320.0, 10.0)],
            "the stroke is not the drawn polyline"
        );
        assert_eq!(stroke.stroke.color, [232, 17, 35, 255]);
        assert!((stroke.stroke.width - 4.0).abs() < 0.001);
        assert!(!annotation.shadow, "the default has no shadow");
        assert_eq!(tab.selection(), [annotation.id], "not the new selection");
        assert_eq!(tab.undo_depth_for_test(), 1, "not exactly one undo step");
        assert_eq!(message.as_deref(), Some("Freehand stroke added."));
        assert!(!gesture.is_active(), "the gesture did not end");

        assert!(tab.undo());
        assert!(tab.document.annotations.is_empty(), "undo left the stroke");
    }

    #[test]
    fn every_move_in_one_frame_is_collected_not_just_the_latest() {
        // The polygon-at-60-fps bug: three events in one frame must all land in
        // the stroke, so the curve through them is the real one.
        let mut tab = tab();
        let styles = ToolStyles::default();
        let _ = draw_stroke(
            &mut tab,
            &styles,
            Point::new(10.0, 10.0),
            &[&[
                Point::new(100.0, 10.0),
                Point::new(100.0, 200.0),
                Point::new(300.0, 200.0),
            ]],
            Point::new(300.0, 200.0),
        );

        // A stroke through those points has a corner: RDP keeps all three.
        assert_eq!(
            points_of(&tab),
            vec![
                Point::new(10.0, 10.0),
                Point::new(100.0, 10.0),
                Point::new(100.0, 200.0),
                Point::new(300.0, 200.0),
            ],
            "a move inside the frame was dropped"
        );
    }

    #[test]
    fn samples_closer_than_one_screen_point_to_the_previous_one_are_dropped() {
        let mut tab = tab();
        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;
        let _ = handle(
            &mut tab,
            &styles,
            &pressed(Point::new(10.0, 10.0)),
            &mut gesture,
        );
        let _ = handle(
            &mut tab,
            &styles,
            &moved(&[
                Point::new(10.4, 10.0), // dropped: 0.4 points from (10, 10)
                Point::new(10.9, 10.0), // dropped: still under one point away
                Point::new(15.0, 10.0), // kept: five points away
            ]),
            &mut gesture,
        );

        assert_eq!(
            gesture.freehand_points().expect("a stroke being drawn"),
            [Point::new(10.0, 10.0), Point::new(15.0, 10.0)],
            "the near-duplicate samples were kept"
        );
    }
    /// The dedupe is in screen points: the same image-space move is kept at a
    /// high zoom and dropped at a low one.
    #[test]
    fn the_sample_gap_is_measured_in_screen_points() {
        // At 400%, half an image pixel is two screen points: kept.
        let mut close_up = tab();
        close_up.viewport.zoom = 4.0;
        let mut gesture = Gesture::None;
        let styles = ToolStyles::default();
        let _ = handle(
            &mut close_up,
            &styles,
            &pressed(Point::new(10.0, 10.0)),
            &mut gesture,
        );
        let _ = handle(
            &mut close_up,
            &styles,
            &moved(&[Point::new(10.5, 10.0)]),
            &mut gesture,
        );
        assert_eq!(
            gesture.freehand_points().expect("a stroke").len(),
            2,
            "a two-point move at 400% was dropped"
        );

        // At 25%, two image pixels are half a screen point: dropped.
        let mut far_out = tab();
        far_out.viewport.zoom = 0.25;
        let mut gesture = Gesture::None;
        let _ = handle(
            &mut far_out,
            &styles,
            &pressed(Point::new(10.0, 10.0)),
            &mut gesture,
        );
        let _ = handle(
            &mut far_out,
            &styles,
            &moved(&[Point::new(12.0, 10.0)]),
            &mut gesture,
        );
        assert_eq!(
            gesture.freehand_points().expect("a stroke").len(),
            1,
            "a sub-point move at 25% was kept"
        );
    }

    #[test]
    fn a_stroke_shorter_than_three_screen_points_creates_nothing() {
        let mut tab = tab();
        let styles = ToolStyles::default();
        let (message, gesture) = draw_stroke(
            &mut tab,
            &styles,
            Point::new(50.0, 50.0),
            &[&[Point::new(50.5, 50.0), Point::new(51.0, 50.0)]],
            Point::new(51.0, 50.0),
        );

        assert!(tab.document.annotations.is_empty(), "a 1 px scribble drew");
        assert_eq!(tab.undo_depth_for_test(), 0, "it cost an undo step");
        assert!(tab.selection().is_empty());
        assert_eq!(message, None, "it should not claim to have drawn");
        assert!(!gesture.is_active());
    }

    /// A scribble that stays near the start but travels is a real stroke: the
    /// rule measures the length of the stroke, not the distance between its
    /// ends.
    #[test]
    fn a_curly_stroke_with_close_ends_is_still_a_stroke() {
        let mut tab = tab();
        let styles = ToolStyles::default();
        let (_, _) = {
            let mut gesture = Gesture::None;
            let _ = handle(
                &mut tab,
                &styles,
                &pressed(Point::new(50.0, 50.0)),
                &mut gesture,
            );
            let _ = handle(
                &mut tab,
                &styles,
                &moved(&[Point::new(50.0, 56.0)]),
                &mut gesture,
            );
            let message = handle(
                &mut tab,
                &styles,
                &released(Point::new(50.0, 50.0)),
                &mut gesture,
            );
            (message, gesture)
        };

        assert_eq!(tab.document.annotations.len(), 1, "the scribble was lost");
    }

    #[test]
    fn the_stroke_takes_the_freehand_style() {
        let mut tab = tab();
        let mut styles = ToolStyles::default();
        {
            let style = styles.get_mut(Tool::Freehand).expect("a freehand style");
            style.stroke_color = [1, 2, 3, 255];
            style.stroke_width = 12.0;
            style.shadow = true;
        }
        let _ = draw_stroke(
            &mut tab,
            &styles,
            Point::new(100.0, 100.0),
            &[&[Point::new(140.0, 100.0)]],
            Point::new(140.0, 100.0),
        );

        let annotation = &tab.document.annotations[0];
        let Kind::Freehand(stroke) = &annotation.kind else {
            panic!("expected a freehand stroke");
        };
        assert_eq!(stroke.stroke.color, [1, 2, 3, 255]);
        assert!((stroke.stroke.width - 12.0).abs() < 0.001);
        assert!(annotation.shadow, "the tool's shadow was ignored");
    }

    #[test]
    fn a_release_with_no_pointer_position_still_creates_the_stroke_from_the_samples() {
        // R16: the button can come up outside the canvas. Unlike a rubber-band
        // drag, the stroke does not depend on the release position â€” the
        // samples already are the stroke â€” so it is still created.
        let mut tab = tab();
        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;
        let _ = handle(
            &mut tab,
            &styles,
            &pressed(Point::new(10.0, 10.0)),
            &mut gesture,
        );
        let _ = handle(
            &mut tab,
            &styles,
            &moved(&[Point::new(110.0, 60.0)]),
            &mut gesture,
        );
        let message = handle(
            &mut tab,
            &styles,
            &CanvasInput {
                primary_released: true,
                ..CanvasInput::default()
            },
            &mut gesture,
        );

        assert_eq!(message.as_deref(), Some("Freehand stroke added."));
        assert_eq!(tab.document.annotations.len(), 1);
        assert!(!gesture.is_active(), "the gesture was left stuck");
    }

    #[test]
    fn an_escaped_stroke_is_cancelled_and_costs_no_undo_step() {
        // Esc leaves `Gesture::None` behind (the app's `CancelGesture`), and
        // the release after it must add nothing: the document is only touched
        // on the release.
        let mut tab = tab();
        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;
        let _ = handle(
            &mut tab,
            &styles,
            &pressed(Point::new(10.0, 10.0)),
            &mut gesture,
        );
        let _ = handle(
            &mut tab,
            &styles,
            &moved(&[Point::new(110.0, 10.0)]),
            &mut gesture,
        );
        assert!(gesture.is_active(), "the stroke was not under way");
        gesture = Gesture::None;
        let message = handle(
            &mut tab,
            &styles,
            &released(Point::new(110.0, 10.0)),
            &mut gesture,
        );

        assert_eq!(tab.document.annotations.len(), 0, "Esc left a stroke");
        assert_eq!(tab.undo_depth_for_test(), 0, "Esc cost an undo step");
        assert_eq!(message, None);
    }
}
