//! `--debug-overlay`: the self-check for the region-selection overlay
//! (NEXT_STEPS section 4.3).
//!
//! The overlay covers the whole desktop and blocks until the user decides, so
//! it is exactly the piece the agent cannot test with pure functions and must
//! not leave to the human either. This command runs the **real** overlay over a
//! **real** capture, injects a real drag (or Escape, or a right click, or waits
//! for the safety timeout), checks what came back, and then asks the platform
//! whether any window of the overlay's class is still alive.
//!
//! Exit codes: 0 = passed, 1 = failed, 3 = skipped because somebody was using
//! the machine (the input would take over their screen).
//!
//! It only injects input once an overlay window really exists, so a failed
//! start cannot spray clicks across whatever is on screen.
//!
//! The variant names, the exit codes and the reporting are this file's; the
//! run itself belongs to the platform — Windows drives the Win32 overlay here,
//! X11 hands over to [`snp_ligase_platform::linux::x11::overlay::selfcheck`],
//! which injects with XTest and walks the X window tree for leftovers
//! (LINUX_SUPPORT.md section 4, phase L1b).

use std::process::ExitCode;

#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use snp_ligase_platform::windows::{WindowsPlatform, overlay, synth};
#[cfg(windows)]
use snp_ligase_platform::{CaptureOptions, Platform, RectI};

/// The last keyboard or mouse input must be at least this old before the check
/// runs: it takes over the screen for a second or two.
#[cfg(windows)]
const IDLE_REQUIRED_SECONDS: f64 = 3.0;

/// How long the overlay is given to appear before the injected input gives up.
#[cfg(windows)]
const OVERLAY_APPEARS_WITHIN: Duration = Duration::from_secs(3);

/// The short timeout the `timeout` variant asks the overlay to use.
#[cfg(windows)]
const SHORT_TIMEOUT: Duration = Duration::from_millis(1500);

/// The rectangle the `drag` variant selects, as an offset and a size inside the
/// primary monitor.
#[cfg(windows)]
const DRAG_AT: (i32, i32) = (150, 150);
#[cfg(windows)]
const DRAG_SIZE: (i32, i32) = (400, 300);

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
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|variant| variant.name() == text)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|variant| variant.name()).collect();
                anyhow::anyhow!(
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
}

/// How the check ended.
enum Outcome {
    Passed(String),
    Skipped(String),
    Failed(String),
}

/// Runs the self-check and reports through the process's exit code.
///
/// `allow_busy` skips the idle guard. It exists for the human: the agent must
/// not pass it without being told to, because the overlay takes the screen for
/// a moment and the guard is what stops that happening mid-keystroke.
pub fn run(variant: Variant, allow_busy: bool) -> ExitCode {
    match attempt(variant, allow_busy) {
        Outcome::Passed(detail) => {
            tracing::info!(variant = variant.name(), "{detail}");
            println!("PASS [{}] {detail}", variant.name());
            ExitCode::SUCCESS
        }
        Outcome::Skipped(why) => {
            tracing::warn!(variant = variant.name(), "{why}");
            println!("SKIPPED [{}] {why}", variant.name());
            // A distinct code, so "the machine was busy" is never mistaken for
            // "the overlay is broken".
            ExitCode::from(3)
        }
        Outcome::Failed(why) => {
            tracing::error!(variant = variant.name(), "{why}");
            eprintln!("FAIL [{}] {why}", variant.name());
            ExitCode::FAILURE
        }
    }
}

