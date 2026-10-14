//! The Crop tool: drag a rectangle, adjust it, apply it (SPEC 7.3).
//!
//! Nothing in the document changes until the crop is applied, so `Esc` and a
//! click elsewhere both simply drop the rectangle. Applying is one undo step
//! (SPEC 7.3), and it changes the base image's size, so the viewport is told to
//! start over (the app does that part).

use snp_ligase_core::edit::{self, ResizeDir};
use snp_ligase_core::geom::{Point, Rect};

use crate::canvas::CanvasInput;
use crate::tab::Tab;
use crate::tools::{self, Gesture};

/// The smallest crop worth applying, in screen points (SPEC 7.3: a drag under
/// three screen px creates nothing, and `edit::crop` returns a 0x0 document for
/// an empty rectangle).
pub const MIN_CROP_POINTS: f32 = 3.0;

/// Runs the Crop tool for one frame.
pub fn handle(tab: &mut Tab, input: &CanvasInput, gesture: &mut Gesture) -> Option<String> {
    match gesture {
        Gesture::CropDraw { from, to } => {
            if let Some(pointer) = input.image_pointer {
                *to = clip_point(tab, pointer);
            }
            if !input.primary_released {
                return None;
            }
            let rect = Rect::from_ltrb(from.x, from.y, to.x, to.y);
            if crop_big_enough(tab, rect) {
                *gesture = Gesture::CropPending { rect, handle: None };
                Some(format!(
                    "Crop {} × {} px. Drag the handles to adjust; Enter applies it.",
                    rect.w.round(),
                    rect.h.round()
                ))
            } else {
                *gesture = Gesture::None;
                Some("That crop is too small; drag a bigger rectangle.".to_owned())
            }
        }
        Gesture::CropPending { rect, handle } => {
            // Continue a handle drag. The rectangle is clipped to the image as
            // it is resized, so an edge dragged past the image stops there (F3).
            if let Some(direction) = *handle {
                if input.primary_released {
                    *handle = None;
                    return None;
                }
                if let Some(pointer) = input.image_pointer {
                    *rect = clip_to_image(tab, edit::resize_rect(*rect, direction, pointer));
                }
                return None;
            }
            if !input.primary_pressed {
                return None;
            }
            let pointer = input.pointer?;
            // A press on one of the eight handles adjusts that edge.
            if let Some(direction) = handle_at(tab, *rect, pointer) {
                *handle = Some(direction);
                return None;
            }
            // A press anywhere else starts a new rectangle.
            let image = clip_point(tab, input.image_pointer?);
            *gesture = Gesture::CropDraw {
                from: image,
                to: image,
            };
            None
        }
        _ => {
            if !input.primary_pressed {
                return None;
            }
            let image = clip_point(tab, input.image_pointer?);
            *gesture = Gesture::CropDraw {
                from: image,
                to: image,
            };
            None
        }
    }
}

/// The handle of the pending crop rectangle under the pointer, if any.
///
/// The tolerance is in screen points, so handles are the same size to grab at
/// every zoom (SPEC 7.2 and R12).
#[must_use]
pub fn handle_at(tab: &Tab, rect: Rect, pointer: egui::Pos2) -> Option<ResizeDir> {
    ResizeDir::ALL
        .iter()
        .map(|direction| {
            let at = tab.viewport.to_screen(direction.at(rect));
            (*direction, (at - pointer).length())
        })
        .filter(|(_, distance)| *distance <= tools::HANDLE_TOLERANCE_POINTS)
        .min_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(direction, _)| direction)
}

/// Whether a crop is big enough to apply: at least [`MIN_CROP_POINTS`] in both
/// directions **and** at least one image pixel of the image left after
/// clipping.
///
/// Both halves are needed. A rectangle dragged entirely on the grey area around
/// the image is 100 screen points across and passes the first test, but
/// `edit::crop` clips it to nothing and produced a 0×0 document (F3). Under one
/// image pixel, on the other hand, there is no pixel to keep even when the zoom
/// makes it wide enough on screen.
#[must_use]
pub fn crop_big_enough(tab: &Tab, rect: Rect) -> bool {
    let rect = clip_to_image(tab, rect);
    if rect.w < 1.0 || rect.h < 1.0 {
        return false;
    }
    let top_left = tab.viewport.to_screen(Point::new(rect.x, rect.y));
    let bottom_right = tab
        .viewport
        .to_screen(Point::new(rect.right(), rect.bottom()));
    (bottom_right.x - top_left.x) >= MIN_CROP_POINTS
        && (bottom_right.y - top_left.y) >= MIN_CROP_POINTS
}

