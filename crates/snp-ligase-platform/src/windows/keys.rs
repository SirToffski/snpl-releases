//! One small piece of the keyboard the app cannot read any other way.
//!
//! egui-winit turns Ctrl+C / Ctrl+X / Ctrl+V into [`egui::Event::Copy`],
//! [`egui::Event::Cut`] and [`egui::Event::Paste`] and then **returns without
//! emitting a key event** (`egui-winit-0.36.2/src/lib.rs:1017-1036`). For Ctrl+V
//! it only pushes `Event::Paste` when the clipboard holds *text*, so a
//! clipboard that holds an image — the normal case after "copy image" or a
//! screenshot — produces no event at all and the shortcut looks dead.
//!
//! The only way to notice it is to ask Windows whether the key is down, which
//! is why this lives here rather than in the app (SPEC section 0, rule 7: all
//! `unsafe` and all `windows`-crate usage live in the platform crate).

use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL,
    VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};

/// Whether a key is down *right now*, anywhere on the desktop.
///
/// This is the only input read outside egui, so it is deliberately narrow: it
/// exists only to notice the Ctrl+V that egui swallowed, and the caller must
/// already know the window has focus and no text field does.
#[must_use]
pub fn is_key_down(key: VIRTUAL_KEY) -> bool {
    // The high bit is "down now"; the low bit is "was pressed since the last
    // call", which this does not care about.
    (unsafe { GetAsyncKeyState(key.0 as i32) } as u16 & 0x8000) != 0
}

/// The virtual key code for `V`, the key Ctrl+V needs.
pub const VK_V: VIRTUAL_KEY = VIRTUAL_KEY(0x56);
/// The virtual key code for either Ctrl key.
pub const VK_CONTROL: VIRTUAL_KEY = VIRTUAL_KEY(0x11);

/// Whether Ctrl+V is being held: both keys down at once.
///
/// This is how the app notices a paste that egui never reported.
#[must_use]
pub fn paste_held() -> bool {
    is_key_down(VK_CONTROL) && is_key_down(VK_V)
}

/// Whether either Shift key is down right now.
///
/// The Ctrl+Shift+V fallback needs it: egui-winit emits no event at all for an
/// image-only clipboard, so the two paste shortcuts have to be told apart from
/// the key state (NEXT_STEPS step 11).
#[must_use]
pub fn shift_down() -> bool {
    is_key_down(VK_LSHIFT) || is_key_down(VK_RSHIFT) || is_key_down(VK_SHIFT)
}

/// A snapshot of the keyboard for the hotkey recorder (NEXT_STEPS step 7).
///
/// The recorder cannot use egui's key events: `egui-winit` maps no Print
/// Screen key, and the app's own registered hotkeys take a press before the
/// window sees it. `GetAsyncKeyState` sees the key wherever it lands, so the
/// recorder polls this instead. The snapshot is plain data — the app turns
/// virtual keys into hotkey names — so no `windows` type leaves this crate
/// (SPEC section 0, rule 7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyboardSnapshot {
    /// Either Ctrl is down.
    pub ctrl: bool,
    /// Either Shift is down.
    pub shift: bool,
    /// Either Alt is down.
    pub alt: bool,
    /// Either Windows key is down.
    pub win: bool,
    /// The virtual keys of the non-modifier keys that are down right now, in
    /// ascending order.
    pub keys: Vec<u16>,
}

