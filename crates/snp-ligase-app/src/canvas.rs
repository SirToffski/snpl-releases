//! The canvas: draws the image and its annotations, and reports what the
//! pointer is doing (SPEC section 7.2).
//!
//! Everything that is UI chrome — selection outlines, handles, guides — is drawn
//! here with egui's painter at a **constant screen size**, so it does not grow
//! when zooming and is never part of the exported image (SPEC 7.2).
//!
//! This module does not change the document. It turns egui's input into a
//! [`CanvasInput`] and hands that to the tools, which keeps "what the user did"
//! and "what gets drawn" apart.

use egui::{Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Sense, Stroke, Vec2, pos2};
use snp_ligase_core::edit::{self, HandleKind};
use snp_ligase_core::geom::Point;
use snp_ligase_core::model::Annotation;
use snp_ligase_core::render::{self, FlattenOptions};
use snp_ligase_core::text::Fonts;

use crate::panels::Action;
use crate::tab::Tab;
use crate::tool::Tool;
use crate::viewport::Viewport;

/// Canvas background: neutral grey (SPEC 7.2).
const BACKGROUND: Color32 = Color32::from_rgb(58, 58, 62);
/// One square of the transparency checkerboard.
const CHECKER: f32 = 12.0;
const CHECKER_LIGHT: Color32 = Color32::from_rgb(110, 110, 115);
const CHECKER_DARK: Color32 = Color32::from_rgb(80, 80, 85);

/// The whole of a texture, in the **normalized** coordinates egui samples with:
/// 0 to 1, not pixels. Passing pixel counts here silently clamps every sample
/// to the bottom-right texel.
const WHOLE_TEXTURE: Rect = Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0));

/// Selection outline and handle sizes, in points, never scaled by zoom
/// (SPEC 7.2).
const SELECTION_WIDTH: f32 = 1.5;
const HANDLE_RADIUS: f32 = 4.0;
const ADD_HANDLE_RADIUS: f32 = 3.0;

/// What the pointer did this frame, in both coordinate systems.
#[derive(Debug, Default, Clone)]
pub struct CanvasInput {
    /// Where the pointer is, in window points.
    ///
    /// This is the *global* latest position, not the hover position: while a
    /// gesture is under way the pointer may be outside the canvas, and the
    /// gesture has to keep following it (R16).
    pub pointer: Option<Pos2>,
    /// The same place as an image pixel.
    pub image_pointer: Option<Point>,
    /// Every pointer position egui reported this frame, in image pixels
    /// (NEXT_STEPS step 14b).
    ///
    /// egui can deliver several `Event::PointerMoved` in one frame, and the
    /// Freehand tool needs all of them: keeping only [`Self::image_pointer`]
    /// turns a fast stroke into a polygon at 60 fps.
    pub pointer_samples: Vec<Point>,
    /// The left button went down **on this widget** this frame, and only on
    /// that frame.
    ///
    /// The widget-ownership half is `is_pointer_button_down_on`: a click on a
    /// menu, a popup or a window that happens to overlap the canvas must not
    /// start a gesture, and egui will not give the press to a widget under a
    /// higher layer (R15). That method is true for the *whole* drag, though, so
    /// it has to be combined with `primary_pressed()`. Using it alone let a
    /// handle grab hijack a move or a drawing gesture mid-drag, the moment the
    /// pointer passed within [`crate::tools::HANDLE_TOLERANCE_POINTS`] of a
    /// handle of the selection.
    pub primary_pressed: bool,
    /// The left button came up this frame.
    pub primary_released: bool,
    /// The left button is down now, wherever it is.
    pub primary_down: bool,
    /// Two clicks in quick succession at the same place (SPEC 7.3).
    pub double_clicked: bool,
    /// The middle button is down: panning (SPEC 7.2).
    pub middle_down: bool,
    /// How far the pointer moved since the last frame, in points (R02: the
    /// per-frame delta, not the total since the press).
    pub drag_delta: Vec2,
    /// egui's own zoom factor for this frame, 1.0 for no change. Ctrl+wheel
    /// arrives here and not as a scroll delta (R03).
    pub zoom_delta: f32,
    /// The scroll wheel, in points.
    pub wheel: Vec2,
    /// Space is held: drag to pan (SPEC 7.2).
    pub space_held: bool,
    pub shift: bool,
    pub ctrl: bool,
    /// The size of the canvas widget this frame, which panning and zooming
    /// need (SPEC 7.2). Filled in by [`show`].
    pub canvas: Vec2,
    /// The rectangle the view may pan and zoom over, in image coordinates:
    /// the export area with NEXT_STEPS step 10's expansion, the image with
    /// "Cut". Filled in by [`show`], like `canvas`, because panning clamps to
    /// it.
    pub content: snp_ligase_core::geom::Rect,
}