/// The part of `rect` that lies inside the image (F3).
///
/// `edit::crop` rounds the rectangle out and *then* clips it, so a rectangle
/// that misses the image entirely becomes a 0×0 document rather than an error.
/// Everything that draws or applies a crop clips first, so what the user sees
/// selected is exactly what the crop will keep.
#[must_use]
pub fn clip_to_image(tab: &Tab, rect: Rect) -> Rect {
    let (width, height) = tab.size();
    rect.intersection(Rect::new(0.0, 0.0, width as f32, height as f32))
}

/// `point` clamped to the image, so a rectangle being dragged is clipped while
/// it is drawn (F3).
fn clip_point(tab: &Tab, point: Point) -> Point {
    let (width, height) = tab.size();
    Point::new(
        point.x.clamp(0.0, width as f32),
        point.y.clamp(0.0, height as f32),
    )
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

    fn run_message(tab: &mut Tab, gesture: &mut Gesture, input: &CanvasInput) -> Option<String> {
        let mut fonts = Fonts::bundled();
        let mut active_point = None;
        crate::tools::handle(
            tab,
            crate::tool::Tool::Crop,
            &crate::tool::ToolStyles::default(),
            input,
            &mut fonts,
            gesture,
            &mut active_point,
        )
    }

    fn run(tab: &mut Tab, gesture: &mut Gesture, input: &CanvasInput) {
        let _ = run_message(tab, gesture, input);
    }

    #[test]
    fn a_drag_leaves_a_pending_crop_rectangle() {
        let mut tab = tab();
        let mut gesture = Gesture::None;
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_pressed: true,
                ..at(Point::new(20.0, 30.0))
            },
        );
        run(&mut tab, &mut gesture, &at(Point::new(120.0, 130.0)));
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_released: true,
                ..at(Point::new(120.0, 130.0))
            },
        );

        let Gesture::CropPending { rect, handle } = gesture else {
            panic!("no pending crop: {gesture:?}");
        };
        assert_eq!(rect, Rect::new(20.0, 30.0, 100.0, 100.0));
        assert_eq!(handle, None);
        // Nothing in the document changed until it is applied.
        assert_eq!(tab.document.base.width(), 400);
        assert_eq!(tab.undo_depth_for_test(), 0);
    }

    #[test]
    fn a_too_small_drag_leaves_nothing() {
        let mut tab = tab();
        let mut gesture = Gesture::None;
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_pressed: true,
                ..at(Point::new(20.0, 30.0))
            },
        );
        run(&mut tab, &mut gesture, &at(Point::new(21.0, 31.0)));
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_released: true,
                ..at(Point::new(21.0, 31.0))
            },
        );
        assert!(!gesture.is_active(), "a 1 px crop was kept: {gesture:?}");
    }

    #[test]
    fn a_handle_drag_resizes_the_pending_rectangle() {
        let mut tab = tab();
        let mut gesture = Gesture::CropPending {
            rect: Rect::new(20.0, 30.0, 100.0, 100.0),
            handle: None,
        };
        // Grab the south-east corner: it is at (120, 130) in image pixels,
        // which is the same in screen points at 100% zoom.
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_pressed: true,
                ..at(Point::new(120.0, 130.0))
            },
        );
        run(&mut tab, &mut gesture, &at(Point::new(200.0, 180.0)));
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_released: true,
                ..at(Point::new(200.0, 180.0))
            },
        );

        let Gesture::CropPending { rect, handle } = gesture else {
            panic!("the crop went away");
        };
        assert_eq!(rect, Rect::new(20.0, 30.0, 180.0, 150.0));
        assert_eq!(handle, None, "the handle is still grabbed");
    }

    #[test]
    fn a_press_away_from_the_handles_starts_a_new_rectangle() {
        let mut tab = tab();
        let mut gesture = Gesture::CropPending {
            rect: Rect::new(20.0, 30.0, 100.0, 100.0),
            handle: None,
        };
        // The middle of the rectangle is nowhere near a handle.
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_pressed: true,
                ..at(Point::new(70.0, 80.0))
            },
        );
        assert!(
            matches!(gesture, Gesture::CropDraw { .. }),
            "the old rectangle stayed: {gesture:?}"
        );
    }

    #[test]
    fn the_handles_are_grabbable_at_any_zoom() {
        let mut tab = tab();
        let rect = Rect::new(20.0, 30.0, 100.0, 100.0);
        for zoom in [0.25_f32, 4.0] {
            tab.viewport.zoom = zoom;
            let corner = tab.viewport.to_screen(Point::new(rect.x, rect.y));
            assert_eq!(
                handle_at(&tab, rect, egui::pos2(corner.x + 5.0, corner.y)),
                Some(ResizeDir::NorthWest),
                "5 points away at zoom {zoom}"
            );
            assert_eq!(
                handle_at(&tab, rect, egui::pos2(corner.x + 40.0, corner.y)),
                None,
                "40 points away at zoom {zoom}"
            );
        }
    }

    #[test]
    fn a_crop_is_refused_below_three_screen_points() {
        let mut tab = tab();
        assert!(crop_big_enough(&tab, Rect::new(10.0, 10.0, 50.0, 50.0)));
        assert!(!crop_big_enough(&tab, Rect::new(10.0, 10.0, 2.0, 50.0)));
        assert!(!crop_big_enough(&tab, Rect::new(10.0, 10.0, 50.0, 1.0)));
        assert!(!crop_big_enough(&tab, Rect::new(10.0, 10.0, 0.0, 0.0)));
        // At 25% zoom, 10 image pixels are 2.5 screen points: too small.
        tab.viewport.zoom = 0.25;
        assert!(!crop_big_enough(&tab, Rect::new(10.0, 10.0, 10.0, 10.0)));
    }

    #[test]
    fn a_crop_is_refused_when_it_misses_the_image() {
        // F3: the size was measured unclipped, so a rectangle dragged on the
        // grey area around the image passed and `edit::crop` clipped it to a
        // 0×0 document.
        let mut tab = tab();
        assert!(
            !crop_big_enough(&tab, Rect::new(500.0, 500.0, 100.0, 100.0)),
            "a crop that misses the image was accepted"
        );
        assert!(
            crop_big_enough(&tab, Rect::new(350.0, 350.0, 100.0, 100.0)),
            "a crop that overlaps the image was refused"
        );
        assert_eq!(
            clip_to_image(&tab, Rect::new(350.0, 350.0, 100.0, 100.0)),
            Rect::new(350.0, 350.0, 50.0, 50.0),
            "the crop was not clipped to the intersection"
        );
        assert!(clip_to_image(&tab, Rect::new(500.0, 500.0, 10.0, 10.0)).is_empty());

        // Under one image pixel there is nothing to keep, even at 4× zoom,
        // where it is wider than three screen points.
        tab.viewport.zoom = 4.0;
        assert!(!crop_big_enough(&tab, Rect::new(10.0, 10.0, 0.9, 20.0)));
        assert!(crop_big_enough(&tab, Rect::new(10.0, 10.0, 1.0, 20.0)));
    }

    #[test]
    fn a_drag_wholly_outside_the_image_leaves_nothing() {
        // The same F3 case through the tool, with the real press/move/release
        // sequence.
        let mut tab = tab();
        let mut gesture = Gesture::None;
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_pressed: true,
                ..at(Point::new(450.0, 450.0))
            },
        );
        run(&mut tab, &mut gesture, &at(Point::new(600.0, 600.0)));
        let message = run_message(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_released: true,
                ..at(Point::new(600.0, 600.0))
            },
        )
        .expect("it should explain itself");
        assert!(message.contains("too small"), "{message}");
        assert!(
            !gesture.is_active(),
            "a crop outside the image was kept: {gesture:?}"
        );

        // A drag that starts outside but reaches into the image keeps exactly
        // the part that overlaps it.
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_pressed: true,
                ..at(Point::new(450.0, 450.0))
            },
        );
        run(&mut tab, &mut gesture, &at(Point::new(200.0, 200.0)));
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_released: true,
                ..at(Point::new(200.0, 200.0))
            },
        );
        let Gesture::CropPending { rect, .. } = gesture else {
            panic!("no pending crop: {gesture:?}");
        };
        assert_eq!(
            rect,
            Rect::new(200.0, 200.0, 200.0, 200.0),
            "the pending crop was not clipped to the image"
        );
    }

    #[test]
    fn a_handle_drag_past_the_image_clamps_to_the_edge() {
        // F3: the rectangle could be pulled out of the image while resizing, so
        // the crop it later applied reached past the pixels that exist.
        let mut tab = tab();
        let mut gesture = Gesture::CropPending {
            rect: Rect::new(100.0, 100.0, 200.0, 200.0),
            handle: None,
        };
        // Grab the south-east corner at (300, 300) and pull it past the
        // bottom-right corner of the 400×400 image.
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_pressed: true,
                ..at(Point::new(300.0, 300.0))
            },
        );
        run(&mut tab, &mut gesture, &at(Point::new(500.0, 500.0)));
        run(
            &mut tab,
            &mut gesture,
            &CanvasInput {
                primary_released: true,
                ..at(Point::new(500.0, 500.0))
            },
        );

        let Gesture::CropPending { rect, .. } = gesture else {
            panic!("the crop went away");
        };
        assert_eq!(
            rect,
            Rect::new(100.0, 100.0, 300.0, 300.0),
            "the rectangle left the image"
        );
    }
}
