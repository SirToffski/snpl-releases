//! The overlay child process: `snp-ligase --overlay <dir>` (LINUX_SUPPORT.md
//! section 4, phase L1b).
//!
//! The parent ([`super`]) writes the frozen frames and the layout, then runs
//! this binary again with `--overlay <dir>`. This module reads them, opens one
//! borderless window per monitor with winit, paints them with `softbuffer`,
//! feeds the input events to the pure state machine ([`crate::selection`]) and the
//! pixels to the pure painter ([`super::paint`]), and prints **one JSON line**
//! on stdout when the user decides.
//!
//! What is deliberately here and not in the parent:
//!
//! - The windows. eframe owns the app's event loop and winit allows one event
//!   loop per process, so the overlay has to be a process of its own.
//! - The safety: [`crate::linux::overlay_parent::die_with_parent`] arms `PR_SET_PDEATHSIG`, so even a
//!   `SIGKILL`ed parent cannot leave an input-swallowing window behind. The
//!   parent's own kill (`TIMEOUT` + `KILL_GRACE`) is the second net.
//!
//! Everything that is not winit is elsewhere: the geometry is [`super::paint`]'s
//! and [`super::paint`] is tested pixel by pixel; the decisions are
//! [`crate::selection`]'s and it is tested event by event.

use std::cell::Cell;
use std::io::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::read_layout;

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::platform::x11::WindowAttributesExtX11 as _;
use winit::raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use winit::window::{Window, WindowId};
use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{self, EventMask, GrabMode, GrabStatus, InputFocus};
use x11rb::rust_connection::RustConnection;

use super::paint::{self, Frame};
use super::{Answer, Layout, LayoutMonitor, OVERLAY_CLASS, VisibleMarker, unix_ms};
use crate::RectI;
use crate::selection::{Outcome, SelectionState};

/// How often a window that would not paint is asked again before the child
/// gives up on it. A window that never becomes paintable is a broken run, and
/// a run that keeps retrying would spin the event loop.
const MAX_PAINT_ATTEMPTS: u32 = 50;

/// How many times a refused grab is tried again, and how long between tries.
/// Another client can hold the keyboard or the pointer for a moment (a menu
/// closing, a window manager's own grab); a grab that stays refused is logged
/// and the winit events are the fallback. 10 × 10 ms keeps the whole retry
/// inside the overlay's 300 ms visible-latency budget.
const GRAB_ATTEMPTS: u32 = 10;
const GRAB_RETRY: Duration = Duration::from_millis(10);

/// How often the X connection is asked for grabbed input events. The winit
/// loop cannot be woken by another connection, so while a grab is held the
/// loop waits this long instead of for the whole deadline.
const GRAB_POLL: Duration = Duration::from_millis(8);

/// The `XK_Escape` keysym, for finding the keycode Escape is bound to in the
/// user's layout.
const ESCAPE_KEYSYM: u32 = 0xff1b;