/// Reads the keyboard's current state, for the hotkey recorder.
///
/// Every non-modifier key is polled, so a key egui never reports (Print
/// Screen) is seen like any other. This is the only input read outside egui,
/// and only while the Settings dialog is recording a hotkey.
#[must_use]
pub fn keyboard_snapshot() -> KeyboardSnapshot {
    // The generic Shift/Ctrl/Alt codes report either key of the pair; the
    // left/right codes are checked as well so an injected key
    // (`keybd_event(VK_CONTROL)`, for instance) is noticed whichever way
    // Windows recorded it.
    let mut snapshot = KeyboardSnapshot {
        ctrl: is_key_down(VK_LCONTROL) || is_key_down(VK_RCONTROL) || is_key_down(VK_CONTROL),
        shift: is_key_down(VK_LSHIFT) || is_key_down(VK_RSHIFT) || is_key_down(VK_SHIFT),
        alt: is_key_down(VK_LMENU) || is_key_down(VK_RMENU) || is_key_down(VK_MENU),
        win: is_key_down(VK_LWIN) || is_key_down(VK_RWIN),
        keys: Vec::new(),
    };
    for vk in 0x01u16..=0xFE {
        if is_modifier_or_mouse(vk) {
            continue;
        }
        if is_key_down(VIRTUAL_KEY(vk)) {
            snapshot.keys.push(vk);
        }
    }
    snapshot
}

/// The virtual keys a recording never reports as "the key": the mouse buttons
/// (the click that opened the recorder may still be down when it starts) and
/// the modifier keys, which are held around the recorded key rather than
/// recorded themselves. The generic Shift/Ctrl/Alt codes (0x10–0x12) report
/// the same state as the left/right ones, so they are skipped too.
fn is_modifier_or_mouse(vk: u16) -> bool {
    matches!(
        vk,
        0x01 | 0x02 | 0x04 | 0x05 | 0x06 // left, right, middle and X mouse buttons
            | 0x10 | 0x11 | 0x12 // the generic Shift, Ctrl and Alt codes
            | 0x5B | 0x5C // left and right Windows
            | 0xA0..=0xA5 // left/right Shift, Ctrl and Alt
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_virtual_key_codes_are_the_windows_ones() {
        // V is 0x56 and either Ctrl is 0x11 in the Win32 virtual key table.
        assert_eq!(VK_V.0, 0x56);
        assert_eq!(VK_CONTROL.0, 0x11);
    }

    #[test]
    fn asking_about_a_key_that_is_not_down_is_harmless() {
        // A high-numbered virtual key nothing uses. The call must not panic,
        // even when the process has no window at all (tests).
        for key in [VIRTUAL_KEY(0xFE), VK_V, VK_CONTROL] {
            let _ = is_key_down(key);
        }
    }

    #[test]
    fn a_snapshot_can_be_read_and_only_reports_real_keys() {
        // The machine's own state is unknowable from a test, but reading it
        // must not panic, and the keys list must never contain the mouse or a
        // modifier (the recorder would treat one as the recorded key).
        let snapshot = keyboard_snapshot();
        for vk in &snapshot.keys {
            assert!(
                !is_modifier_or_mouse(*vk),
                "the snapshot reported {vk:#x}, which is a modifier or a mouse button"
            );
        }
    }

    #[test]
    fn mouse_buttons_and_modifiers_are_never_reported_as_keys() {
        for vk in [
            0x01, 0x02, 0x04, 0x05, 0x06, // mouse buttons
            0x10, 0x11, 0x12, // generic Shift, Ctrl, Alt
            0x5B, 0x5C, // Windows keys
            0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, // left/right modifiers
        ] {
            assert!(is_modifier_or_mouse(vk), "{vk:#x} should be excluded");
        }
        // A real key is not excluded.
        assert!(!is_modifier_or_mouse(0x50), "P must be recordable");
    }

    #[test]
    fn the_modifier_virtual_keys_are_the_windows_ones() {
        // The recorder's Ctrl/Shift/Alt/Win flags are built from the left and
        // right codes, which are the ones a keyboard really sends.
        assert_eq!(VK_LSHIFT.0, 0xA0);
        assert_eq!(VK_RSHIFT.0, 0xA1);
        assert_eq!(VK_LCONTROL.0, 0xA2);
        assert_eq!(VK_RCONTROL.0, 0xA3);
        assert_eq!(VK_LMENU.0, 0xA4);
        assert_eq!(VK_RMENU.0, 0xA5);
        assert_eq!(VK_LWIN.0, 0x5B);
        assert_eq!(VK_RWIN.0, 0x5C);
    }
}
