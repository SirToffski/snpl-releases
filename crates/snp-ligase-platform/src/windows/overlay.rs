//! The native region-selection overlay (SPEC section 5.6).
//!
//! This is deliberately **not** egui: it must cover each monitor exactly, in
//! physical pixels, and show the frozen capture 1:1. A GUI toolkit that scales
//! by DPI gets that wrong on a mixed-DPI desktop.
//!
//! One topmost popup window per monitor, each painted from two prepared 32-bit
//! bitmaps — the frame and the frame dimmed to 60% — with the selection drawn
//! bright. The selection is tracked in **virtual-desktop physical pixels** from
//! `GetCursorPos`; the window-local rectangles are derived from it with
//! [`crate::RectI::to_local`], which is where negative monitor origins would
//! otherwise bite.
//!
//! The one rule that matters more than any feature: **no overlay window may
//! survive the call.** Cancel, error, timeout, or an early return from a failed
//! `CreateWindowExW` — every path runs [`WindowGuard::destroy_windows`], both
//! explicitly and from `Drop`. An invisible topmost window that swallows input
//! is the worst failure this program could have.

// This file is the unsafe boundary for the overlay: it creates and destroys
// windows, runs a message loop and paints with GDI. Every function below is
// unsafe Win32 by nature, so the edition-2024 "wrap every call in its own
// block" style would add noise without adding information; each function
// carries its own safety argument instead.
#![allow(unsafe_op_in_unsafe_fn)]

use std::time::Duration;

use ::windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use ::windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, CreateSolidBrush, DEFAULT_GUI_FONT,
    DIB_RGB_COLORS, EndPaint, FillRect, GetStockObject, GetTextExtentPoint32W, HBRUSH, HDC, HFONT,
    HGDIOBJ, IntersectClipRect, InvalidateRect, NULLREGION, PAINTSTRUCT, RestoreDC, SaveDC,
    SelectObject, SetBkMode, SetDIBitsToDevice, SetTextColor, TRANSPARENT, TextOutW, UpdateWindow,
};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetWindowLongPtrW, GetWindowThreadProcessId, HCURSOR, IDC_CROSS, LoadCursorW, MSG,
    RegisterClassW, SW_SHOWNOACTIVATE, SetCursor, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    ShowWindow, TranslateMessage, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_NCCREATE, WM_PAINT, WM_RBUTTONDOWN, WM_SETCURSOR, WM_TIMER, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use ::windows::core::w;
use image::RgbaImage;

use crate::selection::{Outcome, SelectionState};
use crate::{Error, RectI, Result, VirtualCapture};

/// The overlay's window class. The self-check looks for windows of this class to
/// prove none was left behind.
pub const OVERLAY_CLASS: &str = "snp_ligase_overlay";

/// How long the overlay waits before giving up (SPEC 5.6: a stuck overlay
/// blocks the whole desktop).
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// The safety timer's id.
const TIMER_ID: usize = 1;

/// Padding around the size label, and its distance from the selection corner.
const LABEL_PADDING: i32 = 3;

/// How much dimmer the unselected pixels are (SPEC 5.6: 60%).
const DIM_FACTOR: f32 = 0.6;

/// Selects a region over a frozen capture. `None` means the user cancelled.
///
/// # Errors
///
/// An OS failure, which destroys every window before returning.
pub fn select_region(capture: &VirtualCapture) -> Result<Option<RectI>> {
    select_region_with_timeout(capture, TIMEOUT)
}

/// The same, with the safety timeout chosen by the caller.
///
/// The `--debug-overlay` self-check uses a short timeout to prove the timeout
/// path really cancels and really cleans up.
pub fn select_region_with_timeout(
    capture: &VirtualCapture,
    timeout: Duration,
) -> Result<Option<RectI>> {
    if capture.frames.is_empty() {
        return Err(Error::Capture {
            message: "there is nothing to select a region from".to_owned(),
        });
    }
    // Safety: every call inside needs the process's message queue and raw Win32
    // handles; none of them can be expressed in safe Rust.
    unsafe { run(capture, timeout) }
}

/// A top-down 32-bit BGRA bitmap: the layout `SetDIBitsToDevice` wants.
struct Dib {
    pixels: Vec<u8>,
    width: i32,
    height: i32,
    info: BITMAPINFO,
}

impl Dib {
    /// The frame as BGRA, alpha forced opaque.
    fn from_frame(image: &RgbaImage) -> Self {
        let (width, height) = (image.width() as i32, image.height() as i32);
        let mut pixels = Vec::with_capacity((width.max(0) * height.max(0) * 4) as usize);
        for pixel in image.pixels() {
            pixels.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
        Self::new(pixels, width, height)
    }

    /// The same bitmap with every channel multiplied by [`DIM_FACTOR`].
    ///
    /// Alpha stays opaque: the overlay windows are not layered, so GDI ignores
    /// it, and a half-transparent overlay would be a different feature.
    fn dimmed(original: &Dib) -> Self {
        let mut pixels = Vec::with_capacity(original.pixels.len());
        for pixel in original.pixels.as_chunks::<4>().0 {
            pixels.extend_from_slice(&[
                dim_channel(pixel[0]),
                dim_channel(pixel[1]),
                dim_channel(pixel[2]),
                255,
            ]);
        }
        Self::new(pixels, original.width, original.height)
    }

    fn new(pixels: Vec<u8>, width: i32, height: i32) -> Self {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative: a top-down bitmap, so row 0 is the top of the screen
                // and `SetDIBitsToDevice` can copy the whole thing in one call.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..BITMAPINFOHEADER::default()
            },
            ..BITMAPINFO::default()
        };
        Self {
            pixels,
            width,
            height,
            info,
        }
    }
}

