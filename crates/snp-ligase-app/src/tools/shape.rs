//! The Shape tool: click to add each vertex, close to make a polygon
//! (SPEC section 7.3).
//!
//! This is the one gesture that is not held by the mouse button: it lasts until
//! the shape is closed (a click near the first vertex, a double-click or
//! `Enter`), a vertex is removed with `Backspace`, or `Esc` cancels it. The
//! preview while drawing is the canvas's job.

use snp_ligase_core::geom::Point;
use snp_ligase_core::model::{Kind, PolygonA, Stroke};

use crate::canvas::CanvasInput;
use crate::tab::Tab;
use crate::tool::{Tool, ToolStyles};
use crate::tools::Gesture;

/// How close a click has to be to the first vertex to close the shape, in
/// screen points (SPEC 7.3).
pub const CLOSE_TOLERANCE_POINTS: f32 = 8.0;

/// A shape needs at least this many vertices to close (SPEC 7.3).
pub const MIN_POINTS: usize = 3;

/// Runs the Shape tool for one frame.
pub fn handle(
    tab: &mut Tab,
    styles: &ToolStyles,
    input: &CanvasInput,
    gesture: &mut Gesture,
) -> Option<String> {
    if let Gesture::Polygon { points } = gesture {
        // A double-click closes the shape (SPEC 7.3). It arrives on the release
        // frame of the second click, which is also a press frame's release, so
        // it is checked before the press is handled.
        if input.double_clicked {
            return finish(tab, styles, gesture);
        }
        if !input.primary_pressed {
            return None;
        }
        let point = input.image_pointer?;
        // A click near the first vertex closes the shape (SPEC 7.3).
        if points.len() >= MIN_POINTS && near_first_vertex(tab, points, point) {
            return finish(tab, styles, gesture);
        }
        // A double-click is press, release, press, release+`double_clicked`
        // (F1): its second press lands on the vertex the first press added, and
        // pushing it again made a duplicate `[.., C, C]` that then closed as a
        // "triangle" which is really a line. A press on the last vertex adds
        // nothing.
        if points.last().is_some_and(|last| near(tab, *last, point)) {
            return None;
        }
        points.push(point);
        return Some(match points.len() {
            1 => "Shape started. Click each corner; Enter closes it.".to_owned(),
            count => format!("{count} points. Enter or click the first point to close."),
        });
    }

    if !input.primary_pressed {
        return None;
    }
    let point = input.image_pointer?;
    *gesture = Gesture::Polygon {
        points: vec![point],
    };
    Some("Shape started. Click each corner; Enter closes it.".to_owned())
}

/// Removes the last vertex (`Backspace`, SPEC 7.3).
///
/// Returns the message for the status bar. With one vertex left the whole shape
/// is dropped: there is nothing to draw from a single point.
pub fn remove_last_point(gesture: &mut Gesture) -> Option<String> {
    let Gesture::Polygon { points } = gesture else {
        return None;
    };
    if points.is_empty() {
        *gesture = Gesture::None;
        return Some("Shape cancelled.".to_owned());
    }
    points.pop();
    if points.is_empty() {
        *gesture = Gesture::None;
        return Some("Shape cancelled.".to_owned());
    }
    Some(format!(
        "{} point{} left.",
        points.len(),
        if points.len() == 1 { "" } else { "s" }
    ))
}

/// Closes the shape into a polygon (`Enter`, a double-click or a click on the
/// first vertex, SPEC 7.3).
///
/// A shape with fewer than three vertices cannot close, and stays in progress
/// with a message saying so. The vertices must also be three *different*
/// places: three clicks on the same two spots are still a line (F1).
pub fn finish(tab: &mut Tab, styles: &ToolStyles, gesture: &mut Gesture) -> Option<String> {
    let Gesture::Polygon { points } = gesture else {
        return None;
    };
    if points.len() < MIN_POINTS {
        return Some(format!(
            "A shape needs at least {MIN_POINTS} points; {} so far.",
            points.len()
        ));
    }
    if distinct_points(tab, points) < MIN_POINTS {
        return Some(format!(
            "A shape needs at least {MIN_POINTS} distinct points to close."
        ));
    }
    let style = styles.get(Tool::Shape)?;
    let kind = Kind::Polygon(PolygonA {
        points: points.clone(),
        stroke: Some(Stroke {
            color: style.stroke_color,
            width: style.stroke_width,
        }),
        fill: style.fill,
    });
    let shadow = style.shadow;
    let count = points.len();

    // One undo step for the whole shape, pushed before it exists (SPEC 7.3).
    tab.begin_edit();
    let id = tab.document.push(kind);
    if shadow && let Some(annotation) = tab.document.find_mut(id) {
        annotation.shadow = true;
    }
    tab.select_only(Some(id));
    *gesture = Gesture::None;
    Some(format!("Shape added with {count} points."))
}