/// Runs the overlay child and returns the process's exit code.
///
/// 0 covers both answers — the selection and the cancel — because the answer
/// itself is the JSON line on stdout. A non-zero exit code means the run broke
/// before it could answer, and stderr says why (the parent keeps it in
/// `stderr.log` and reports it with the error).
pub fn run_child(dir: &Path) -> ExitCode {
    // The very first thing: arm the parent-death signal and check the parent
    // did not die in the moment between its `spawn` and this line.
    if let Err(message) = crate::linux::overlay_parent::die_with_parent() {
        eprintln!("snp-ligase: the overlay: {message}");
        return ExitCode::FAILURE;
    }

    match run(dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            tracing::error!(error = %message, "the overlay child failed");
            eprintln!("snp-ligase: the overlay: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The child's whole run: read, paint, answer.
fn run(dir: &Path) -> Result<(), String> {
    let layout = read_layout(dir)?;
    let frames = read_frames(dir, &layout)?;
    let deadline = Instant::now() + Duration::from_millis(layout.timeout_ms);
    tracing::info!(
        monitors = layout.monitors.len(),
        timeout_ms = layout.timeout_ms,
        cursor = ?layout.cursor,
        "the overlay child starts"
    );

    let event_loop =
        EventLoop::new().map_err(|err| format!("the overlay cannot open its event loop: {err}"))?;
    event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));

    let mut child = OverlayChild {
        dir: dir.to_path_buf(),
        layout,
        frames,
        state: SelectionState::new(),
        deadline,
        context: None,
        windows: Vec::new(),
        // The window geometry, from the X server rather than from winit (see
        // `OverlayWindow::winit_rect`). Connecting is not fatal.
        screen: Screen::connect().map(Rc::new),
        failure: None,
        visible_written: false,
        grabs: Grabs::default(),
        grab_attempts: 0,
        next_grab: Instant::now(),
    };
    let looped = event_loop.run_app(&mut child);
    // The windows go first and the answer second: the parent's next step is a
    // drag on the desktop, and an overlay window may not still be in the way
    // when the line arrives.
    let answer = child.close();
    looped.map_err(|err| format!("the overlay's event loop failed: {err}"))?;
    let answer = answer?;

    let text = serde_json::to_string(&answer)
        .map_err(|err| format!("the overlay's answer cannot be encoded: {err}"))?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{text}")
        .map_err(|err| format!("the overlay's answer cannot be written: {err}"))?;
    out.flush()
        .map_err(|err| format!("the overlay's answer cannot be flushed: {err}"))?;
    tracing::info!(answer = %text, "the overlay child answers");
    Ok(())
}

/// Every monitor's frame, as the painter wants it: the original pixels and the
/// dimmed copy, both `0x00RRGGBB`.
fn read_frames(dir: &Path, layout: &Layout) -> Result<Vec<FrameData>, String> {
    let mut frames = Vec::with_capacity(layout.monitors.len());
    for monitor in &layout.monitors {
        // The parent always writes `frame_<index>.rgba`; anything else in the
        // layout is a mistake worth refusing instead of joining (a name with a
        // `/` in it would leave the run directory).
        if monitor.file.is_empty()
            || monitor.file.contains(['/', '\\'])
            || monitor.file.starts_with('.')
        {
            return Err(format!(
                "the layout names a frame that is not a plain file name: {:?}",
                monitor.file
            ));
        }
        let path = dir.join(&monitor.file);
        let bytes = std::fs::read(&path)
            .map_err(|err| format!("could not read {}: {err}", path.display()))?;
        let original = paint::pixels_from_rgba(&bytes, monitor.frame_w, monitor.frame_h)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        let dimmed = paint::dim(&original);
        frames.push(FrameData {
            original,
            dimmed,
            width: monitor.frame_w,
            height: monitor.frame_h,
            rect: monitor.rect(),
        });
    }
    Ok(frames)
}

/// One monitor's frozen frame, as the overlay paints it.
#[derive(Debug)]
struct FrameData {
    original: Vec<u32>,
    dimmed: Vec<u32>,
    width: u32,
    height: u32,
    /// Where those pixels belong, in root-window pixels: the monitor's own
    /// rectangle, which the frame may be smaller than.
    rect: RectI,
}

/// One overlay window: the monitor it was opened for, its pixels, and what the
/// event loop has to remember about it.
struct OverlayWindow {
    surface: Surface<OwnedDisplayHandle, Window>,
    /// The monitor's rectangle in root-window pixels: where the window was
    /// asked to be, and where its frame's pixels belong. Where it really is is
    /// [`OverlayWindow::root_rect`]'s answer, which a window manager can have
    /// made a different one.
    monitor: RectI,
    frame: FrameData,
    /// The last pointer position seen in this window, in root-window pixels.
    pointer: (i32, i32),
    presented: bool,
    attempts: u32,
}

impl OverlayWindow {
    /// The window's rectangle in root-window pixels: where its pixels really
    /// are on the desktop.
    ///
    /// The size is winit's, the origin is the X server's ([`Screen::origin`]),
    /// and [`OverlayWindow::winit_rect`] is what winit would have said — which
    /// is not always the same, see there.
    fn root_rect(&self, screen: Option<&Screen>) -> RectI {
        let window = self.surface.window();
        let size = window.inner_size();
        let width = i32::try_from(size.width).unwrap_or(self.monitor.w);
        let height = i32::try_from(size.height).unwrap_or(self.monitor.h);
        let origin = self.origin(screen);
        RectI::new(origin.0, origin.1, width, height)
    }

    /// Where the window starts, from the best of the three answers there are.
    fn origin(&self, screen: Option<&Screen>) -> (i32, i32) {
        let window = self.surface.window();
        origin_of(
            screen.and_then(|screen| screen.origin(window)),
            self.winit_rect().map(|rect| (rect.x, rect.y)),
            (self.monitor.x, self.monitor.y),
        )
    }

    /// The window's rectangle as winit sees it: `inner_position` plus
    /// `inner_size`.
    ///
    /// This is the fallback for a child whose X connection did not work out,
    /// and it is what the drag used to be mapped through. It is wrong often
    /// enough to matter: winit's X11 pointer positions are relative to where
    /// the window really is, so a window manager that moves a window (xfwm4
    /// clamps new windows to its work area) leaves the two halves of this
    /// rectangle describing different places, and a drag comes back 35 pixels
    /// off. The X server is the authority, so it is asked first.
    fn winit_rect(&self) -> Option<RectI> {
        let window = self.surface.window();
        let size = window.inner_size();
        let width = i32::try_from(size.width).unwrap_or(self.monitor.w);
        let height = i32::try_from(size.height).unwrap_or(self.monitor.h);
        window
            .inner_position()
            .ok()
            .map(|position| RectI::new(position.x, position.y, width, height))
    }
}

/// The X server itself, asked the questions about windows that winit answers
/// from its own bookkeeping: where a window really is, and the input grab the
/// overlay needs so a click in the panel strip reaches it (LINUX_SUPPORT.md
/// section 4, L1c carry-over 1).
///
/// One extra connection, opened while the event loop runs. It carries
/// `TranslateCoordinates` and the input API: the active grabs, the focus, the
/// Escape keycode and the events the grabs deliver. Grabs are held by *this*
/// connection, so the X server releases them when the child dies, whatever
/// kills it.
struct Screen {
    conn: RustConnection,
    root: xproto::Window,
    /// The keycode Escape is bound to in the user's layout, from the server's
    /// keyboard mapping. `None` when the mapping has no Escape key, which
    /// turns the keyboard fallback off rather than guessing a keycode.
    escape: Option<u8>,
    /// The window that had the input focus before the overlay took it, to be
    /// handed back when the overlay ends. `0` (no focus) is never stored.
    focus_before: Cell<Option<u32>>,
}

impl Screen {
    /// Connects to `$DISPLAY`. `None` is not fatal: the overlay then works the
    /// way it did before this connection existed — winit's events, no grab —
    /// and says so.
    fn connect() -> Option<Self> {
        let (conn, screen) = match x11rb::connect(None) {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "the overlay cannot ask the X server where its windows are or grab the input; \
                     falling back to winit's own geometry and events"
                );
                return None;
            }
        };
        let root = conn.setup().roots.get(screen).map(|info| info.root);
        match root {
            Some(root) => {
                let escape = escape_keycode(&conn);
                if escape.is_none() {
                    tracing::warn!(
                        "the keyboard mapping has no Escape key; Escape will only work through \
                         winit's events"
                    );
                }
                Some(Self {
                    conn,
                    root,
                    escape,
                    focus_before: Cell::new(None),
                })
            }
            None => {
                tracing::warn!(
                    screen,
                    "the X display has no such screen; using winit's own geometry"
                );
                None
            }
        }
    }

    /// The root-window position of `window`, or `None` if the X server cannot
    /// be asked (the window is gone, the handle is not an X11 one).
    fn origin(&self, window: &Window) -> Option<(i32, i32)> {
        let origin = x11_window_id(window)?;
        let reply = xproto::translate_coordinates(&self.conn, origin, self.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some((i32::from(reply.dst_x), i32::from(reply.dst_y)))
    }

    /// Gives `window` the input focus, remembering what had it before.
    ///
    /// An override-redirect window gets no focus from a window manager, so
    /// this is explicit. The focus is what the self-check waits for before it
    /// sends Escape, and it is what delivers key events when the keyboard grab
    /// is refused.
    fn focus(&self, window: &Window) {
        let Some(window) = x11_window_id(window) else {
            return;
        };
        if self.focus_before.get().is_none() {
            // Focus `None` (0) means "nothing", which is not worth restoring.
            let previous = xproto::get_input_focus(&self.conn)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|reply| reply.focus)
                .filter(|focus| *focus != 0);
            self.focus_before.set(previous);
        }
        let focused = match xproto::set_input_focus(
            &self.conn,
            InputFocus::PARENT,
            window,
            x11rb::CURRENT_TIME,
        ) {
            Ok(cookie) => cookie.check().map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        };
        if let Err(err) = focused {
            tracing::warn!(
                window = format!("{window:#x}"),
                error = %err,
                "the overlay window could not take the input focus"
            );
        }
    }

    /// Hands the input focus back to whatever had it before the overlay.
    fn restore_focus(&self) {
        let Some(previous) = self.focus_before.take() else {
            return;
        };
        let restored = match xproto::set_input_focus(
            &self.conn,
            InputFocus::PARENT,
            previous,
            x11rb::CURRENT_TIME,
        ) {
            Ok(cookie) => cookie.check().map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        };
        if let Err(err) = restored {
            tracing::warn!(
                focus = format!("{previous:#x}"),
                error = %err,
                "the input focus could not be handed back"
            );
        }
    }

    /// Actively grabs the pointer on the root window.
    ///
    /// `owner_events = false`, so every button and motion event is reported
    /// against the root window: the coordinates are already desktop ones, and
    /// the events reach the child even while the pointer is over the panel.
    fn grab_pointer(&self) -> Result<GrabStatus, String> {
        let cookie = xproto::grab_pointer(
            &self.conn,
            false,
            self.root,
            EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        )
        .map_err(|err| err.to_string())?;
        cookie
            .reply()
            .map(|reply| reply.status)
            .map_err(|err| err.to_string())
    }

    /// Actively grabs the keyboard on the root window; see
    /// [`Screen::grab_pointer`] for `owner_events`.
    fn grab_keyboard(&self) -> Result<GrabStatus, String> {
        let cookie = xproto::grab_keyboard(
            &self.conn,
            false,
            self.root,
            x11rb::CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )
        .map_err(|err| err.to_string())?;
        cookie
            .reply()
            .map(|reply| reply.status)
            .map_err(|err| err.to_string())
    }

    /// Releases the pointer grab, if this connection holds one.
    fn ungrab_pointer(&self) {
        let released = match xproto::ungrab_pointer(&self.conn, x11rb::CURRENT_TIME) {
            Ok(cookie) => cookie.check().map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        };
        if let Err(err) = released {
            tracing::warn!(error = %err, "the pointer grab could not be released");
        }
    }

    /// Releases the keyboard grab, if this connection holds one.
    fn ungrab_keyboard(&self) {
        let released = match xproto::ungrab_keyboard(&self.conn, x11rb::CURRENT_TIME) {
            Ok(cookie) => cookie.check().map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        };
        if let Err(err) = released {
            tracing::warn!(error = %err, "the keyboard grab could not be released");
        }
    }

    /// The keycode Escape is bound to, if the layout has one.
    fn escape_keycode(&self) -> Option<u8> {
        self.escape
    }

    /// Every event waiting on this connection, without blocking.
    ///
    /// The grabbed input arrives here, not through winit: an active grab
    /// delivers events only to the grabbing client (Linux's core `XGrab*`).
    fn poll_events(&self) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(event)) => events.push(event),
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "the overlay's X events could not be read");
                    break;
                }
            }
        }
        events
    }
}

