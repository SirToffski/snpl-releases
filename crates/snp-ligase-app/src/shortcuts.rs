//! Keyboard shortcuts: one pure function that turns a frame's input into an
//! [`Action`] (SPEC sections 7.3 and 7.4).
//!
//! Two things about egui 0.36 make this more than a `match` on key names:
//!
//! - **Ctrl+C, Ctrl+X and Ctrl+V never arrive as keys.** egui-winet converts
//!   them into `Event::Copy` / `Event::Cut` / `Event::Paste` and *returns
//!   without emitting `Event::Key`*, and it only emits `Event::Paste` when the
//!   clipboard holds **text**. A clipboard that holds an image — the normal case
//!   after "copy image" or a screenshot — therefore produces no event at all,
//!   which is why the app also watches the physical keys.
//! - **A shortcut must not fire while a text field has focus.** Otherwise Ctrl+A
//!   selects every annotation instead of the whole line, and Ctrl+Z rewrites the
//!   document instead of undoing a typo.
//!
//! The routing is a pure function of this frame's input so it can be tested
//! without a window.

use egui::{Event, Key, Modifiers};

use crate::panels::Action;

/// The document-level shortcuts that still mean something while a text field has
/// focus. All of them open, close or save: none of them can be something the
/// user wanted to do to the text.
const KEPT_WHILE_TYPING: [Key; 5] = [
    Key::S,
    Key::O,
    Key::W,
    Key::Tab,
    // Ctrl+0 / Ctrl+1 change the zoom, not the text.
    Key::Num0,
];

/// Turns one input event into an action, or `None` when it is not a shortcut
/// this app acts on.
///
/// `text_focus` is `Context::egui_wants_keyboard_input()`: when it is true only
/// the shortcuts in [`KEPT_WHILE_TYPING`] survive, and every single key, arrow
/// key, Delete, Backspace and Escape is left for the text field (SPEC 7.5).
///
/// `shape_drawing` is true while the Shape tool is collecting vertices:
/// `Backspace` then removes the last one and `Enter` closes the shape, instead
/// of `Backspace` deleting the selection (SPEC 7.3). `crop_pending` is true
/// while the Crop tool has a rectangle to apply: `Enter` applies it.
///
/// The frame's modifiers are not read: `Event::Key` carries the modifiers that
/// were in force when the key went down, which is the state the shortcut
/// actually belongs to.
#[must_use]
pub fn route(
    event: &Event,
    modifiers: &Modifiers,
    text_focus: bool,
    shape_drawing: bool,
    crop_pending: bool,
) -> Option<Action> {
    match event {
        // egui turns Ctrl+C into this and nothing else. It is the app's "copy
        // image to the clipboard" (SPEC 7.4), and only when no text field wants
        // the keys.
        Event::Copy => (!text_focus).then_some(Action::CopyImage),
        // A clipboard that holds text as well as an image still produces this,
        // and with nothing focused it still means "paste the image". Shift
        // tells the two paste shortcuts apart: egui-winit checks only for the
        // command key, so Ctrl+Shift+V arrives here as well (NEXT_STEPS step
        // 11).
        Event::Paste(_) => (!text_focus).then_some(if modifiers.shift {
            Action::PasteIntoTab
        } else {
            Action::PasteAsNewTab
        }),
        Event::Key {
            key,
            pressed: true,
            repeat: false,
            modifiers: event_modifiers,
            ..
        } => route_key(
            *key,
            event_modifiers.ctrl,
            event_modifiers.shift,
            text_focus,
            shape_drawing,
            crop_pending,
        ),
        _ => None,
    }
}

/// Turns one key press into an action.
#[must_use]
pub fn route_key(
    key: Key,
    ctrl: bool,
    shift: bool,
    text_focus: bool,
    shape_drawing: bool,
    crop_pending: bool,
) -> Option<Action> {
    if ctrl {
        if text_focus && !KEPT_WHILE_TYPING.contains(&key) {
            return None;
        }
        return route_control(key, shift);
    }
    if text_focus {
        // Every single-key shortcut belongs to the canvas, never to a text
        // field (SPEC 7.5).
        return None;
    }
    if shape_drawing {
        match key {
            // SPEC 7.3: Backspace removes the last vertex, Enter closes the
            // shape.
            Key::Backspace => return Some(Action::ShapeRemoveLastPoint),
            Key::Enter => return Some(Action::ShapeFinish),
            _ => {}
        }
    }
    if crop_pending && key == Key::Enter {
        // SPEC 7.3: Enter applies the crop.
        return Some(Action::ApplyCrop);
    }
    route_plain(key, shift)
}

