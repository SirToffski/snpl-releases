//! The foreground window and the cursor, in virtual-desktop physical pixels
//! (SPEC section 5.7).
//!
//! Two capture modes are built from this: "active window" crops to the
//! foreground window's rectangle, "current monitor" crops to the monitor that
//! contains the cursor.
//!
//! `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` is preferred over
//! `GetWindowRect` because the latter includes the invisible resize border
//! Windows 10/11 puts around every window, which would leave a few pixels of
//! the desktop in the screenshot.

use ::windows::Win32::Foundation::{POINT, RECT};
use ::windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use ::windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetForegroundWindow, GetWindowRect};

use crate::{Error, RectI, Result};

/// The foreground window's rectangle, or `None` when nothing is in front.
///
/// The rectangle is in virtual-desktop physical pixels, like everything else
/// the process sees (SPEC section 5.2: Per-Monitor DPI Awareness V2).
pub fn foreground_window_rect() -> Result<Option<RectI>> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return Ok(None);
    }

    let mut rect = RECT::default();
    let dwm = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            std::ptr::from_mut(&mut rect).cast(),
            size_of::<RECT>() as u32,
        )
    };
    if dwm.is_err() {
        // Not every window answers the DWMWA (it can fail on a window being
        // destroyed); the plain rectangle is better than no answer at all.
        unsafe { GetWindowRect(hwnd, &mut rect) }.map_err(Error::from)?;
    }

    Ok(Some(rect_i(rect)))
}

/// Where the cursor is, in virtual-desktop physical pixels.
pub fn cursor_pos() -> Result<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.map_err(Error::from)?;
    Ok((point.x, point.y))
}

/// A Win32 `RECT` as [`RectI`], with a negative width or height clamped to
/// zero (a degenerate window can report `right < left`).
fn rect_i(rect: RECT) -> RectI {
    RectI {
        x: rect.left,
        y: rect.top,
        w: (rect.right - rect.left).max(0),
        h: (rect.bottom - rect.top).max(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rect_with_its_edges_in_order_keeps_its_size() {
        let rect = rect_i(RECT {
            left: -100,
            top: 20,
            right: 100,
            bottom: 220,
        });
        assert_eq!(
            rect,
            RectI {
                x: -100,
                y: 20,
                w: 200,
                h: 200
            }
        );
    }

    #[test]
    fn a_degenerate_rect_is_clamped_instead_of_negative() {
        // A window mid-destroy can report right < left; a negative width would
        // make `crop` build an image with a huge unsigned size.
        let rect = rect_i(RECT {
            left: 50,
            top: 60,
            right: 10,
            bottom: 20,
        });
        assert_eq!(rect.w, 0);
        assert_eq!(rect.h, 0);
    }

    #[test]
    fn the_cursor_position_is_answered() {
        // There is always a cursor on a desktop session; the call must not
        // panic and must not fail on this machine.
        let (x, y) = cursor_pos().expect("the cursor position should be readable");
        let _ = (x, y);
    }

    #[test]
    fn the_foreground_window_is_asked_for_without_panicking() {
        // The answer depends on what is in front while the tests run, so only
        // the call itself is asserted.
        let _ = foreground_window_rect().expect("the foreground window query should not fail");
    }
}
