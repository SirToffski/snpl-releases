//! The Select tool, plus the editing commands that act on the selection
//! (SPEC sections 7.3 and 7.4).

use snp_ligase_core::edit;
use snp_ligase_core::edit::HandleKind;
use snp_ligase_core::geom::{Point, distance_to_segment};
use snp_ligase_core::model::{Annotation, Kind};
use snp_ligase_core::text::Fonts;

use crate::canvas::CanvasInput;
use crate::tab::Tab;
use crate::tools::{self, Gesture};

/// Runs the Select tool for one frame.
///
/// Returns a status-bar message when something worth saying happened.
pub fn handle(
    tab: &mut Tab,
    input: &CanvasInput,
    fonts: &mut Fonts,
    gesture: &mut Gesture,
    active_point: &mut Option<HandleKind>,
) -> Option<String> {
    if let Gesture::Moving { from, last } = *gesture {
        // A release is checked before the pointer: the release frame may have
        // no pointer position at all when the button came up outside the canvas,
        // and the gesture must still end (R16).
        if input.primary_released {
            *gesture = Gesture::None;
            return None;
        }
        // Fall back to the last known point, so a frame with no pointer (the
        // pointer left the window) does not stall the drag.
        let point = input.image_pointer.unwrap_or(last);
        // Move by the pointer's own delta, so a drag that starts anywhere moves
        // the shape by the same amount.
        translate_selection(tab, point.x - last.x, point.y - last.y);
        *gesture = Gesture::Moving { from, last: point };
        return None;
    }

    if !input.primary_pressed {
        return None;
    }
    let point = input.image_pointer?;
    // A press anywhere but on a handle stops pointing at a point, so `Delete`
    // goes back to deleting whole annotations.
    *active_point = None;

    let tolerance = tools::tolerance_in_pixels(tools::HIT_TOLERANCE_POINTS, tab.viewport.zoom);
    let hit = tools::annotation_at(tab, point, tolerance, fonts);

    match hit {
        Some(id) => {
            if input.shift {
                // Shift+click adds or removes from the selection (SPEC 7.3, P1).
                tab.toggle_selection(id);
            } else if !tab.selection().contains(&id) {
                tab.select_only(Some(id));
            }

            // One undo step for the whole drag (SPEC 7.3), staged so that a
            // press which moves nothing pushes nothing (R20).
            tab.stage_edit();
            *gesture = Gesture::Moving {
                from: point,
                last: point,
            };
            None
        }
        // Clicking empty space deselects (SPEC 7.3).
        None => {
            if !input.shift {
                tab.select_only(None);
            }
            None
        }
    }
}

/// Double click on the selected path or polygon: inserts a point on the nearest
/// edge (SPEC 7.3).
///
/// The double-click flag is only true on the **release** frame of the second
/// click, so this runs on its own rather than where the press is handled, and
/// only when the press did not already start a move of the same shape. Because
/// the press only *stages* its snapshot, a press that changed nothing leaves the
/// undo stack untouched (R20).
pub fn double_click_point(tab: &mut Tab, input: &CanvasInput) -> Option<String> {
    if input.image_pointer.is_none() && !input.primary_released {
        return None;
    }
    let point = input.image_pointer?;
    let id = *tab.selection().first()?;
    insert_point(tab, id, point)
}

/// Moves every selected annotation by `(dx, dy)`.
pub fn translate_selection(tab: &mut Tab, dx: f32, dy: f32) {
    for id in tab.selection().to_vec() {
        if let Some(annotation) = tab.document.find_mut(id) {
            edit::translate(annotation, dx, dy);
        }
    }
}