/// The Ctrl shortcuts (SPEC 7.4, 8.2).
fn route_control(key: Key, shift: bool) -> Option<Action> {
    Some(match key {
        Key::Z if shift => Action::Redo,
        Key::Z => Action::Undo,
        Key::Y => Action::Redo,
        Key::D => Action::Duplicate,
        Key::A => Action::SelectAll,
        // Ctrl+C arrives as `Event::Copy`, so it is not here.
        Key::S if shift => Action::SaveAs,
        Key::S => Action::Save,
        Key::O => Action::Open,
        Key::W => Action::CloseTab(usize::MAX),
        Key::Num0 => Action::ActualSize,
        Key::Num1 => Action::FitToWindow,
        Key::Equals | Key::Plus => Action::ZoomIn,
        Key::Minus => Action::ZoomOut,
        Key::Tab if shift => Action::SwitchTab(-1),
        Key::Tab => Action::SwitchTab(1),
        Key::F1 => Action::KeyboardShortcuts,
        _ => return None,
    })
}

/// The single-key shortcuts (SPEC 7.3).
fn route_plain(key: Key, shift: bool) -> Option<Action> {
    use crate::tool::Tool;
    // SPEC 7.4: Shift steps by 10 px instead of 1.
    let step = if shift { 10.0 } else { 1.0 };
    Some(match key {
        Key::V => Action::Tool(Tool::Select),
        Key::L => Action::Tool(Tool::Line),
        Key::A => Action::Tool(Tool::Arrow),
        Key::R => Action::Tool(Tool::Rectangle),
        Key::P => Action::Tool(Tool::Shape),
        Key::T => Action::Tool(Tool::Text),
        Key::N => Action::Tool(Tool::Number),
        Key::H => Action::Tool(Tool::Highlight),
        Key::B => Action::Tool(Tool::Blur),
        // NEXT_STEPS step 14b: the Freehand tool takes the free key F.
        Key::F => Action::Tool(Tool::Freehand),
        Key::C => Action::Tool(Tool::Crop),
        Key::Delete | Key::Backspace => Action::Delete,
        Key::Escape => Action::CancelGesture,
        Key::ArrowLeft => Action::Nudge(-step, 0.0),
        Key::ArrowRight => Action::Nudge(step, 0.0),
        Key::ArrowUp => Action::Nudge(0.0, -step),
        Key::ArrowDown => Action::Nudge(0.0, step),
        // SPEC 8.3: F1 opens the Keyboard Shortcuts window. Ctrl+F1 works too
        // (route_control); this is the plain key the Help menu advertises.
        Key::F1 => Action::KeyboardShortcuts,
        _ => return None,
    })
}

/// One row of the Keyboard Shortcuts window (SPEC 8.3's Help row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutRow {
    /// The heading this row belongs to.
    pub group: &'static str,
    /// The keys, as the user presses them.
    pub keys: String,
    /// What they do.
    pub what: String,
}

/// A shortcut the window lists, with the routing inputs it stands for.
///
/// Keeping the table as data is what lets the test below prove the window
/// cannot drift from the router: every row that claims a key must still produce
/// an action from [`route_key`].
struct Shortcut {
    group: &'static str,
    /// The keys as the user presses them, e.g. `Ctrl+Shift+S`.
    keys: &'static str,
    what: &'static str,
    /// What [`route_key`] gets for this row. `None` for the rows egui delivers
    /// as events (Ctrl+C, Ctrl+V), the ones the text editor handles itself, and
    /// the mouse gestures.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the routing test is the only reader")
    )]
    route: Option<(Key, bool, bool, Gesture)>,
}

/// The gesture state a row's routing needs: some keys only mean something while
/// a shape or a crop is being drawn (SPEC 7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gesture {
    None,
    Shape,
    Crop,
}

