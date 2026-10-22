//! `--debug-overlay`: the X11 region overlay's self-check (LINUX_SUPPORT.md
//! section 4, phase L1b).
//!
//! The overlay covers the whole desktop and blocks until the user decides, so
//! it is exactly the piece that cannot be left to the human on every change:
//! this check runs the **real** overlay over a **real** capture, injects a real
//! drag (or Escape, or a right click, or waits for the safety timeout) with
//! XTest, checks what came back, and then walks the X window tree for windows of
//! the overlay's class to prove none was left behind.
//!
//! It only injects input once an overlay window really exists — and, for the
//! keyboard, only once one of them really has the input focus, so a failed
//! start cannot spray clicks or keystrokes across whatever is on screen.
//!
//! What it does not do is decide how the run is reported: the app's
//! `--debug-overlay` command turns [`Outcome`] into the exit code, the same way
//! on both platforms.

use std::time::{Duration, Instant};

use x11rb::connection::Connection as _;
use x11rb::protocol::screensaver;
use x11rb::protocol::xproto;
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;

use super::{OVERLAY_CLASS, TIMEOUT, select_region_with_timeout};
use crate::linux::x11::X11Platform;
use crate::{CaptureOptions, RectI};

/// The last keyboard or mouse input must be at least this old before the check
/// runs: it takes over the screen for a second or two.
pub const IDLE_REQUIRED_SECONDS: f64 = 3.0;

/// How long the overlay is given to appear before the injected input gives up.
const OVERLAY_APPEARS_WITHIN: Duration = Duration::from_secs(3);

/// The short timeout the `timeout` variant asks the overlay to use.
const SHORT_TIMEOUT: Duration = Duration::from_millis(1500);

/// The rectangle the `drag` variant selects, as an offset and a size inside the
/// monitor. The same numbers as the Windows check, so both prove the same
/// rectangle.
const DRAG_AT: (i32, i32) = (150, 150);
const DRAG_SIZE: (i32, i32) = (400, 300);

/// How many motion events the drag is split into, and how long between them:
/// a real drag is a stream, not one jump.
const DRAG_STEPS: i32 = 12;
const STEP_PAUSE: Duration = Duration::from_millis(8);

/// How long the destroyed windows are given to land before the tree is asked
/// what still exists.
const SETTLE: Duration = Duration::from_millis(250);

/// An Esc or a right click that arrives late is not a cancel worth passing: a
/// real one lands in a moment, and anything slower means the input never got
/// there and the safety timeout did the work.
const CANCEL_ARRIVES_WITHIN: Duration = Duration::from_secs(5);

/// How much of the window tree is walked before the search gives up. A
/// desktop's tree is a few hundred windows; a runaway one is a bug, not a
/// reason to hang.
const MAX_TREE_WINDOWS: usize = 10_000;

/// The left and right mouse buttons (X11's numbering).
const LEFT_BUTTON: u8 = 1;
const RIGHT_BUTTON: u8 = 3;

/// What the command line asked this command to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Drag a rectangle out and expect it back.
    Drag,
    /// Press Escape and expect a cancel.
    Escape,
    /// Right-click and expect a cancel.
    RightClick,
    /// Do nothing and expect the safety timeout to cancel.
    Timeout,
}

impl Variant {
    /// The variants the command line accepts.
    pub const ALL: [Self; 4] = [Self::Drag, Self::Escape, Self::RightClick, Self::Timeout];

    /// Parses the variant name after `--debug-overlay`.
    ///
    /// # Errors
    ///
    /// An unknown name lists the ones that exist.
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|variant| variant.name() == text)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|variant| variant.name()).collect();
                format!(
                    "unknown --debug-overlay variant {text:?}; expected one of {}",
                    names.join(", ")
                )
            })
    }

    /// The name used on the command line and in the report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Drag => "drag",
            Self::Escape => "esc",
            Self::RightClick => "right-click",
            Self::Timeout => "timeout",
        }
    }

    /// The timeout this variant asks the overlay to use.
    fn timeout(self) -> Duration {
        match self {
            // The timeout variant's whole point is to reach it, so it uses a
            // short one; every other variant only wants it as a backstop.
            Self::Timeout => SHORT_TIMEOUT,
            _ => TIMEOUT,
        }
    }
}

