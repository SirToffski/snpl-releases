//! Zoom and pan for one tab (SPEC section 7.2).
//!
//! The model never changes with zoom: an annotation stays where it is in image
//! pixels, and the viewport is a mapping between image pixels and the canvas.
//! Keeping that mapping in one small type means the canvas, the tools and the
//! properties panel cannot disagree about where something is.

use egui::{Vec2, pos2, vec2};
use snp_ligase_core::geom::{Point, Rect};

use crate::tab::{ZOOM_MAX, ZOOM_MIN};

/// How one tab's image is placed on the canvas.
///
/// `origin` is where image pixel (0, 0) sits **on the canvas**, in points, and
/// `zoom` is how many points one image pixel takes. Everything the canvas draws
/// is in *window* coordinates, so `canvas_min` — where the canvas starts inside
/// the window — has to be added on the way out and taken off on the way in.
/// Getting that wrong put the whole image up under the tools panel and the
/// toolbar, and hid the top-left corner of the image for good.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Viewport {
    pub zoom: f32,
    /// Image (0, 0) relative to the canvas's own top-left corner.
    pub origin: Vec2,
    /// Where the canvas starts in the window, set every frame by the canvas.
    pub canvas_min: Vec2,
    /// Set once the canvas has been sized, so a new tab can decide between 100%
    /// and fit (SPEC 7.2).
    pub initialised: bool,
}

impl Viewport {
    /// 100%, image origin at the canvas origin.
    ///
    /// A new tab is shown at 100% or fitted to the canvas (SPEC 7.2); which of
    /// the two applies depends on the image and the canvas size, so the canvas
    /// calls [`Viewport::fit_if_larger`] once it knows both.
    #[must_use]
    pub fn new() -> Self {
        Self {
            zoom: 1.0,
            origin: Vec2::ZERO,
            canvas_min: Vec2::ZERO,
            initialised: false,
        }
    }

    /// An image pixel as a point in the window.
    #[must_use]
    pub fn to_screen(self, point: Point) -> egui::Pos2 {
        pos2(
            point.x * self.zoom + self.origin.x + self.canvas_min.x,
            point.y * self.zoom + self.origin.y + self.canvas_min.y,
        )
    }

    /// A window position as an image pixel.
    #[must_use]
    pub fn to_image(self, pos: egui::Pos2) -> Point {
        Point::new(
            (pos.x - self.origin.x - self.canvas_min.x) / self.zoom,
            (pos.y - self.origin.y - self.canvas_min.y) / self.zoom,
        )
    }

    /// The image rectangle as a window rectangle.
    #[must_use]
    pub fn rect_to_screen(&self, rect: Rect) -> egui::Rect {
        let top_left = self.to_screen(Point::new(rect.x, rect.y));
        egui::Rect::from_min_size(
            egui::Pos2::new(top_left.x, top_left.y),
            vec2(rect.w * self.zoom, rect.h * self.zoom),
        )
    }

    /// The drawn size of a content rectangle at the current zoom.
    ///
    /// `content` is in image coordinates and may have a negative origin: with
    /// NEXT_STEPS step 10's expansion the view has to reach an annotation above
    /// or left of the image, which is at negative coordinates.
    #[must_use]
    pub fn drawn_size(&self, content: Rect) -> Vec2 {
        vec2(content.w * self.zoom, content.h * self.zoom)
    }

    /// Fits the whole of `content` in the canvas with `margin` points to spare
    /// (Ctrl+1, "Fit to Window").
    pub fn fit(&mut self, content: Rect, canvas: Vec2, margin: f32) {
        let (width, height) = (content.w, content.h);
        if width <= 0.0 || height <= 0.0 || canvas.x <= margin * 2.0 || canvas.y <= margin * 2.0 {
            return;
        }
        let available = vec2(canvas.x - margin * 2.0, canvas.y - margin * 2.0);
        let zoom = (available.x / width).min(available.y / height);
        self.zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        self.centre(content, canvas);
    }

    /// The zoom a new tab should start at (SPEC 7.2): 100%, or fitted when the
    /// content is larger than the canvas.
    pub fn fit_if_larger(&mut self, content: Rect, canvas: Vec2) {
        let drawn = self.drawn_size(content);
        if drawn.x > canvas.x || drawn.y > canvas.y {
            self.fit(content, canvas, 16.0);
        } else {
            self.zoom = 1.0;
            self.centre(content, canvas);
        }
    }

    /// Puts `content` in the middle of the canvas at the current zoom, or at
    /// its top-left corner when it is larger than the canvas.
    pub fn centre(&mut self, content: Rect, canvas: Vec2) {
        let drawn = self.drawn_size(content);
        let left = if drawn.x <= canvas.x {
            (canvas.x - drawn.x) / 2.0
        } else {
            0.0
        };
        let top = if drawn.y <= canvas.y {
            (canvas.y - drawn.y) / 2.0
        } else {
            0.0
        };
        self.origin = vec2(left - content.x * self.zoom, top - content.y * self.zoom);
    }