/// The check itself: the platform's own overlay self-check.
///
/// The variant crossing a process's worth of code is only this mapping: the
/// three outcomes and their wording are the platforms' own, so the report a
/// run prints is written where the run happened.
#[cfg(target_os = "linux")]
fn attempt(variant: Variant, allow_busy: bool) -> Outcome {
    use snp_ligase_platform::linux::{SessionEnv, SessionKind};

    // The Wayland session: there is no X11 server to synthesise input on, and
    // the Wayland overlay's check is the real runs of this step (L3b,
    // ydotool), not this --debug-overlay machinery. Saying so is honest; a
    // half-working check would lie (NEXT_STEPS section 6c).
    if SessionEnv::read().kind() == SessionKind::Wayland {
        return Outcome::Skipped(format!(
            "--debug-overlay does not drive the Wayland overlay; the L3b real runs \
             (ydotool) are its check (variant {variant:?})"
        ));
    }

    use snp_ligase_platform::linux::x11::overlay::selfcheck;

    match selfcheck::run(platform_variant(variant), allow_busy) {
        selfcheck::Outcome::Passed(detail) => Outcome::Passed(detail),
        selfcheck::Outcome::Skipped(why) => Outcome::Skipped(why),
        selfcheck::Outcome::Failed(why) => Outcome::Failed(why),
    }
}

/// This command's variant as the X11 self-check's own (LINUX_SUPPORT.md section
/// 4, phase L1b).
#[cfg(target_os = "linux")]
#[must_use]
fn platform_variant(
    variant: Variant,
) -> snp_ligase_platform::linux::x11::overlay::selfcheck::Variant {
    use snp_ligase_platform::linux::x11::overlay::selfcheck::Variant as X11;

    match variant {
        Variant::Drag => X11::Drag,
        Variant::Escape => X11::Escape,
        Variant::RightClick => X11::RightClick,
        Variant::Timeout => X11::Timeout,
    }
}