/// Whether a click is close enough to the first vertex to close the shape.
///
/// The distance is measured in **screen points**, so the target is the same
/// size at every zoom (SPEC 7.3).
fn near_first_vertex(tab: &Tab, points: &[Point], point: Point) -> bool {
    points.first().is_some_and(|first| near(tab, *first, point))
}

/// Whether two points are within [`CLOSE_TOLERANCE_POINTS`] on screen.
///
/// The same tolerance a click on the first vertex uses, because the question is
/// the same one: "is the user clicking the same place twice?" (F1).
fn near(tab: &Tab, a: Point, b: Point) -> bool {
    (tab.viewport.to_screen(a) - tab.viewport.to_screen(b)).length() <= CLOSE_TOLERANCE_POINTS
}

/// How many of `points` are not within the close tolerance of an earlier one.
///
/// A shape needs [`MIN_POINTS`] *distinct* vertices: with only two, its "edges"
/// are the same line walked twice and it has no area to fill or to hit (F1).
fn distinct_points(tab: &Tab, points: &[Point]) -> usize {
    let mut distinct: Vec<Point> = Vec::with_capacity(points.len());
    for point in points {
        if distinct.iter().all(|seen| !near(tab, *seen, *point)) {
            distinct.push(*point);
        }
    }
    distinct.len()
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

    fn click(at: Point) -> CanvasInput {
        CanvasInput {
            pointer: Some(egui::pos2(at.x, at.y)),
            image_pointer: Some(at),
            primary_pressed: true,
            primary_down: true,
            ..CanvasInput::default()
        }
    }

    /// The release frame of a click.
    fn release(at: Point) -> CanvasInput {
        CanvasInput {
            pointer: Some(egui::pos2(at.x, at.y)),
            image_pointer: Some(at),
            primary_released: true,
            ..CanvasInput::default()
        }
    }

    /// The release frame of the second click of a double-click.
    fn double_click_release(at: Point) -> CanvasInput {
        CanvasInput {
            double_clicked: true,
            ..release(at)
        }
    }

    /// One whole click: the press frame, then the release frame, exactly as the
    /// canvas delivers them.
    fn click_through(tab: &mut Tab, styles: &ToolStyles, gesture: &mut Gesture, at: Point) {
        run(tab, styles, gesture, &click(at));
        run(tab, styles, gesture, &release(at));
    }

    fn run_message(
        tab: &mut Tab,
        styles: &ToolStyles,
        gesture: &mut Gesture,
        input: &CanvasInput,
    ) -> Option<String> {
        let mut fonts = Fonts::bundled();
        let mut active_point = None;
        crate::tools::handle(
            tab,
            Tool::Shape,
            styles,
            input,
            &mut fonts,
            gesture,
            &mut active_point,
        )
    }

    fn run(tab: &mut Tab, styles: &ToolStyles, gesture: &mut Gesture, input: &CanvasInput) {
        let _ = run_message(tab, styles, gesture, input);
    }

    /// Clicks a whole shape and closes it with `Enter`.
    fn draw_shape(tab: &mut Tab, points: &[Point]) -> Gesture {
        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;
        for point in points {
            run(tab, &styles, &mut gesture, &click(*point));
        }
        run(
            tab,
            &styles,
            &mut gesture,
            &CanvasInput {
                primary_released: true,
                ..CanvasInput::default()
            },
        );
        finish(tab, &styles, &mut gesture);
        gesture
    }

    #[test]
    fn clicking_the_first_vertex_closes_the_shape() {
        let mut tab = tab();
        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;
        for point in [
            Point::new(10.0, 10.0),
            Point::new(100.0, 20.0),
            Point::new(60.0, 120.0),
        ] {
            run(&mut tab, &styles, &mut gesture, &click(point));
        }
        // A click two image pixels from the first vertex is within the 8-point
        // tolerance at 100% zoom.
        run(
            &mut tab,
            &styles,
            &mut gesture,
            &click(Point::new(11.0, 11.0)),
        );

        assert!(!gesture.is_active(), "the shape did not close");
        assert_eq!(tab.document.annotations.len(), 1);
        let Kind::Polygon(polygon) = &tab.document.annotations[0].kind else {
            panic!("expected a polygon");
        };
        assert_eq!(polygon.points.len(), 3);
        assert_eq!(tab.undo_depth_for_test(), 1, "not exactly one undo step");
        assert!(tab.undo());
        assert!(tab.document.annotations.is_empty());
    }

    #[test]
    fn a_double_click_closes_the_shape_without_duplicating_the_last_vertex() {
        // F1: a double-click is press, release, press, release+`double_clicked`,
        // and both presses used to reach the tool, so the last vertex was added
        // twice and the shape closed with a duplicate point.
        let mut tab = tab();
        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;
        for point in [
            Point::new(10.0, 10.0),
            Point::new(100.0, 20.0),
            Point::new(60.0, 120.0),
        ] {
            click_through(&mut tab, &styles, &mut gesture, point);
        }
        let last = Point::new(20.0, 60.0);
        // The double-click: its first click places the last vertex...
        click_through(&mut tab, &styles, &mut gesture, last);
        // ...and its second press must not place it again.
        run(&mut tab, &styles, &mut gesture, &click(last));
        run(&mut tab, &styles, &mut gesture, &double_click_release(last));

        assert!(!gesture.is_active(), "the shape did not close");
        assert_eq!(tab.document.annotations.len(), 1);
        let Kind::Polygon(polygon) = &tab.document.annotations[0].kind else {
            panic!("expected a polygon");
        };
        assert_eq!(
            polygon.points,
            vec![
                Point::new(10.0, 10.0),
                Point::new(100.0, 20.0),
                Point::new(60.0, 120.0),
                last
            ],
            "the double click added the last vertex twice"
        );
        assert_eq!(tab.undo_depth_for_test(), 1, "not exactly one undo step");
        assert!(tab.undo());
        assert!(tab.document.annotations.is_empty());
    }

    #[test]
    fn a_double_click_with_only_two_places_does_not_close_a_shape() {
        // F1's degenerate case: with one vertex placed, a double-click at the
        // second place used to close `[A, B, B]` as a "triangle" that is really
        // a line.
        let mut tab = tab();
        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;
        let a = Point::new(10.0, 10.0);
        let b = Point::new(100.0, 100.0);
        click_through(&mut tab, &styles, &mut gesture, a);
        // The double-click's first click places B, the second press is ignored.
        click_through(&mut tab, &styles, &mut gesture, b);
        run(&mut tab, &styles, &mut gesture, &click(b));
        let message = run_message(&mut tab, &styles, &mut gesture, &double_click_release(b))
            .expect("it should explain itself");
        assert!(message.contains("at least 3 points"), "{message}");

        assert!(gesture.is_active(), "the shape closed as a line");
        assert_eq!(
            gesture.polygon_points().map(<[Point]>::len),
            Some(2),
            "the second press of the double click was not ignored"
        );
        assert!(
            tab.document.annotations.is_empty(),
            "a degenerate polygon was created"
        );

        // Three clicks on two places are still two places, so closing is
        // refused even when the click lands on the first vertex.
        click_through(&mut tab, &styles, &mut gesture, a);
        let message = run_message(&mut tab, &styles, &mut gesture, &click(a))
            .expect("it should explain itself");
        assert!(message.contains("distinct"), "{message}");
        assert!(
            tab.document.annotations.is_empty(),
            "a polygon without three distinct corners was created"
        );
        assert!(gesture.is_active(), "the shape was lost");
    }

    #[test]
    fn a_double_click_mid_shape_closes_the_new_one_not_the_selected_old_one() {
        // F2: the previous polygon is still selected after it was closed, so a
        // double-click while a new shape is being drawn used to reach the
        // shared "insert a point on the selected polygon" handler first: the
        // old shape got a point and the new one stayed open.
        let mut tab = tab();
        let old = tab.document.push(Kind::Polygon(PolygonA {
            points: vec![
                Point::new(10.0, 10.0),
                Point::new(110.0, 10.0),
                Point::new(110.0, 110.0),
                Point::new(10.0, 110.0),
            ],
            stroke: Some(Stroke {
                color: [255, 0, 0, 255],
                width: 2.0,
            }),
            fill: None,
        }));
        tab.select_only(Some(old));

        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;
        // A new shape, two vertices placed.
        click_through(&mut tab, &styles, &mut gesture, Point::new(300.0, 300.0));
        click_through(&mut tab, &styles, &mut gesture, Point::new(340.0, 300.0));
        // A double-click on the *old* square's top edge, 25 px away from its
        // nearest handle: that is where the shared handler would insert a point.
        let at = Point::new(35.0, 10.0);
        click_through(&mut tab, &styles, &mut gesture, at);
        run(&mut tab, &styles, &mut gesture, &click(at));
        run(&mut tab, &styles, &mut gesture, &double_click_release(at));

        assert!(!gesture.is_active(), "the new shape stayed open");
        assert_eq!(
            tab.document.annotations.len(),
            2,
            "the new shape was not added"
        );
        let Kind::Polygon(old_shape) = &tab.document.find(old).expect("still there").kind else {
            panic!("expected a polygon");
        };
        assert_eq!(
            old_shape.points.len(),
            4,
            "the double click inserted a point into the old polygon"
        );
        let new_shape = tab.document.annotations.last().expect("the new shape");
        let Kind::Polygon(new_shape) = &new_shape.kind else {
            panic!("expected a polygon");
        };
        assert_eq!(new_shape.points.first(), Some(&Point::new(300.0, 300.0)));
        assert_eq!(new_shape.points.get(1), Some(&Point::new(340.0, 300.0)));
        assert_eq!(new_shape.points.last(), Some(&at));
        assert_eq!(tab.undo_depth_for_test(), 1, "not exactly one undo step");
    }

    #[test]
    fn a_shape_needs_three_points_to_close() {
        let mut tab = tab();
        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;
        for point in [Point::new(10.0, 10.0), Point::new(100.0, 20.0)] {
            run(&mut tab, &styles, &mut gesture, &click(point));
        }
        let message = finish(&mut tab, &styles, &mut gesture).expect("it should explain itself");
        assert!(message.contains("at least 3 points"), "{message}");
        assert!(tab.document.annotations.is_empty(), "it closed anyway");
        assert!(gesture.is_active(), "the shape was lost");
        assert_eq!(tab.undo_depth_for_test(), 0, "it cost an undo step");
    }

    #[test]
    fn backspace_removes_the_last_point() {
        let mut tab = tab();
        let styles = ToolStyles::default();
        let mut gesture = Gesture::None;
        for point in [
            Point::new(10.0, 10.0),
            Point::new(100.0, 20.0),
            Point::new(60.0, 120.0),
        ] {
            run(&mut tab, &styles, &mut gesture, &click(point));
        }
        let message = remove_last_point(&mut gesture).expect("it should say something");
        assert!(message.contains("2 points"), "{message}");
        assert_eq!(
            gesture.polygon_points().map(<[Point]>::len),
            Some(2),
            "the wrong number of points is left"
        );
        // Backspacing the rest cancels the shape rather than leaving a stub.
        remove_last_point(&mut gesture);
        remove_last_point(&mut gesture);
        assert!(!gesture.is_active(), "the shape was not cancelled");
    }

    #[test]
    fn the_whole_shape_is_one_undo_step() {
        let mut tab = tab();
        let points = [
            Point::new(10.0, 10.0),
            Point::new(100.0, 20.0),
            Point::new(60.0, 120.0),
            Point::new(20.0, 100.0),
        ];
        let gesture = draw_shape(&mut tab, &points);
        assert!(!gesture.is_active());
        assert_eq!(tab.undo_depth_for_test(), 1);
        assert!(tab.undo());
        assert!(tab.document.annotations.is_empty());
    }

    #[test]
    fn the_shape_style_comes_from_the_tool() {
        let mut tab = tab();
        let mut styles = ToolStyles::default();
        styles.get_mut(Tool::Shape).expect("a shape style").fill = Some([9, 9, 9, 255]);
        let mut gesture = Gesture::None;
        for point in [
            Point::new(10.0, 10.0),
            Point::new(100.0, 20.0),
            Point::new(60.0, 120.0),
        ] {
            run(&mut tab, &styles, &mut gesture, &click(point));
        }
        finish(&mut tab, &styles, &mut gesture);
        let Kind::Polygon(polygon) = &tab.document.annotations[0].kind else {
            panic!("expected a polygon");
        };
        assert_eq!(polygon.fill, Some([9, 9, 9, 255]));
    }
}