    /// "Actual size" (Ctrl+0).
    pub fn actual_size(&mut self, content: Rect, canvas: Vec2) {
        self.zoom = 1.0;
        self.centre(content, canvas);
    }

    /// Zooms by `factor`, keeping the image point under `cursor` in place
    /// (SPEC 7.2: Ctrl+wheel zooms around the cursor).
    ///
    /// `cursor` is a window position, which is what `to_screen` and `to_image`
    /// work in, so it can be used as it comes from the pointer.
    pub fn zoom_around(&mut self, factor: f32, cursor: egui::Pos2, content: Rect, canvas: Vec2) {
        let anchor = self.to_image(cursor);
        self.zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        // Put the anchor back under the cursor, then stop the view from sliding
        // so far that the content is gone.
        self.origin += cursor - self.to_screen(anchor);
        self.clamp_origin(content, canvas);
    }

    /// Scroll wheel: vertical pan, or horizontal with Shift held (SPEC 7.2).
    pub fn pan_by(&mut self, delta: Vec2, content: Rect, canvas: Vec2) {
        self.origin += delta;
        self.clamp_origin(content, canvas);
    }

    /// A middle-button or Space+drag pan (SPEC 7.2).
    pub fn pan_by_drag(&mut self, delta: Vec2, content: Rect, canvas: Vec2) {
        self.origin += delta;
        self.clamp_origin(content, canvas);
    }

    /// Keeps `content` reachable: when it is smaller than the canvas it is
    /// centred, otherwise it may be panned until an edge reaches the canvas
    /// edge but no further.
    pub fn clamp_origin(&mut self, content: Rect, canvas: Vec2) {
        let drawn = self.drawn_size(content);
        let left = clamp_axis(self.origin.x + content.x * self.zoom, canvas.x, drawn.x);
        let top = clamp_axis(self.origin.y + content.y * self.zoom, canvas.y, drawn.y);
        self.origin = vec2(left - content.x * self.zoom, top - content.y * self.zoom);
    }
}