/// How the canvas should draw this frame: what the app knows that the canvas
/// does not.
#[derive(Debug, Clone, Copy)]
pub struct CanvasFrame<'a> {
    /// Where a drawing gesture started, for the rubber band (SPEC 7.3).
    pub drawing_from: Option<Point>,
    /// The vertices of the shape being drawn, for its preview (SPEC 7.3).
    pub polygon: Option<&'a [Point]>,
    /// The Crop tool's rectangle, drawn with its eight handles (SPEC 7.3).
    pub crop: Option<snp_ligase_core::geom::Rect>,
    /// The Freehand stroke being drawn, for its preview (NEXT_STEPS step
    /// 14b).
    pub freehand: Option<&'a [Point]>,
    /// The picked-out point handle, drawn in its own colour (SPEC 7.3).
    pub active_point: Option<HandleKind>,
    /// True while the selection is being moved by the pointer (H4): only then
    /// may the sprite cache reuse a translated sprite's pixels.
    pub moving: bool,
    /// Space is held: drag to pan. Space is a keyboard shortcut the app owns,
    /// not something the canvas sees (SPEC 7.2).
    pub space_held: bool,
    /// The rectangle the exported image would cover, in image coordinates
    /// (NEXT_STEPS step 10). With "Expand" it holds annotations outside the
    /// image: the viewport fits and clamps to it, and the export area behind
    /// the image is painted over it.
    pub content: snp_ligase_core::geom::Rect,
    /// What the export does with annotations outside the image: the fill
    /// colour, or the checkerboard for a transparent fill (NEXT_STEPS step 10).
    pub flatten: FlattenOptions,
}

impl Default for CanvasFrame<'_> {
    fn default() -> Self {
        Self {
            drawing_from: None,
            polygon: None,
            crop: None,
            freehand: None,
            active_point: None,
            moving: false,
            space_held: false,
            content: snp_ligase_core::geom::Rect::ZERO,
            flatten: FlattenOptions::default(),
        }
    }
}

