//! Synthetic input and desktop queries for the `--debug-overlay` self-check
//! (NEXT_STEPS section 4.3).
//!
//! These are the only functions in the program that move the cursor or press
//! keys without a human doing it, so they exist for the self-check alone and
//! the self-check refuses to run while somebody is using the machine (see
//! [`last_input_seconds`]).

use std::time::Duration;

use ::windows::Win32::Foundation::{HWND, LPARAM, POINT, TRUE};
use ::windows::Win32::System::SystemInformation::GetTickCount64;
use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    GetLastInputInfo, KEYBD_EVENT_FLAGS, LASTINPUTINFO, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, VK_ESCAPE, keybd_event, mouse_event,
};
use ::windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW, SetCursorPos};
use ::windows::core::BOOL;

/// How long ago the last keyboard or mouse input was, in seconds.
///
/// Used as the self-check's foreground guard: taking over the screen while
/// somebody is typing is rude and useless.
#[must_use]
pub fn last_input_seconds() -> Option<f64> {
    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // Safety: `GetLastInputInfo` writes into a local of the size it was told.
    let ok = unsafe { GetLastInputInfo(&mut info) }.as_bool();
    if !ok {
        return None;
    }
    // Safety: no arguments, no state.
    let now = unsafe { GetTickCount64() };
    let elapsed_ms = now.saturating_sub(u64::from(info.dwTime));
    Some(elapsed_ms as f64 / 1000.0)
}

/// Moves the cursor and drags the left button from `from` to `to`.
///
/// The cursor ends exactly on `to`, which is how the self-check knows the
/// rectangle it should get back. Returns `false` when another program kept
/// moving the cursor, so the press would have landed somewhere else: the
/// overlay reads the *real* cursor when the button goes down, so the check
/// must hold it still first (or report honestly that it could not).
pub fn drag(from: (i32, i32), to: (i32, i32), steps: u32) -> bool {
    let steps = steps.max(1);

    // Safety: cursor and button injection; nothing here can be expressed in
    // safe Rust, and the caller has already checked that nobody is typing.
    unsafe {
        let mut held = false;
        for _ in 0..20 {
            SetCursorPos(from.0, from.1).ok();
            std::thread::sleep(Duration::from_millis(25));
            if cursor_pos() == from {
                held = true;
                break;
            }
        }
        if !held {
            return false;
        }

        mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
        // Let the overlay process the press before moving: it reads the real
        // cursor when the button goes down, and a move first would make the
        // selection start at the first step instead of where the drag began.
        std::thread::sleep(Duration::from_millis(60));
        for step in 1..=steps {
            let x = from.0 + (to.0 - from.0) * step as i32 / steps as i32;
            let y = from.1 + (to.1 - from.1) * step as i32 / steps as i32;
            SetCursorPos(x, y).ok();
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(80));
        mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
    }
    true
}

/// Where the cursor is, in virtual-desktop physical pixels.
#[must_use]
pub fn cursor_pos() -> (i32, i32) {
    let mut point = POINT::default();
    // Safety: `GetCursorPos` writes into a local.
    unsafe {
        let _ = ::windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point);
    }
    (point.x, point.y)
}

/// Presses and releases Escape.
pub fn press_escape() {
    // Safety: key injection; see [`drag`].
    unsafe {
        keybd_event(VK_ESCAPE.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
        std::thread::sleep(Duration::from_millis(50));
        keybd_event(
            VK_ESCAPE.0 as u8,
            0,
            KEYBD_EVENT_FLAGS(KEYBD_EVENT_FLAGS_KEYUP),
            0,
        );
    }
}

/// The `KEYEVENTF_KEYUP` flag: 2.
const KEYBD_EVENT_FLAGS_KEYUP: u32 = 2;

/// Presses and releases the right mouse button.
pub fn right_click() {
    // Safety: button injection; see [`drag`].
    unsafe {
        mouse_event(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0, 0);
        std::thread::sleep(Duration::from_millis(50));
        mouse_event(MOUSEEVENTF_RIGHTUP, 0, 0, 0, 0);
    }
}

/// Moves the cursor to `at` and leaves the buttons alone.
pub fn move_cursor(at: (i32, i32)) {
    // Safety: cursor injection; see [`drag`].
    unsafe {
        SetCursorPos(at.0, at.1).ok();
    }
}

/// How many windows of `class` exist right now.
///
/// The overlay must leave none behind, on any exit path; the self-check proves
/// it by looking for the class after the overlay has returned.
#[must_use]
pub fn windows_with_class(class: &str) -> usize {
    struct Search<'a> {
        class: &'a str,
        found: usize,
    }

    // Safety: called by `EnumWindows` with the pointer we passed, which stays
    // alive for the whole enumeration.
    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let search = &mut *(lparam.0 as *mut Search<'_>);
            let mut buffer = [0u16; 128];
            let length = GetClassNameW(hwnd, &mut buffer);
            if length > 0 {
                let length = (length as usize).min(buffer.len());
                let name = String::from_utf16_lossy(&buffer[..length]);
                if name == search.class {
                    search.found += 1;
                }
            }
            TRUE
        }
    }

    let mut search = Search { class, found: 0 };
    // Safety: the callback and its state outlive the call.
    let _ = unsafe { EnumWindows(Some(callback), LPARAM(&raw mut search as isize)) };
    search.found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_input_time_is_readable() {
        // There is always some machine state to read; the value is not
        // asserted because the agent may be typing while the tests run.
        let _ = last_input_seconds();
    }

    #[test]
    fn a_class_nothing_uses_has_no_windows() {
        assert_eq!(windows_with_class("snp_ligase_no_such_window_class"), 0);
    }
}