/// One channel of the dimmed copy.
fn dim_channel(value: u8) -> u8 {
    (f32::from(value) * DIM_FACTOR).round().clamp(0.0, 255.0) as u8
}

/// Where the `W × H` label goes: just inside the selection's top-left corner,
/// clamped so it stays inside the monitor (SPEC 5.6).
///
/// Pure, so the clamping is unit-tested rather than clicked at.
#[must_use]
fn label_rect(selection_local: RectI, monitor: (i32, i32), label: (i32, i32)) -> RectI {
    // A selection at the very edge of a monitor must not push the label off it.
    let max_x = (monitor.0 - label.0 - LABEL_PADDING).max(0);
    let max_y = (monitor.1 - label.1 - LABEL_PADDING).max(0);
    RectI {
        x: (selection_local.x + LABEL_PADDING).clamp(0, max_x),
        y: (selection_local.y + LABEL_PADDING).clamp(0, max_y),
        w: label.0.max(0),
        h: label.1.max(0),
    }
}

/// One monitor's overlay: its window, its frozen pixels and its monitor.
struct OverlayWindow {
    hwnd: HWND,
    /// The monitor's rectangle in virtual-desktop pixels; also the window's
    /// position, so window-local coordinates are `virtual - monitor.origin`.
    monitor: RectI,
    original: Dib,
    dimmed: Dib,
}

/// Everything the window procedure needs. One per `select_region` call, on the
/// calling thread, with the pointer parked in each window's `GWLP_USERDATA`.
struct OverlayState {
    windows: Vec<OverlayWindow>,
    selection: SelectionState,
    black: HBRUSH,
    white: HBRUSH,
    cross: Option<HCURSOR>,
    label_font: HFONT,
    /// Paint timing, for SPEC 12 M7's "the overlay selection tracks the mouse
    /// without visible lag": the number of paints and the worst one are logged
    /// when the overlay ends.
    paints: u32,
    paint_total: Duration,
    paint_max: Duration,
}

impl OverlayState {
    /// Records one window paint's duration.
    fn record_paint(&mut self, elapsed: Duration) {
        self.paints += 1;
        self.paint_total += elapsed;
        self.paint_max = self.paint_max.max(elapsed);
    }
}

impl OverlayState {
    fn new(capture: &VirtualCapture) -> Self {
        // Two prepared bitmaps per monitor (the frame and its dimmed copy) are
        // the expensive part of opening the overlay — about 15 ms for a 1440p
        // monitor — and they are independent, so one thread each keeps SPEC
        // 12 M7's "hotkey to overlay visible" budget flat as monitors are
        // added. No window exists yet, so a panic here cannot leave one behind.
        // Only the bitmaps cross the thread boundary: a window handle is not
        // `Send`, so the windows themselves are built here.
        let bitmaps: Vec<(Dib, Dib)> = std::thread::scope(|scope| {
            let spawned: Vec<_> = capture
                .frames
                .iter()
                .map(|frame| {
                    scope.spawn(|| {
                        let original = Dib::from_frame(&frame.image);
                        let dimmed = Dib::dimmed(&original);
                        (original, dimmed)
                    })
                })
                .collect();
            spawned
                .into_iter()
                .map(|handle| handle.join().expect("the overlay's bitmap thread panicked"))
                .collect()
        });
        let windows = capture
            .frames
            .iter()
            .zip(bitmaps)
            .map(|(frame, (original, dimmed))| OverlayWindow {
                hwnd: HWND::default(),
                monitor: frame.info.rect,
                original,
                dimmed,
            })
            .collect();
        // Safety: three GDI object creations with fixed arguments; none can
        // fail in a way that needs handling (they return NULL, which the
        // drawing code tolerates).
        unsafe {
            Self {
                windows,
                selection: SelectionState::new(),
                black: CreateSolidBrush(COLORREF(0x0000_0000)),
                white: CreateSolidBrush(COLORREF(0x00FF_FFFF)),
                cross: LoadCursorW(None, IDC_CROSS).ok(),
                label_font: HFONT(GetStockObject(DEFAULT_GUI_FONT).0),
                paints: 0,
                paint_total: Duration::ZERO,
                paint_max: Duration::ZERO,
            }
        }
    }
}

/// Lives inside `run`, after the boxed state, so it drops before that state.
/// Only a raw pointer is retained: neither startup nor teardown holds a state
/// borrow across a Win32 call that can synchronously re-enter `window_proc`.
struct WindowGuard(*mut OverlayState);

impl WindowGuard {
    /// Destroys every window, including partial startup. Idempotent.
    fn destroy_windows(&mut self) {
        // Safety: the box in `run` outlives this guard. End the borrow before
        // calling Win32; a callback can still arrive until all user data is clear.
        let handles: Vec<_> = unsafe { &mut *self.0 }
            .windows
            .iter_mut()
            .map(|window| std::mem::take(&mut window.hwnd))
            .filter(|hwnd| !hwnd.is_invalid())
            .collect();
        destroy_windows(&handles);
    }
}

