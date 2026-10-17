//! The tools: what the pointer does to the document (SPEC section 7.3).
//!
//! Each tool is a module. They all follow the same contract:
//!
//! - A **gesture** starts on the press, pushes one undo snapshot, changes the
//!   document while the pointer moves, and ends on the release (SPEC 7.3: "each
//!   completed gesture is one undo step").
//! - A drag shorter than 3 screen px creates nothing.
//! - With a drawing tool active, pressing on a **handle of the current
//!   selection** manipulates that handle instead of starting a new annotation.

pub mod crop;
pub mod draw;
pub mod freehand;
pub mod number;
pub mod select;
pub mod shape;
pub mod text;

use egui::Vec2;
use snp_ligase_core::edit::{self, HandleId, HandleKind};
use snp_ligase_core::geom::Point;
use snp_ligase_core::model::{Annotation, Kind};
use snp_ligase_core::text::Fonts;

use crate::canvas::CanvasInput;
use crate::tab::Tab;
use crate::tool::{Tool, ToolStyles};

/// A drag shorter than this many screen points creates nothing (SPEC 7.3).
pub const MIN_DRAG_POINTS: f32 = 3.0;

/// The hit tolerance for selecting an annotation, in screen points (SPEC 7.3).
pub const HIT_TOLERANCE_POINTS: f32 = 4.0;

/// How close the pointer has to be to a handle, in screen points. A handle is a
/// small target, so it gets a slightly wider tolerance than an annotation does.
pub const HANDLE_TOLERANCE_POINTS: f32 = 7.0;

/// The angle Shift snaps to while dragging, in degrees (SPEC 7.3).
pub const SNAP_STEP_DEGREES: f32 = 15.0;

/// What the pointer is currently doing.
#[derive(Debug, Clone, Default)]
pub enum Gesture {
    #[default]
    None,
    /// A drawing tool is dragging out a shape.
    Drawing { start: Point },
    /// The Freehand tool is collecting one stroke's pointer samples
    /// (NEXT_STEPS step 14b).
    ///
    /// Unlike [`Gesture::Drawing`] this is not two points: it is every sample
    /// the pointer reported while the button was held, deduplicated in screen
    /// points. Nothing is added to the document until the release, which is
    /// also where the Ramer–Douglas–Peucker simplification runs.
    Freehand { points: Vec<Point> },
    /// The Shape tool is collecting vertices, one click each (SPEC 7.3).
    ///
    /// Unlike the other gestures this one is not held by the mouse button: it
    /// lasts until the shape is closed, `Enter`, `Backspace` or `Esc`.
    Polygon { points: Vec<Point> },
    /// The Number tool is being pressed, so a drag can add a pointer
    /// (SPEC 7.3): the badge goes at `center`, the tip at the release point.
    NumberDrag { center: Point },
    /// The Crop tool is dragging out a new rectangle (SPEC 7.3).
    CropDraw { from: Point, to: Point },
    /// The Crop tool has a rectangle to adjust; `handle` is the edge being
    /// dragged, if any (SPEC 7.3).
    CropPending {
        rect: snp_ligase_core::geom::Rect,
        handle: Option<snp_ligase_core::edit::ResizeDir>,
    },
    /// The Text tool is waiting for its click to finish before the editor
    /// opens (SPEC 7.5).
    ///
    /// The editor must not open while the click that created the annotation is
    /// still in flight: egui surrenders a focused widget's focus on the
    /// *release* of a click outside it (`SurrenderFocusOn::Clicks`), so the
    /// brand-new editor would lose focus immediately and commit itself shut.
    TextPlace { pos: Point },
    /// The Text tool placed an annotation (or a double-click found one) and the
    /// app should open the floating editor (SPEC 7.5).
    ///
    /// `snapshot` is the document as it was before the annotation existed, so a
    /// whole editing session is one undo step.
    TextEdit {
        id: u64,
        created: bool,
        snapshot: Box<snp_ligase_core::model::Document>,
    },
    /// The selection is being moved by the pointer.
    Moving { from: Point, last: Point },
    /// A handle of the selected annotation is being dragged.
    ///
    /// Both the index into the handle list and what that handle *is* are kept,
    /// because inserting a point shifts that list under the pointer: `id` is
    /// only valid until that happens, and `kind` says which point is moving.
    Handle {
        id: HandleId,
        kind: HandleKind,
        /// Where the press was, in image pixels. A handle only starts moving
        /// the annotation once the pointer has moved away from here, so a plain
        /// click (which SPEC 7.3 says creates nothing) cannot insert an
        /// "add point" point or nudge a point under the pointer.
        from: Point,
        /// True once an "add point" handle has inserted its point. From then on
        /// the drag moves that new point instead of inserting another, because
        /// `id` no longer refers to the same handle.
        inserted: bool,
    },
}