/// Draws the canvas and collects one frame of input.
///
/// `frame` carries what the app knows and the canvas does not: where a drawing
/// gesture started, which point is picked out, and whether Space is held.
/// `fonts` is the one font system the app owns (SPEC 7.5).
///
/// `actions` collects the right-click menu rows the canvas owns; the app runs
/// them, so the document is only ever changed from one place.
pub fn show(
    ui: &mut egui::Ui,
    tab: &mut Tab,
    tool: Tool,
    frame: CanvasFrame<'_>,
    fonts: &mut Fonts,
    actions: &mut Vec<Action>,
) -> CanvasInput {
    let CanvasFrame {
        drawing_from,
        polygon,
        crop,
        freehand,
        active_point,
        moving,
        space_held,
        content,
        flatten,
    } = frame;
    let ctx = ui.ctx().clone();
    let available = ui.available_size();
    // The painter `allocate_painter` hands back is the one whose coordinate
    // space and clip rect match this `Ui`. Building a painter by hand instead
    // puts everything at window coordinates with no clipping, which paints the
    // canvas over the tools and properties panels.
    let (response, painter) = ui.allocate_painter(available, Sense::click_and_drag());

    // Where this canvas starts in the window. Without it the viewport places
    // the image at the window's origin, which is under the tools panel (R01).
    tab.viewport.canvas_min = response.rect.min.to_vec2();

    // A neutral background behind everything (SPEC 7.2).
    painter.rect_filled(response.rect, 0.0, BACKGROUND);

    // One pass over the pointer state, read once: `Context::input` takes a
    // lock, and calling any other `Context` method from inside its closure
    // deadlocks the whole frame (DECISIONS.md, M3).
    let pointer_state = ui.input(|input| input.pointer.clone());
    let modifiers = ui.input(|input| input.modifiers);
    let zoom_delta = ui.input(|input| input.zoom_delta());
    let wheel = ui.input(|input| input.smooth_scroll_delta);
    // NEXT_STEPS step 14b: everything the pointer reported this frame, not
    // just where it ended up. The viewport is a plain copy, so the mapping can
    // run inside the same pass over the events.
    let viewport = tab.viewport;
    let samples = ui.input(|input| samples_from_events(&input.events, &viewport));

    // A gesture follows the global pointer, so it keeps working when the drag
    // leaves the canvas and ends when the button comes up out there (R16).
    let pointer = pointer_state.latest_pos();
    let input = CanvasInput {
        pointer,
        image_pointer: pointer.map(|pos| tab.viewport.to_image(pos)),
        pointer_samples: samples,
        primary_pressed: pointer_state.primary_pressed() && response.is_pointer_button_down_on(),
        primary_released: pointer_state.primary_released(),
        primary_down: pointer_state.primary_down(),
        double_clicked: response.double_clicked(),
        middle_down: pointer_state.button_down(egui::PointerButton::Middle),
        drag_delta: pointer_state.delta(),
        zoom_delta,
        wheel,
        space_held,
        shift: modifiers.shift,
        ctrl: modifiers.ctrl,
        canvas: available,
        content,
    };

    // The first frame, or after the canvas changed size: show the whole
    // content if it does not fit, and at 100% if it does (SPEC 7.2). The
    // content is the export area, so an annotation outside the image can be
    // scrolled to (NEXT_STEPS step 10).
    if !tab.viewport.initialised {
        tab.viewport.initialised = true;
        tab.viewport.fit_for_auto_fit(content, available);
    } else {
        // NEXT_STEPS step 13: with auto-fit on, the same fit runs again when
        // the canvas (a window resize, the properties panel, a DPI change) or
        // the content (a crop, a pasted image) changed since the last fit.
        // Not mid-drag: see `auto_fit_may_run` (Step 13 review).
        let pointer_busy = ui.input(|input| input.pointer.any_down());
        let auto_fit = crate::viewport::auto_fit_may_run(tab.auto_fit, pointer_busy);
        if tab.viewport.refit_if_needed(auto_fit, content, available) {
            // Info, not debug: the zoom changing on its own is exactly the
            // thing a bug report is about, and release builds log at info
            // (SPEC section 0 rule 11).
            tracing::info!(
                target: "snp_ligase::ui",
                zoom = tab.viewport.zoom,
                canvas = %format!("{:.0} × {:.0}", available.x, available.y),
                content = %format!("{:.0},{:.0} {:.0} × {:.0}", content.x, content.y, content.w, content.h),
                "auto-fit re-fitted the view"
            );
        }
        // A resize must not leave the content somewhere unreachable.
        tab.viewport.clamp_origin(content, available);
    }

    // The export area first: the fill colour, or the checkerboard where it is
    // transparent (NEXT_STEPS step 10), so the user sees the grown image.
    let viewport = tab.viewport;
    draw_export_area(&painter, viewport, content, flatten, response.rect);

    let max_texture_side = ctx.input(|input| input.max_texture_side);
    // The zoom is read once and handed to both texture paths, so a texture is
    // created with the sampling this zoom wants (R30).
    let zoom = tab.viewport.zoom;
    draw_base(&ctx, &painter, tab, max_texture_side, zoom);
    if tab.display_error.is_none() {
        draw_sprites(&ctx, &painter, tab, fonts, zoom, moving, flatten.expand);
    } else if let Some(message) = tab.display_error.clone() {
        draw_message(&painter, response.rect, &message);
    }

    for id in tab.selection().to_vec() {
        let Some(annotation) = tab.document.find(id).cloned() else {
            continue;
        };
        draw_outline(&painter, tab, &annotation, fonts);
        if tab.selection().len() == 1 {
            draw_handles(&painter, tab, &annotation, active_point);
        }
    }

    draw_preview(&painter, tab, drawing_from, input.image_pointer);
    draw_freehand_preview(&painter, tab, freehand, input.image_pointer);
    draw_shape_preview(&painter, tab, polygon, input.image_pointer);
    draw_crop_overlay(&painter, tab, crop);
    draw_corner_hint(&painter, response.rect, tab, tool);

    // Right-click on a point handle of the selection: SPEC 7.3's "Delete point".
    // The menu lives here because this is where the response is; the app still
    // owns the change.
    if response.secondary_clicked()
        && let Some(handle) = handle_at(tab, response.hover_pos().or(pointer), active_point)
    {
        response.context_menu(|ui| {
            if ui.button("Delete point").clicked() {
                actions.push(Action::DeletePoint(handle));
                ui.close();
            }
        });
    }

    input
}