/// Clamps one axis of the content's leading edge, in canvas space.
fn clamp_axis(edge: f32, canvas: f32, drawn: f32) -> f32 {
    if drawn <= canvas {
        // Smaller than the canvas: it stays centred, which is where it fits.
        (canvas - drawn) / 2.0
    } else {
        edge.clamp(canvas - drawn, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The content rectangle the tests mostly use: the image itself.
    const IMAGE: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 1000.0,
        h: 800.0,
    };
    const CANVAS: Vec2 = Vec2::new(800.0, 600.0);
    /// Where the canvas sits in the window: below the menu bar and the toolbar,
    /// right of the tools panel.
    const CANVAS_MIN: Vec2 = Vec2::new(150.0, 70.0);

    /// A viewport as the canvas leaves it: fitted, 100%, and placed in the
    /// window rather than at its origin.
    fn placed() -> Viewport {
        let mut viewport = Viewport::new();
        viewport.canvas_min = CANVAS_MIN;
        viewport.fit(IMAGE, CANVAS, 16.0);
        viewport
    }

    #[test]
    fn image_and_screen_points_round_trip() {
        let viewport = Viewport {
            zoom: 1.5,
            origin: Vec2::new(37.0, -12.0),
            canvas_min: CANVAS_MIN,
            initialised: true,
        };
        let image_point = Point::new(123.0, 456.0);
        let back = viewport.to_image(viewport.to_screen(image_point));
        assert!((back.x - image_point.x).abs() < 0.001);
        assert!((back.y - image_point.y).abs() < 0.001);
    }

    #[test]
    fn the_canvas_offset_moves_the_image_into_the_window() {
        // The reason `canvas_min` exists: the same image point must land at the
        // canvas's own position in the window, not at the window's origin.
        let viewport = placed();
        let top_left = viewport.to_screen(Point::new(0.0, 0.0));
        assert!((top_left.x - (CANVAS_MIN.x + viewport.origin.x)).abs() < 0.001);
        assert!((top_left.y - (CANVAS_MIN.y + viewport.origin.y)).abs() < 0.001);
    }

    #[test]
    fn a_fitted_image_is_inside_the_canvas_rectangle() {
        // SPEC R01: after a fit, the whole image must lie inside
        // [canvas_min, canvas_min + canvas] — not under the tools panel.
        let viewport = placed();
        let first = viewport.to_screen(Point::new(0.0, 0.0));
        let last = viewport.to_screen(Point::new(IMAGE.w, IMAGE.h));

        assert!(
            first.x >= CANVAS_MIN.x && last.x <= CANVAS_MIN.x + CANVAS.x,
            "horizontally the image spans {}..{}",
            first.x,
            last.x
        );
        assert!(
            first.y >= CANVAS_MIN.y && last.y <= CANVAS_MIN.y + CANVAS.y,
            "vertically the image spans {}..{}",
            first.y,
            last.y
        );
    }

    #[test]
    fn panning_can_reach_the_images_left_edge_exactly() {
        // The clamp works in canvas space, so the image's left edge can reach
        // the canvas's left edge — no more and no less.
        let mut viewport = Viewport {
            zoom: 1.0,
            origin: Vec2::new(40.0, 0.0),
            canvas_min: CANVAS_MIN,
            initialised: true,
        };
        viewport.clamp_origin(IMAGE, CANVAS);
        assert_eq!(viewport.origin.x, 0.0, "it over-panned to the right");
        let left = viewport.to_screen(Point::new(0.0, 0.0));
        assert!(
            (left.x - CANVAS_MIN.x).abs() < 0.001,
            "the image's left edge is at {}, the canvas's is at {}",
            left.x,
            CANVAS_MIN.x
        );

        // And the far side: panning left stops with the right edge at the
        // canvas's right edge.
        viewport.origin = Vec2::new(-4000.0, -4000.0);
        viewport.clamp_origin(IMAGE, CANVAS);
        let drawn = viewport.drawn_size(IMAGE);
        let right = viewport.to_screen(Point::new(IMAGE.w, 0.0));
        assert!((right.x - (CANVAS_MIN.x + CANVAS.x)).abs() < 0.001);
        assert!((viewport.origin.y - (CANVAS.y - drawn.y)).abs() < 0.001);
    }

    /// NEXT_STEPS step 10: with the image expanded to keep an annotation above
    /// it, the view has to be able to reach into the negative coordinates.
    #[test]
    fn an_expanded_content_rect_with_a_negative_origin_can_be_reached() {
        let content = Rect {
            x: -60.0,
            y: -120.0,
            w: IMAGE.w,
            h: IMAGE.h + 120.0,
        };
        let mut viewport = Viewport {
            zoom: 1.0,
            origin: Vec2::ZERO,
            canvas_min: CANVAS_MIN,
            initialised: true,
        };

        // The content is bigger than the canvas, so panning up-until the top
        // edge reaches the canvas top is allowed: that is where the annotation
        // above the image lives.
        viewport.clamp_origin(content, CANVAS);
        assert_eq!(viewport.origin.y, 0.0, "the bottom of the content");

        viewport.origin = Vec2::new(0.0, 500.0);
        viewport.clamp_origin(content, CANVAS);
        let top = viewport.to_screen(Point::new(0.0, content.y));
        assert!(
            (top.y - CANVAS_MIN.y).abs() < 0.001,
            "the top of an expanded content rect is at {}, the canvas top at {}",
            top.y,
            CANVAS_MIN.y
        );
        // And the annotation itself (in the negative strip) is on screen.
        let label = viewport.to_screen(Point::new(10.0, -40.0));
        assert!(
            label.y >= CANVAS_MIN.y && label.y <= CANVAS_MIN.y + CANVAS.y,
            "the label above the image is at y={}, outside the canvas",
            label.y
        );
    }

    #[test]
    fn a_content_rect_smaller_than_the_canvas_stays_centred() {
        // The same rule as before step 10, now measured from the content's
        // centre rather than the image's top-left corner.
        let content = Rect {
            x: -20.0,
            y: -10.0,
            w: 200.0,
            h: 100.0,
        };
        let mut viewport = Viewport {
            zoom: 1.0,
            origin: Vec2::ZERO,
            canvas_min: CANVAS_MIN,
            initialised: true,
        };
        viewport.clamp_origin(content, CANVAS);
        let centre = viewport.to_screen(content.center());
        assert!(
            (centre.x - (CANVAS_MIN.x + CANVAS.x / 2.0)).abs() < 0.001,
            "the content is not centred horizontally: {}",
            centre.x
        );
        assert!(
            (centre.y - (CANVAS_MIN.y + CANVAS.y / 2.0)).abs() < 0.001,
            "the content is not centred vertically: {}",
            centre.y
        );
    }

    #[test]
    fn zoom_around_keeps_the_point_under_the_cursor() {
        let mut viewport = Viewport::new();
        viewport.canvas_min = CANVAS_MIN;
        viewport.origin = Vec2::new(20.0, 30.0);
        let cursor = egui::pos2(411.0, 377.0);
        let anchor = viewport.to_image(cursor);

        viewport.zoom_around(1.2, cursor, IMAGE, CANVAS);

        let now = viewport.to_image(cursor);
        assert!((now.x - anchor.x).abs() < 0.01, "x moved from {}", anchor.x);
        assert!((now.y - anchor.y).abs() < 0.01, "y moved from {}", anchor.y);
        assert!((viewport.zoom - 1.2).abs() < 0.001);
    }

    #[test]
    fn zoom_is_clamped_to_the_documented_range() {
        let mut viewport = Viewport::new();
        viewport.canvas_min = CANVAS_MIN;
        let cursor = egui::pos2(10.0, 10.0);
        for _ in 0..200 {
            viewport.zoom_around(2.0, cursor, IMAGE, CANVAS);
        }
        assert!(
            (viewport.zoom - ZOOM_MAX).abs() < 0.001,
            "{}",
            viewport.zoom
        );

        for _ in 0..400 {
            viewport.zoom_around(0.5, cursor, IMAGE, CANVAS);
        }
        assert!(
            (viewport.zoom - ZOOM_MIN).abs() < 0.001,
            "{}",
            viewport.zoom
        );
    }

    #[test]
    fn fit_shows_the_whole_image() {
        let mut viewport = Viewport::new();
        viewport.canvas_min = CANVAS_MIN;
        viewport.fit(IMAGE, CANVAS, 16.0);

        let drawn = viewport.drawn_size(IMAGE);
        assert!(drawn.x <= CANVAS.x, "width {} does not fit", drawn.x);
        assert!(drawn.y <= CANVAS.y, "height {} does not fit", drawn.y);
        assert!(viewport.origin.x >= 0.0 && viewport.origin.y >= 0.0);
    }

    #[test]
    fn a_new_tab_is_at_full_size_when_the_image_fits() {
        let mut viewport = Viewport::new();
        viewport.canvas_min = CANVAS_MIN;
        viewport.fit_if_larger(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 400.0,
                h: 300.0,
            },
            CANVAS,
        );
        assert!((viewport.zoom - 1.0).abs() < 0.001, "{}", viewport.zoom);
    }

    #[test]
    fn a_new_tab_is_fitted_when_the_image_is_too_big() {
        let mut viewport = Viewport::new();
        viewport.canvas_min = CANVAS_MIN;
        let big = Rect {
            x: 0.0,
            y: 0.0,
            w: 4000.0,
            h: 3000.0,
        };
        viewport.fit_if_larger(big, CANVAS);
        assert!(viewport.zoom < 1.0, "{}", viewport.zoom);

        let drawn = viewport.drawn_size(big);
        assert!(drawn.x <= CANVAS.x && drawn.y <= CANVAS.y);
    }

    #[test]
    fn panning_cannot_lose_the_image() {
        // At 100% the 1000x800 image is larger than the 800x600 canvas, so
        // there is room to pan and room to get lost.
        let mut viewport = Viewport {
            zoom: 1.0,
            origin: Vec2::ZERO,
            canvas_min: CANVAS_MIN,
            initialised: true,
        };
        // Push it far away in both directions.
        for _ in 0..50 {
            viewport.pan_by(Vec2::new(-400.0, -400.0), IMAGE, CANVAS);
        }
        let drawn = viewport.drawn_size(IMAGE);
        assert_eq!(
            viewport.origin.x,
            CANVAS.x - drawn.x,
            "panning left should stop with the image's right edge at the canvas's right edge"
        );

        for _ in 0..200 {
            viewport.pan_by(Vec2::new(400.0, 400.0), IMAGE, CANVAS);
        }
        assert_eq!(
            viewport.origin.x, 0.0,
            "panning right should stop with the image's left edge at the canvas's left edge"
        );
        assert!(
            viewport.origin.y <= 0.0,
            "the image should never start below the canvas"
        );
    }

    #[test]
    fn a_smaller_than_canvas_image_stays_centred_while_panning() {
        let mut viewport = Viewport::new();
        viewport.canvas_min = CANVAS_MIN;
        let small = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        viewport.pan_by(Vec2::new(500.0, 500.0), small, CANVAS);
        assert!((viewport.origin.x - (CANVAS.x - 100.0) / 2.0).abs() < 0.001);
    }

    #[test]
    fn a_drag_pan_uses_the_per_frame_delta_not_the_total() {
        // R02: three frames of +5 points move the image 15 points, not 30.
        let mut viewport = Viewport {
            zoom: 1.0,
            origin: Vec2::new(-50.0, -50.0),
            canvas_min: CANVAS_MIN,
            initialised: true,
        };
        for _ in 0..3 {
            viewport.pan_by_drag(Vec2::new(5.0, 5.0), IMAGE, CANVAS);
        }
        assert!(
            (viewport.origin.x - -35.0).abs() < 0.001,
            "{}",
            viewport.origin.x
        );
        assert!(
            (viewport.origin.y - -35.0).abs() < 0.001,
            "{}",
            viewport.origin.y
        );
    }
}