/// The keycode Escape is bound to, from the server's keyboard mapping.
fn escape_keycode(conn: &RustConnection) -> Option<u8> {
    let setup = conn.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode - setup.min_keycode + 1;
    let reply = xproto::get_keyboard_mapping(conn, first, count)
        .ok()?
        .reply()
        .ok()?;
    let per = usize::from(reply.keysyms_per_keycode);
    if per == 0 {
        return None;
    }
    reply
        .keysyms
        .iter()
        .position(|keysym| *keysym == ESCAPE_KEYSYM)
        .map(|index| first.wrapping_add(u8::try_from(index / per).unwrap_or(0)))
}

/// Which devices the overlay holds an active grab on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Grabs {
    pointer: bool,
    keyboard: bool,
}

impl Grabs {
    /// Whether at least one device is grabbed.
    const fn any(self) -> bool {
        self.pointer || self.keyboard
    }

    /// Whether both are.
    const fn complete(self) -> bool {
        self.pointer && self.keyboard
    }
}

/// What the overlay does with a button event it received through the grab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ButtonAction {
    /// The left button went down: anchor the selection.
    Press,
    /// The left button came up: finish the selection if it is big enough.
    Release,
    /// The right button went down: cancel, like Esc.
    Cancel,
    /// Anything else (a middle click, a release of the right button).
    Ignore,
}

/// The decision for one button event, as a pure function of the event's
/// button number and state (X11 numbers: 1 left, 3 right).
#[must_use]
fn button_action(detail: u8, pressed: bool) -> ButtonAction {
    match (detail, pressed) {
        (1, true) => ButtonAction::Press,
        (1, false) => ButtonAction::Release,
        (3, true) => ButtonAction::Cancel,
        _ => ButtonAction::Ignore,
    }
}

/// Whether the X server said the grab is ours.
///
/// Every other status is a moment's refusal — another client holds the
/// keyboard or the pointer, or a window is not viewable yet — and is worth
/// trying again inside the bounded window [`GRAB_ATTEMPTS`] allows. The owner
/// of a refused grab is logged, not matched on: a grab that stays refused ends
/// with winit's own events as the fallback.
#[must_use]
fn grab_held(status: GrabStatus) -> bool {
    status == GrabStatus::SUCCESS
}

/// Whether another grab attempt is due: the attempts left, and the time the
/// retry was scheduled for.
#[must_use]
fn grab_due(attempts: u32, next: Instant, now: Instant) -> bool {
    attempts < GRAB_ATTEMPTS && now >= next
}

/// The X11 window id behind a winit window, through `raw-window-handle` (winit
/// re-exports 0.6 as `winit::raw_window_handle`).
fn x11_window_id(window: &Window) -> Option<xproto::Window> {
    match window.window_handle().ok()?.as_raw() {
        // `window` is an Xlib `Window`, which is 32 bits wide on every server
        // this backend talks to; anything larger is not one of ours.
        RawWindowHandle::Xlib(handle) => xproto::Window::try_from(handle.window).ok(),
        RawWindowHandle::Xcb(handle) => Some(handle.window.get()),
        _ => None,
    }
}

/// Which of the three answers to believe: the X server's, winit's, or the
/// layout's.
///
/// In that order, and the X server goes first because it is the one the
/// pointer events agree with: winit's `inner_position` has been measured to
/// say (0,0) for a window whose pointer positions are relative to y=35 (xfwm4
/// moves the window after it is mapped). The layout rectangle is the last
/// resort for a window that cannot be asked at all.
#[must_use]
fn origin_of(
    from_x11: Option<(i32, i32)>,
    from_winit: Option<(i32, i32)>,
    layout: (i32, i32),
) -> (i32, i32) {
    from_x11.or(from_winit).unwrap_or(layout)
}

/// The child's state between events.
struct OverlayChild {
    dir: PathBuf,
    layout: Layout,
    frames: Vec<FrameData>,
    state: SelectionState,
    deadline: Instant,
    context: Option<Context<OwnedDisplayHandle>>,
    windows: Vec<OverlayWindow>,
    /// The child's own X connection, for [`OverlayWindow::root_rect`] and the
    /// input grab.
    screen: Option<Rc<Screen>>,
    /// Which devices the input is grabbed on. Both are `false` when there is
    /// no X connection or the grab stayed refused, and then winit's own
    /// events drive the overlay.
    grabs: Grabs,
    /// How many grab attempts have been made, against [`GRAB_ATTEMPTS`].
    grab_attempts: u32,
    /// The earliest time the next refused grab may be tried again.
    next_grab: Instant,
    /// The reason this run is over before it decided anything.
    failure: Option<String>,
    /// Whether the first present has already been reported to the parent.
    visible_written: bool,
}

