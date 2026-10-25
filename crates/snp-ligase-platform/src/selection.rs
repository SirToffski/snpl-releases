//! The region overlay's selection state machine (SPEC section 5.6), pure.
//!
//! Everything the overlay decides — when a drag starts, which rectangle is
//! being dragged, when a drag is big enough to count, when Esc or a right click
//! cancels — is here, as a plain state machine over **virtual-desktop pixels**.
//! The Windows, X11 and Wayland windows only turn events into calls, so a
//! monitor edge, a negative monitor origin or a drag that leaves the monitor it
//! started on are tests, not clicks.
//!
//! The rectangle semantics are the Windows overlay's: the selection runs from
//! the press to the current pointer position, and a drag in any direction is
//! normalised, so dragging up and to the left produces the same rectangle as
//! dragging down and to the right over the same two points. Finishing needs at
//! least [`MIN_SELECTION`]×[`MIN_SELECTION`] pixels; anything smaller resets the
//! drag and the overlay keeps waiting.

use crate::RectI;

/// The smallest selection that counts as one (SPEC section 5.6: "at least 3×3
/// px, otherwise keep waiting").
pub const MIN_SELECTION: i32 = 3;

/// How the overlay ends: still waiting for the user, a finished selection, or
/// cancelled by Esc, a right click or the safety timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Outcome {
    /// The overlay is up and the user has not finished yet.
    #[default]
    Waiting,
    /// The user dragged a selection and let go.
    Selected(RectI),
    /// Esc, the right button or the timeout.
    Cancelled,
}

/// The overlay's selection state, in virtual-desktop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectionState {
    /// Where the left button went down, while a drag is in progress.
    start: Option<(i32, i32)>,
    /// The last pointer position seen, in virtual-desktop pixels.
    pointer: (i32, i32),
    outcome: Outcome,
}

impl SelectionState {
    /// A fresh overlay: waiting, with nothing selected.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            start: None,
            pointer: (0, 0),
            outcome: Outcome::Waiting,
        }
    }

    /// The left button went down at `(x, y)`.
    ///
    /// A press while a drag is already running or after the overlay has ended
    /// is ignored, so a second button (or a repeat) cannot move the anchor.
    pub fn press(&mut self, x: i32, y: i32) {
        if self.outcome != Outcome::Waiting || self.start.is_some() {
            return;
        }
        self.start = Some((x, y));
        self.pointer = (x, y);
    }

    /// The pointer moved to `(x, y)`.
    ///
    /// The position is remembered even with no button down: a press that
    /// arrives without a move before it still knows where the pointer is.
    pub fn pointer_moved(&mut self, x: i32, y: i32) {
        self.pointer = (x, y);
    }

    /// The left button went up at `(x, y)`.
    ///
    /// Returns `true` when this finished the selection. A drag smaller than
    /// [`MIN_SELECTION`] in either direction is thrown away and the overlay
    /// goes back to waiting (SPEC section 5.6). Once Esc, a right click or the
    /// timeout has ended the overlay, a release is ignored: the outcome is
    /// already decided.
    pub fn release(&mut self, x: i32, y: i32) -> bool {
        if self.outcome != Outcome::Waiting {
            return false;
        }
        let Some(start) = self.start else {
            return false;
        };
        self.pointer = (x, y);
        self.start = None;

        let rect = drag_rect(start, self.pointer);
        if rect.w < MIN_SELECTION || rect.h < MIN_SELECTION {
            return false;
        }
        self.outcome = Outcome::Selected(rect);
        true
    }

    /// Esc: cancel, whatever the drag was doing.
    pub fn escape(&mut self) {
        self.cancel();
    }

    /// A right click: cancel, whatever the drag was doing.
    pub fn right_click(&mut self) {
        self.cancel();
    }

    /// The safety timeout expired: cancel.
    pub fn timed_out(&mut self) {
        self.cancel();
    }

    /// The rectangle being dragged right now, in virtual-desktop pixels.
    ///
    /// `None` while no button is down, or when the drag has zero area.
    #[must_use]
    pub fn selection(&self) -> Option<RectI> {
        match self.outcome {
            Outcome::Selected(rect) => Some(rect),
            Outcome::Waiting => self
                .start
                .map(|start| drag_rect(start, self.pointer))
                .filter(|rect| rect.w > 0 && rect.h > 0),
            Outcome::Cancelled => None,
        }
    }

    /// How the overlay ended, or that it has not.
    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        self.outcome
    }

    fn cancel(&mut self) {
        if self.outcome == Outcome::Waiting {
            self.outcome = Outcome::Cancelled;
        }
    }
}