/// How the check ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Passed(String),
    Skipped(String),
    Failed(String),
}

/// Runs the self-check.
///
/// `allow_busy` skips the idle guard. It exists for the human: the agent must
/// not pass it without being told to, because the overlay takes the screen for
/// a moment and the guard is what stops that happening mid-keystroke.
pub fn run(variant: Variant, allow_busy: bool) -> Outcome {
    let display = match Display::connect() {
        Ok(display) => display,
        Err(message) => return Outcome::Failed(message),
    };

    // The foreground guard: never take the screen while somebody is using it,
    // unless the human explicitly said so.
    match display.idle_seconds() {
        Some(seconds) if seconds < IDLE_REQUIRED_SECONDS && !allow_busy => {
            return Outcome::Skipped(format!(
                "the last keyboard or mouse input was {seconds:.1} s ago (needs \
                 {IDLE_REQUIRED_SECONDS:.0} s of quiet; pass --allow-busy when the human says it \
                 is fine)"
            ));
        }
        Some(seconds) if seconds < IDLE_REQUIRED_SECONDS => {
            tracing::warn!(
                seconds,
                "running the overlay self-check although the machine is busy (--allow-busy)"
            );
        }
        Some(_) => {}
        None => return Outcome::Failed("the last input time could not be read".to_owned()),
    }

    // The real backend and a real capture: the overlay runs over the frames it
    // froze, exactly like Capture > Region does.
    let platform = match X11Platform::new() {
        Ok(platform) => platform,
        Err(reason) => {
            return Outcome::Failed(format!("the X11 backend is not available: {reason}"));
        }
    };
    let capture = match platform.capture_all(&CaptureOptions::default()) {
        Ok(capture) => capture,
        Err(err) => {
            return Outcome::Failed(format!("the capture before the overlay failed: {err}"));
        }
    };
    let Some(reference) = capture
        .frames
        .iter()
        .find(|frame| frame.info.is_primary)
        .or_else(|| capture.frames.first())
    else {
        return Outcome::Failed("the desktop reported no monitor".to_owned());
    };
    let monitor = reference.info.rect;
    let Some(target) = drag_target(monitor) else {
        return Outcome::Skipped(format!(
            "the monitor ({monitor:?}) is too small for a {}x{} drag",
            DRAG_SIZE.0, DRAG_SIZE.1
        ));
    };

    // Park the cursor where the drag will start, so the overlay focuses the
    // monitor under it and the press lands in the right window.
    if let Err(message) = display.move_pointer(target.from) {
        return Outcome::Failed(format!("the pointer could not be parked: {message}"));
    }

    let injector = spawn_injector(variant, target);
    let started = Instant::now();
    let result = select_region_with_timeout(&capture, target.from, variant.timeout());
    let elapsed = started.elapsed();
    if let Some(injector) = injector {
        let _ = injector.join();
    }

    // The windows are destroyed by the child's exit; a moment keeps the
    // self-check's own tree walk from racing it.
    std::thread::sleep(SETTLE);
    let leftovers = display.overlay_windows();

    let detail = match (&result, variant) {
        (Ok(Some(rect)), Variant::Drag) if close_enough(*rect, target.expected) => Ok(format!(
            "dragged {:?} and got {rect:?} back",
            target.expected
        )),
        (Ok(Some(rect)), Variant::Drag) => Err(format!(
            "the overlay returned {rect:?} instead of {:?}",
            target.expected
        )),
        (Ok(None), Variant::Escape) if elapsed > CANCEL_ARRIVES_WITHIN => Err(format!(
            "the overlay only ended after {elapsed:?}; the Escape never reached it"
        )),
        (Ok(None), Variant::Escape) => Ok("Escape cancelled the overlay".to_owned()),
        (Ok(None), Variant::RightClick) if elapsed > CANCEL_ARRIVES_WITHIN => Err(format!(
            "the overlay only ended after {elapsed:?}; the right click never reached it"
        )),
        (Ok(None), Variant::RightClick) => Ok("a right click cancelled the overlay".to_owned()),
        (Ok(None), Variant::Timeout) => {
            if elapsed > variant.timeout() + Duration::from_secs(2) {
                Err(format!(
                    "the timeout took {elapsed:?} instead of about {:?}",
                    variant.timeout()
                ))
            } else {
                Ok(format!(
                    "the safety timeout cancelled the overlay after {elapsed:?}"
                ))
            }
        }
        (Ok(rect), variant) => Err(format!(
            "{variant:?} produced {rect:?}, which is not what it should"
        )),
        (Err(err), _) => Err(format!("the overlay failed: {err}")),
    };

    // Whatever happened, an overlay window that survived is a failure: it sits
    // on top of everything and swallows every click.
    match (detail, leftovers) {
        (Ok(detail), 0) => Outcome::Passed(format!("{detail}; no overlay windows left behind")),
        (Ok(detail), count) => {
            Outcome::Failed(format!("{detail}, but {count} overlay window(s) survived"))
        }
        (Err(detail), 0) => Outcome::Failed(detail),
        (Err(detail), count) => {
            Outcome::Failed(format!("{detail}, and {count} overlay window(s) survived"))
        }
    }
}