impl OverlayChild {
    /// Ends the run with a reason that is not a decision.
    fn fail(&mut self, event_loop: &ActiveEventLoop, message: String) {
        tracing::error!(error = %message, "the overlay cannot run");
        self.failure = Some(message);
        event_loop.exit();
    }

    /// The window this event is about, if it is still one of ours.
    fn index_of(&self, window_id: WindowId) -> Option<usize> {
        self.windows
            .iter()
            .position(|window| window.surface.window().id() == window_id)
    }

    /// The monitor the pointer is on, else the primary one, else the first:
    /// where the keyboard focus goes.
    fn focus_index(&self) -> usize {
        let cursor = (self.layout.cursor[0], self.layout.cursor[1]);
        self.layout
            .monitors
            .iter()
            .position(|monitor| monitor.rect().contains(cursor.0, cursor.1))
            .or_else(|| {
                self.layout
                    .monitors
                    .iter()
                    .position(|monitor| monitor.primary)
            })
            .unwrap_or(0)
    }

    /// Asks every window for a repaint.
    ///
    /// The selection can cover more than one monitor, so a move repaints all
    /// of them; the Windows overlay invalidates every window for the same
    /// reason.
    fn redraw(&self) {
        for window in &self.windows {
            window.surface.window().request_redraw();
        }
    }

    /// Tries to grab the pointer and the keyboard, while attempts are left.
    ///
    /// Called from the event loop, not from a blocking loop: a refused grab
    /// (another client holds the keyboard or the pointer for a moment) is
    /// retried on later iterations, so the overlay's first paint stays inside
    /// its 300 ms budget (LINUX_SUPPORT.md section 4, L1c carry-over 1).
    fn attempt_grabs(&mut self) {
        let Some(screen) = self.screen.clone() else {
            return;
        };
        if self.grabs.complete() || !grab_due(self.grab_attempts, self.next_grab, Instant::now()) {
            return;
        }
        self.grab_attempts += 1;
        if let Some(window) = self.windows.get(self.focus_index()) {
            // The focus goes first: it is what carries the keyboard when the
            // grab is refused, and what the self-check waits for before it
            // sends Escape.
            screen.focus(window.surface.window());
        }
        if !self.grabs.pointer {
            match screen.grab_pointer() {
                Ok(status) if grab_held(status) => {
                    self.grabs.pointer = true;
                    tracing::info!("the pointer is grabbed; the overlay sees every click");
                }
                Ok(status) => {
                    tracing::debug!(status = ?status, "the pointer grab was refused; trying again");
                }
                Err(err) => {
                    tracing::warn!(error = %err, "the pointer grab failed; trying again");
                }
            }
        }
        if !self.grabs.keyboard {
            match screen.grab_keyboard() {
                Ok(status) if grab_held(status) => {
                    self.grabs.keyboard = true;
                    tracing::info!("the keyboard is grabbed; the overlay sees every key");
                }
                Ok(status) => {
                    tracing::debug!(status = ?status, "the keyboard grab was refused; trying again");
                }
                Err(err) => {
                    tracing::warn!(error = %err, "the keyboard grab failed; trying again");
                }
            }
        }
        if !self.grabs.complete() {
            if self.grab_attempts >= GRAB_ATTEMPTS {
                tracing::warn!(
                    attempts = self.grab_attempts,
                    pointer = self.grabs.pointer,
                    keyboard = self.grabs.keyboard,
                    "the input could not be grabbed; winit's own events are the fallback"
                );
            } else {
                self.next_grab = Instant::now() + GRAB_RETRY;
            }
        }
    }

    /// Releases the grabs and hands the input focus back.
    ///
    /// The X server would release both when this process's connection dies,
    /// but the overlay usually ends with the child still alive: the desktop
    /// has to be usable the moment the answer is printed, and the next
    /// window's keys must not go into a dead grab.
    fn release_grabs(&mut self) {
        let Some(screen) = self.screen.clone() else {
            return;
        };
        if self.grabs.keyboard {
            screen.ungrab_keyboard();
            self.grabs.keyboard = false;
        }
        if self.grabs.pointer {
            screen.ungrab_pointer();
            self.grabs.pointer = false;
        }
        screen.restore_focus();
    }

    /// Feeds everything the grabs delivered into the state machine.
    ///
    /// The grabbed events carry root-window coordinates (`owner_events` is
    /// false and the grab window is the root), so unlike winit's window-local
    /// ones they need no translation.
    fn drain_grabbed(&mut self, event_loop: &ActiveEventLoop) {
        let Some(screen) = self.screen.clone() else {
            return;
        };
        if !self.grabs.any() {
            return;
        }
        for event in screen.poll_events() {
            let decided = match event {
                Event::MotionNotify(event) if self.grabs.pointer => {
                    let pointer = (i32::from(event.root_x), i32::from(event.root_y));
                    // Only a move during a drag changes any pixel.
                    let dragging = self.state.selection().is_some();
                    self.state.pointer_moved(pointer.0, pointer.1);
                    if dragging {
                        self.redraw();
                    }
                    false
                }
                Event::ButtonPress(event) if self.grabs.pointer => {
                    let pointer = (i32::from(event.root_x), i32::from(event.root_y));
                    match button_action(event.detail, true) {
                        ButtonAction::Press => {
                            self.state.press(pointer.0, pointer.1);
                            self.redraw();
                            false
                        }
                        ButtonAction::Cancel => {
                            self.state.right_click();
                            true
                        }
                        ButtonAction::Release | ButtonAction::Ignore => false,
                    }
                }
                Event::ButtonRelease(event) if self.grabs.pointer => {
                    let pointer = (i32::from(event.root_x), i32::from(event.root_y));
                    match button_action(event.detail, false) {
                        ButtonAction::Release => {
                            if self.state.release(pointer.0, pointer.1) {
                                true
                            } else {
                                // Too small to count: the overlay keeps waiting
                                // (SPEC 5.6) and the discarded drag is
                                // repainted away.
                                self.redraw();
                                false
                            }
                        }
                        ButtonAction::Press | ButtonAction::Cancel | ButtonAction::Ignore => false,
                    }
                }
                Event::KeyPress(event)
                    if self.grabs.keyboard && screen.escape_keycode() == Some(event.detail) =>
                {
                    self.state.escape();
                    true
                }
                _ => false,
            };
            if decided {
                self.decide(event_loop);
                return;
            }
        }
    }

    /// When the event loop should come back: the overlay's deadline, or
    /// sooner while there is grabbed input to poll or a grab to retry.
    fn next_wake(&self, now: Instant) -> Instant {
        let mut wake = self.deadline;
        if self.screen.is_some() {
            if self.grabs.any() {
                wake = wake.min(now + GRAB_POLL);
            }
            if !self.grabs.complete() && self.grab_attempts < GRAB_ATTEMPTS {
                wake = wake.min(self.next_grab.max(now));
            }
        }
        wake
    }