impl Drop for WindowGuard {
    fn drop(&mut self) {
        self.destroy_windows();
    }
}

/// Clears ALL windows' state pointers before destroying any window.
///
/// `GWLP_USERDATA` is cleared **before** `DestroyWindow` (G5), so the window
/// procedure sees a null state during `WM_DESTROY`/`WM_NCDESTROY` and can
/// never reach state through another overlay window during re-entry either.
/// Even a failed destruction leaves no pointer to the soon-to-be-freed state.
fn destroy_windows(handles: &[HWND]) {
    // Safety: all handles belong to this thread. No OverlayState reference is
    // live here, and clearing user data itself sends no window messages.
    unsafe {
        for &hwnd in handles {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
        for &hwnd in handles {
            if let Err(err) = DestroyWindow(hwnd) {
                tracing::error!(?hwnd, %err, "could not destroy an overlay window (state pointer cleared)");
            }
        }
    }
}

impl Drop for OverlayState {
    fn drop(&mut self) {
        // Safety: freeing the GDI objects this state created. A NULL handle is
        // tolerated by DeleteObject through the HGDIOBJ conversion.
        unsafe {
            for object in [
                HGDIOBJ(self.black.0),
                HGDIOBJ(self.white.0),
                HGDIOBJ(self.label_font.0),
            ] {
                if !object.is_invalid() {
                    let _ = ::windows::Win32::Graphics::Gdi::DeleteObject(object);
                }
            }
        }
    }
}

/// The overlay, end to end. Returns the selection, or `None` if cancelled.
///
/// # Safety
///
/// Creates windows, runs a message loop and calls GDI with raw handles; must be
/// called on a thread that can own a message queue (the controller thread does).
unsafe fn run(capture: &VirtualCapture, timeout: Duration) -> Result<Option<RectI>> {
    let started = std::time::Instant::now();
    let instance: HINSTANCE = GetModuleHandleW(None).map_err(Error::from)?.into();
    register_class(instance)?;

    let mut state = Box::new(OverlayState::new(capture));
    let state_ptr: *mut OverlayState = &raw mut *state;
    let mut windows = WindowGuard(state_ptr);

    // One window per monitor, exactly on it (SPEC 5.6).
    for index in 0..state.windows.len() {
        let monitor = state.windows[index].monitor;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("snp_ligase_overlay"),
            w!("snp-ligase region"),
            WS_POPUP,
            monitor.x,
            monitor.y,
            monitor.w,
            monitor.h,
            None,
            None,
            Some(instance),
            Some(state_ptr.cast()),
        )
        .map_err(|err| Error::Os {
            message: format!("could not create an overlay window: {err}"),
        })?;
        state.windows[index].hwnd = hwnd;
    }

    // Show every window, and put the one under the cursor in front so it
    // receives the keyboard (SPEC 5.6).
    let cursor = cursor_pos()?;
    for index in 0..state.windows.len() {
        let hwnd = state.windows[index].hwnd;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = InvalidateRect(Some(hwnd), None, false);
        let _ = UpdateWindow(hwnd);
    }

    // SPEC 12 M7's budget: hotkey to overlay visible. This is the time from
    // `select_region` being entered to every window being painted once; the
    // controller logs the hide-and-capture time that comes before it. The
    // foreground work below is not part of it: the overlay is already on the
    // screen at this point, and taking the foreground only matters for the keys.
    tracing::info!(
        windows = state.windows.len(),
        visible_ms = started.elapsed().as_secs_f64() * 1000.0,
        "overlay visible"
    );

    let front = state
        .windows
        .iter()
        .find(|window| window.monitor.contains(cursor.0, cursor.1))
        .or_else(|| state.windows.first())
        .map(|window| window.hwnd);
    if let Some(front) = front {
        force_foreground(front);
        let timer = SetTimer(
            Some(front),
            TIMER_ID,
            timeout.as_millis().min(u128::from(u32::MAX)) as u32,
            None,
        );
        require_timer(timer, &mut state.selection)?;
    }

    // The message loop: blocking, so nothing spins while the user decides.
    // Every decision (mouse up, Esc, right click, the safety timer) sets
    // the shared selection outcome from inside the window procedure. A failure
    // from `GetMessageW` is a cancel, never another message to dispatch (G5).
    let mut message = MSG::default();
    loop {
        match message_loop_step(GetMessageW(&mut message, None, 0, 0).0) {
            MessageLoop::Message => {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
                if state.selection.outcome() != Outcome::Waiting {
                    break;
                }
            }
            MessageLoop::Quit => break,
            MessageLoop::Error => {
                tracing::warn!("the overlay's message loop failed; cancelling the overlay");
                state.selection.timed_out();
                break;
            }
        }
    }

    // The windows go before the caller sees anything, and `Drop` is the belt to
    // this pair of braces. A message-loop failure with no decision is a cancel.
    windows.destroy_windows();
    if state.paints > 0 {
        let average = state.paint_total.as_secs_f64() * 1000.0 / f64::from(state.paints);
        tracing::info!(
            paints = state.paints,
            avg_ms = average,
            max_ms = state.paint_max.as_secs_f64() * 1000.0,
            "overlay paints"
        );
    }
    match state.selection.outcome() {
        Outcome::Selected(rect) => Ok(Some(rect)),
        Outcome::Waiting | Outcome::Cancelled => Ok(None),
    }
}