/// Every fixed row of the Keyboard Shortcuts window, in display order.
const SHORTCUTS: &[Shortcut] = &[
    // File (SPEC 8.3).
    Shortcut {
        group: "File",
        keys: "Ctrl+O",
        what: "Open an image",
        route: Some((Key::O, true, false, Gesture::None)),
    },
    Shortcut {
        group: "File",
        keys: "Ctrl+S",
        what: "Save",
        route: Some((Key::S, true, false, Gesture::None)),
    },
    Shortcut {
        group: "File",
        keys: "Ctrl+Shift+S",
        what: "Save As…",
        route: Some((Key::S, true, true, Gesture::None)),
    },
    Shortcut {
        group: "File",
        keys: "Ctrl+W",
        what: "Close the tab",
        route: Some((Key::W, true, false, Gesture::None)),
    },
    Shortcut {
        group: "File",
        keys: "Ctrl+Tab / Ctrl+Shift+Tab",
        what: "Next / previous tab",
        route: Some((Key::Tab, true, false, Gesture::None)),
    },
    Shortcut {
        group: "File",
        keys: "Ctrl+V",
        what: "Paste an image as a new tab",
        route: None,
    },
    Shortcut {
        group: "File",
        keys: "Ctrl+Shift+V",
        what: "Paste an image into this tab",
        route: None,
    },
    Shortcut {
        group: "File",
        keys: "Ctrl+C",
        what: "Copy the flattened image to the clipboard",
        route: None,
    },
    // Edit (SPEC 7.4).
    Shortcut {
        group: "Edit",
        keys: "Ctrl+Z",
        what: "Undo",
        route: Some((Key::Z, true, false, Gesture::None)),
    },
    Shortcut {
        group: "Edit",
        keys: "Ctrl+Y / Ctrl+Shift+Z",
        what: "Redo",
        route: Some((Key::Y, true, false, Gesture::None)),
    },
    Shortcut {
        group: "Edit",
        keys: "Ctrl+D",
        what: "Duplicate the selection",
        route: Some((Key::D, true, false, Gesture::None)),
    },
    Shortcut {
        group: "Edit",
        keys: "Ctrl+A",
        what: "Select every annotation",
        route: Some((Key::A, true, false, Gesture::None)),
    },
    Shortcut {
        group: "Edit",
        keys: "Delete / Backspace",
        what: "Delete the selection (or the picked-out point)",
        route: Some((Key::Delete, false, false, Gesture::None)),
    },
    Shortcut {
        group: "Edit",
        keys: "Arrow keys",
        what: "Nudge the selection by 1 px",
        route: Some((Key::ArrowLeft, false, false, Gesture::None)),
    },
    Shortcut {
        group: "Edit",
        keys: "Shift+Arrow keys",
        what: "Nudge the selection by 10 px",
        route: Some((Key::ArrowLeft, false, true, Gesture::None)),
    },
    Shortcut {
        group: "Edit",
        keys: "Esc",
        what: "Cancel the gesture, the crop or the text editor",
        route: Some((Key::Escape, false, false, Gesture::None)),
    },
    // View (SPEC 7.2 and 8.3).
    Shortcut {
        group: "View",
        keys: "Ctrl++ / Ctrl+-",
        what: "Zoom in / out",
        route: Some((Key::Plus, true, false, Gesture::None)),
    },
    Shortcut {
        group: "View",
        keys: "Ctrl+0",
        what: "Actual size (100%)",
        route: Some((Key::Num0, true, false, Gesture::None)),
    },
    Shortcut {
        group: "View",
        keys: "Ctrl+1",
        what: "Fit the image to the window",
        route: Some((Key::Num1, true, false, Gesture::None)),
    },
    Shortcut {
        group: "View",
        keys: "Ctrl+scroll",
        what: "Zoom around the pointer",
        route: None,
    },
    Shortcut {
        group: "View",
        keys: "Scroll",
        what: "Pan the image",
        route: None,
    },
    Shortcut {
        group: "View",
        keys: "Space+drag / middle drag",
        what: "Pan the image",
        route: None,
    },
    Shortcut {
        group: "View",
        keys: "Shift (while drawing)",
        what: "Snap a line to 15° or square a rectangle",
        route: None,
    },
    // Shape and crop (SPEC 7.3).
    Shortcut {
        group: "Shape and crop",
        keys: "Backspace (while drawing a shape)",
        what: "Remove the last vertex",
        route: Some((Key::Backspace, false, false, Gesture::Shape)),
    },
    Shortcut {
        group: "Shape and crop",
        keys: "Enter (while drawing a shape)",
        what: "Close the shape",
        route: Some((Key::Enter, false, false, Gesture::Shape)),
    },
    Shortcut {
        group: "Shape and crop",
        keys: "Enter (with a crop rectangle)",
        what: "Apply the crop",
        route: Some((Key::Enter, false, false, Gesture::Crop)),
    },
    // Text (SPEC 7.5).
    Shortcut {
        group: "Text editing",
        keys: "Ctrl+Enter",
        what: "Finish the text editor",
        route: None,
    },
    Shortcut {
        group: "Text editing",
        keys: "Esc",
        what: "Cancel the text editor",
        route: None,
    },
    // Help (SPEC 8.3).
    Shortcut {
        group: "Help",
        keys: "F1",
        what: "This window",
        route: Some((Key::F1, false, false, Gesture::None)),
    },
];