    /// The decision the user made, or why there is none.
    fn answer(&self) -> Result<Answer, String> {
        match self.state.outcome() {
            Outcome::Selected(rect) => Ok(Answer::selected(rect)),
            Outcome::Cancelled => Ok(Answer::cancelled()),
            Outcome::Waiting => Err(
                "the overlay ended without a selection and without a cancel; \
                 the event loop stopped for no reason it can name"
                    .to_owned(),
            ),
        }
    }

    /// Closes every window, then says what happened.
    ///
    /// The windows are dropped — which destroys them, winit's own `Drop` — so
    /// the desktop is clear before the answer reaches the parent. The grabs go
    /// first, or the focus would sit on a window that no longer exists.
    fn close(&mut self) -> Result<Answer, String> {
        self.release_grabs();
        // Override-redirect windows are where they were asked to be, so this
        // is now expected to say nothing; the check stays because "the window
        // manager did not manage it" is exactly what the fix promises and a
        // mismatch is the one thing that would explain a wrong selection.
        for window in &self.windows {
            let real = window.root_rect(self.screen.as_deref());
            if real != window.monitor {
                tracing::info!(
                    asked = ?window.monitor,
                    real = ?real,
                    "an overlay window is not where it was asked to be"
                );
            }
        }
        self.windows.clear();
        self.context = None;
        if let Some(failure) = self.failure.take() {
            return Err(failure);
        }
        self.answer()
    }

    /// Paints one window and reports the first present to the parent.
    fn paint(&mut self, index: usize) {
        // Read before the window is borrowed: the painter is told what the
        // state machine decided, and the state machine cannot change while a
        // window is being painted.
        let selection = self.state.selection();
        let first_presented = {
            let Some(window) = self.windows.get_mut(index) else {
                return;
            };
            // Read before the window is destructured: where the window really
            // is, which is what both the painter and the event translation use.
            let window_rect = window.root_rect(self.screen.as_deref());
            let winit_rect = window.winit_rect();
            let OverlayWindow {
                surface,
                frame,
                presented,
                attempts,
                ..
            } = window;

            // A window that the window manager resized (a fullscreen hint, a
            // monitor that changed) is painted at its real size; the frame may
            // be smaller, and the painter fills the rest with black.
            let size = surface.window().inner_size();
            let (Some(width), Some(height)) = (
                NonZeroU32::new(size.width.max(1)),
                NonZeroU32::new(size.height.max(1)),
            ) else {
                return;
            };

            let result = (|| -> Result<(), softbuffer::SoftBufferError> {
                surface.resize(width, height)?;
                let mut buffer = surface.buffer_mut()?;
                let pixels = Frame {
                    original: &frame.original,
                    dimmed: &frame.dimmed,
                    width: frame.width,
                    height: frame.height,
                    rect: frame.rect,
                };
                paint::paint_window(
                    &mut buffer,
                    (width.get(), height.get()),
                    &pixels,
                    selection,
                    window_rect,
                );
                buffer.present()
            })();

            match result {
                Ok(()) => {
                    let first = !*presented;
                    *presented = true;
                    *attempts = 0;
                    if first {
                        if let Some(screen) = self.screen.as_deref()
                            && screen.origin(surface.window()).is_none()
                        {
                            tracing::warn!(
                                "the overlay cannot read a window's position from the X server; \
                                 winit's own answer is used for it"
                            );
                        }
                        if winit_rect != Some(window_rect) {
                            // Recorded because it is the reason the origin does
                            // not come from winit: the two describe different
                            // places, and the pointer events follow the X one.
                            tracing::info!(
                                window = ?window_rect,
                                winit = ?winit_rect,
                                "winit and the X server disagree about where an overlay window is"
                            );
                        }
                        tracing::debug!(
                            window = ?window_rect,
                            size = %format!("{}x{}", width.get(), height.get()),
                            "an overlay window is painted"
                        );
                    }
                    first
                }
                Err(err) => {
                    *attempts += 1;
                    if *attempts <= MAX_PAINT_ATTEMPTS {
                        tracing::warn!(
                            window = ?window_rect,
                            attempt = *attempts,
                            error = %err,
                            "an overlay window could not be painted; asking again"
                        );
                        surface.window().request_redraw();
                    } else {
                        tracing::error!(
                            window = ?window_rect,
                            error = %err,
                            "an overlay window will not paint; giving up on it"
                        );
                    }
                    false
                }
            }
        };

        if first_presented {
            self.write_visible_marker();
        }
    }

    /// Tells the parent when the first window really reached the screen.
    ///
    /// The parent reads this after the child is gone (it removes the run
    /// directory) and logs it against SPEC 12 M7's 300 ms budget.
    fn write_visible_marker(&mut self) {
        if self.visible_written {
            return;
        }
        self.visible_written = true;
        let marker = VisibleMarker {
            visible_unix_ms: unix_ms(),
            started_unix_ms: self.layout.started_unix_ms,
        };
        let path = self.dir.join("visible.json");
        let written = serde_json::to_vec(&marker)
            .map_err(|err| err.to_string())
            .and_then(|bytes| std::fs::write(&path, bytes).map_err(|err| err.to_string()));
        match written {
            Ok(()) => {
                let visible_ms = marker
                    .visible_unix_ms
                    .saturating_sub(marker.started_unix_ms);
                tracing::info!(
                    visible_ms,
                    "the overlay's first frame is on screen (SPEC 12 M7's 300 ms budget)"
                );
            }
            Err(err) => {
                // Only the latency report is lost; the overlay itself is fine.
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "the overlay's visible marker could not be written"
                );
            }
        }
    }

    /// The root-window position of a window-local one.
    ///
    /// `window` is the window's own rectangle (see
    /// [`OverlayWindow::root_rect`]), because an event's position is relative
    /// to the window's own top-left corner, wherever the window manager put it.
    fn root_position(window: RectI, position: PhysicalPosition<f64>) -> (i32, i32) {
        let x = window.x.saturating_add(position.x.round() as i32);
        let y = window.y.saturating_add(position.y.round() as i32);
        (x, y)
    }

    /// The user decided: Esc, a right click, a finished drag or the timeout.
    fn decide(&self, event_loop: &ActiveEventLoop) {
        match self.state.outcome() {
            Outcome::Selected(rect) => tracing::info!(?rect, "the overlay ended with a selection"),
            Outcome::Cancelled => tracing::info!("the overlay was cancelled"),
            Outcome::Waiting => tracing::debug!("the overlay is still waiting"),
        }
        event_loop.exit();
    }
}