/// Where the drag goes and what rectangle it should produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target {
    from: (i32, i32),
    to: (i32, i32),
    expected: RectI,
}

/// The drag's two corners on `monitor`, or `None` when it is too small.
///
/// The same shape as the Windows check: an offset inside the monitor, clamped
/// so the drag stays on it, and the rectangle from the press to the release.
#[must_use]
fn drag_target(monitor: RectI) -> Option<Target> {
    let from = (
        monitor.x + DRAG_AT.0.min((monitor.w / 2).max(0)),
        monitor.y + DRAG_AT.1.min((monitor.h / 2).max(0)),
    );
    let to = (
        (from.0 + DRAG_SIZE.0).min(monitor.right() - 1),
        (from.1 + DRAG_SIZE.1).min(monitor.bottom() - 1),
    );
    let expected = RectI::new(from.0, from.1, to.0 - from.0, to.1 - from.1);
    (expected.w >= 200 && expected.h >= 150).then_some(Target { from, to, expected })
}

/// Whether two rectangles differ by at most one pixel, which is all a pointer
/// coordinate can explain.
fn close_enough(a: RectI, b: RectI) -> bool {
    (a.x - b.x).abs() <= 1
        && (a.y - b.y).abs() <= 1
        && (a.w - b.w).abs() <= 1
        && (a.h - b.h).abs() <= 1
}

/// Whether a `WM_CLASS` property belongs to the overlay.
///
/// The property holds one or two NUL-terminated strings (`instance`, then
/// `general`); winit is given the overlay's class for both.
#[must_use]
fn class_matches(value: &[u8]) -> bool {
    value
        .split(|byte| *byte == 0)
        .any(|part| part == OVERLAY_CLASS.as_bytes())
}