/// The rows the Keyboard Shortcuts window shows.
///
/// The tool rows come from [`Tool::ALL`] and the capture rows from the user's
/// own hotkey settings, so neither can list a key that does not exist (SPEC
/// 8.3, and the "every property is reachable" rule of SPEC 8.1).
#[must_use]
#[cfg(test)]
pub fn help_rows(settings: &crate::settings::Settings) -> Vec<ShortcutRow> {
    help_rows_for(settings, snp_ligase_platform::HotkeySupport::Native)
}

pub fn capture_bindings(
    settings: &crate::settings::Settings,
    support: snp_ligase_platform::HotkeySupport,
) -> Vec<(crate::panels::CaptureMode, String)> {
    #[cfg(target_os = "linux")]
    let bound = snp_ligase_platform::linux::wayland::shortcuts::bound();
    #[cfg(not(target_os = "linux"))]
    let bound = Vec::new();
    capture_bindings_from(settings, support, &bound)
}

fn capture_bindings_from(
    settings: &crate::settings::Settings,
    support: snp_ligase_platform::HotkeySupport,
    bound: &[(String, String)],
) -> Vec<(crate::panels::CaptureMode, String)> {
    use crate::panels::CaptureMode;
    CaptureMode::ALL
        .into_iter()
        .map(|mode| {
            let text = if support == snp_ligase_platform::HotkeySupport::Portal {
                bound
                    .iter()
                    .find(|(id, _)| *id == crate::hotkeys::portal_id(mode))
                    .map(|(_, text)| text.clone())
                    .unwrap_or_default()
            } else if mode == CaptureMode::RepeatLastRegion {
                String::new()
            } else {
                settings.hotkey_text(mode).trim().to_owned()
            };
            (mode, text)
        })
        .collect()
}