impl ApplicationHandler for OverlayChild {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.context.is_some() {
            // X11 calls this once; a second call (another platform's event
            // loop, a re-resume) must not open a second set of windows.
            return;
        }
        let context = match Context::new(event_loop.owned_display_handle()) {
            Ok(context) => context,
            Err(err) => {
                return self.fail(
                    event_loop,
                    format!("the overlay's pixel buffers could not be created: {err}"),
                );
            }
        };
        let monitors = self.layout.monitors.clone();
        let mut windows = Vec::with_capacity(monitors.len());
        let mut frames = std::mem::take(&mut self.frames).into_iter();
        for monitor in monitors {
            let Some(frame) = frames.next() else {
                return self.fail(
                    event_loop,
                    format!("the layout has no frame for {}", monitor.name),
                );
            };
            let window = match create_window(event_loop, &monitor) {
                Ok(window) => window,
                Err(message) => return self.fail(event_loop, message),
            };
            // The pointer may already be inside this monitor.
            let rect = monitor.rect();
            let pointer = if rect.contains(self.layout.cursor[0], self.layout.cursor[1]) {
                (self.layout.cursor[0], self.layout.cursor[1])
            } else {
                (rect.x + rect.w / 2, rect.y + rect.h / 2)
            };
            let surface = match Surface::new(&context, window) {
                Ok(surface) => surface,
                Err(err) => {
                    return self.fail(
                        event_loop,
                        format!(
                            "the overlay's buffer for {} is not usable: {err}",
                            monitor.name
                        ),
                    );
                }
            };
            windows.push(OverlayWindow {
                surface,
                monitor: rect,
                frame,
                pointer,
                presented: false,
                attempts: 0,
            });
        }
        self.context = Some(context);
        self.windows = windows;

        // The keyboard focus goes to the monitor the pointer is on, the way
        // the Win32 overlay brings that monitor's window forward. An
        // override-redirect window gets no focus from a window manager, so it
        // is asked for explicitly through the child's X connection. The
        // screen's focus call comes first: it records what had the focus
        // before the overlay, which is what `close` hands back.
        let index = self.focus_index();
        if let Some(window) = self.windows.get(index) {
            if let Some(screen) = self.screen.as_deref() {
                screen.focus(window.surface.window());
            }
            window.surface.window().focus_window();
        }
        // The grab is what makes the panel strip selectable; the windows were
        // just mapped, so this is the earliest it can work.
        self.attempt_grabs();
        self.redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(index) = self.index_of(window_id) else {
            return;
        };
        match event {
            WindowEvent::RedrawRequested => self.paint(index),

            // The window manager moved or resized the window — a window it
            // does not manage cannot be moved, since the overlay windows are
            // override-redirect, but a monitor that changed can still resize
            // one. The pixels an overlay window shows belong to the screen
            // area it covers, so they are painted again instead of being left
            // where the window used to be.
            WindowEvent::Moved(position) => {
                let window = self.windows[index].root_rect(self.screen.as_deref());
                tracing::debug!(?position, ?window, "an overlay window moved");
                self.windows[index].surface.window().request_redraw();
            }
            WindowEvent::Resized(_) => {
                self.windows[index].surface.window().request_redraw();
            }

            WindowEvent::CursorMoved { position, .. } => {
                // A grabbed pointer delivers its moves on the child's own X
                // connection, with root coordinates; winit's window-local
                // copy would be a second, possibly conflicting one.
                if self.grabs.pointer {
                    return;
                }
                let window = self.windows[index].root_rect(self.screen.as_deref());
                let pointer = Self::root_position(window, position);
                self.windows[index].pointer = pointer;
                // Only a move during a drag changes any pixel.
                let dragging = self.state.selection().is_some();
                self.state.pointer_moved(pointer.0, pointer.1);
                if dragging {
                    self.redraw();
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if self.grabs.pointer {
                    return;
                }
                let pointer = self.windows[index].pointer;
                match (button, state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        self.state.press(pointer.0, pointer.1);
                        self.redraw();
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        if self.state.release(pointer.0, pointer.1) {
                            self.decide(event_loop);
                        } else {
                            // Too small to count: the overlay keeps waiting
                            // (SPEC 5.6), and the discarded drag is repainted
                            // away.
                            self.redraw();
                        }
                    }
                    (MouseButton::Right, ElementState::Pressed) => {
                        self.state.right_click();
                        self.decide(event_loop);
                    }
                    _ => {}
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if self.grabs.keyboard {
                    return;
                }
                let escape = matches!(event.logical_key, Key::Named(NamedKey::Escape))
                    || event.physical_key == PhysicalKey::Code(KeyCode::Escape);
                if escape && event.state == ElementState::Pressed && !event.repeat {
                    self.state.escape();
                    self.decide(event_loop);
                }
            }

            // The window manager asked us to close: the user is done with the
            // overlay whatever they meant, and the desktop must not be left
            // covered.
            WindowEvent::CloseRequested => {
                self.state.escape();
                self.decide(event_loop);
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Grabbed input first: the user's release may have landed in the same
        // instant the deadline did, and a finished selection is not a timeout.
        self.drain_grabbed(event_loop);
        if self.state.outcome() != Outcome::Waiting {
            // `drain_grabbed` already exited the loop.
            return;
        }
        if Instant::now() >= self.deadline {
            // SPEC 5.6: the safety timeout cancels, exactly like Esc does.
            tracing::warn!(
                timeout_ms = self.layout.timeout_ms,
                "the overlay reached its deadline; cancelling"
            );
            self.state.timed_out();
            self.decide(event_loop);
            return;
        }
        self.attempt_grabs();
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_wake(Instant::now())));
    }
}

/// Where one overlay window goes, before winit is asked for it.
///
/// The windows are **override-redirect**: an override-redirect window is not
/// managed by the window manager at all, so it keeps the exact monitor
/// rectangle even when a panel owns a strip of the work area (LINUX_SUPPORT.md
/// section 4, L1c carry-over 1). That is the whole difference from the managed
/// fullscreen window L1b used, and it is decided here, as pure data, so the
/// window's shape is a test and not only a real run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowPlan {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    override_redirect: bool,
}

/// The plan for `monitor`: its exact rectangle, at least one pixel per side.
#[must_use]
fn window_plan(monitor: &LayoutMonitor) -> WindowPlan {
    let rect = monitor.rect();
    WindowPlan {
        x: rect.x,
        y: rect.y,
        width: u32::try_from(rect.w).unwrap_or(1).max(1),
        height: u32::try_from(rect.h).unwrap_or(1).max(1),
        override_redirect: true,
    }
}