/// Every pointer move in one frame, as image pixels (NEXT_STEPS step 14b).
///
/// egui reports a fast drag as several `Event::PointerMoved` in the same frame;
/// the tools that draw a stroke need all of them. Anything that is not a move
/// (keys, scroll, button events) is ignored.
#[must_use]
pub fn samples_from_events(events: &[egui::Event], viewport: &Viewport) -> Vec<Point> {
    events
        .iter()
        .filter_map(|event| match event {
            egui::Event::PointerMoved(pos) => Some(viewport.to_image(*pos)),
            _ => None,
        })
        .collect()
}

/// The point handle under the pointer, for the right-click menu (SPEC 7.3).
fn handle_at(
    tab: &Tab,
    pointer: Option<Pos2>,
    active_point: Option<HandleKind>,
) -> Option<HandleKind> {
    let pointer = pointer?;
    let annotation = tab.selected()?;
    if !matches!(
        annotation.kind,
        snp_ligase_core::model::Kind::Path(_) | snp_ligase_core::model::Kind::Polygon(_)
    ) {
        return None;
    }
    // Only a picked-out point, or one right under the cursor, is a "point" the
    // menu can mean: the fallback is the nearest one within reach.
    if let Some(active) = active_point
        && matches!(active, HandleKind::Point(_))
    {
        let at = edit::handles(annotation)
            .into_iter()
            .find(|handle| handle.kind == active)
            .map(|handle| handle.at);
        if let Some(at) = at
            && (tab.viewport.to_screen(at) - pointer).length()
                <= crate::tools::HANDLE_TOLERANCE_POINTS
        {
            return Some(active);
        }
    }
    edit::handles(annotation)
        .into_iter()
        .filter(|handle| matches!(handle.kind, HandleKind::Point(_)))
        .map(|handle| {
            (
                handle.kind,
                (tab.viewport.to_screen(handle.at) - pointer).length(),
            )
        })
        .filter(|(_, distance)| *distance <= crate::tools::HANDLE_TOLERANCE_POINTS)
        .min_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(kind, _)| kind)
}

/// The export area behind everything: the checkerboard for transparency, plus
/// the fill colour when the export has one (NEXT_STEPS step 10).
///
/// With "Cut" the content is the image itself, which is exactly what this drew
/// before step 10. With "Expand" the content is the grown area; a transparent
/// fill shows the checkerboard, a colour paints over it (so a translucent fill
/// looks like it would over a checkerboard background, which is what the saved
/// PNG shows).
fn draw_export_area(
    painter: &Painter,
    viewport: Viewport,
    content: snp_ligase_core::geom::Rect,
    flatten: FlattenOptions,
    area: Rect,
) {
    draw_checkerboard(painter, viewport, content, area);
    let Some(fill) = flatten.fill.filter(|_| flatten.expand) else {
        return;
    };
    let filled = viewport.rect_to_screen(content).intersect(area);
    if filled.width() <= 0.0 || filled.height() <= 0.0 {
        return;
    }
    painter.rect_filled(
        filled,
        0.0,
        Color32::from_rgba_unmultiplied(fill[0], fill[1], fill[2], fill[3]),
    );
}