/// SetTimer returns zero on failure. Never enter a blocking loop without the
/// safety timer; the caller's guard tears down all windows on this error.
fn require_timer(timer: usize, selection: &mut SelectionState) -> Result<()> {
    if timer == 0 {
        selection.timed_out();
        tracing::error!("could not create the region overlay safety timer; cancelling the overlay");
        return Err(Error::Os {
            message: "could not create the region overlay safety timer (SetTimer returned 0)"
                .to_owned(),
        });
    }
    Ok(())
}

/// Registers the overlay's window class, once per process.
///
/// # Safety
///
/// Calls `RegisterClassW` with a `WNDCLASSW` that lives for the call.
unsafe fn register_class(instance: HINSTANCE) -> Result<()> {
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: w!("snp_ligase_overlay"),
        // No background brush: WM_ERASEBKGND is answered instead, so the
        // window is painted exactly once per invalid region and never flashes.
        ..WNDCLASSW::default()
    };
    let atom = RegisterClassW(&raw const class);
    if atom == 0 {
        // A second overlay in the same process (the self-check opens one right
        // after a real capture) legitimately finds the class already
        // registered: ERROR_CLASS_ALREADY_EXISTS is 1410.
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(1410) {
            return Err(Error::Os {
                message: format!("could not register the overlay window class: {error}"),
            });
        }
    }
    Ok(())
}

/// The cursor position, in virtual-desktop physical pixels.
fn cursor_pos() -> Result<(i32, i32)> {
    let mut point = POINT::default();
    // Safety: `GetCursorPos` writes into a local.
    unsafe { GetCursorPos(&mut point) }.map_err(Error::from)?;
    Ok((point.x, point.y))
}

/// What one `GetMessageW` result means (G5).
///
/// `GetMessageW` returns `-1` on error, `0` for `WM_QUIT` and a positive value
/// for a message. `BOOL::as_bool` is `!= 0`, so the old loop read the error as
/// a message and would have spun forever instead of cancelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageLoop {
    /// A message to translate and dispatch.
    Message,
    /// `WM_QUIT`: the loop is over.
    Quit,
    /// The call failed.
    Error,
}

/// Classifies one `GetMessageW` result.
#[must_use]
fn message_loop_step(result: i32) -> MessageLoop {
    match result {
        0 => MessageLoop::Quit,
        1.. => MessageLoop::Message,
        _ => MessageLoop::Error,
    }
}

