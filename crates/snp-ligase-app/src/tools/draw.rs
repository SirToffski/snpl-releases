//! The drawing tools: Line, Arrow, Rectangle, Highlight and Blur
//! (SPEC section 7.3).
//!
//! All of them are the same gesture — press, drag, release — and what differs
//! is what comes out. Per SPEC 7.3: the new annotation becomes the selection,
//! the tool stays active, and a drag shorter than 3 screen px creates nothing.

use snp_ligase_core::geom::{Point, Rect};
use snp_ligase_core::model::{BlurA, HighlightA, Kind, PathA, RectA, Stroke};

use crate::canvas::CanvasInput;
use crate::tab::Tab;
use crate::tool::{Tool, ToolStyles};
use crate::tools::{self, Gesture};

/// Line and Arrow: press, drag, release to make a two-point path.
///
/// The Line tool never has a head (SPEC 7.3); the Arrow tool uses whichever end
/// the tool's style says, and that can be changed at any time from the
/// properties panel.
pub fn path(
    tab: &mut Tab,
    styles: &ToolStyles,
    input: &CanvasInput,
    gesture: &mut Gesture,
    tool: Tool,
) -> Option<String> {
    let current = finish_gesture(tab, input, gesture)?;
    let Gesture::Drawing { start } = *gesture else {
        return None;
    };
    // SPEC 7.3: Shift snaps a new line to 15-degree steps.
    let current = if input.shift {
        tools::snap_to_angle(start, current)
    } else {
        current
    };

    let style = styles.get(tool)?;
    let (head_start, head_end) = match tool {
        Tool::Arrow => style.arrow_ends.flags(),
        _ => (false, false),
    };
    let kind = Kind::Path(PathA {
        points: vec![start, current],
        curved: style.curved,
        stroke: Stroke {
            color: style.stroke_color,
            width: style.stroke_width,
        },
        head_start,
        head_end,
    });
    let shadow = style.shadow;
    let id = tab.document.push(kind);
    if shadow && let Some(annotation) = tab.document.find_mut(id) {
        annotation.shadow = true;
    }
    tab.select_only(Some(id));
    Some(format!(
        "{} added. Drag its middle handles to add points and bend it.",
        tool.name()
    ))
}

/// Rectangle: press, drag, release. Shift constrains it to a square (SPEC 7.3).
pub fn rectangle(
    tab: &mut Tab,
    styles: &ToolStyles,
    input: &CanvasInput,
    gesture: &mut Gesture,
) -> Option<String> {
    let current = finish_gesture(tab, input, gesture)?;
    let Gesture::Drawing { start } = *gesture else {
        return None;
    };
    let style = styles.get(Tool::Rectangle)?;

    let rect = if input.shift {
        square_from(start, current)
    } else {
        Rect::from_ltrb(start.x, start.y, current.x, current.y)
    };
    let kind = Kind::Rect(RectA {
        rect,
        stroke: Some(Stroke {
            color: style.stroke_color,
            width: style.stroke_width,
        }),
        fill: style.fill,
        corner_radius: style.corner_radius,
    });
    let shadow = style.shadow;
    let id = tab.document.push(kind);
    if shadow && let Some(annotation) = tab.document.find_mut(id) {
        annotation.shadow = true;
    }
    tab.select_only(Some(id));
    Some("Rectangle added.".to_owned())
}

/// Highlight: press, drag, release (SPEC 7.3).
///
/// A highlight is an effect: its pixels come from the base image, it covers its
/// rectangle exactly, and it takes no shadow (SPEC 4.2).
pub fn highlight(
    tab: &mut Tab,
    styles: &ToolStyles,
    input: &CanvasInput,
    gesture: &mut Gesture,
) -> Option<String> {
    let current = finish_gesture(tab, input, gesture)?;
    let Gesture::Drawing { start } = *gesture else {
        return None;
    };
    let style = styles.get(Tool::Highlight)?;
    let id = tab.document.push(Kind::Highlight(HighlightA {
        rect: Rect::from_ltrb(start.x, start.y, current.x, current.y),
        color: style.highlight_color,
    }));
    tab.select_only(Some(id));
    Some("Highlight added.".to_owned())
}