/// Inserts a point where the user double-clicked, when the click was on the edge
/// of a path or a polygon (SPEC section 7.3).
fn insert_point(tab: &mut Tab, id: u64, point: Point) -> Option<String> {
    let points = match tab.document.find(id).map(|annotation| &annotation.kind) {
        Some(Kind::Path(path)) => path.points.clone(),
        Some(Kind::Polygon(polygon)) => polygon.points.clone(),
        _ => return None,
    };

    // The closest segment decides where the point goes. A polygon is closed, so
    // its last-to-first edge counts as a segment too.
    let mut best: Option<(usize, f32)> = None;
    let count = points.len();
    for index in 0..count {
        let from = points[index];
        let to = points[(index + 1) % count];
        let distance = distance_to_segment(point, from, to);
        if best.is_none_or(|(_, closest)| distance < closest) {
            best = Some((index, distance));
        }
    }
    let (index, _) = best?;

    // One undo step for the whole double click. The press that began it already
    // staged its snapshot (R20), and the release frame commits it; pushing a
    // second snapshot here gave the user an undo step that visibly did nothing
    // (R13).
    if !tab.edit_in_flight() {
        tab.begin_edit();
    }
    let at = index + 1;
    let annotation = tab.document.find_mut(id)?;
    match &mut annotation.kind {
        Kind::Path(path) => {
            path.points.insert(at, point);
        }
        Kind::Polygon(polygon) => {
            polygon.points.insert(at, point);
        }
        _ => return None,
    }
    tab.forget_deleted();
    Some("Inserted a point on the edge.".to_owned())
}

/// Removes one point of the selected path or polygon (`Delete` with a point
/// picked out, or right-click > "Delete point").
///
/// A path keeps at least two points and a polygon at least three, because below
/// that they stop being a line or a shape. A point index that no longer exists —
/// because the document was undone, switched to another tab or reselected while
/// the same point was picked out — is a message, not a panic (R10).
pub fn delete_point(tab: &mut Tab, kind: HandleKind) -> Option<String> {
    let HandleKind::Point(index) = kind else {
        return None;
    };
    let id = *tab.selection().first()?;

    // Check it can be done before anything changes.
    let (length, keep) = match tab.document.find(id).map(|annotation| &annotation.kind) {
        Some(Kind::Path(path)) => (path.points.len(), 2),
        Some(Kind::Polygon(polygon)) => (polygon.points.len(), 3),
        _ => return None,
    };
    if index >= length {
        return Some("That point is no longer there.".to_owned());
    }
    if length <= keep {
        return Some(if keep == 2 {
            "A line keeps at least two points.".to_owned()
        } else {
            "A shape keeps at least three points.".to_owned()
        });
    }

    tab.begin_edit();
    let annotation = tab.document.find_mut(id)?;
    match &mut annotation.kind {
        Kind::Path(path) => {
            path.points.remove(index);
        }
        Kind::Polygon(polygon) => {
            polygon.points.remove(index);
        }
        _ => return None,
    }
    tab.forget_deleted();
    Some(format!("Deleted point {}. {} left.", index + 1, length - 1))
}

/// Deletes the selected annotations (`Delete`, SPEC 7.4).
pub fn delete_selection(tab: &mut Tab) -> Option<String> {
    if tab.selection().is_empty() {
        return None;
    }
    tab.begin_edit();
    let selected = tab.selection().to_vec();
    let count = selected.len();
    tab.document
        .annotations
        .retain(|annotation| !selected.contains(&annotation.id));
    tab.select_only(None);
    tab.forget_deleted();
    Some(format!(
        "Deleted {count} annotation{}.",
        if count == 1 { "" } else { "s" }
    ))
}

/// Nudges the selection by the arrow keys: 1 px, or 10 px with Shift (SPEC 7.4).
pub fn nudge(tab: &mut Tab, dx: f32, dy: f32) {
    if tab.selection().is_empty() {
        return;
    }
    tab.begin_edit();
    translate_selection(tab, dx, dy);
}

/// Duplicates the selection, offset by (10, 10) (`Ctrl+D`, SPEC 7.4).
pub fn duplicate_selection(tab: &mut Tab) -> Option<String> {
    if tab.selection().is_empty() {
        return None;
    }
    tab.begin_edit();
    let mut copies = Vec::new();
    for id in tab.selection().to_vec() {
        let Some(mut copy) = tab.document.find(id).cloned() else {
            continue;
        };
        edit::translate(&mut copy, 10.0, 10.0);
        copy.id = tab.document.allocate_id();
        copies.push(copy);
    }
    let new_ids: Vec<u64> = copies.iter().map(|copy| copy.id).collect();
    tab.document.annotations.extend(copies);
    tab.selection = new_ids.clone();
    Some(format!(
        "Duplicated {} annotation{}.",
        new_ids.len(),
        if new_ids.len() == 1 { "" } else { "s" }
    ))
}