impl Gesture {
    /// True when a gesture is in progress.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// True while the selection is being moved by the pointer.
    ///
    /// This is the one change a sprite cache may reuse pixels for (SPEC 12 M7,
    /// H4): a move translates the annotation, so the cached texture can be
    /// drawn at a shifted origin while the drag runs. Once the move ends, the
    /// sprite must be rasterized at its real position again — a fractional
    /// offset drawn from the cache is resampled and is not what a save writes.
    #[must_use]
    pub fn is_moving(&self) -> bool {
        matches!(self, Self::Moving { .. })
    }

    /// True when the gesture is held by the mouse button, so it must end the
    /// moment no button is down.
    ///
    /// The Shape tool's vertices and the Crop tool's pending rectangle are
    /// *click* gestures: they wait, with the button up, for the next click.
    /// Treating them like a drag made every click start a new shape (found by
    /// driving the real window).
    #[must_use]
    pub fn needs_button(&self) -> bool {
        matches!(
            self,
            Self::Drawing { .. }
                | Self::Freehand { .. }
                | Self::Moving { .. }
                | Self::Handle { .. }
                | Self::NumberDrag { .. }
                | Self::CropDraw { .. }
                | Self::TextPlace { .. }
        )
    }

    /// Where a drawing gesture started, for the canvas preview.
    #[must_use]
    pub fn drawing_from(&self) -> Option<Point> {
        match self {
            Self::Drawing { start } => Some(*start),
            _ => None,
        }
    }

    /// The samples of the stroke being drawn, for the canvas preview
    /// (NEXT_STEPS step 14b).
    #[must_use]
    pub fn freehand_points(&self) -> Option<&[Point]> {
        match self {
            Self::Freehand { points } => Some(points),
            _ => None,
        }
    }

    /// The vertices of the shape being drawn, for the canvas preview
    /// (SPEC 7.3).
    #[must_use]
    pub fn polygon_points(&self) -> Option<&[Point]> {
        match self {
            Self::Polygon { points } => Some(points),
            _ => None,
        }
    }

    /// The crop rectangle to draw, whether it is still being dragged or is
    /// waiting to be applied (SPEC 7.3).
    #[must_use]
    pub fn crop_rect(&self) -> Option<snp_ligase_core::geom::Rect> {
        match self {
            Self::CropDraw { from, to } => Some(snp_ligase_core::geom::Rect::from_ltrb(
                from.x, from.y, to.x, to.y,
            )),
            Self::CropPending { rect, .. } => Some(*rect),
            _ => None,
        }
    }
}

/// Runs the active tool for one frame.
///
/// Returns a status-bar message when something worth saying happened.
/// `active_point` is the point handle that is currently picked out, which is
/// what `Delete` removes when one is set.
pub fn handle(
    tab: &mut Tab,
    tool: Tool,
    styles: &ToolStyles,
    input: &CanvasInput,
    fonts: &mut Fonts,
    gesture: &mut Gesture,
    active_point: &mut Option<HandleKind>,
) -> Option<String> {
    // A gesture outlives the frame it started in: if the button has gone up
    // without the tool seeing it — released outside the window, or focus lost
    // mid-drag — the gesture has to end anyway, or it keeps following a mouse
    // button nobody is holding (R16). Click gestures (the Shape tool's
    // vertices, the Crop tool's rectangle) are not held by the button at all.
    if gesture.needs_button() && !input.primary_down && !input.primary_released {
        *gesture = Gesture::None;
        return None;
    }

    // Panning is a view operation, not an edit, so it applies whatever the
    // active tool is (SPEC 7.2).
    if handle_panning(tab, input) {
        *gesture = Gesture::None;
        return None;
    }

    // A double click on the selected path or polygon inserts a point there
    // (SPEC 7.3). It is handled on the frame `double_clicked` is true, which is
    // the *release* frame of the second click, so it cannot be checked where a
    // press is handled (R13). A double click on a text annotation opens the
    // editor instead (SPEC 7.5).
    //
    // While the Shape tool is collecting vertices, the double click belongs to
    // the Shape tool: it closes the shape being drawn (SPEC 7.3). The previous
    // polygon is still selected after it was closed, so letting the shared
    // handler run first inserted a point into the *old* shape and left the new
    // one open (F2).
    if input.double_clicked && !matches!(gesture, Gesture::Polygon { .. }) {
        if let Some(message) = select::double_click_point(tab, input) {
            return Some(message);
        }
        if let Some(message) = text::double_click(tab, input, fonts, gesture) {
            return Some(message);
        }
    }

    // A handle of the current selection wins over the active tool (SPEC 7.3).
    if handle_handle_gesture(tab, input, gesture, active_point) {
        return None;
    }

    match tool {
        Tool::Select => select::handle(tab, input, fonts, gesture, active_point),
        Tool::Line | Tool::Arrow => draw::path(tab, styles, input, gesture, tool),
        Tool::Rectangle => draw::rectangle(tab, styles, input, gesture),
        Tool::Highlight => draw::highlight(tab, styles, input, gesture),
        Tool::Blur => draw::blur(tab, styles, input, gesture),
        Tool::Freehand => freehand::handle(tab, styles, input, gesture),
        Tool::Shape => shape::handle(tab, styles, input, gesture),
        Tool::Number => number::handle(tab, styles, input, gesture),
        Tool::Crop => crop::handle(tab, input, gesture),
        Tool::Text => text::handle(tab, styles, input, gesture),
    }
}