/// Blur: press, drag, release (SPEC 7.3).
///
/// Like a highlight, a blur is an effect that covers its rectangle exactly and
/// takes no shadow (SPEC 4.2).
pub fn blur(
    tab: &mut Tab,
    styles: &ToolStyles,
    input: &CanvasInput,
    gesture: &mut Gesture,
) -> Option<String> {
    let current = finish_gesture(tab, input, gesture)?;
    let Gesture::Drawing { start } = *gesture else {
        return None;
    };
    let style = styles.get(Tool::Blur)?;
    let id = tab.document.push(Kind::Blur(BlurA {
        rect: Rect::from_ltrb(start.x, start.y, current.x, current.y),
        radius: style.blur_radius,
    }));
    tab.select_only(Some(id));
    Some("Blur added.".to_owned())
}

/// A square that keeps the drag direction (SPEC 7.3: Shift constrains to a
/// square).
#[must_use]
pub fn square_from(start: Point, current: Point) -> Rect {
    let side = (current.x - start.x).abs().max((current.y - start.y).abs());
    let right = if current.x < start.x {
        start.x - side
    } else {
        start.x + side
    };
    let bottom = if current.y < start.y {
        start.y - side
    } else {
        start.y + side
    };
    Rect::from_ltrb(start.x, start.y, right, bottom)
}