/// The overlay's window procedure.
///
/// # Safety
///
/// Called by Windows with the message for one of our windows. The state pointer
/// comes from `GWLP_USERDATA`, which is only ever set from the
/// `lpCreateParams` this module passed in.
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
    if state_ptr.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    // Keep the pointer raw until a branch needs it. In particular SetCapture,
    // ReleaseCapture, BeginPaint and DefWindowProcW can synchronously call us
    // again. No mutable reference to the state may survive any of those calls.
    match message {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut paint);
            let index = (*state_ptr)
                .windows
                .iter()
                .position(|window| window.hwnd == hwnd);
            if let Some(index) = index {
                let started = std::time::Instant::now();
                // Only GDI bitmap/text calls inside paint_window; no message
                // dispatch. The shared borrow ends before recording timing.
                paint_window(&*state_ptr, index, hdc);
                (*state_ptr).record_paint(started.elapsed());
            }
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // Handled: the paint below covers the whole invalid region, and
            // erasing first would flash.
            LRESULT(1)
        }
        WM_SETCURSOR => {
            if let Some(cross) = (*state_ptr).cross {
                SetCursor(Some(cross));
            }
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            // Capture so the drag keeps reporting when it leaves a monitor.
            SetCapture(hwnd);
            if let Ok((x, y)) = cursor_pos() {
                change_selection(state_ptr, |selection| selection.press(x, y));
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Ok((x, y)) = cursor_pos() {
                change_selection(state_ptr, |selection| selection.pointer_moved(x, y));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            // The release position is the last word, even if no move event
            // arrived after it.
            if let Ok((x, y)) = cursor_pos() {
                change_selection(state_ptr, |selection| {
                    selection.release(x, y);
                });
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            (*state_ptr).selection.right_click();
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 as u16 == VK_ESCAPE.0 {
                (*state_ptr).selection.escape();
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_ID {
                tracing::warn!("the region overlay timed out");
                (*state_ptr).selection.timed_out();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// Apply only pure state changes while mutably borrowed, then repaint after
/// that borrow ends. The pointer has the same lifetime as in `window_proc`.
unsafe fn change_selection(state_ptr: *mut OverlayState, change: impl FnOnce(&mut SelectionState)) {
    let (old, new) = {
        let selection = &mut (*state_ptr).selection;
        let old = selection.selection();
        change(selection);
        (old, selection.selection())
    };
    if old != new {
        invalidate_change(&*state_ptr, old, new);
    }
}

/// Brings a window to the foreground, so the overlay receives the keyboard
/// (SPEC 5.6).
///
/// `SetForegroundWindow` alone is refused while another process is in front:
/// Windows only lets a process take the foreground if it already has it, or if
/// it owns the thread that does. The standard way around that is to attach our
/// input queue to the foreground thread's for the length of the call — after
/// which the window can be focused, and the queues are detached again.
///
/// # Safety
///
/// Calls Win32 with handles owned by this thread's windows.
unsafe fn force_foreground(hwnd: HWND) {
    let foreground = GetForegroundWindow();
    let foreground_thread = if foreground.is_invalid() {
        0
    } else {
        GetWindowThreadProcessId(foreground, None)
    };
    let our_thread = GetCurrentThreadId();
    let attached = foreground_thread != 0
        && foreground_thread != our_thread
        && AttachThreadInput(our_thread, foreground_thread, true).as_bool();

    let _ = SetForegroundWindow(hwnd);
    let _ = BringWindowToTop(hwnd);
    if attached {
        let _ = AttachThreadInput(our_thread, foreground_thread, false);
    }
    let _ = SetFocus(Some(hwnd));
    tracing::debug!(attached, "brought the overlay to the foreground");
}

/// Paints one window: the dimmed frame everywhere, the original inside the
/// selection, the two outlines and the size label.
unsafe fn paint_window(state: &OverlayState, index: usize, hdc: HDC) {
    let window = &state.windows[index];
    let monitor_local = RectI {
        x: 0,
        y: 0,
        w: window.monitor.w,
        h: window.monitor.h,
    };

    let saved = SaveDC(hdc);
    // G6: only the parts of the window the frame does not cover are filled
    // black. Filling the whole invalid area first flashed black under the
    // blitted frame; everything the old fill covered is blitted below, so the
    // only pixels that can need a background are the strips of a frame that
    // turned out smaller than its monitor (SPEC 5.4 uses it as-is).
    for strip in uncovered_strips(
        (monitor_local.w, monitor_local.h),
        (window.original.width, window.original.height),
    ) {
        FillRect(
            hdc,
            &RECT {
                left: strip.x,
                top: strip.y,
                right: strip.right(),
                bottom: strip.bottom(),
            },
            state.black,
        );
    }
    blit(hdc, &window.dimmed);

    let Some(selection) = state.selection.selection() else {
        if saved != 0 {
            let _ = RestoreDC(hdc, saved);
        }
        return;
    };
    let local = selection
        .intersection(window.monitor)
        .to_local(window.monitor);
    // The bright part: the original pixels, clipped to the selection.
    let inner = SaveDC(hdc);
    if IntersectClipRect(hdc, local.x, local.y, local.right(), local.bottom()) != NULLREGION {
        blit(hdc, &window.original);
    }
    if inner != 0 {
        let _ = RestoreDC(hdc, inner);
    }

    // A white line on the selection's edge, with a black line around it.
    // Clip the real edges to this monitor, never outline the intersection:
    // that would introduce a false vertical/horizontal line at a monitor seam.
    for (offset, brush) in [(1, state.black), (0, state.white)] {
        for edge in outline_edges(selection, window.monitor, offset) {
            FillRect(
                hdc,
                &RECT {
                    left: edge.x,
                    top: edge.y,
                    right: edge.right(),
                    bottom: edge.bottom(),
                },
                brush,
            );
        }
    }

    // The size label, on the monitor that owns the selection's top-left corner,
    // so a selection spanning two monitors shows it exactly once.
    if window.monitor.contains(selection.x, selection.y) {
        draw_size_label(state, hdc, local, monitor_local, selection);
    }

    if saved != 0 {
        let _ = RestoreDC(hdc, saved);
    }
}

/// One-pixel outline strips in monitor-local coordinates. Construct the full
/// outline BEFORE clipping, so even an outer black edge just across a monitor
/// boundary belongs to the neighboring monitor rather than disappearing.
fn outline_edges(selection: RectI, monitor: RectI, offset: i32) -> Vec<RectI> {
    if selection.is_empty() {
        return Vec::new();
    }
    let left = selection.x - offset;
    let top = selection.y - offset;
    let right = selection.right() + offset;
    let bottom = selection.bottom() + offset;
    [
        RectI::new(left, top, 1, bottom - top),
        RectI::new(right - 1, top, 1, bottom - top),
        RectI::new(left, top, right - left, 1),
        RectI::new(left, bottom - 1, right - left, 1),
    ]
    .into_iter()
    .map(|edge| edge.intersection(monitor).to_local(monitor))
    .filter(|edge| !edge.is_empty())
    .collect()
}

/// Draws `W × H` near the selection's top-left corner (SPEC 5.6).
unsafe fn draw_size_label(
    state: &OverlayState,
    hdc: HDC,
    selection_local: RectI,
    monitor_local: RectI,
    selection: RectI,
) {
    let text = format!("{} × {}", selection.w, selection.h);
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = ::windows::Win32::Foundation::SIZE::default();
    SelectObject(hdc, HGDIOBJ(state.label_font.0));
    if !GetTextExtentPoint32W(hdc, &wide, &mut size).as_bool() {
        return;
    }
    let label = label_rect(
        selection_local,
        (monitor_local.w, monitor_local.h),
        (size.cx + LABEL_PADDING * 2, size.cy + LABEL_PADDING * 2),
    );
    FillRect(
        hdc,
        &RECT {
            left: label.x,
            top: label.y,
            right: label.right(),
            bottom: label.bottom(),
        },
        state.black,
    );
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(0x00FF_FFFF));
    let _ = TextOutW(hdc, label.x + LABEL_PADDING, label.y + LABEL_PADDING, &wide);
}

/// Copies a whole DIB into the DC at the window's origin. The DC's clip region
/// decides how much of it survives.
unsafe fn blit(hdc: HDC, dib: &Dib) {
    SetDIBitsToDevice(
        hdc,
        0,
        0,
        dib.width as u32,
        dib.height as u32,
        0,
        0,
        0,
        dib.height as u32,
        dib.pixels.as_ptr().cast(),
        &raw const dib.info,
        DIB_RGB_COLORS,
    );
}

/// The parts of a monitor a frame does not cover: the strips to fill black, so
/// a frame smaller than its monitor leaves no stale pixels (G6).
///
/// Pure, so the geometry is unit-tested. A frame as large as its monitor — the
/// normal case — leaves no strips at all: nothing is filled, so nothing can
/// flash black under the blitted frame.
#[must_use]
fn uncovered_strips(monitor: (i32, i32), frame: (i32, i32)) -> Vec<RectI> {
    let width = monitor.0.max(0);
    let height = monitor.1.max(0);
    let frame_width = frame.0.clamp(0, width);
    let frame_height = frame.1.clamp(0, height);
    let mut strips = Vec::new();
    if frame_height < height {
        // Everything below the frame.
        strips.push(RectI::new(0, frame_height, width, height - frame_height));
    }
    if frame_width < width {
        // The column to the right of the frame, up to the frame's height, so
        // the two strips do not overlap.
        strips.push(RectI::new(
            frame_width,
            0,
            width - frame_width,
            frame_height,
        ));
    }
    strips.retain(|strip| !strip.is_empty());
    strips
}

/// Invalidates the part of every monitor whose appearance changed.
///
/// A full repaint on every mouse move over two 4K monitors is wasteful; only
/// the union of the old and the new selection (plus room for the outlines and
/// the label) is repainted.
fn invalidate_change(state: &OverlayState, old: Option<RectI>, new: Option<RectI>) {
    let Some(dirty) = union(old, new) else {
        return;
    };
    // The outline sits one pixel outside the selection, and the label hangs
    // inside its top-left corner; a generous margin keeps both in the repaint.
    let margin = 4 + state_label_margin();
    let dirty = RectI::new(
        dirty.x - margin,
        dirty.y - margin,
        dirty.w + margin * 2,
        dirty.h + margin * 2,
    );
    for window in &state.windows {
        let local = dirty.intersection(window.monitor).to_local(window.monitor);
        if local.is_empty() {
            continue;
        }
        let rect = RECT {
            left: local.x,
            top: local.y,
            right: local.right(),
            bottom: local.bottom(),
        };
        // Safety: invalidating one of our own windows; the result is not
        // interesting (InvalidateRect only fails for a bad handle).
        unsafe {
            let _ = InvalidateRect(Some(window.hwnd), Some(&raw const rect), false);
        }
    }
}

/// Room for the size label's height, in pixels. The label is drawn with the
/// default GUI font, which is under 30 px even at 200% scaling.
const fn state_label_margin() -> i32 {
    30
}

/// The union of two optional rectangles.
fn union(a: Option<RectI>, b: Option<RectI>) -> Option<RectI> {
    match (a, b) {
        (Some(a), Some(b)) => Some(RectI {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
            w: a.right().max(b.right()) - a.x.min(b.x),
            h: a.bottom().max(b.bottom()) - a.y.min(b.y),
        }),
        (Some(rect), None) | (None, Some(rect)) => Some(rect),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Windows review's finding 1: every drag direction gives the same
    /// selection, not only down-right.
    #[test]
    fn a_drag_selects_the_same_rectangle_in_every_direction() {
        let expected = RectI::new(100, 50, 400, 300);
        for (start, end) in [
            ((100, 50), (500, 350)), // down-right
            ((500, 350), (100, 50)), // up-left
            ((500, 50), (100, 350)), // down-left
            ((100, 350), (500, 50)), // up-right
        ] {
            let mut selection = SelectionState::new();
            selection.press(start.0, start.1);
            selection.pointer_moved(end.0, end.1);
            assert!(selection.release(end.0, end.1));
            assert_eq!(
                selection.outcome(),
                Outcome::Selected(expected),
                "{start:?} -> {end:?}"
            );
        }
        let mut selection = SelectionState::new();
        selection.press(7, 7);
        assert!(!selection.release(7, 7));
        assert_eq!(selection.outcome(), Outcome::Waiting);
    }
    use image::Rgba;

    #[test]
    fn a_missing_safety_timer_cancels_instead_of_waiting_forever() {
        let mut selection = SelectionState::new();
        assert!(require_timer(1, &mut selection).is_ok());
        assert_eq!(selection.outcome(), Outcome::Waiting);
        selection.press(10, 10);
        let err = require_timer(0, &mut selection).unwrap_err();
        assert!(err.to_string().contains("SetTimer returned 0"));
        assert_eq!(selection.outcome(), Outcome::Cancelled);
        assert!(!selection.release(100, 100));
    }

    #[test]
    fn spanning_outlines_have_no_edges_at_monitor_seams() {
        let selection = RectI::new(-30, -20, 60, 40);
        // Horizontal and vertical seams, negative origins, and a monitor
        // wholly inside the selection. Compare pixels with the full outline.
        for monitor in [
            RectI::new(-100, -100, 100, 200),
            RectI::new(0, -100, 100, 200),
            RectI::new(-100, -100, 200, 100),
            RectI::new(-100, 0, 200, 100),
            RectI::new(-10, -10, 20, 20),
        ] {
            for offset in [0, 1] {
                let edges = outline_edges(selection, monitor, offset);
                for y in monitor.y..monitor.bottom() {
                    for x in monitor.x..monitor.right() {
                        let left = selection.x - offset;
                        let right = selection.right() - 1 + offset;
                        let top = selection.y - offset;
                        let bottom = selection.bottom() - 1 + offset;
                        let expected = ((x == left || x == right) && (top..=bottom).contains(&y))
                            || ((y == top || y == bottom) && (left..=right).contains(&x));
                        let painted = edges
                            .iter()
                            .any(|edge| edge.contains(x - monitor.x, y - monitor.y));
                        assert_eq!(
                            painted, expected,
                            "{monitor:?}, offset {offset}, ({x}, {y})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_outer_outline_can_land_on_the_neighboring_monitor() {
        let selection = RectI::new(0, 10, 30, 40);
        let neighbor = RectI::new(-100, 0, 100, 100);
        assert!(outline_edges(selection, neighbor, 0).is_empty());
        assert_eq!(
            outline_edges(selection, neighbor, 1),
            vec![
                RectI::new(99, 9, 1, 42),
                RectI::new(99, 9, 1, 1),
                RectI::new(99, 50, 1, 1),
            ]
        );
        assert!(outline_edges(RectI::new(0, 0, 0, 5), neighbor, 1).is_empty());
    }

    #[test]
    fn the_dimmed_copy_uses_the_spec_factor() {
        // SPEC 5.6: every channel times 0.6, alpha left opaque (the windows are
        // not layered, so GDI never reads it).
        let rgb = [100u8, 200, 255];
        let mut pixels = Vec::new();
        for channel in rgb {
            pixels.extend_from_slice(&[channel, channel, channel, 255]);
        }
        let original = Dib::new(pixels, 3, 1);
        let dimmed = Dib::dimmed(&original);
        assert_eq!(dimmed.width, 3);
        assert_eq!(dimmed.height, 1);
        let channels: Vec<u8> = dimmed
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[0])
            .collect();
        assert_eq!(
            channels,
            vec![
                dim_channel(rgb[0]),
                dim_channel(rgb[1]),
                dim_channel(rgb[2])
            ]
        );
        assert_eq!(dim_channel(255), 153, "255 * 0.6 rounds to 153");
        assert_eq!(dim_channel(100), 60);
        assert_eq!(
            dimmed.pixels.as_chunks::<4>().0[0][3],
            255,
            "alpha must stay opaque"
        );
    }

    #[test]
    fn a_frame_becomes_a_top_down_bgra_bitmap() {
        let mut image = RgbaImage::new(2, 1);
        image.put_pixel(0, 0, Rgba([10, 20, 30, 40])); // translucent: the overlay is opaque
        image.put_pixel(1, 0, Rgba([200, 100, 50, 255]));
        let dib = Dib::from_frame(&image);

        assert_eq!(dib.width, 2);
        assert_eq!(dib.height, 1);
        assert_eq!(
            &dib.pixels[0..4],
            &[30, 20, 10, 255],
            "BGRA, alpha forced opaque"
        );
        assert_eq!(&dib.pixels[4..8], &[50, 100, 200, 255]);
        // A negative height is what makes `SetDIBitsToDevice` read row 0 first.
        assert_eq!(dib.info.bmiHeader.biHeight, -1);
        assert_eq!(dib.info.bmiHeader.biBitCount, 32);
        assert_eq!(dib.info.bmiHeader.biCompression, BI_RGB.0);
    }

    #[test]
    fn the_size_label_is_kept_inside_the_monitor() {
        let monitor = (1920, 1080);
        let label = (60, 18);

        // An ordinary selection: the label sits just inside its top-left corner.
        assert_eq!(
            label_rect(
                RectI {
                    x: 10,
                    y: 20,
                    w: 100,
                    h: 100
                },
                monitor,
                label
            ),
            RectI {
                x: 13,
                y: 23,
                w: 60,
                h: 18
            }
        );

        // A selection touching the right and bottom edges pulls the label back
        // inside the monitor instead of painting it off-screen.
        assert_eq!(
            label_rect(
                RectI {
                    x: 1900,
                    y: 1070,
                    w: 20,
                    h: 10
                },
                monitor,
                label
            ),
            RectI {
                x: 1857,
                y: 1059,
                w: 60,
                h: 18
            }
        );

        // A selection that reaches into a neighbouring monitor (negative local
        // coordinates) still keeps the label at the window's top-left.
        assert_eq!(
            label_rect(
                RectI {
                    x: -50,
                    y: -10,
                    w: 200,
                    h: 200
                },
                monitor,
                label
            ),
            RectI {
                x: 0,
                y: 0,
                w: 60,
                h: 18
            }
        );
    }

    #[test]
    fn the_union_of_two_selections_covers_both() {
        let a = RectI {
            x: 100,
            y: 100,
            w: 50,
            h: 50,
        };
        let b = RectI {
            x: 10,
            y: 200,
            w: 30,
            h: 30,
        };
        assert_eq!(
            union(Some(a), Some(b)),
            Some(RectI {
                x: 10,
                y: 100,
                w: 140,
                h: 130
            })
        );
        assert_eq!(union(Some(a), None), Some(a));
        assert_eq!(union(None, Some(b)), Some(b));
        assert_eq!(union(None, None), None);
    }

    #[test]
    fn only_the_strips_a_frame_does_not_cover_are_filled() {
        // G6: a frame as large as its monitor leaves nothing to fill, which is
        // what removes the black flash under the blit.
        assert!(uncovered_strips((1920, 1080), (1920, 1080)).is_empty());
        assert!(
            uncovered_strips((100, 100), (200, 200)).is_empty(),
            "a larger frame covers everything"
        );

        // A smaller frame: everything below it, and the column beside it.
        assert_eq!(
            uncovered_strips((200, 100), (120, 60)),
            vec![RectI::new(0, 60, 200, 40), RectI::new(120, 0, 80, 60)]
        );

        // An empty frame covers nothing: the whole monitor is one strip.
        assert_eq!(
            uncovered_strips((200, 100), (0, 0)),
            vec![RectI::new(0, 0, 200, 100)]
        );

        // Only one dimension short.
        assert_eq!(
            uncovered_strips((200, 100), (200, 60)),
            vec![RectI::new(0, 60, 200, 40)]
        );
        assert_eq!(
            uncovered_strips((200, 100), (120, 100)),
            vec![RectI::new(120, 0, 80, 100)]
        );
    }

    #[test]
    fn a_failed_get_message_is_a_cancel_not_a_message() {
        // G5: -1 is what `GetMessageW` returns on failure. `BOOL(-1).as_bool()`
        // is true, which is what the old loop checked, so an error would have
        // been dispatched like a message and the loop would never end.
        assert!(
            ::windows::core::BOOL(-1).as_bool(),
            "the old check really did read the error as a message"
        );
        assert_eq!(message_loop_step(-1), MessageLoop::Error);
        assert_eq!(message_loop_step(0), MessageLoop::Quit);
        assert_eq!(message_loop_step(1), MessageLoop::Message);
        assert_eq!(message_loop_step(0x7FFF), MessageLoop::Message);
    }

    #[test]
    fn timer_failure_destroys_all_windows_with_all_state_pointers_cleared_first() {
        // G5: the window procedure must never see the state pointer during
        // destruction, or a torn-down `OverlayState` could be used after it was
        // freed. A real window is created for this, and its procedure records
        // what `GWLP_USERDATA` held at `WM_NCDESTROY`.
        use ::windows::Win32::UI::WindowsAndMessaging::WM_NCDESTROY;
        use std::sync::atomic::{AtomicIsize, Ordering};

        static SEEN: AtomicIsize = AtomicIsize::new(-1);
        static OTHER: AtomicIsize = AtomicIsize::new(0);
        static SEEN_OTHER: AtomicIsize = AtomicIsize::new(-1);

        unsafe extern "system" fn test_proc(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if message == WM_NCDESTROY {
                SEEN.store(GetWindowLongPtrW(hwnd, GWLP_USERDATA), Ordering::SeqCst);
                let other = OTHER.load(Ordering::SeqCst);
                if other != 0 && hwnd.0 as isize != other {
                    SEEN_OTHER.store(
                        GetWindowLongPtrW(HWND(other as *mut _), GWLP_USERDATA),
                        Ordering::SeqCst,
                    );
                }
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }

        let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }
            .expect("the module handle should work")
            .into();
        let class = WNDCLASSW {
            lpfnWndProc: Some(test_proc),
            hInstance: instance,
            lpszClassName: w!("snp_ligase_overlay_destroy_test"),
            ..WNDCLASSW::default()
        };
        let atom = unsafe { RegisterClassW(&raw const class) };
        assert_ne!(atom, 0, "the test window class could not be registered");

        let create_window = || unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("snp_ligase_overlay_destroy_test"),
                w!("destroy test"),
                WS_POPUP,
                0,
                0,
                10,
                10,
                None,
                None,
                Some(instance),
                None,
            )
        };
        let handles = [
            create_window().expect("the first test window could not be created"),
            create_window().expect("the second test window could not be created"),
        ];
        OTHER.store(handles[1].0 as isize, Ordering::SeqCst);
        let mut state = Box::new(OverlayState::new(&VirtualCapture::default()));
        for hwnd in handles {
            // Safety: our own window; the value only has to be non-zero.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 42);
            }
            state.windows.push(OverlayWindow {
                hwnd,
                monitor: RectI::new(0, 0, 10, 10),
                original: Dib::new(Vec::new(), 0, 0),
                dimmed: Dib::new(Vec::new(), 0, 0),
            });
        }
        // Exercise the actual early-return path: the guard must run when
        // the safety timer is unavailable, before the state can be freed.
        let result = (|| {
            let _guard = WindowGuard(&raw mut *state);
            require_timer(0, &mut state.selection)?;
            Ok::<(), Error>(())
        })();
        assert!(result.is_err());
        assert_eq!(state.selection.outcome(), Outcome::Cancelled);
        assert!(
            state.windows.iter().all(|window| window.hwnd.is_invalid()),
            "the handle was not cleared"
        );
        assert_eq!(
            SEEN.load(Ordering::SeqCst),
            0,
            "the window procedure saw a state pointer during destruction"
        );
        assert_eq!(
            SEEN_OTHER.load(Ordering::SeqCst),
            0,
            "another overlay window still had a state pointer during destruction"
        );
        assert!(
            handles.iter().all(|&hwnd| !unsafe {
                ::windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd))
            }
            .as_bool()),
            "a window survived the timer failure"
        );
    }
}