/// Moves the selection as far as it goes in the z-order: Bring to Front or Send
/// to Back (SPEC 7.4).
pub fn reorder(tab: &mut Tab, to_front: bool) -> Option<String> {
    move_to_end(tab, to_front)
}

/// Moves the selection one place in the z-order: Bring Forward or Send Backward
/// (SPEC 7.4).
pub fn reorder_one_place(tab: &mut Tab, to_front: bool) -> Option<String> {
    swap_with_neighbour(tab, to_front)
}

/// Takes the selection out of the list and puts it all at one end, keeping the
/// selected annotations in their existing relative order.
fn move_to_end(tab: &mut Tab, to_front: bool) -> Option<String> {
    if tab.selection().is_empty() {
        return None;
    }
    let selected = tab.selection().to_vec();
    let original: Vec<u64> = tab
        .document
        .annotations
        .iter()
        .map(|annotation| annotation.id)
        .collect();
    let mut moving: Vec<Annotation> = Vec::with_capacity(selected.len());
    let mut others: Vec<Annotation> = Vec::with_capacity(tab.document.annotations.len());
    for annotation in tab.document.annotations.drain(..) {
        if selected.contains(&annotation.id) {
            moving.push(annotation);
        } else {
            others.push(annotation);
        }
    }
    if moving.is_empty() {
        return None;
    }
    let reordered = rebuild(to_front, moving, others);
    if reordered
        .iter()
        .map(|annotation| annotation.id)
        .eq(original)
    {
        // Nothing was out of place. Put the list back (the drain emptied it)
        // and change nothing: SPEC 7.3 pushes the undo snapshot when the
        // gesture starts *changing* the document (R20).
        tab.document.annotations = reordered;
        return Some(if to_front {
            "Already at the front.".to_owned()
        } else {
            "Already at the back.".to_owned()
        });
    }
    tab.begin_edit();
    tab.document.annotations = reordered;
    Some(if to_front {
        "Brought to front.".to_owned()
    } else {
        "Sent to back.".to_owned()
    })
}

/// Moves every selected annotation past its neighbour in the list, one place.
///
/// The annotations move from the top of the z-order backwards so that two
/// adjacent selected annotations both get past the unselected one above them.
fn swap_with_neighbour(tab: &mut Tab, to_front: bool) -> Option<String> {
    if tab.selection().is_empty() {
        return None;
    }
    // Would anything actually move? A "Bring Forward" on the top item changes
    // nothing, so it must not push an undo step and clear the redo stack (R20).
    let mut moved_any = would_move(tab, to_front);

    if moved_any {
        tab.begin_edit();
    }
    for id in order_for_shift(tab, to_front) {
        let Some(index) = tab
            .document
            .annotations
            .iter()
            .position(|annotation| annotation.id == id)
        else {
            continue;
        };
        let neighbour = if to_front {
            index + 1
        } else if index == 0 {
            continue;
        } else {
            index - 1
        };
        if neighbour >= tab.document.annotations.len() {
            continue;
        }
        // Selected annotations keep their relative order, so they do not
        // swap with each other.
        if tab
            .selection()
            .contains(&tab.document.annotations[neighbour].id)
        {
            continue;
        }
        tab.document.annotations.swap(index, neighbour);
        moved_any = true;
    }

    if !moved_any {
        return Some(if to_front {
            "Already at the front.".to_owned()
        } else {
            "Already at the back.".to_owned()
        });
    }
    Some(if to_front {
        "Brought forward.".to_owned()
    } else {
        "Sent backward.".to_owned()
    })
}

/// The selected ids in the order one shift pass must visit them.
fn order_for_shift(tab: &Tab, to_front: bool) -> Vec<u64> {
    let mut ids = tab.selection().to_vec();
    if to_front {
        // Walk from the top of the list downwards.
        ids.sort_by_key(|id| std::cmp::Reverse(position_of(tab, *id)));
    } else {
        ids.sort_by_key(|id| position_of(tab, *id));
    }
    ids
}

/// Whether one shift pass would move anything at all.
fn would_move(tab: &Tab, to_front: bool) -> bool {
    let mut moved_any = false;
    let mut list = tab.document.annotations.clone();
    for id in order_for_shift(tab, to_front) {
        let Some(index) = list.iter().position(|annotation| annotation.id == id) else {
            continue;
        };
        let neighbour = if to_front {
            index + 1
        } else if index == 0 {
            continue;
        } else {
            index - 1
        };
        if neighbour >= list.len() {
            continue;
        }
        if tab.selection().contains(&list[neighbour].id) {
            continue;
        }
        list.swap(index, neighbour);
        moved_any = true;
    }
    moved_any
}