/// The checkerboard under the export area, so transparency is visible
/// (SPEC 7.2).
fn draw_checkerboard(
    painter: &Painter,
    viewport: Viewport,
    content: snp_ligase_core::geom::Rect,
    area: Rect,
) {
    let image = viewport.rect_to_screen(content).intersect(area);
    if image.width() <= 0.0 || image.height() <= 0.0 {
        return;
    }

    let columns = (image.width() / CHECKER).ceil() as i32 + 1;
    let rows = (image.height() / CHECKER).ceil() as i32 + 1;
    for row in 0..rows {
        for column in 0..columns {
            let top_left = pos2(
                image.left() + column as f32 * CHECKER,
                image.top() + row as f32 * CHECKER,
            );
            let square = Rect::from_min_size(top_left, Vec2::splat(CHECKER)).intersect(image);
            let colour = if (column + row) % 2 == 0 {
                CHECKER_LIGHT
            } else {
                CHECKER_DARK
            };
            painter.rect_filled(square, 0.0, colour);
        }
    }
}

/// The whole image as a rectangle, from its pixel size.
fn image_rect(width: u32, height: u32) -> snp_ligase_core::geom::Rect {
    snp_ligase_core::geom::Rect::new(0.0, 0.0, width as f32, height as f32)
}

/// The base image, as one texture (SPEC 7.2).
fn draw_base(
    ctx: &egui::Context,
    painter: &Painter,
    tab: &mut Tab,
    max_texture_side: usize,
    zoom: f32,
) {
    let Some(texture) = tab.base_texture(ctx, max_texture_side, zoom) else {
        return;
    };
    // The viewport and the size are read before the texture is borrowed, so the
    // tab is only lent out once.
    let uv = WHOLE_TEXTURE;
    let id = texture.id();
    let (width, height) = tab.size();
    let rect = tab.viewport.rect_to_screen(image_rect(width, height));
    painter.image(id, rect, uv, Color32::WHITE);
}

/// Every annotation's sprite, in the compositing order of SPEC 4.2.
///
/// That order is [`render::compositing_order`], the same one `flatten` uses, so
/// the two cannot drift. `expand` is the "annotations outside the image" mode,
/// which decides how far the layers a Blur or Highlight reads reach (NEXT_STEPS
/// step 12): the screen must grow them exactly like the export does.
fn draw_sprites(
    ctx: &egui::Context,
    painter: &Painter,
    tab: &mut Tab,
    fonts: &mut Fonts,
    zoom: f32,
    moving: bool,
    expand: bool,
) {
    let order = render::compositing_order(&tab.document);

    // `Viewport` is a plain copy, so the mapping can be held on to while the tab
    // is lent to the texture cache.
    let viewport = tab.viewport;
    for id in order {
        let Some(sprite) = tab.sprite_texture(ctx, id, fonts, zoom, moving, expand) else {
            continue;
        };

        let size = sprite.texture.size_vec2();
        let uv = WHOLE_TEXTURE;
        let texture_id = sprite.texture.id();
        let top_left = viewport.to_screen(Point::new(sprite.origin.x, sprite.origin.y));
        let rect = Rect::from_min_size(
            top_left,
            Vec2::new(size.x * viewport.zoom, size.y * viewport.zoom),
        );
        painter.image(texture_id, rect, uv, Color32::WHITE);
    }
}

/// The outline around a selected annotation, in constant screen size.
fn draw_outline(painter: &Painter, tab: &Tab, annotation: &Annotation, fonts: &mut Fonts) {
    let bounds = render::bounds(annotation, fonts);
    painter.rect_stroke(
        tab.viewport.rect_to_screen(bounds),
        CornerRadius::ZERO,
        Stroke::new(SELECTION_WIDTH, Color32::from_rgb(90, 160, 255)),
        egui::StrokeKind::Middle,
    );
}