/// Zoom and pan (SPEC 7.2): Ctrl+wheel zooms around the cursor, the wheel pans,
/// Shift+wheel pans sideways, and the middle button or Space+drag pans.
///
/// The clamp uses `input.content`, not the image: with NEXT_STEPS step 10's
/// expansion that is the export area, so an annotation outside the image can be
/// scrolled to.
fn handle_panning(tab: &mut Tab, input: &CanvasInput) -> bool {
    let content = input.content;
    let canvas = input.canvas;
    let mut handled = false;

    if input.ctrl && input.zoom_delta != 1.0 {
        // egui turns Ctrl+scroll into a *zoom factor* rather than a scroll delta,
        // so `smooth_scroll_delta` is zero while Ctrl is held and the wheel branch
        // below would never fire (R03).
        if let Some(cursor) = input.pointer {
            // NEXT_STEPS step 13: this is a manual zoom, so auto-fit stands
            // down for this tab.
            tab.user_zoomed();
            tab.viewport
                .zoom_around(input.zoom_delta.clamp(0.8, 1.25), cursor, content, canvas);
        }
        handled = true;
    } else if input.wheel.x.abs() > 0.0 || input.wheel.y.abs() > 0.0 {
        let delta = if input.shift {
            Vec2::new(-input.wheel.x, 0.0)
        } else {
            Vec2::new(-input.wheel.x, -input.wheel.y)
        };
        tab.viewport.pan_by(delta, content, canvas);
        handled = true;
    }

    if input.middle_down || (input.space_held && input.primary_down) {
        // The per-frame delta, not the total since the press: the total made the
        // view accelerate, so a 10 px drag ran away with itself (R02).
        tab.viewport.pan_by_drag(input.drag_delta, content, canvas);
        handled = true;
    }

    handled
}

/// Starts and continues a handle drag.
///
/// Returns `true` when the press belongs to a handle, which stops the active
/// tool from acting on the same press.
fn handle_handle_gesture(
    tab: &mut Tab,
    input: &CanvasInput,
    gesture: &mut Gesture,
    active_point: &mut Option<HandleKind>,
) -> bool {
    // Continue a drag that is already under way.
    if let Gesture::Handle {
        id,
        kind,
        from,
        inserted,
    } = *gesture
    {
        let point = input.image_pointer;
        // A release ends the gesture even when the pointer has left the canvas,
        // so `image_pointer` must be checked *after* it (R16).
        if input.primary_released {
            *gesture = Gesture::None;
            return true;
        }
        let Some(pointer) = point else {
            return true;
        };
        // SPEC 7.3: a drag shorter than 3 screen points creates nothing. Until
        // the pointer has really moved, the handle does not touch the document:
        // a click must not insert an "add point" point or shift a point to the
        // click position, and with the staged snapshot (R20) it then costs no
        // undo step either.
        let dragging = dragged_far_enough(tab, from, pointer);
        let point = match input.shift {
            // SPEC 7.3: Shift snaps a dragged point to 15-degree steps, around
            // the annotation's first point.
            true => snap_to_angle(anchor_of(tab), pointer),
            false => pointer,
        };
        if let Some(annotation) = tab.selected_mut() {
            if inserted {
                // The point is already in the list: move exactly that one.
                move_point(annotation, kind, point);
            } else if matches!(kind, HandleKind::AddPoint(_)) {
                // The first real move inserts the point (SPEC 7.3: dragging an
                // "add point" handle inserts a new point there and keeps
                // dragging it).
                if dragging {
                    edit::drag_handle(annotation, id, point);
                    if let Gesture::Handle { inserted, .. } = gesture {
                        *inserted = true;
                    }
                }
            } else if dragging {
                edit::drag_handle(annotation, id, point);
            }
        }
        if input.primary_released {
            *gesture = Gesture::None;
        }
        return true;
    }

    if !input.primary_pressed {
        return false;
    }
    let [selected] = tab.selection() else {
        return false;
    };
    let Some(annotation) = tab.document.find(*selected).cloned() else {
        return false;
    };
    let Some(pointer) = input.pointer else {
        return false;
    };
    let Some(from) = input.image_pointer else {
        return false;
    };

    let handles = edit::handles(&annotation);
    let Some(position) = nearest_handle(tab, &handles, pointer) else {
        return false;
    };

    let kind = handles[position].kind;
    // Clicking a point picks it out, so `Delete` removes that point and nothing
    // else. The canvas draws it differently to show which one it is.
    if matches!(kind, HandleKind::Point(_)) {
        *active_point = Some(kind);
    }

    // One undo step for the whole drag (SPEC 7.3). The snapshot is *staged*, not
    // pushed: a press that turns out to change nothing must leave the undo
    // stack alone (R20).
    tab.stage_edit();
    *gesture = Gesture::Handle {
        id: HandleId(position),
        kind,
        from,
        inserted: false,
    };
    true
}