/// Starts the thread that injects this variant's input once the overlay is up.
///
/// A thread, not the main one: the main thread is blocked inside the overlay
/// until it answers, and the injection has to happen while it waits.
fn spawn_injector(variant: Variant, target: Target) -> Option<std::thread::JoinHandle<()>> {
    let spawned = std::thread::Builder::new()
        .name("overlay-selfcheck-input".to_owned())
        .spawn(move || {
            let display = match Display::connect() {
                Ok(display) => display,
                Err(message) => {
                    tracing::warn!(error = %message, "no input was sent");
                    return;
                }
            };
            // Only touch the mouse once an overlay window really exists.
            let deadline = Instant::now() + OVERLAY_APPEARS_WITHIN;
            while display.overlay_windows() == 0 {
                if Instant::now() > deadline {
                    tracing::warn!("no overlay window appeared; no input was sent");
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            // A moment for the first paint, so the screenshot behind the
            // selection is up before the drag starts.
            std::thread::sleep(Duration::from_millis(150));
            let injected = match variant {
                Variant::Drag => display.drag(target.from, target.to, DRAG_STEPS),
                // A keystroke goes wherever the focus is, and that is not
                // necessarily the overlay on a window manager that refuses to
                // hand the focus over: never send it into someone else's
                // window.
                Variant::Escape => {
                    let focused = Instant::now() + OVERLAY_APPEARS_WITHIN;
                    while !display.overlay_has_focus() {
                        if Instant::now() > focused {
                            tracing::warn!(
                                "no overlay window has the keyboard focus; \
                                 no keystroke was sent"
                            );
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    display.press_escape()
                }
                Variant::RightClick => display.click_right(target.from),
                Variant::Timeout => Ok(()),
            };
            if let Err(err) = injected {
                tracing::warn!(error = %err, "the injected input failed");
            }
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(err) => {
            tracing::warn!("could not start the input thread: {err}");
            None
        }
    }
}

/// One connection to the X server the self-check does its own work on.
///
/// Its own, not the backend's: the injection and the window-tree walks are not
/// part of the platform (LINUX_SUPPORT.md section 4 keeps XTest in the
/// self-check), and a connection of its own is what the injector thread needs.
struct Display {
    conn: RustConnection,
    root: xproto::Window,
}

impl Display {
    /// Connects the way the backend does, and finds the root window.
    fn connect() -> Result<Self, String> {
        let (conn, screen) = x11rb::connect(None)
            .map_err(|err| format!("could not connect to the X display: {err}"))?;
        let root = conn
            .setup()
            .roots
            .get(screen)
            .map(|screen| screen.root)
            .ok_or_else(|| format!("the display has no screen {screen}"))?;
        Ok(Self { conn, root })
    }

    /// How long ago the user last touched the keyboard or the mouse.
    ///
    /// MIT-SCREEN-SAVER's idle time is the user's input seen by the server,
    /// which is what the Windows check reads from `GetLastInputInfo`. `None`
    /// when the extension is not there or refuses to answer.
    fn idle_seconds(&self) -> Option<f64> {
        let reply = screensaver::query_info(&self.conn, self.root)
            .ok()?
            .reply()
            .ok()?;
        Some(f64::from(reply.ms_since_user_input) / 1000.0)
    }

    /// Where the pointer is, in root-window pixels.
    fn pointer(&self) -> Result<(i32, i32), String> {
        let reply = xproto::query_pointer(&self.conn, self.root)
            .map_err(|err| err.to_string())?
            .reply()
            .map_err(|err| err.to_string())?;
        Ok((i32::from(reply.root_x), i32::from(reply.root_y)))
    }

    /// Injects one XTest input event.
    ///
    /// XTest events are the server's own: they travel the normal delivery
    /// path, so the overlay sees them exactly like a user's input.
    fn fake_input(&self, type_: u8, detail: u8, at: (i32, i32)) -> Result<(), String> {
        let (x, y) = (
            i16::try_from(at.0).map_err(|_| format!("x {} is off the screen", at.0))?,
            i16::try_from(at.1).map_err(|_| format!("y {} is off the screen", at.1))?,
        );
        xtest::fake_input(
            &self.conn,
            type_,
            detail,
            x11rb::CURRENT_TIME,
            self.root,
            x,
            y,
            0,
        )
        .map_err(|err| err.to_string())?
        .check()
        .map_err(|err| err.to_string())
    }

    /// Moves the pointer with XTest, then checks it arrived.
    fn move_pointer(&self, to: (i32, i32)) -> Result<(), String> {
        self.fake_input(xproto::MOTION_NOTIFY_EVENT, 0, to)?;
        match self.pointer() {
            Ok(pointer) if pointer != to => {
                tracing::warn!(
                    ?to,
                    ?pointer,
                    "another program kept moving the pointer; the drag will not start where it was \
                     aimed"
                );
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(error = %err, "the pointer position could not be checked"),
        }
        Ok(())
    }

    /// A press at `from`, a walk to `to` in `steps`, and the release there.
    fn drag(&self, from: (i32, i32), to: (i32, i32), steps: i32) -> Result<(), String> {
        let steps = steps.max(1);
        self.fake_input(xproto::BUTTON_PRESS_EVENT, LEFT_BUTTON, from)?;
        for step in 1..=steps {
            let x = from.0 + (to.0 - from.0) * step / steps;
            let y = from.1 + (to.1 - from.1) * step / steps;
            self.fake_input(xproto::MOTION_NOTIFY_EVENT, 0, (x, y))?;
            std::thread::sleep(STEP_PAUSE);
        }
        // The release is at the corner the drag was aimed at, not at the last
        // interpolated step, so the rectangle is the one the check expects.
        self.fake_input(xproto::MOTION_NOTIFY_EVENT, 0, to)?;
        self.fake_input(xproto::BUTTON_RELEASE_EVENT, LEFT_BUTTON, to)
    }

    /// A right click at `at`.
    fn click_right(&self, at: (i32, i32)) -> Result<(), String> {
        self.fake_input(xproto::BUTTON_PRESS_EVENT, RIGHT_BUTTON, at)?;
        self.fake_input(xproto::BUTTON_RELEASE_EVENT, RIGHT_BUTTON, at)
    }

    /// Presses and releases Escape.
    fn press_escape(&self) -> Result<(), String> {
        let keycode = self.escape_keycode()?;
        // The coordinates of a key event are ignored by the server.
        self.fake_input(xproto::KEY_PRESS_EVENT, keycode, (0, 0))?;
        self.fake_input(xproto::KEY_RELEASE_EVENT, keycode, (0, 0))
    }

    /// The keycode Escape is bound to, from the server's keyboard mapping.
    ///
    /// XTest injects **keycodes**, and which one means Escape is whatever the
    /// user's layout says.
    fn escape_keycode(&self) -> Result<u8, String> {
        /// The `XK_Escape` keysym.
        const ESCAPE: u32 = 0xff1b;

        let setup = self.conn.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - setup.min_keycode + 1;
        let reply = xproto::get_keyboard_mapping(&self.conn, first, count)
            .map_err(|err| err.to_string())?
            .reply()
            .map_err(|err| err.to_string())?;
        let per = usize::from(reply.keysyms_per_keycode);
        if per == 0 {
            return Err("the server's keyboard mapping is empty".to_owned());
        }
        reply
            .keysyms
            .iter()
            .position(|keysym| *keysym == ESCAPE)
            .map(|index| first.wrapping_add(u8::try_from(index / per).unwrap_or(0)))
            .ok_or_else(|| "the server's keyboard mapping has no Escape key".to_owned())
    }

    /// How many windows of the overlay's class exist anywhere in the tree.
    ///
    /// The whole tree, not the root's children: a window manager reparents its
    /// clients into frames of its own, so an overlay window is usually a
    /// grandchild.
    fn overlay_windows(&self) -> usize {
        let mut count = 0;
        let mut queue = vec![self.root];
        let mut visited = 0_usize;
        while let Some(window) = queue.pop() {
            visited += 1;
            if visited > MAX_TREE_WINDOWS {
                tracing::warn!(
                    visited,
                    "the window tree is bigger than the search allows; stopping the walk"
                );
                break;
            }
            if self.window_class_matches(window) {
                count += 1;
            }
            match xproto::query_tree(&self.conn, window)
                .map_err(|err| err.to_string())
                .and_then(|cookie| cookie.reply().map_err(|err| err.to_string()))
            {
                Ok(reply) => queue.extend(reply.children),
                Err(err) => tracing::debug!(
                    window = format!("{window:#x}"),
                    error = %err,
                    "a window's children could not be read"
                ),
            }
        }
        count
    }

    /// Whether the window carries the overlay's `WM_CLASS`.
    fn window_class_matches(&self, window: xproto::Window) -> bool {
        let reply = xproto::get_property(
            &self.conn,
            false,
            window,
            xproto::AtomEnum::WM_CLASS,
            xproto::AtomEnum::STRING,
            0,
            256,
        )
        .map_err(|err| err.to_string())
        .and_then(|cookie| cookie.reply().map_err(|err| err.to_string()));
        match reply {
            Ok(reply) => class_matches(&reply.value),
            Err(err) => {
                tracing::debug!(
                    window = format!("{window:#x}"),
                    error = %err,
                    "a window's class could not be read"
                );
                false
            }
        }
    }

    /// Whether one of the overlay's windows has the input focus.
    ///
    /// This is what makes the Escape variant safe: a keystroke is delivered to
    /// the focus window, and if the window manager never handed the focus to
    /// the overlay, injecting one would type into whatever the user had in
    /// front instead.
    fn overlay_has_focus(&self) -> bool {
        let reply = match xproto::get_input_focus(&self.conn)
            .map_err(|err| err.to_string())
            .and_then(|cookie| cookie.reply().map_err(|err| err.to_string()))
        {
            Ok(reply) => reply,
            Err(err) => {
                tracing::debug!(error = %err, "the input focus could not be read");
                return false;
            }
        };
        // `None` (0) and `PointerRoot` (1) are not windows at all.
        reply.focus > 1 && self.window_class_matches(reply.focus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_variant_names_round_trip() {
        for variant in Variant::ALL {
            assert_eq!(
                Variant::parse(variant.name()).expect("it should parse"),
                variant
            );
        }
        let error = Variant::parse("nonsense").expect_err("it should be refused");
        assert!(error.contains("nonsense"), "{error}");
        assert!(error.contains("right-click"), "{error}");
    }

    #[test]
    fn only_the_timeout_variant_asks_for_the_short_timeout() {
        assert_eq!(Variant::Timeout.timeout(), SHORT_TIMEOUT);
        for variant in [Variant::Drag, Variant::Escape, Variant::RightClick] {
            assert_eq!(variant.timeout(), TIMEOUT, "{variant:?}");
        }
        assert!(SHORT_TIMEOUT < TIMEOUT);
    }

    #[test]
    fn the_drag_lands_inside_the_monitor_it_was_aimed_at() {
        let target = drag_target(RectI::new(0, 0, 1920, 1080)).expect("a big monitor");
        assert_eq!(target.from, (150, 150));
        assert_eq!(target.to, (550, 450));
        assert_eq!(target.expected, RectI::new(150, 150, 400, 300));

        // A monitor left of and above the origin: the drag keeps the monitor's
        // own coordinates.
        let target = drag_target(RectI::new(-1920, -200, 1920, 1080)).expect("a monitor");
        assert_eq!(target.from, (-1770, -50));
        assert_eq!(target.expected, RectI::new(-1770, -50, 400, 300));

        // A monitor too small for the drag is skipped rather than clipped into
        // something the check would then expect wrongly.
        assert_eq!(drag_target(RectI::new(0, 0, 300, 200)), None);
    }

    #[test]
    fn the_drag_stays_inside_a_monitor_smaller_than_the_drag() {
        // 800x600 can hold 400x300 from (150, 150), but 500x400 cannot: the
        // corners are clamped, and the rectangle that comes out is smaller.
        let target = drag_target(RectI::new(0, 0, 500, 400)).expect("a monitor");
        assert_eq!(target.to, (499, 399));
        assert_eq!(target.expected, RectI::new(150, 150, 349, 249));
    }

    #[test]
    fn a_rectangle_one_pixel_off_is_close_enough() {
        let rect = RectI::new(10, 20, 400, 300);
        assert!(close_enough(rect, rect));
        assert!(close_enough(rect, RectI::new(11, 19, 401, 299)));
        assert!(!close_enough(rect, RectI::new(12, 20, 400, 300)));
    }

    #[test]
    fn the_overlay_is_recognised_by_its_window_class() {
        // What winit sets: both parts of WM_CLASS.
        assert!(class_matches(b"snp-ligase-overlay\0snp-ligase-overlay\0"));
        // The app's own class, and anything else, is not the overlay.
        assert!(!class_matches(b"snp-ligase\0snp-ligase\0"));
        assert!(!class_matches(b""));
        assert!(!class_matches(b"\0\0"));
        // A window that only has the general part (a hand-made window, or a
        // toolkit that fills it differently) still counts.
        assert!(class_matches(b"instance\0snp-ligase-overlay\0"));
    }
}