/// The press and release handling every drawing tool shares.
///
/// Returns the pointer position on the frame a gesture ends, and `None` on every
/// other frame. The drag itself is only previewed on screen; nothing is added to
/// the document until the release.
fn finish_gesture(tab: &mut Tab, input: &CanvasInput, gesture: &mut Gesture) -> Option<Point> {
    if let Gesture::Drawing { start } = *gesture {
        // The gesture is already under way, so this is not a press. The release
        // is handled first, because the release frame can have no pointer
        // position at all when the button came up outside the canvas (R16).
        let current = input.image_pointer;
        if !input.primary_released {
            return None;
        }
        let Some(current) = current else {
            *gesture = Gesture::None;
            return None;
        };

        // A drag shorter than 3 screen px creates nothing (SPEC 7.3).
        if !tools::dragged_far_enough(tab, start, current) {
            *gesture = Gesture::None;
            return None;
        }

        // One undo step for the whole gesture, pushed before the annotation
        // exists so undo takes it away again (SPEC 7.3).
        tab.begin_edit();
        return Some(current);
    }

    let start = input.image_pointer?;
    if !input.primary_pressed {
        return None;
    }
    *gesture = Gesture::Drawing { start };
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use snp_ligase_core::model::Document;
    use snp_ligase_core::text::Fonts;

    #[test]
    fn a_square_keeps_the_drag_direction() {
        let start = Point::new(100.0, 100.0);

        let right_down = square_from(start, Point::new(160.0, 130.0));
        assert_eq!(right_down, Rect::new(100.0, 100.0, 60.0, 60.0));

        let left_down = square_from(start, Point::new(40.0, 130.0));
        assert_eq!(left_down, Rect::new(40.0, 100.0, 60.0, 60.0));

        // Dragging up and left by 30 rows still gives a 60 px square, whose top
        // edge is 60 px above the start, not at the pointer.
        let left_up = square_from(start, Point::new(40.0, 70.0));
        assert_eq!(left_up, Rect::new(40.0, 40.0, 60.0, 60.0));

        let right_up = square_from(start, Point::new(160.0, 70.0));
        assert_eq!(right_up, Rect::new(100.0, 40.0, 60.0, 60.0));
    }

    #[test]
    fn a_square_takes_the_longer_side() {
        let start = Point::new(0.0, 0.0);
        let square = square_from(start, Point::new(10.0, 100.0));
        assert!((square.w - 100.0).abs() < 0.001, "width {}", square.w);
        assert!((square.h - 100.0).abs() < 0.001, "height {}", square.h);
    }

    #[test]
    fn a_square_from_a_diagonal_drag_is_square() {
        let square = square_from(Point::new(10.0, 10.0), Point::new(40.0, 25.0));
        assert!((square.w - square.h).abs() < 0.001);
        assert_eq!(square.x, 10.0, "it moved away from the drag");
        assert_eq!(square.y, 10.0, "it moved away from the drag");
    }

    /// A tab with an image, at 100% with no pan, so image and screen
    /// coordinates are the same numbers.
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

    /// Press, drag, release: the three frames of one drag.
    fn drag(tab: &mut Tab, tool: Tool, styles: &ToolStyles, from: Point, to: Point) {
        let mut fonts = Fonts::bundled();
        let mut gesture = Gesture::None;
        let mut active_point = None;
        let press = CanvasInput {
            primary_pressed: true,
            ..at(from)
        };
        let moved = at(to);
        let release = CanvasInput {
            primary_released: true,
            ..at(to)
        };
        for input in [&press, &moved, &release] {
            tools::handle(
                tab,
                tool,
                styles,
                input,
                &mut fonts,
                &mut gesture,
                &mut active_point,
            );
        }
        tab.commit_edit();
    }

    #[test]
    fn a_highlight_drag_creates_one_annotation_for_one_undo_step() {
        let mut tab = tab();
        let styles = ToolStyles::default();
        drag(
            &mut tab,
            Tool::Highlight,
            &styles,
            Point::new(20.0, 30.0),
            Point::new(120.0, 80.0),
        );

        assert_eq!(tab.document.annotations.len(), 1);
        let annotation = &tab.document.annotations[0];
        let Kind::Highlight(highlight) = &annotation.kind else {
            panic!("expected a highlight, got {:?}", annotation.kind);
        };
        assert_eq!(highlight.rect, Rect::new(20.0, 30.0, 100.0, 50.0));
        assert_eq!(
            highlight.color,
            snp_ligase_core::model::defaults::HIGHLIGHT_COLOR
        );
        assert!(
            !annotation.shadow,
            "an effect takes no shadow: it would never be seen"
        );
        assert_eq!(tab.undo_depth_for_test(), 1, "not exactly one undo step");
        assert!(tab.undo());
        assert!(tab.document.annotations.is_empty());
    }

    #[test]
    fn a_blur_drag_takes_the_radius_from_the_tool_style() {
        let mut tab = tab();
        let mut styles = ToolStyles::default();
        styles
            .get_mut(Tool::Blur)
            .expect("a blur style")
            .blur_radius = 6.0;
        drag(
            &mut tab,
            Tool::Blur,
            &styles,
            Point::new(10.0, 10.0),
            Point::new(60.0, 60.0),
        );

        let Kind::Blur(blur) = &tab.document.annotations[0].kind else {
            panic!("expected a blur");
        };
        assert!((blur.radius - 6.0).abs() < 0.001, "radius {}", blur.radius);
        assert_eq!(blur.rect, Rect::new(10.0, 10.0, 50.0, 50.0));
    }

    #[test]
    fn a_short_drag_creates_no_effect() {
        for tool in [Tool::Highlight, Tool::Blur] {
            let mut tab = tab();
            let styles = ToolStyles::default();
            // One image pixel at 100% zoom is one screen point: under the
            // three-point minimum (SPEC 7.3).
            drag(
                &mut tab,
                tool,
                &styles,
                Point::new(50.0, 50.0),
                Point::new(51.0, 51.0),
            );
            assert!(
                tab.document.annotations.is_empty(),
                "{tool:?} created something from a 1 px drag"
            );
            assert_eq!(
                tab.undo_depth_for_test(),
                0,
                "{tool:?} cost an undo step for nothing"
            );
        }
    }

    #[test]
    fn a_release_outside_the_canvas_still_ends_the_drag() {
        // R16's rule, for the effect tools: the button can come up where there
        // is no pointer position at all.
        for tool in [Tool::Highlight, Tool::Blur] {
            let mut tab = tab();
            let mut fonts = Fonts::bundled();
            let styles = ToolStyles::default();
            let mut gesture = Gesture::None;
            let mut active_point = None;
            let press = CanvasInput {
                primary_pressed: true,
                ..at(Point::new(20.0, 20.0))
            };
            let release = CanvasInput {
                primary_released: true,
                ..CanvasInput::default()
            };
            for input in [&press, &release] {
                tools::handle(
                    &mut tab,
                    tool,
                    &styles,
                    input,
                    &mut fonts,
                    &mut gesture,
                    &mut active_point,
                );
            }
            assert!(!gesture.is_active(), "{tool:?} left the gesture stuck");
            assert!(tab.document.annotations.is_empty());
        }
    }
}