/// The handle closest to the pointer, if it is within reach.
///
/// The distance is in **screen points**: `point_distance` has already mapped the
/// handle through the viewport, so multiplying by the zoom again made handles
/// ungrabbable at 400% and greedy at 25% (R12). The nearest one wins, so a
/// cluster of handles does not depend on list order.
fn nearest_handle(tab: &Tab, handles: &[edit::Handle], pointer: egui::Pos2) -> Option<usize> {
    handles
        .iter()
        .enumerate()
        .map(|(index, handle)| (index, point_distance(tab, handle.at, pointer)))
        .filter(|(_, distance)| *distance <= HANDLE_TOLERANCE_POINTS)
        .min_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(index, _)| index)
}

/// The point angle snapping is measured from: the annotation's first point.
fn anchor_of(tab: &Tab) -> Point {
    tab.selected()
        .and_then(first_point)
        .unwrap_or(Point::new(0.0, 0.0))
}

/// The annotation's first point, for the kinds that have one.
fn first_point(annotation: &Annotation) -> Option<Point> {
    match &annotation.kind {
        Kind::Path(path) => path.points.first().copied(),
        Kind::Polygon(polygon) => polygon.points.first().copied(),
        Kind::Text(text) => Some(text.pos),
        Kind::Number(number) => Some(number.center),
        _ => None,
    }
}

/// Moves the one point a handle refers to.
///
/// For an "add point" handle the point landed at `segment + 1`, which is where
/// it is looked for.
fn move_point(annotation: &mut Annotation, kind: HandleKind, point: Point) {
    let index = match kind {
        HandleKind::Point(index) => index,
        HandleKind::AddPoint(at) => at + 1,
        _ => return,
    };
    match &mut annotation.kind {
        Kind::Path(path) => {
            if let Some(target) = path.points.get_mut(index) {
                *target = point;
            }
        }
        Kind::Polygon(polygon) => {
            if let Some(target) = polygon.points.get_mut(index) {
                *target = point;
            }
        }
        _ => {}
    }
}

/// How far, in image pixels, `image_point` is from a canvas position.
fn point_distance(tab: &Tab, image_point: Point, screen: egui::Pos2) -> f32 {
    let mapped = tab.viewport.to_screen(image_point);
    ((mapped.x - screen.x).powi(2) + (mapped.y - screen.y).powi(2)).sqrt()
}

/// The topmost annotation under the pointer (SPEC 7.3).
///
/// The search goes from the top of the z-order down, so the annotation the user
/// can see is the one that gets selected. Text and Number need the font system
/// to be measured, which is why it comes in as an argument (SPEC 7.5).
#[must_use]
pub fn annotation_at(
    tab: &Tab,
    point: Point,
    tolerance_in_pixels: f32,
    fonts: &mut Fonts,
) -> Option<u64> {
    tab.document
        .annotations
        .iter()
        .rev()
        .find(|annotation| edit::hit_test(annotation, point, tolerance_in_pixels, fonts))
        .map(|annotation| annotation.id)
}

/// A screen-space tolerance in image pixels.
#[must_use]
pub fn tolerance_in_pixels(tolerance_points: f32, zoom: f32) -> f32 {
    tolerance_points / zoom.max(0.0001)
}

/// Whether a drag was long enough to count as a gesture (SPEC 7.3).
#[must_use]
pub fn dragged_far_enough(tab: &Tab, from: Point, to: Point) -> bool {
    dragged_far_enough_by(tab, from, to, MIN_DRAG_POINTS)
}