/// Opens one overlay window for `monitor`, exactly over it.
///
/// No window manager is involved: the window is override-redirect, so nothing
/// can move it out of a panel's way, and the child's own input grab is what
/// makes the panel area and the desktop listen to it (LINUX_SUPPORT.md
/// section 4, L1c carry-over 1).
fn create_window(event_loop: &ActiveEventLoop, monitor: &LayoutMonitor) -> Result<Window, String> {
    let plan = window_plan(monitor);
    let attributes = Window::default_attributes()
        .with_title("snp-ligase overlay")
        .with_decorations(false)
        .with_position(PhysicalPosition::new(plan.x, plan.y))
        .with_inner_size(PhysicalSize::new(plan.width, plan.height))
        .with_override_redirect(plan.override_redirect)
        // The class is set before the window is mapped, so the self-check can
        // find (and miss) this window without racing the first paint.
        .with_name(OVERLAY_CLASS, OVERLAY_CLASS);
    let window = event_loop.create_window(attributes).map_err(|err| {
        format!(
            "the overlay window on {} ({}x{} at {}, {}) could not be opened: {err}",
            monitor.name, plan.width, plan.height, plan.x, plan.y
        )
    })?;
    tracing::info!(
        monitor = %monitor.name,
        rect = ?monitor.rect(),
        override_redirect = plan.override_redirect,
        "an overlay window is open"
    );
    Ok(window)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::read_layout;

    use super::super::RunDir;

    /// A run directory with a layout and frames in it, written the way the
    /// parent writes them.
    fn input_dir(name: &str, frames: &[(LayoutMonitor, Vec<u8>)]) -> (RunDir, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "snp-ligase-overlay-child-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("a test base directory");
        let dir = RunDir::create_in(&base).expect("a run directory");
        let layout = Layout {
            monitors: frames.iter().map(|(monitor, _)| monitor.clone()).collect(),
            timeout_ms: 120_000,
            started_unix_ms: 1_000,
            cursor: [10, 20],
        };
        for (monitor, bytes) in frames {
            // A name this test wants refused is not written: the refusal must
            // come from the name, not from the file being missing.
            let plain = !monitor.file.is_empty()
                && !monitor.file.contains(['/', '\\'])
                && !monitor.file.starts_with('.');
            if plain {
                std::fs::write(dir.path().join(&monitor.file), bytes).expect("a frame file");
            }
        }
        std::fs::write(
            dir.path().join("layout.json"),
            serde_json::to_vec(&layout).expect("the layout encodes"),
        )
        .expect("a layout file");
        (dir, base)
    }

    fn monitor(file: &str, frame: (u32, u32)) -> LayoutMonitor {
        LayoutMonitor {
            name: "HDMI-1".to_owned(),
            x: -3,
            y: 5,
            w: 4,
            h: 2,
            frame_w: frame.0,
            frame_h: frame.1,
            file: file.to_owned(),
            primary: true,
            native_scale_x: 1.0,
            native_scale_y: 1.0,
        }
    }

    #[test]
    fn the_child_reads_the_layout_and_the_frames_the_parent_wrote() {
        // One 2x2 monitor: four RGBA pixels, whose blue channel is 1..4 so the
        // pixel order is visible in the test.
        let pixels: Vec<u8> = (1u8..=4).flat_map(|b| [10, 20, b, 255]).collect();
        let (dir, base) = input_dir("read", &[(monitor("frame_0.rgba", (2, 2)), pixels)]);

        let layout = read_layout(dir.path()).expect("the layout should parse");
        assert_eq!(layout.monitors.len(), 1);
        assert_eq!(layout.cursor, [10, 20]);
        assert_eq!(layout.monitors[0].rect(), RectI::new(-3, 5, 4, 2));

        let frames = read_frames(dir.path(), &layout).expect("the frames should load");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].width, 2);
        assert_eq!(frames[0].height, 2);
        assert_eq!(
            frames[0].original[0], 0x000A_1401,
            "row-major, alpha dropped"
        );
        assert_eq!(frames[0].original[3], 0x000A_1404);
        assert_eq!(frames[0].dimmed[3], 0x0006_0C02, "60%, the SPEC's factor");

        drop(dir);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_layout_that_would_leave_the_run_directory_is_refused() {
        for file in ["../secret", "sub/frame.rgba", "..", ".hidden", ""] {
            let pixels = vec![0u8; 2 * 2 * 4];
            let (dir, base) = input_dir("escape", &[(monitor(file, (2, 2)), pixels)]);
            // The parent never writes such a layout; if one arrives, the run
            // must stop with a message instead of reading a file outside.
            let layout = read_layout(dir.path()).expect("the layout should parse");
            let error = read_frames(dir.path(), &layout).expect_err("it should be refused");
            assert!(error.contains("plain file name"), "{file:?}: {error}");
            drop(dir);
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn a_frame_that_is_not_the_size_the_layout_promised_is_refused() {
        // 2x2 promised, three bytes given.
        let (dir, base) = input_dir("short", &[(monitor("frame_0.rgba", (2, 2)), vec![1, 2, 3])]);
        let layout = read_layout(dir.path()).expect("the layout should parse");
        let error = read_frames(dir.path(), &layout).expect_err("it should be refused");
        assert!(error.contains("16 bytes"), "{error}");
        drop(dir);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_missing_or_broken_layout_is_an_error_not_a_panic() {
        let base = std::env::temp_dir().join(format!(
            "snp-ligase-overlay-child-test-{}-missing",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("a test base directory");

        let error = read_layout(&base).expect_err("no layout file");
        assert!(error.contains("layout.json"), "{error}");

        std::fs::write(base.join("layout.json"), b"not json").expect("a broken layout");
        let error = read_layout(&base).expect_err("a broken layout");
        assert!(error.contains("is not a layout"), "{error}");

        std::fs::write(
            base.join("layout.json"),
            br#"{"monitors":[],"timeout_ms":1,"started_unix_ms":0,"cursor":[0,0]}"#,
        )
        .expect("an empty layout");
        let error = read_layout(&base).expect_err("no monitors");
        assert!(error.contains("names no monitors"), "{error}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_child_answers_a_cancel_and_a_selection_the_parent_can_read() {
        // The two answers as they reach the parent's parser: this is the
        // contract between the two processes.
        let cancel = Answer::cancelled();
        let text = serde_json::to_string(&cancel).expect("it encodes");
        assert_eq!(text, r#"{"cancelled":true}"#);
        let selection = Answer::selected(RectI::new(-4, 5, 6, 7));
        let text = serde_json::to_string(&selection).expect("it encodes");
        assert_eq!(text, r#"{"selection":{"x":-4,"y":5,"w":6,"h":7}}"#);
    }

    #[test]
    fn the_x_servers_answer_wins_over_winit_and_the_layout() {
        // What the real runs measured: winit said the window manager had put
        // the window at y=35 while the X server still said y=0, and the pointer
        // positions agreed with the X server. The server is asked first.
        assert_eq!(
            origin_of(Some((0, 35)), Some((0, 0)), (1, 2)),
            (0, 35),
            "the X server's answer, not winit's"
        );
        // A child without a working X connection falls back to winit's answer.
        assert_eq!(origin_of(None, Some((0, 0)), (1, 2)), (0, 0));
        // And a window that cannot be asked at all keeps the layout's place,
        // which is where it was asked to be.
        assert_eq!(origin_of(None, None, (1, 2)), (1, 2));
    }

    #[test]
    fn the_event_position_is_the_windows_own_origin_plus_the_local_one() {
        // A monitor left of and above the root origin, dragged into:
        // window-local (5, 6) is desktop (-1195, -294).
        let window = RectI::new(-1200, -300, 960, 1080);
        assert_eq!(
            OverlayChild::root_position(window, PhysicalPosition::new(5.0, 6.0)),
            (-1195, -294)
        );
        // A drag that left the window it started in (the implicit X pointer
        // grab keeps the events coming, with coordinates outside the window).
        assert_eq!(
            OverlayChild::root_position(window, PhysicalPosition::new(1000.5, -3.4)),
            (-199, -303)
        );
    }

    #[test]
    fn a_window_the_window_manager_moved_still_reports_where_the_user_dragged() {
        // XFCE's xfwm4 keeps windows out of its top panel's work area, so this
        // machine's overlay window really starts 35 pixels down: a pointer at
        // desktop (150, 150) is then at window-local (150, 115), and the
        // rectangle the overlay answers with is the desktop one the user
        // dragged, not the local one (the bug the real run found).
        let monitor = RectI::new(0, 0, 960, 1080);
        let window = RectI::new(0, 35, 960, 1080);
        assert_ne!(monitor, window, "the window manager moved this one");
        assert_eq!(
            OverlayChild::root_position(window, PhysicalPosition::new(150.0, 115.0)),
            (150, 150)
        );
        // The bottom of the screen, which the window shows past its own end:
        // the pointer is outside the window and the position is still right.
        assert_eq!(
            OverlayChild::root_position(window, PhysicalPosition::new(400.0, 1045.0)),
            (400, 1080)
        );
    }

    #[test]
    fn the_focus_goes_to_the_monitor_the_pointer_is_on() {
        let mut child = OverlayChild {
            dir: PathBuf::from("/nonexistent"),
            layout: Layout {
                monitors: vec![
                    LayoutMonitor {
                        name: "left".to_owned(),
                        x: -960,
                        y: 0,
                        w: 960,
                        h: 1080,
                        frame_w: 1,
                        frame_h: 1,
                        file: "a".to_owned(),
                        primary: false,
                        native_scale_x: 1.0,
                        native_scale_y: 1.0,
                    },
                    LayoutMonitor {
                        name: "right".to_owned(),
                        x: 0,
                        y: 0,
                        w: 1920,
                        h: 1080,
                        frame_w: 1,
                        frame_h: 1,
                        file: "b".to_owned(),
                        primary: true,
                        native_scale_x: 1.0,
                        native_scale_y: 1.0,
                    },
                ],
                timeout_ms: 1,
                started_unix_ms: 0,
                cursor: [-900, 100],
            },
            frames: Vec::new(),
            state: SelectionState::new(),
            deadline: Instant::now(),
            context: None,
            windows: Vec::new(),
            screen: None,
            grabs: Grabs::default(),
            grab_attempts: 0,
            next_grab: Instant::now(),
            failure: None,
            visible_written: false,
        };
        assert_eq!(child.focus_index(), 0, "the pointer is on the left monitor");

        child.layout.cursor = [900, 100];
        assert_eq!(child.focus_index(), 1);

        // A pointer that is on no monitor at all (a gap in a layout, a
        // monitor that was unplugged): the primary one takes the focus.
        child.layout.cursor = [5000, 5000];
        assert_eq!(child.focus_index(), 1);
    }

    /// LINUX_SUPPORT.md section 4, L1c carry-over 1: an override-redirect
    /// window at the exact monitor rectangle is what covers XFCE's panel
    /// strip, which a managed (even fullscreen) window never does.
    #[test]
    fn the_overlay_window_is_override_redirect_at_the_exact_monitor_rect() {
        let planned = window_plan(&monitor("frame_0.rgba", (4, 2)));
        assert_eq!(
            planned,
            WindowPlan {
                x: -3,
                y: 5,
                width: 4,
                height: 2,
                override_redirect: true,
            },
            "the monitor's own rectangle, unmanaged"
        );

        // The panel strip case: the window starts at y=0 although xfwm4's work
        // area starts 35 pixels down, because nothing moves it.
        let monitor = LayoutMonitor {
            name: "eDP-1".to_owned(),
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            frame_w: 1,
            frame_h: 1,
            file: "frame_0.rgba".to_owned(),
            primary: true,
            native_scale_x: 1.0,
            native_scale_y: 1.0,
        };
        assert_eq!(window_plan(&monitor).y, 0, "the panel strip is covered");
        assert!(window_plan(&monitor).override_redirect);

        // A broken rectangle is clamped to one pixel per side, like
        // `RectI::new` clamps a negative size: winit refuses a zero-size
        // window and the run would fail before painting anything.
        let broken = LayoutMonitor {
            name: "broken".to_owned(),
            x: 1,
            y: 2,
            w: 0,
            h: -5,
            frame_w: 1,
            frame_h: 1,
            file: "frame_0.rgba".to_owned(),
            primary: false,
            native_scale_x: 1.0,
            native_scale_y: 1.0,
        };
        assert_eq!(window_plan(&broken).width, 1);
        assert_eq!(window_plan(&broken).height, 1);
    }

    /// The grab retry policy: a refused grab is retried on later event loop
    /// iterations, and the whole retry stays inside the 300 ms latency budget
    /// ([`GRAB_ATTEMPTS`] × [`GRAB_RETRY`]).
    #[test]
    fn a_refused_grab_is_retried_while_attempts_are_left() {
        let now = Instant::now();
        let next = now + GRAB_RETRY;
        assert!(grab_due(0, now, now), "the first attempt is due at once");
        assert!(!grab_due(0, next, now), "the retry waits its 10 ms");
        assert!(grab_due(0, next, next), "and then it is due");
        assert!(grab_due(GRAB_ATTEMPTS - 1, next, next));
        assert!(!grab_due(GRAB_ATTEMPTS, next, next), "the budget is spent");
        assert!(
            u128::from(GRAB_ATTEMPTS) * GRAB_RETRY.as_millis() < 300,
            "the retry budget must stay inside the 300 ms budget"
        );

        // The status mapping: only `SUCCESS` means the grab is held, so every
        // refusal takes the retry path.
        assert!(grab_held(GrabStatus::SUCCESS));
        for status in [
            GrabStatus::ALREADY_GRABBED,
            GrabStatus::FROZEN,
            GrabStatus::NOT_VIEWABLE,
            GrabStatus::INVALID_TIME,
        ] {
            assert!(!grab_held(status), "{status:?}");
        }
    }

    /// The grabbed button events the overlay acts on: the left button for the
    /// selection, the right one for the cancel, and nothing else.
    #[test]
    fn only_the_left_and_right_buttons_decide_anything() {
        assert_eq!(button_action(1, true), ButtonAction::Press);
        assert_eq!(button_action(1, false), ButtonAction::Release);
        assert_eq!(button_action(3, true), ButtonAction::Cancel);
        for (detail, pressed) in [(3, false), (2, true), (2, false), (8, true), (0, true)] {
            assert_eq!(
                button_action(detail, pressed),
                ButtonAction::Ignore,
                "button {detail}, pressed {pressed}"
            );
        }
    }
}
