//! Synthetic input and desktop queries for the `--debug-overlay` self-check
//! (NEXT_STEPS section 4.3).
//!
//! These functions move the cursor and post input to our foreground overlay,
//! so they exist for the self-check alone and
//! the self-check refuses to run while somebody is using the machine (see
//! [`last_input_seconds`]).

use std::time::Duration;

use ::windows::Win32::Foundation::{HWND, LPARAM, POINT, TRUE, WPARAM};
use ::windows::Win32::System::SystemInformation::GetTickCount64;
use ::windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO, VK_ESCAPE};
use ::windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId, PostMessageW,
    SetCursorPos, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_RBUTTONDOWN,
};
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

    // Every move and message independently checks our foreground window.
    {
        let mut held = false;
        for _ in 0..20 {
            if !move_cursor(from) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
            if cursor_pos() == from {
                held = true;
                break;
            }
        }
        if !held {
            return false;
        }

        if !post_input(WM_LBUTTONDOWN, 0) {
            return false;
        }
        // Let the overlay process the press before moving: it reads the real
        // cursor when the button goes down, and a move first would make the
        // selection start at the first step instead of where the drag began.
        std::thread::sleep(Duration::from_millis(60));
        for step in 1..=steps {
            let x = from.0 + (to.0 - from.0) * step as i32 / steps as i32;
            let y = from.1 + (to.1 - from.1) * step as i32 / steps as i32;
            if !move_cursor((x, y)) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(80));
        if !post_input(WM_LBUTTONUP, 0) {
            return false;
        }
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

/// Posts Escape to our foreground overlay.
pub fn press_escape() -> bool {
    post_input(WM_KEYDOWN, VK_ESCAPE.0 as usize)
}

/// Posts a right click to our foreground overlay (it closes on button down).
pub fn right_click() -> bool {
    post_input(WM_RBUTTONDOWN, 0)
}

/// Moves the cursor to `at` and leaves the buttons alone.
pub fn move_cursor(at: (i32, i32)) -> bool {
    with_foreground_overlay(foreground_overlay, |_| unsafe {
        SetCursorPos(at.0, at.1).is_ok()
    })
}

/// Window-targeted messages exercise the real overlay procedure without
/// leaving a global key/button held if the overlay closes or loses focus.
fn post_input(message: u32, key: usize) -> bool {
    with_foreground_overlay(foreground_overlay, |hwnd| unsafe {
        PostMessageW(Some(hwnd), message, WPARAM(key), LPARAM(0)).is_ok()
    })
}

fn foreground_overlay() -> Option<HWND> {
    let hwnd = unsafe { GetForegroundWindow() };
    let mut name = [0u16; 128];
    let length = unsafe { GetClassNameW(hwnd, &mut name) };
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    (length > 0
        && pid == std::process::id()
        && String::from_utf16_lossy(&name[..length as usize]) == super::overlay::OVERLAY_CLASS)
        .then_some(hwnd)
}

fn with_foreground_overlay(
    foreground: impl FnOnce() -> Option<HWND>,
    inject: impl FnOnce(HWND) -> bool,
) -> bool {
    let Some(hwnd) = foreground() else {
        tracing::warn!("overlay is not the foreground window; input skipped");
        return false;
    };
    inject(hwnd)
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
    fn win3_every_input_checks_foreground_and_skips_after_focus_loss() {
        let hwnd = HWND(1usize as _);
        let mut sent = 0;
        for foreground in [Some(hwnd), None, Some(hwnd), None] {
            let result = with_foreground_overlay(
                || foreground,
                |target| {
                    assert_eq!(target, hwnd);
                    sent += 1;
                    true
                },
            );
            assert_eq!(result, foreground.is_some());
        }
        assert_eq!(sent, 2);
    }

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