/// Where an annotation sits in the z-order list.
fn position_of(tab: &Tab, id: u64) -> usize {
    tab.document
        .annotations
        .iter()
        .position(|annotation| annotation.id == id)
        .unwrap_or(usize::MAX)
}

/// The z-order list with `moving` at one end.
fn rebuild(
    to_front: bool,
    mut moving: Vec<Annotation>,
    mut others: Vec<Annotation>,
) -> Vec<Annotation> {
    if to_front {
        others.append(&mut moving);
        others
    } else {
        moving.append(&mut others);
        moving
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snp_ligase_core::model::{PathA, RectA, Stroke};
    use snp_ligase_core::text::Fonts;

    fn tab_with_two_rects() -> Tab {
        let image = image::RgbaImage::new(100, 100);
        let mut tab = Tab::new(
            snp_ligase_core::model::Document::new(image),
            "test".to_owned(),
        );
        let first = tab.document.push(Kind::Rect(RectA {
            rect: snp_ligase_core::geom::Rect::new(0.0, 0.0, 10.0, 10.0),
            stroke: Some(Stroke {
                color: [255, 0, 0, 255],
                width: 2.0,
            }),
            fill: None,
            corner_radius: 0.0,
        }));
        let second = tab.document.push(Kind::Rect(RectA {
            rect: snp_ligase_core::geom::Rect::new(20.0, 20.0, 10.0, 10.0),
            stroke: Some(Stroke {
                color: [0, 255, 0, 255],
                width: 2.0,
            }),
            fill: None,
            corner_radius: 0.0,
        }));
        tab.select_only(Some(first));
        tab.document.find_mut(second).expect("just pushed");
        tab
    }

    #[test]
    fn delete_removes_only_the_selection() {
        let mut tab = tab_with_two_rects();
        let kept = tab.document.annotations[1].id;
        delete_selection(&mut tab);
        assert_eq!(tab.document.annotations.len(), 1);
        assert_eq!(tab.document.annotations[0].id, kept);
        assert!(tab.selection().is_empty());
    }

    #[test]
    fn undo_puts_a_deleted_annotation_back() {
        let mut tab = tab_with_two_rects();
        let before = tab.document.clone();
        delete_selection(&mut tab);
        assert!(tab.undo());
        assert_eq!(tab.document, before, "undo did not restore");
        assert!(tab.redo());
        assert_eq!(tab.document.annotations.len(), 1);
    }

    #[test]
    fn duplicate_offsets_by_ten_and_selects_the_copies() {
        let mut tab = tab_with_two_rects();
        let original = tab.document.annotations[0].id;
        duplicate_selection(&mut tab);
        assert_eq!(tab.document.annotations.len(), 3);

        let copy = tab.document.find(tab.selection()[0]).expect("copy");
        let source = tab.document.find(original).expect("original");
        let (Kind::Rect(copy), Kind::Rect(source)) = (&copy.kind, &source.kind) else {
            panic!("expected rectangles");
        };
        assert!((copy.rect.x - source.rect.x - 10.0).abs() < 0.001);
        assert!((copy.rect.y - source.rect.y - 10.0).abs() < 0.001);
    }

    #[test]
    fn bring_to_front_puts_the_selection_last_in_the_list() {
        let mut tab = tab_with_two_rects();
        let first = tab.document.annotations[0].id;
        reorder(&mut tab, true);
        assert_eq!(
            tab.document.annotations.last().map(|a| a.id),
            Some(first),
            "the selection is not on top"
        );

        reorder(&mut tab, false);
        assert_eq!(
            tab.document.annotations.first().map(|a| a.id),
            Some(first),
            "the selection is not at the bottom"
        );
    }

    #[test]
    fn nudge_moves_by_the_step_given() {
        let mut tab = tab_with_two_rects();
        let before = tab.document.find(tab.selection()[0]).cloned().expect("sel");
        nudge(&mut tab, 10.0, 0.0);
        let after = tab.document.find(before.id).cloned().expect("sel");
        let (Kind::Rect(before), Kind::Rect(after)) = (&before.kind, &after.kind) else {
            panic!("expected rectangles");
        };
        assert!((after.rect.x - before.rect.x - 10.0).abs() < 0.001);
    }

    #[test]
    fn inserting_a_point_splits_the_closest_segment() {
        let image = image::RgbaImage::new(100, 100);
        let mut tab = Tab::new(
            snp_ligase_core::model::Document::new(image),
            "test".to_owned(),
        );
        let id = tab.document.push(Kind::Path(PathA {
            points: vec![Point::new(0.0, 0.0), Point::new(100.0, 0.0)],
            curved: false,
            stroke: Stroke {
                color: [255, 0, 0, 255],
                width: 2.0,
            },
            head_start: false,
            head_end: false,
        }));
        tab.select_only(Some(id));
        insert_point(&mut tab, id, Point::new(50.0, 1.0));

        let Kind::Path(path) = &tab.document.find(id).expect("still there").kind else {
            panic!("expected a path");
        };
        assert_eq!(path.points.len(), 3);
        assert!((path.points[1].x - 50.0).abs() < 0.001);
    }

    #[test]
    fn deleting_a_point_removes_it_and_leaves_the_rest() {
        let image = image::RgbaImage::new(100, 100);
        let mut tab = Tab::new(
            snp_ligase_core::model::Document::new(image),
            "test".to_owned(),
        );
        let id = tab.document.push(Kind::Path(PathA {
            points: vec![
                Point::new(0.0, 0.0),
                Point::new(30.0, 10.0),
                Point::new(60.0, 0.0),
                Point::new(90.0, 10.0),
            ],
            curved: true,
            stroke: Stroke {
                color: [255, 0, 0, 255],
                width: 2.0,
            },
            head_start: false,
            head_end: true,
        }));
        tab.select_only(Some(id));

        delete_point(&mut tab, snp_ligase_core::edit::HandleKind::Point(1));

        let Kind::Path(path) = &tab.document.find(id).expect("still there").kind else {
            panic!("expected a path");
        };
        assert_eq!(path.points.len(), 3);
        assert!(
            (path.points[1].x - 60.0).abs() < 0.001,
            "the wrong point went"
        );
        assert!(path.head_end, "deleting a point dropped the arrowhead");
    }

    #[test]
    fn a_line_keeps_at_least_two_points() {
        let image = image::RgbaImage::new(100, 100);
        let mut tab = Tab::new(
            snp_ligase_core::model::Document::new(image),
            "test".to_owned(),
        );
        let id = tab.document.push(Kind::Path(PathA {
            points: vec![Point::new(0.0, 0.0), Point::new(50.0, 50.0)],
            curved: false,
            stroke: Stroke {
                color: [255, 0, 0, 255],
                width: 2.0,
            },
            head_start: false,
            head_end: true,
        }));
        tab.select_only(Some(id));

        let message = delete_point(&mut tab, snp_ligase_core::edit::HandleKind::Point(0))
            .expect("it should explain itself");
        assert!(message.contains("two points"), "{message}");

        let Kind::Path(path) = &tab.document.find(id).expect("still there").kind else {
            panic!("expected a path");
        };
        assert_eq!(path.points.len(), 2, "the point went anyway");
    }

    #[test]
    fn deleting_a_point_that_no_longer_exists_is_a_message_not_a_panic() {
        // R10's sequence: a point was picked out, then undo left the document
        // with fewer points. `Delete` used to index a stale point and panic.
        let image = image::RgbaImage::new(100, 100);
        let mut tab = Tab::new(
            snp_ligase_core::model::Document::new(image),
            "test".to_owned(),
        );
        let id = tab.document.push(Kind::Path(PathA {
            points: vec![
                Point::new(0.0, 0.0),
                Point::new(30.0, 10.0),
                Point::new(60.0, 0.0),
            ],
            curved: true,
            stroke: Stroke {
                color: [255, 0, 0, 255],
                width: 2.0,
            },
            head_start: false,
            head_end: true,
        }));
        tab.select_only(Some(id));
        let before = tab.document.clone();

        // The picked-out point is index 4, but only three points exist.
        let message = delete_point(&mut tab, snp_ligase_core::edit::HandleKind::Point(4))
            .expect("it should explain itself");
        assert!(message.contains("no longer there"), "{message}");
        assert_eq!(tab.document, before, "the document changed anyway");
    }

    #[test]
    fn a_reorder_of_one_step_moves_past_one_neighbour() {
        let mut tab = tab_with_two_rects();
        let first = tab.document.annotations[0].id;
        let second = tab.document.annotations[1].id;
        reorder_one_place(&mut tab, true);
        assert_eq!(tab.document.annotations[0].id, second, "the front stayed");
        assert_eq!(tab.document.annotations[1].id, first, "it did not go up");
    }

    #[test]
    fn reorder_one_place_says_so_when_there_is_nowhere_to_go() {
        let mut tab = tab_with_two_rects();
        // Give the tab an undo step and a redo entry, so the test can see
        // whether the no-op command touches them (R20).
        tab.begin_edit();
        nudge(&mut tab, 1.0, 0.0);
        assert!(tab.undo());
        assert!(tab.can_redo(), "the setup did not leave a redo entry");

        let second = tab.document.annotations[1].id;
        tab.select_only(Some(second));
        let can_undo = tab.can_undo();
        let can_redo = tab.can_redo();

        let message = reorder_one_place(&mut tab, true).expect("it should explain itself");
        assert!(message.contains("Already at the front"), "{message}");
        assert_eq!(tab.can_undo(), can_undo, "a no-op pushed an undo step");
        assert_eq!(tab.can_redo(), can_redo, "a no-op cleared the redo stack");
    }

    #[test]
    fn bring_to_front_on_the_top_item_is_a_no_op_with_no_undo_step() {
        // R20's exact case: the annotation is already at the front.
        let mut tab = tab_with_two_rects();
        tab.begin_edit();
        nudge(&mut tab, 1.0, 0.0);
        assert!(tab.undo());
        assert!(tab.can_redo(), "the setup did not leave a redo entry");

        let top = tab.document.annotations[1].id;
        tab.select_only(Some(top));
        let can_undo = tab.can_undo();
        let can_redo = tab.can_redo();

        let message = reorder(&mut tab, true).expect("it should explain itself");
        assert!(message.contains("Already at the front"), "{message}");
        assert_eq!(tab.can_undo(), can_undo, "a no-op pushed an undo step");
        assert_eq!(tab.can_redo(), can_redo, "a no-op cleared the redo stack");
    }

    #[test]
    fn shift_click_adds_and_removes_from_the_selection() {
        // SPEC 7.3: Shift+click adds or removes from the selection.
        let mut tab = tab_with_two_rects();
        let first = tab.document.annotations[0].id;
        let second = tab.document.annotations[1].id;
        tab.select_only(Some(first));

        // The second rectangle is at (20, 20), 10x10: a click at (25, 25) is on
        // its stroke. At 100% zoom image and screen coordinates agree.
        let click = |point: Point, shift: bool| CanvasInput {
            pointer: Some(egui::pos2(point.x, point.y)),
            image_pointer: Some(point),
            primary_pressed: true,
            primary_down: true,
            shift,
            ..CanvasInput::default()
        };

        let mut fonts = Fonts::bundled();
        let mut gesture = Gesture::None;
        let mut active_point = None;
        let release = CanvasInput {
            primary_released: true,
            pointer: Some(egui::pos2(20.0, 25.0)),
            image_pointer: Some(Point::new(20.0, 25.0)),
            ..CanvasInput::default()
        };
        handle(
            &mut tab,
            &click(Point::new(20.0, 25.0), true),
            &mut fonts,
            &mut gesture,
            &mut active_point,
        );
        handle(
            &mut tab,
            &release,
            &mut fonts,
            &mut gesture,
            &mut active_point,
        );
        assert_eq!(
            tab.selection().len(),
            2,
            "Shift+click did not add to the selection"
        );
        assert!(tab.selection().contains(&second));

        // Shift+clicking the same one again removes it.
        handle(
            &mut tab,
            &click(Point::new(20.0, 25.0), true),
            &mut fonts,
            &mut gesture,
            &mut active_point,
        );
        assert_eq!(tab.selection(), [first], "Shift+click did not remove it");
    }

    #[test]
    fn fonts_are_not_needed_by_any_of_this() {
        // A guard against a future change pulling font loading into the editing
        // commands: Select never measures text, so the placeholder stays unused.
        let _ = Fonts::bundled();
    }
}