pub fn help_rows_for(
    settings: &crate::settings::Settings,
    support: snp_ligase_platform::HotkeySupport,
) -> Vec<ShortcutRow> {
    let mut rows: Vec<ShortcutRow> = SHORTCUTS
        .iter()
        .map(|shortcut| ShortcutRow {
            group: shortcut.group,
            keys: shortcut.keys.to_owned(),
            what: shortcut.what.to_owned(),
        })
        .collect();

    for tool in crate::tool::Tool::ALL {
        rows.push(ShortcutRow {
            group: "Tools",
            keys: tool.key().to_owned(),
            what: format!("{} tool", tool.name()),
        });
    }

    for (mode, text) in capture_bindings(settings, support) {
        rows.push(ShortcutRow {
            group: "Capture (global hotkeys)",
            keys: if text.is_empty() { "none".into() } else { text },
            what: if mode == crate::panels::CaptureMode::RepeatLastRegion {
                "Repeat Last Region (on the Capture menu and the tray)".into()
            } else {
                format!("Capture {}", mode.label().to_lowercase())
            },
        });
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, ctrl: bool, shift: bool, text_focus: bool) -> Option<Action> {
        route_key(key, ctrl, shift, text_focus, false, false)
    }

    fn press(key: Key, ctrl: bool, shift: bool) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers {
                ctrl,
                shift,
                ..Modifiers::NONE
            },
        }
    }

    #[test]
    fn copy_is_the_event_not_the_key() {
        // R04: Ctrl+C never arrives as a key, only as `Event::Copy`.
        assert_eq!(
            route(&Event::Copy, &Modifiers::NONE, false, false, false),
            Some(Action::CopyImage)
        );
        assert_eq!(key(Key::C, true, false, false), None);
    }

    #[test]
    fn paste_as_an_event_covers_text_and_image_clipboards() {
        assert_eq!(
            route(
                &Event::Paste("x".to_owned()),
                &Modifiers::NONE,
                false,
                false,
                false
            ),
            Some(Action::PasteAsNewTab)
        );
    }

    /// NEXT_STEPS step 11: egui-winit emits the same `Event::Paste` for
    /// Ctrl+Shift+V (it only checks the command key), so the frame's modifiers
    /// have to tell the two shortcuts apart.
    #[test]
    fn shift_turns_the_paste_event_into_paste_into_this_tab() {
        let shifted = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            route(&Event::Paste("x".to_owned()), &shifted, false, false, false),
            Some(Action::PasteIntoTab)
        );
        // And a text field still keeps both to itself.
        assert_eq!(
            route(&Event::Paste("x".to_owned()), &shifted, true, false, false),
            None
        );
    }

    #[test]
    fn neither_copy_nor_paste_runs_while_a_text_field_has_focus() {
        assert_eq!(
            route(&Event::Copy, &Modifiers::NONE, true, false, false),
            None
        );
        assert_eq!(
            route(
                &Event::Paste("x".to_owned()),
                &Modifiers::NONE,
                true,
                false,
                false
            ),
            None
        );
    }

    #[test]
    fn the_control_shortcuts_work_with_nothing_focused() {
        assert_eq!(key(Key::Z, true, false, false), Some(Action::Undo));
        assert_eq!(key(Key::Z, true, true, false), Some(Action::Redo));
        assert_eq!(key(Key::Y, true, false, false), Some(Action::Redo));
        assert_eq!(key(Key::D, true, false, false), Some(Action::Duplicate));
        assert_eq!(key(Key::A, true, false, false), Some(Action::SelectAll));
        assert_eq!(key(Key::S, true, false, false), Some(Action::Save));
        assert_eq!(key(Key::S, true, true, false), Some(Action::SaveAs));
        assert_eq!(key(Key::O, true, false, false), Some(Action::Open));
        assert_eq!(key(Key::Num0, true, false, false), Some(Action::ActualSize));
        assert_eq!(
            key(Key::Num1, true, false, false),
            Some(Action::FitToWindow)
        );
    }

    #[test]
    fn only_the_document_level_shortcuts_survive_a_text_field() {
        // R05: Ctrl+Z / Ctrl+A / Ctrl+D belong to the text, not the document.
        assert_eq!(key(Key::Z, true, false, true), None);
        assert_eq!(key(Key::A, true, false, true), None);
        assert_eq!(key(Key::D, true, false, true), None);
        assert_eq!(key(Key::C, true, false, true), None);

        // But saving, opening, closing a tab and switching tabs still work.
        assert_eq!(key(Key::S, true, false, true), Some(Action::Save));
        assert_eq!(key(Key::S, true, true, true), Some(Action::SaveAs));
        assert_eq!(key(Key::O, true, false, true), Some(Action::Open));
        assert_eq!(
            key(Key::W, true, false, true),
            Some(Action::CloseTab(usize::MAX))
        );
        assert_eq!(key(Key::Tab, true, false, true), Some(Action::SwitchTab(1)));
        assert_eq!(key(Key::Num0, true, false, true), Some(Action::ActualSize));
    }

    #[test]
    fn no_single_key_reaches_the_document_while_typing() {
        // SPEC 7.5: Enter, Backspace and Escape belong to the text field.
        for key in [
            Key::V,
            Key::L,
            Key::A,
            Key::R,
            Key::F,
            Key::Delete,
            Key::Backspace,
            Key::Escape,
            Key::ArrowLeft,
            Key::ArrowRight,
        ] {
            assert_eq!(
                route_key(key, false, false, true, false, false),
                None,
                "{key:?} leaked"
            );
        }
    }

    #[test]
    fn the_arrow_keys_nudge_and_shift_steps_by_ten() {
        // SPEC 7.4.
        assert_eq!(
            route_key(Key::ArrowLeft, false, false, false, false, false),
            Some(Action::Nudge(-1.0, 0.0))
        );
        assert_eq!(
            route_key(Key::ArrowRight, false, true, false, false, false),
            Some(Action::Nudge(10.0, 0.0))
        );
        assert_eq!(
            route_key(Key::ArrowUp, false, true, false, false, false),
            Some(Action::Nudge(0.0, -10.0))
        );
        // But never while typing.
        assert_eq!(
            route_key(Key::ArrowUp, false, false, true, false, false),
            None
        );
    }

    #[test]
    fn the_single_key_shortcuts_work_with_nothing_focused() {
        use crate::tool::Tool;
        assert_eq!(
            key(Key::V, false, false, false),
            Some(Action::Tool(Tool::Select))
        );
        assert_eq!(
            key(Key::L, false, false, false),
            Some(Action::Tool(Tool::Line))
        );
        assert_eq!(
            key(Key::A, false, false, false),
            Some(Action::Tool(Tool::Arrow))
        );
        assert_eq!(
            key(Key::R, false, false, false),
            Some(Action::Tool(Tool::Rectangle))
        );
        assert_eq!(
            key(Key::F, false, false, false),
            Some(Action::Tool(Tool::Freehand)),
            "NEXT_STEPS step 14b: F selects the Freehand tool"
        );
        assert_eq!(key(Key::Delete, false, false, false), Some(Action::Delete));
        assert_eq!(
            key(Key::Backspace, false, false, false),
            Some(Action::Delete)
        );
        assert_eq!(
            key(Key::Escape, false, false, false),
            Some(Action::CancelGesture)
        );
    }

    #[test]
    fn backspace_and_enter_are_routed_by_the_shape_gesture() {
        // SPEC 7.3: while the Shape tool is collecting vertices, Backspace
        // removes the last one and Enter closes the shape; outside it,
        // Backspace is still Delete and Enter does nothing.
        assert_eq!(
            route_key(Key::Backspace, false, false, false, true, false),
            Some(Action::ShapeRemoveLastPoint)
        );
        assert_eq!(
            route_key(Key::Enter, false, false, false, true, false),
            Some(Action::ShapeFinish)
        );
        assert_eq!(
            route_key(Key::Backspace, false, false, false, false, false),
            Some(Action::Delete)
        );
        assert_eq!(
            route_key(Key::Enter, false, false, false, false, false),
            None
        );
        // A text field keeps both keys whatever the gesture is doing.
        assert_eq!(
            route_key(Key::Backspace, false, false, true, true, false),
            None
        );
        assert_eq!(route_key(Key::Enter, false, false, true, true, false), None);
    }

    #[test]
    fn every_tool_key_selects_its_tool() {
        use crate::tool::Tool;
        for tool in Tool::ALL {
            let shortcut = match tool.key() {
                "V" => Key::V,
                "L" => Key::L,
                "A" => Key::A,
                "R" => Key::R,
                "P" => Key::P,
                "T" => Key::T,
                "N" => Key::N,
                "H" => Key::H,
                "B" => Key::B,
                "F" => Key::F,
                "C" => Key::C,
                other => panic!("unexpected key {other}"),
            };
            assert_eq!(
                key(shortcut, false, false, false),
                Some(Action::Tool(tool)),
                "{tool:?} is not reachable by its shortcut"
            );
        }
    }

    #[test]
    fn a_key_release_or_repeat_is_never_a_shortcut() {
        let release = Event::Key {
            key: Key::Z,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        };
        assert_eq!(route(&release, &Modifiers::NONE, false, false, false), None);

        let repeat = Event::Key {
            key: Key::Z,
            physical_key: None,
            pressed: true,
            repeat: true,
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        };
        assert_eq!(route(&repeat, &Modifiers::NONE, false, false, false), None);
    }

    #[test]
    fn the_zoom_shortcuts_come_from_the_keyboard_again() {
        // R03: with `zoom_with_keyboard` off, Ctrl+0 and Ctrl++ reach us.
        assert_eq!(key(Key::Plus, true, false, false), Some(Action::ZoomIn));
        assert_eq!(key(Key::Minus, true, false, false), Some(Action::ZoomOut));
    }

    #[test]
    fn an_event_carries_its_own_modifiers() {
        // The event's modifiers are what count, not the frame's.
        let event = press(Key::S, true, false);
        assert_eq!(
            route(&event, &Modifiers::NONE, false, false, false),
            Some(Action::Save)
        );
        let plain = press(Key::S, false, false);
        assert_eq!(route(&plain, &Modifiers::NONE, false, false, false), None);
    }

    // ---------------------------------------------------------------------
    // M7: the Keyboard Shortcuts window (SPEC 8.3)
    // ---------------------------------------------------------------------

    #[test]
    fn every_row_that_claims_a_key_is_one_the_router_produces() {
        // The window must not list a shortcut the app does not have.
        for shortcut in SHORTCUTS {
            let Some((key, ctrl, shift, gesture)) = shortcut.route else {
                continue;
            };
            let (shape, crop) = match gesture {
                Gesture::None => (false, false),
                Gesture::Shape => (true, false),
                Gesture::Crop => (false, true),
            };
            assert!(
                route_key(key, ctrl, shift, false, shape, crop).is_some(),
                "\"{}\" ({}) does not route to anything",
                shortcut.keys,
                shortcut.what
            );
        }
    }

    #[test]
    fn f1_opens_the_shortcuts_window() {
        // SPEC 8.3's Help row, both plain and with Ctrl.
        assert_eq!(
            key(Key::F1, false, false, false),
            Some(Action::KeyboardShortcuts)
        );
        assert_eq!(
            key(Key::F1, true, false, false),
            Some(Action::KeyboardShortcuts)
        );
    }

    #[test]
    fn the_help_rows_name_every_tool_and_the_configured_hotkeys() {
        use crate::settings::Settings;

        let settings = Settings {
            hotkey_active_window: String::new(),
            ..Settings::default()
        };
        let rows = help_rows(&settings);

        // Every tool has a row with its real key.
        for tool in crate::tool::Tool::ALL {
            assert!(
                rows.iter().any(|row| row.group == "Tools"
                    && row.keys == tool.key()
                    && row.what.contains(tool.name())),
                "no row for {tool:?}"
            );
        }

        // The hotkeys come from the settings: the defaults are shown, and an
        // unbound mode says so instead of naming a key that does not work.
        assert!(rows.iter().any(|row| row.keys == "Print Screen"));
        assert!(rows.iter().any(|row| row.keys == "Ctrl+Shift+Print Screen"));
        let active_window = rows
            .iter()
            .find(|row| row.what == "Capture active window")
            .expect("the active-window row");
        assert_eq!(active_window.keys, "none");

        // Every group has at least one row, so no heading is empty.
        for group in [
            "File",
            "Edit",
            "View",
            "Shape and crop",
            "Text editing",
            "Tools",
            "Capture (global hotkeys)",
            "Help",
        ] {
            assert!(
                rows.iter().any(|row| row.group == group),
                "the {group} group is empty"
            );
        }
    }
}