/// The rectangle between two points, in any direction.
///
/// Pure and shared with the tests: `RectI::new` clamps a negative size to zero,
/// which is exactly wrong here, so the two corners are sorted first.
#[must_use]
fn drag_rect(start: (i32, i32), end: (i32, i32)) -> RectI {
    RectI::new(
        start.0.min(end.0),
        start.1.min(end.1),
        (end.0 - start.0).abs(),
        (end.1 - start.1).abs(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> RectI {
        RectI { x, y, w, h }
    }

    #[test]
    fn a_drag_finishes_with_the_rectangle_between_press_and_release() {
        let mut state = SelectionState::new();
        state.press(150, 150);
        state.pointer_moved(300, 250);
        // The release position is the last word, even if no move arrived after
        // it (the Windows overlay does the same).
        assert!(state.release(550, 450));
        assert_eq!(state.outcome(), Outcome::Selected(rect(150, 150, 400, 300)));
        assert_eq!(state.selection(), Some(rect(150, 150, 400, 300)));
    }

    #[test]
    fn a_drag_in_any_direction_is_the_same_rectangle() {
        // Dragging up and to the left out of the bottom-right corner must
        // select the same pixels, which is what `RectI::new` alone would get
        // wrong (it clamps a negative size to zero).
        for (press, release) in [
            ((100, 100), (400, 300)),
            ((400, 300), (100, 100)),
            ((400, 100), (100, 300)),
            ((100, 300), (400, 100)),
        ] {
            let mut state = SelectionState::new();
            state.press(press.0, press.1);
            state.pointer_moved(release.0, release.1);
            assert!(state.release(release.0, release.1), "{press:?} {release:?}");
            assert_eq!(
                state.outcome(),
                Outcome::Selected(rect(100, 100, 300, 200)),
                "{press:?} -> {release:?}"
            );
        }
    }

    #[test]
    fn a_drag_smaller_than_three_pixels_keeps_waiting() {
        let mut state = SelectionState::new();
        state.press(100, 100);
        assert!(!state.release(102, 105), "2x5 is too small");
        assert_eq!(state.outcome(), Outcome::Waiting);
        assert_eq!(state.selection(), None, "the too-small drag is forgotten");

        // The overlay is still usable, and the next press starts a new drag.
        state.press(200, 200);
        assert!(state.release(200 + MIN_SELECTION, 200 + MIN_SELECTION));
        assert_eq!(
            state.outcome(),
            Outcome::Selected(rect(200, 200, MIN_SELECTION, MIN_SELECTION))
        );
    }

    #[test]
    fn three_by_three_finishes_and_two_by_five_does_not() {
        let mut state = SelectionState::new();
        state.press(0, 0);
        assert!(!state.release(2, 5), "2 wide");
        state.press(0, 0);
        assert!(!state.release(5, 2), "2 tall");
        state.press(0, 0);
        assert!(state.release(3, 3), "3x3 is the smallest that counts");
    }

    #[test]
    fn escape_and_a_right_click_cancel_a_drag_in_progress() {
        for cancel in [SelectionState::escape, SelectionState::right_click] {
            let mut state = SelectionState::new();
            state.press(10, 10);
            state.pointer_moved(200, 200);
            cancel(&mut state);
            assert_eq!(state.outcome(), Outcome::Cancelled);
            assert_eq!(state.selection(), None, "a cancelled drag paints nothing");
            // A release after the cancel cannot finish the overlay.
            assert!(!state.release(300, 300));
            assert_eq!(state.outcome(), Outcome::Cancelled);
        }
    }

    #[test]
    fn escape_and_a_right_click_cancel_without_a_drag() {
        for cancel in [SelectionState::escape, SelectionState::right_click] {
            let mut state = SelectionState::new();
            cancel(&mut state);
            assert_eq!(state.outcome(), Outcome::Cancelled);
        }
    }

    #[test]
    fn the_timeout_cancels() {
        let mut state = SelectionState::new();
        state.press(10, 10);
        state.timed_out();
        assert_eq!(state.outcome(), Outcome::Cancelled);
    }

    #[test]
    fn a_move_or_release_without_a_press_does_nothing() {
        let mut state = SelectionState::new();
        state.pointer_moved(500, 500);
        assert_eq!(state.selection(), None);
        assert!(!state.release(500, 500), "no drag was running");
        assert_eq!(state.outcome(), Outcome::Waiting);
        assert_eq!(state.selection(), None);
    }

    #[test]
    fn a_second_press_does_not_move_an_anchor_already_down() {
        let mut state = SelectionState::new();
        state.press(100, 100);
        state.press(400, 400);
        assert!(state.release(200, 200));
        assert_eq!(
            state.outcome(),
            Outcome::Selected(rect(100, 100, 100, 100)),
            "the first press is the anchor"
        );
    }

    #[test]
    fn a_drag_that_crosses_a_monitor_edge_is_one_rectangle() {
        // Two 960-pixel monitors side by side: the overlay does not care where
        // the edges are; each backend's painter clips per monitor.
        let mut state = SelectionState::new();
        state.press(900, 500);
        state.pointer_moved(1200, 700);
        assert!(state.release(1200, 700));
        assert_eq!(state.outcome(), Outcome::Selected(rect(900, 500, 300, 200)));
    }

    #[test]
    fn a_monitor_with_a_negative_origin_selects_negative_coordinates() {
        // A monitor left of and above the root origin, which is what a
        // negative RandR monitor position produces.
        let mut state = SelectionState::new();
        state.press(-1200, -300);
        state.pointer_moved(-1000, -100);
        assert!(state.release(-1000, -100));
        assert_eq!(
            state.outcome(),
            Outcome::Selected(rect(-1200, -300, 200, 200))
        );
    }

    #[test]
    fn a_selection_that_is_only_a_click_leaves_the_overlay_waiting() {
        let mut state = SelectionState::new();
        state.press(500, 400);
        assert!(!state.release(500, 400), "a click is not a selection");
        assert_eq!(state.outcome(), Outcome::Waiting);
    }
}