/// The check itself, on Windows.
#[cfg(windows)]
fn attempt(variant: Variant, allow_busy: bool) -> Outcome {
    // The foreground guard: never take the screen while somebody is using it,
    // unless the human explicitly said so.
    match synth::last_input_seconds() {
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
                "running the self-check although the machine is busy (--allow-busy)"
            );
        }
        None => return Outcome::Failed("the last input time could not be read".to_owned()),
        Some(_) => {}
    }

    let platform = WindowsPlatform::new();
    let capture = match platform.capture_all(&CaptureOptions::default()) {
        Ok(capture) => capture,
        Err(err) => {
            return Outcome::Failed(format!("the capture before the overlay failed: {err}"));
        }
    };
    let Some(primary) = capture.frames.iter().find(|frame| frame.info.is_primary) else {
        return Outcome::Failed("the desktop reported no primary monitor".to_owned());
    };
    let monitor = primary.info.rect;

    // The rectangle the drag variant selects.
    let from = (
        monitor.x + DRAG_AT.0.min((monitor.w / 2).max(0)),
        monitor.y + DRAG_AT.1.min((monitor.h / 2).max(0)),
    );
    let to = (
        (from.0 + DRAG_SIZE.0).min(monitor.right() - 1),
        (from.1 + DRAG_SIZE.1).min(monitor.bottom() - 1),
    );
    let expected = RectI::new(from.0, from.1, to.0 - from.0, to.1 - from.1);
    if expected.w < 200 || expected.h < 150 {
        return Outcome::Skipped(format!(
            "the primary monitor ({monitor:?}) is too small for a {}x{} drag",
            DRAG_SIZE.0, DRAG_SIZE.1
        ));
    }

    let injector = {
        let handle = std::thread::Builder::new()
            .name("overlay-selfcheck-input".to_owned())
            .spawn(move || {
                // Only touch the mouse once an overlay window really exists.
                let deadline = std::time::Instant::now() + OVERLAY_APPEARS_WITHIN;
                while synth::windows_with_class(overlay::OVERLAY_CLASS) == 0 {
                    if std::time::Instant::now() > deadline {
                        tracing::warn!("no overlay window appeared; no input was sent");
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                // A moment for the first paint, so the screenshot behind the
                // selection is up before the drag starts.
                std::thread::sleep(Duration::from_millis(150));
                match variant {
                    Variant::Drag => synth::drag(from, to, 12),
                    Variant::Escape => synth::press_escape(),
                    Variant::RightClick => synth::right_click(),
                    Variant::Timeout => true,
                }
            });
        match handle {
            Ok(handle) => handle,
            Err(err) => return Outcome::Failed(format!("could not start the input thread: {err}")),
        }
    };

    let timeout = match variant {
        Variant::Timeout => SHORT_TIMEOUT,
        // A skipped injection must not strand the desktop for the normal
        // two-minute user-selection timeout.
        _ => Duration::from_secs(5),
    };
    let started = std::time::Instant::now();
    let result = overlay::select_region_with_timeout(&capture, timeout);
    let elapsed = started.elapsed();
    let injected = injector.join();

    // Give the window procedure's `DestroyWindow` calls a moment to land
    // before asking Windows what still exists.
    std::thread::sleep(Duration::from_millis(250));
    let leftovers = synth::windows_with_class(overlay::OVERLAY_CLASS);
    let input_sent = match injected {
        Ok(sent) => sent,
        Err(_) => {
            return Outcome::Failed(format!(
                "the input thread panicked; {leftovers} overlay windows left behind"
            ));
        }
    };
    if !input_sent && leftovers == 0 && result.is_ok() {
        return Outcome::Skipped("input skipped: overlay was not foreground, did not appear, or the cursor would not stay put; no overlay windows left behind".to_owned());
    }

    let detail = match (&result, variant) {
        (Ok(Some(rect)), Variant::Drag) if close_enough(*rect, expected) => {
            Ok(format!("dragged {expected:?} and got {rect:?} back"))
        }
        (Ok(Some(rect)), Variant::Drag) => Err(format!(
            "the overlay returned {rect:?} instead of {expected:?}"
        )),
        (Ok(None), Variant::Escape) => Ok("Escape cancelled the overlay".to_owned()),
        (Ok(None), Variant::RightClick) => Ok("a right click cancelled the overlay".to_owned()),
        (Ok(None), Variant::Timeout) => {
            if elapsed > timeout + Duration::from_secs(2) {
                Err(format!(
                    "the timeout took {elapsed:?} instead of about {timeout:?}"
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

/// Whether two rectangles differ by at most one pixel, which is all the
/// cursor's own rounding can explain.
#[cfg(windows)]
fn close_enough(a: RectI, b: RectI) -> bool {
    (a.x - b.x).abs() <= 1
        && (a.y - b.y).abs() <= 1
        && (a.w - b.w).abs() <= 1
        && (a.h - b.h).abs() <= 1
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
        assert!(error.to_string().contains("nonsense"), "{error}");
        assert!(error.to_string().contains("right-click"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn a_rectangle_one_pixel_off_is_close_enough() {
        let rect = RectI {
            x: 10,
            y: 20,
            w: 400,
            h: 300,
        };
        assert!(close_enough(rect, rect));
        assert!(close_enough(
            rect,
            RectI {
                x: 11,
                y: 19,
                w: 401,
                h: 299
            }
        ));
        assert!(!close_enough(
            rect,
            RectI {
                x: 12,
                y: 20,
                w: 400,
                h: 300
            }
        ));
    }

    /// The four names this command parses are the four the X11 self-check
    /// runs, one to one: a variant added on one side and forgotten on the other
    /// would run the wrong check (LINUX_SUPPORT.md section 4, phase L1b).
    #[cfg(target_os = "linux")]
    #[test]
    fn every_variant_has_an_x11_twin() {
        use snp_ligase_platform::linux::x11::overlay::selfcheck::Variant as X11;

        let expected = [
            (Variant::Drag, X11::Drag),
            (Variant::Escape, X11::Escape),
            (Variant::RightClick, X11::RightClick),
            (Variant::Timeout, X11::Timeout),
        ];
        assert_eq!(expected.len(), Variant::ALL.len());
        assert_eq!(expected.len(), X11::ALL.len());
        for (ours, theirs) in expected {
            assert_eq!(platform_variant(ours), theirs, "{}", ours.name());
            // The name has to match as well: it is what `--debug-overlay
            // <name>` takes.
            assert_eq!(ours.name(), theirs.name(), "{ours:?}");
        }
    }
}