#[cfg(test)]
#[test]
fn w2_portal_bindings_replace_native_defaults_including_repeat() {
    use crate::panels::CaptureMode;
    use snp_ligase_platform::HotkeySupport;
    let settings = crate::settings::Settings::default();
    let bound = vec![
        ("region".into(), "Ctrl+Print".into()),
        ("repeat".into(), "Meta+R".into()),
    ];
    let portal = capture_bindings_from(&settings, HotkeySupport::Portal, &bound);
    assert_eq!(
        portal
            .iter()
            .find(|(m, _)| *m == CaptureMode::Region)
            .unwrap()
            .1,
        "Ctrl+Print"
    );
    assert_eq!(
        portal
            .iter()
            .find(|(m, _)| *m == CaptureMode::RepeatLastRegion)
            .unwrap()
            .1,
        "Meta+R"
    );
    assert!(
        portal
            .iter()
            .find(|(m, _)| *m == CaptureMode::CurrentMonitor)
            .unwrap()
            .1
            .is_empty()
    );
    let native = capture_bindings_from(&settings, HotkeySupport::Native, &bound);
    assert_eq!(
        native
            .iter()
            .find(|(m, _)| *m == CaptureMode::Region)
            .unwrap()
            .1,
        settings.hotkey_text(CaptureMode::Region)
    );
    assert!(
        native
            .iter()
            .find(|(m, _)| *m == CaptureMode::RepeatLastRegion)
            .unwrap()
            .1
            .is_empty()
    );
}