/// Whether a drag covered at least `points` screen points.
///
/// Measured in screen points, so the threshold means the same thing at every
/// zoom (SPEC 7.3).
#[must_use]
pub fn dragged_far_enough_by(tab: &Tab, from: Point, to: Point, points: f32) -> bool {
    (tab.viewport.to_screen(from) - tab.viewport.to_screen(to)).length() >= points
}

/// Snaps `to` to the nearest multiple of [`SNAP_STEP_DEGREES`] away from `from`,
/// keeping the distance (SPEC 7.3: "Shift while dragging a path point or creating
/// a line snaps the angle to 15° steps").
#[must_use]
pub fn snap_to_angle(from: Point, to: Point) -> Point {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let distance = (dx * dx + dy * dy).sqrt();
    if distance <= f32::EPSILON {
        return to;
    }
    let step = SNAP_STEP_DEGREES.to_radians();
    let angle = (dy.atan2(dx) / step).round() * step;
    Point::new(
        from.x + distance * angle.cos(),
        from.y + distance * angle.sin(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled test font, so no test depends on the machine (SPEC 11.3).
    fn fonts() -> Fonts {
        Fonts::bundled()
    }

    /// H4: only a move gesture lets the sprite cache reuse pixels. Every other
    /// gesture changes the shape in some way.
    #[test]
    fn only_a_move_gesture_counts_as_moving() {
        let from = Point::new(0.0, 0.0);
        let gestures = [
            Gesture::None,
            Gesture::Drawing { start: from },
            Gesture::Freehand { points: vec![from] },
            Gesture::Polygon { points: vec![from] },
            Gesture::NumberDrag { center: from },
            Gesture::CropDraw { from, to: from },
            Gesture::TextPlace { pos: from },
            Gesture::Handle {
                id: HandleId(0),
                kind: HandleKind::Point(0),
                from,
                inserted: false,
            },
        ];
        for gesture in gestures {
            assert!(!gesture.is_moving(), "{gesture:?} is not a move");
        }
        assert!(
            Gesture::Moving {
                from,
                last: Point::new(5.0, 5.0),
            }
            .is_moving()
        );
    }

    #[test]
    fn snapping_keeps_the_distance_and_rounds_the_angle() {
        let from = Point::new(0.0, 0.0);
        // 40 degrees snaps to 45, at the same distance.
        let to = Point::new(100.0, 83.9);
        let snapped = snap_to_angle(from, to);
        let distance = ((snapped.x).powi(2) + (snapped.y).powi(2)).sqrt();
        let original = (to.x.powi(2) + to.y.powi(2)).sqrt();
        assert!((distance - original).abs() < 0.01, "distance changed");

        let angle = snapped.y.atan2(snapped.x).to_degrees();
        assert!(
            (angle - 45.0).abs() < 0.01,
            "40 degrees became {angle} degrees"
        );
    }

    #[test]
    fn snapping_leaves_exact_multiples_alone() {
        let from = Point::new(10.0, 10.0);
        let to = Point::new(10.0, 110.0); // straight up: -90 degrees
        let snapped = snap_to_angle(from, to);
        assert!((snapped.x - to.x).abs() < 0.001, "x moved");
        assert!((snapped.y - to.y).abs() < 0.001, "y moved");
    }

    #[test]
    fn snapping_a_point_onto_itself_is_harmless() {
        let at = Point::new(5.0, 5.0);
        let snapped = snap_to_angle(at, at);
        assert_eq!(snapped, at);
    }

    /// A tab with a large rectangle, selected, for the handle tests.
    fn tab_with_a_rectangle() -> Tab {
        use snp_ligase_core::model::{Document, RectA, Stroke};
        let mut tab = Tab::new(
            Document::new(image::RgbaImage::new(1000, 1000)),
            "test".to_owned(),
        );
        let id = tab.document.push(Kind::Rect(RectA {
            rect: snp_ligase_core::geom::Rect::new(100.0, 100.0, 400.0, 400.0),
            stroke: Some(Stroke {
                color: [255, 0, 0, 255],
                width: 2.0,
            }),
            fill: None,
            corner_radius: 0.0,
        }));
        tab.select_only(Some(id));
        tab
    }

    #[test]
    fn a_handle_grabs_within_five_points_and_not_ten_at_any_zoom() {
        // R12: the distance is already in screen points, so the tolerance must
        // not be scaled by the zoom a second time.
        for zoom in [0.25_f32, 4.0] {
            let mut tab = tab_with_a_rectangle();
            tab.viewport.zoom = zoom;
            let annotation = tab.document.annotations[0].clone();
            let handles = edit::handles(&annotation);
            let at = tab.viewport.to_screen(handles[0].at);

            assert_eq!(
                nearest_handle(&tab, &handles, egui::pos2(at.x + 5.0, at.y)),
                Some(0),
                "5 points away at zoom {zoom}"
            );
            assert_eq!(
                nearest_handle(&tab, &handles, egui::pos2(at.x + 10.0, at.y)),
                None,
                "10 points away at zoom {zoom}"
            );
        }
    }

    #[test]
    fn ctrl_wheel_zooms_by_the_zoom_delta_around_the_pointer() {
        // R03: egui turns Ctrl+scroll into `zoom_delta`, never a scroll delta.
        let mut tab = Tab::new(
            snp_ligase_core::model::Document::new(image::RgbaImage::new(2000, 1500)),
            "test".to_owned(),
        );
        tab.viewport.origin = Vec2::new(-100.0, -80.0);
        let cursor = egui::pos2(400.0, 300.0);
        let anchor = tab.viewport.to_image(cursor);
        assert!(tab.auto_fit, "a new tab starts with auto-fit on");

        let input = CanvasInput {
            pointer: Some(cursor),
            ctrl: true,
            zoom_delta: 1.2,
            // The view content and canvas the app would have given the tools:
            // panning and zooming clamp to them (NEXT_STEPS step 10 made the
            // content rect explicit).
            content: snp_ligase_core::geom::Rect::new(0.0, 0.0, 2000.0, 1500.0),
            ..CanvasInput::default()
        };
        let mut gesture = Gesture::None;
        let mut active_point = None;
        handle(
            &mut tab,
            Tool::Select,
            &ToolStyles::default(),
            &input,
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );

        assert!(
            (tab.viewport.zoom - 1.2).abs() < 1e-4,
            "zoom is {}",
            tab.viewport.zoom
        );
        let now = tab.viewport.to_image(cursor);
        assert!((now.x - anchor.x).abs() < 0.01, "x moved");
        assert!((now.y - anchor.y).abs() < 0.01, "y moved");
        // NEXT_STEPS step 13: Ctrl+wheel is a manual zoom, so auto-fit stands
        // down for this tab.
        assert!(
            !tab.auto_fit,
            "the Ctrl+wheel zoom did not turn auto-fit off"
        );
    }

    /// NEXT_STEPS step 13: panning is not a zoom, so it must leave auto-fit
    /// alone — the wheel, Shift+wheel and a middle-drag all keep it on.
    #[test]
    fn panning_leaves_auto_fit_on() {
        let cases = [
            CanvasInput {
                wheel: Vec2::new(0.0, 40.0),
                ..CanvasInput::default()
            },
            CanvasInput {
                wheel: Vec2::new(40.0, 0.0),
                shift: true,
                ..CanvasInput::default()
            },
            CanvasInput {
                middle_down: true,
                drag_delta: Vec2::new(5.0, 5.0),
                ..CanvasInput::default()
            },
        ];
        for input in cases {
            let mut tab = Tab::new(
                snp_ligase_core::model::Document::new(image::RgbaImage::new(2000, 1500)),
                "test".to_owned(),
            );
            tab.auto_fit = true;
            let input = CanvasInput {
                content: snp_ligase_core::geom::Rect::new(0.0, 0.0, 2000.0, 1500.0),
                ..input
            };
            let mut gesture = Gesture::None;
            let mut active_point = None;
            handle(
                &mut tab,
                Tool::Select,
                &ToolStyles::default(),
                &input,
                &mut fonts(),
                &mut gesture,
                &mut active_point,
            );
            assert!(tab.auto_fit, "panning ({input:?}) turned auto-fit off");
        }
    }

    #[test]
    fn a_release_with_no_pointer_ends_the_gesture() {
        // R16: the button can come up outside the canvas, where there is no
        // pointer position; the gesture still has to end.
        let mut tab = Tab::new(
            snp_ligase_core::model::Document::new(image::RgbaImage::new(100, 100)),
            "test".to_owned(),
        );
        let mut gesture = Gesture::Moving {
            from: Point::new(10.0, 10.0),
            last: Point::new(10.0, 10.0),
        };
        let input = CanvasInput {
            primary_released: true,
            ..CanvasInput::default()
        };
        let mut active_point = None;
        handle(
            &mut tab,
            Tool::Select,
            &ToolStyles::default(),
            &input,
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        assert!(
            !gesture.is_active(),
            "a release with no pointer left the gesture stuck"
        );
    }

    #[test]
    fn a_click_gesture_survives_the_button_going_up() {
        // Found by driving the real window: the Shape tool's vertices arrive as
        // separate clicks, so the "no button down" guard must not wipe the
        // gesture between them — every click started a new shape instead.
        for gesture in [
            Gesture::Polygon {
                points: vec![Point::new(10.0, 10.0)],
            },
            Gesture::CropPending {
                rect: snp_ligase_core::geom::Rect::new(0.0, 0.0, 10.0, 10.0),
                handle: None,
            },
        ] {
            assert!(!gesture.needs_button(), "{gesture:?} is a click gesture");
        }
        for gesture in [
            Gesture::Drawing {
                start: Point::new(0.0, 0.0),
            },
            Gesture::Freehand {
                points: vec![Point::new(0.0, 0.0)],
            },
            Gesture::Moving {
                from: Point::new(0.0, 0.0),
                last: Point::new(1.0, 1.0),
            },
            Gesture::NumberDrag {
                center: Point::new(0.0, 0.0),
            },
            Gesture::CropDraw {
                from: Point::new(0.0, 0.0),
                to: Point::new(1.0, 1.0),
            },
        ] {
            assert!(gesture.needs_button(), "{gesture:?} is held by the mouse");
        }

        // And the guard really leaves a click gesture alone.
        let mut tab = Tab::new(
            snp_ligase_core::model::Document::new(image::RgbaImage::new(100, 100)),
            "test".to_owned(),
        );
        let mut gesture = Gesture::Polygon {
            points: vec![Point::new(10.0, 10.0)],
        };
        let mut active_point = None;
        handle(
            &mut tab,
            Tool::Shape,
            &ToolStyles::default(),
            &CanvasInput::default(),
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        assert!(
            gesture.is_active(),
            "a frame with no button down wiped the shape"
        );
    }

    #[test]
    fn a_gesture_ends_when_no_button_is_down_any_more() {
        let mut tab = Tab::new(
            snp_ligase_core::model::Document::new(image::RgbaImage::new(100, 100)),
            "test".to_owned(),
        );
        let mut gesture = Gesture::Drawing {
            start: Point::new(10.0, 10.0),
        };
        let mut active_point = None;
        handle(
            &mut tab,
            Tool::Select,
            &ToolStyles::default(),
            &CanvasInput::default(),
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        assert!(
            !gesture.is_active(),
            "the gesture followed a mouse nobody holds"
        );
    }

    /// A two-point straight path, selected, at 100% with no pan, so image
    /// coordinates and screen points are the same numbers.
    fn tab_with_a_line() -> Tab {
        use snp_ligase_core::model::{Document, PathA, Stroke};
        let mut tab = Tab::new(
            Document::new(image::RgbaImage::new(200, 200)),
            "test".to_owned(),
        );
        let id = tab.document.push(Kind::Path(PathA {
            points: vec![Point::new(10.0, 10.0), Point::new(150.0, 10.0)],
            curved: false,
            stroke: Stroke {
                color: [255, 0, 0, 255],
                width: 2.0,
            },
            head_start: false,
            head_end: false,
        }));
        tab.select_only(Some(id));
        tab
    }

    #[test]
    fn a_click_on_an_add_point_handle_inserts_nothing_and_costs_no_undo_step() {
        // A press on the small hollow handle must not insert on its own: SPEC
        // 7.3 says *dragging* it inserts, and a click that changes nothing must
        // not cost an undo step (R20).
        let mut tab = tab_with_a_line();
        let id = tab.document.annotations[0].id;
        let add = edit::handles(&tab.document.annotations[0])
            .into_iter()
            .find(|handle| matches!(handle.kind, HandleKind::AddPoint(0)))
            .expect("an add-point handle");
        let at = tab.viewport.to_screen(add.at);

        let mut gesture = Gesture::None;
        let mut active_point = None;
        let styles = ToolStyles::default();
        let press = CanvasInput {
            pointer: Some(at),
            image_pointer: Some(add.at),
            primary_pressed: true,
            primary_down: true,
            ..CanvasInput::default()
        };
        let release = CanvasInput {
            pointer: Some(at),
            image_pointer: Some(add.at),
            primary_released: true,
            ..CanvasInput::default()
        };
        handle(
            &mut tab,
            Tool::Select,
            &styles,
            &press,
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        handle(
            &mut tab,
            Tool::Select,
            &styles,
            &release,
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        tab.commit_edit();

        let Kind::Path(path) = &tab.document.find(id).expect("still there").kind else {
            panic!("expected a path");
        };
        assert_eq!(path.points.len(), 2, "a click inserted a point");
        assert_eq!(tab.undo_depth_for_test(), 0, "a click cost an undo step");
    }

    #[test]
    fn a_drag_on_an_add_point_handle_inserts_one_point_for_one_undo_step() {
        let mut tab = tab_with_a_line();
        let id = tab.document.annotations[0].id;
        let add = edit::handles(&tab.document.annotations[0])
            .into_iter()
            .find(|handle| matches!(handle.kind, HandleKind::AddPoint(0)))
            .expect("an add-point handle");
        let at = tab.viewport.to_screen(add.at);
        let to = Point::new(add.at.x, add.at.y - 40.0);

        let mut gesture = Gesture::None;
        let mut active_point = None;
        let styles = ToolStyles::default();
        let press = CanvasInput {
            pointer: Some(at),
            image_pointer: Some(add.at),
            primary_pressed: true,
            primary_down: true,
            ..CanvasInput::default()
        };
        let moved = CanvasInput {
            pointer: Some(tab.viewport.to_screen(to)),
            image_pointer: Some(to),
            primary_down: true,
            ..CanvasInput::default()
        };
        let release = CanvasInput {
            pointer: Some(tab.viewport.to_screen(to)),
            image_pointer: Some(to),
            primary_released: true,
            ..CanvasInput::default()
        };
        handle(
            &mut tab,
            Tool::Select,
            &styles,
            &press,
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        handle(
            &mut tab,
            Tool::Select,
            &styles,
            &moved,
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        handle(
            &mut tab,
            Tool::Select,
            &styles,
            &release,
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        tab.commit_edit();

        let Kind::Path(path) = &tab.document.find(id).expect("still there").kind else {
            panic!("expected a path");
        };
        assert_eq!(path.points.len(), 3, "the drag did not insert a point");
        assert_eq!(tab.undo_depth_for_test(), 1, "not exactly one undo step");
        assert!(tab.undo());
        let Kind::Path(path) = &tab.document.find(id).expect("still there").kind else {
            panic!("expected a path");
        };
        assert_eq!(path.points.len(), 2, "undo did not remove the point");
    }

    #[test]
    fn a_double_click_inserts_one_point_for_one_undo_step() {
        // R13: the double-click arrives on the release frame of the second
        // click, and it must not leave an empty undo step behind.
        use snp_ligase_core::model::{Document, PathA, Stroke};
        let mut tab = Tab::new(
            Document::new(image::RgbaImage::new(200, 200)),
            "test".to_owned(),
        );
        let id = tab.document.push(Kind::Path(PathA {
            points: vec![Point::new(10.0, 10.0), Point::new(150.0, 10.0)],
            curved: false,
            stroke: Stroke {
                color: [255, 0, 0, 255],
                width: 2.0,
            },
            head_start: false,
            head_end: false,
        }));
        tab.select_only(Some(id));

        let styles = ToolStyles::default();
        let at = Point::new(40.0, 10.0);
        let mut gesture = Gesture::None;
        let mut active_point = None;
        let press = CanvasInput {
            pointer: Some(egui::pos2(at.x, at.y)),
            image_pointer: Some(at),
            primary_pressed: true,
            primary_down: true,
            ..CanvasInput::default()
        };
        let release = CanvasInput {
            pointer: Some(egui::pos2(at.x, at.y)),
            image_pointer: Some(at),
            primary_released: true,
            ..CanvasInput::default()
        };
        let second_release = CanvasInput {
            double_clicked: true,
            ..release.clone()
        };

        // First click: selects (it is already selected) and changes nothing.
        handle(
            &mut tab,
            Tool::Select,
            &styles,
            &press,
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        handle(
            &mut tab,
            Tool::Select,
            &styles,
            &release,
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        tab.commit_edit();
        let steps = tab.undo_depth_for_test();

        // Second click: the press stages, the release inserts the point.
        handle(
            &mut tab,
            Tool::Select,
            &styles,
            &press,
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        handle(
            &mut tab,
            Tool::Select,
            &styles,
            &second_release,
            &mut fonts(),
            &mut gesture,
            &mut active_point,
        );
        tab.commit_edit();

        let Kind::Path(path) = &tab.document.find(id).expect("still there").kind else {
            panic!("expected a path");
        };
        assert_eq!(
            path.points.len(),
            3,
            "the double click did not insert a point"
        );
        assert_eq!(
            tab.undo_depth_for_test(),
            steps + 1,
            "the double click was not exactly one undo step"
        );

        assert!(tab.undo());
        let Kind::Path(path) = &tab.document.find(id).expect("still there").kind else {
            panic!("expected a path");
        };
        assert_eq!(path.points.len(), 2, "one undo did not remove the point");
    }
}