/// The draggable handles, in constant screen size (SPEC 7.2).
///
/// "Add point" handles are smaller, which is what SPEC 7.2 asks for.
fn draw_handles(
    painter: &Painter,
    tab: &Tab,
    annotation: &Annotation,
    active_point: Option<HandleKind>,
) {
    for handle in edit::handles(annotation) {
        let centre = tab.viewport.to_screen(handle.at);
        let is_add = matches!(handle.kind, HandleKind::AddPoint(_));
        // The point `Delete` would remove is drawn in the other colour, so it is
        // clear which one is picked out.
        let picked_out = active_point == Some(handle.kind);
        let radius = if is_add {
            ADD_HANDLE_RADIUS
        } else {
            HANDLE_RADIUS
        };
        let colour = if picked_out {
            Color32::from_rgb(255, 140, 0)
        } else {
            Color32::from_rgb(40, 120, 220)
        };
        painter.circle_stroke(centre, radius, Stroke::new(1.5, colour));
        painter.circle_filled(centre, radius * 0.35, Color32::WHITE);
    }
}

/// The edge being dragged out, before it is released.
fn draw_preview(painter: &Painter, tab: &Tab, from: Option<Point>, current: Option<Point>) {
    let (Some(from), Some(current)) = (from, current) else {
        return;
    };
    let stroke = Stroke::new(1.0, Color32::from_gray(210));
    let line = [
        tab.viewport.to_screen(from),
        tab.viewport.to_screen(current),
    ];
    painter.line_segment(line, stroke);
    painter.circle_stroke(line[0], ADD_HANDLE_RADIUS, stroke);
}

/// The Freehand stroke being drawn: the samples so far, plus a line to the
/// pointer (NEXT_STEPS step 14b).
fn draw_freehand_preview(
    painter: &Painter,
    tab: &Tab,
    points: Option<&[Point]>,
    current: Option<Point>,
) {
    let Some(points) = points else {
        return;
    };
    let mut screen: Vec<Pos2> = points
        .iter()
        .map(|point| tab.viewport.to_screen(*point))
        .collect();
    if let Some(current) = current {
        let current = tab.viewport.to_screen(current);
        if screen.last() != Some(&current) {
            screen.push(current);
        }
    }
    if screen.len() < 2 {
        return;
    }
    painter.add(egui::Shape::line(
        screen,
        Stroke::new(1.0, Color32::from_gray(210)),
    ));
}

/// The Shape tool's preview: the edges so far, a preview edge to the cursor,
/// and a marker on the first vertex to show where to click to close
/// (SPEC 7.3).
fn draw_shape_preview(
    painter: &Painter,
    tab: &Tab,
    points: Option<&[Point]>,
    current: Option<Point>,
) {
    let Some(points) = points else {
        return;
    };
    let stroke = Stroke::new(1.0, Color32::from_gray(210));
    for pair in points.windows(2) {
        painter.line_segment(
            [
                tab.viewport.to_screen(pair[0]),
                tab.viewport.to_screen(pair[1]),
            ],
            stroke,
        );
    }
    if let Some(last) = points.last()
        && let Some(current) = current
    {
        painter.line_segment(
            [
                tab.viewport.to_screen(*last),
                tab.viewport.to_screen(current),
            ],
            stroke,
        );
    }
    // The first vertex: clicking inside this circle closes the shape.
    if let Some(first) = points.first() {
        let centre = tab.viewport.to_screen(*first);
        painter.circle_stroke(
            centre,
            crate::tools::shape::CLOSE_TOLERANCE_POINTS,
            Stroke::new(1.5, Color32::from_rgb(90, 160, 255)),
        );
        painter.circle_filled(centre, 3.0, Color32::WHITE);
    }
}

/// The Crop tool's rectangle: a bright outline with its eight handles, all at
/// constant screen size (SPEC 7.3 and 7.2).
fn draw_crop_overlay(painter: &Painter, tab: &Tab, crop: Option<snp_ligase_core::geom::Rect>) {
    let Some(rect) = crop else {
        return;
    };
    if rect.w <= 0.0 || rect.h <= 0.0 {
        return;
    }
    let screen = tab.viewport.rect_to_screen(rect);
    let stroke = Stroke::new(1.5, Color32::from_rgb(255, 214, 0));
    painter.rect_stroke(screen, CornerRadius::ZERO, stroke, egui::StrokeKind::Middle);

    for direction in snp_ligase_core::edit::ResizeDir::ALL {
        let centre = tab.viewport.to_screen(direction.at(rect));
        painter.rect_filled(
            Rect::from_center_size(centre, Vec2::splat(8.0)),
            CornerRadius::ZERO,
            Color32::WHITE,
        );
        painter.rect_stroke(
            Rect::from_center_size(centre, Vec2::splat(8.0)),
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::from_rgb(120, 90, 0)),
            egui::StrokeKind::Middle,
        );
    }
}

/// A one-line hint in the corner, so the tool's effect is never a surprise.
fn draw_corner_hint(painter: &Painter, area: Rect, tab: &Tab, tool: Tool) {
    let (width, height) = tab.size();
    let text = format!(
        "{width} × {height} px  |  {} tool ({})",
        tool.name(),
        tool.key()
    );
    let bottom_left = area.left_bottom();
    painter.text(
        pos2(bottom_left.x + 8.0, bottom_left.y - 8.0),
        Align2::LEFT_BOTTOM,
        text,
        FontId::proportional(12.0),
        Color32::from_gray(220),
    );
}

/// The message shown when the image cannot be displayed (SPEC 7.2).
fn draw_message(painter: &Painter, area: Rect, message: &str) {
    let top_left = area.left_top() + Vec2::new(16.0, 16.0);
    painter.text(
        top_left,
        Align2::LEFT_TOP,
        message,
        FontId::proportional(14.0),
        Color32::from_rgb(255, 180, 180),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use snp_ligase_core::model::{Document, PathA, Stroke};

    /// NEXT_STEPS step 14b: a fast drag arrives as several `Event::PointerMoved`
    /// in one frame, and every one of them has to become a sample, mapped
    /// through the viewport. Non-move events are ignored.
    #[test]
    fn every_pointer_move_event_in_a_frame_becomes_a_sample_in_image_pixels() {
        let mut tab = Tab::new(
            Document::new(image::RgbaImage::new(200, 200)),
            "test".to_owned(),
        );
        tab.viewport.canvas_min = Vec2::new(30.0, 20.0);
        tab.viewport.origin = Vec2::new(10.0, 5.0);
        tab.viewport.zoom = 2.0;

        let events = vec![
            egui::Event::PointerMoved(pos2(40.0, 30.0)),
            egui::Event::PointerGone,
            egui::Event::PointerMoved(pos2(60.0, 50.0)),
            egui::Event::PointerButton {
                pos: pos2(60.0, 50.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerMoved(pos2(80.0, 70.0)),
        ];
        let samples = samples_from_events(&events, &tab.viewport);

        // (40 - 30 - 10) / 2 = 0, and so on: the canvas start and the pan
        // come off, and the zoom divides.
        assert_eq!(
            samples,
            vec![
                Point::new(0.0, 2.5),
                Point::new(10.0, 12.5),
                Point::new(20.0, 22.5)
            ],
            "a move event was dropped or mis-mapped"
        );
    }

    /// A two-point straight path, selected, in a tab at 100% with no pan, so
    /// image coordinates and screen points are the same numbers.
    fn tab_with_a_path() -> Tab {
        let mut tab = Tab::new(
            Document::new(image::RgbaImage::new(200, 200)),
            "test".to_owned(),
        );
        let id = tab.document.push(snp_ligase_core::model::Kind::Path(PathA {
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
    fn the_right_click_menu_finds_the_point_handle_under_the_pointer() {
        // R23: "Delete point" has to know which point the pointer is on.
        let tab = tab_with_a_path();
        assert_eq!(
            handle_at(&tab, Some(pos2(10.0, 10.0)), None),
            Some(HandleKind::Point(0)),
            "the first point was not found"
        );
        assert_eq!(
            handle_at(&tab, Some(pos2(150.0, 10.0)), None),
            Some(HandleKind::Point(1)),
            "the last point was not found"
        );
    }

    #[test]
    fn the_right_click_menu_ignores_empty_space_and_add_point_handles() {
        let tab = tab_with_a_path();
        // The middle of the segment is an "add point" handle, not a point: the
        // menu must not offer to delete a point that does not exist yet.
        assert_eq!(handle_at(&tab, Some(pos2(80.0, 10.0)), None), None);
        // And 50 points away from everything is nothing.
        assert_eq!(handle_at(&tab, Some(pos2(60.0, 60.0)), None), None);
    }
}
