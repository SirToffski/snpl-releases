//! The controller thread (SPEC section 6.2).
//!
//! The egui frame callback is not guaranteed to run while the main window is
//! hidden or minimized, so nothing that has to happen while it is hidden can
//! live in the app. Instead:
//!
//! - Hotkey and tray callbacks run on the main thread's message pump (they are
//!   only alive while that pump runs) and do nothing but send an [`AppCommand`]
//!   over a channel. [`crate::hotkeys`] and [`crate::tray`] own those.
//! - This module owns the receiving end, on its own thread: the capture
//!   pipeline, including the blocking region overlay, never touches egui.
//! - When a capture is ready it is sent back as a [`UiEvent`], the main window
//!   is made visible and focused through [`WindowGate`], and a repaint is
//!   requested so the UI drains the queue on the next frame.
//!
//! The one shared thing here is [`WindowGate`]: egui's `Context` is `Send` and
//! `Sync` and its viewport commands are the documented way to show and hide a
//! window from another thread (SPEC 6.2 says to try this before Win32
//! `ShowWindow`; the spike recorded the answer in `DECISIONS.md`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, OnceLock};

use snp_ligase_platform::{
    CaptureOptions, HdrHighlights, MonitorFrame, Platform, RectI, VirtualCapture,
};

use crate::panels::CaptureMode;

/// How long the desktop is given to redraw after the main window is hidden
/// (SPEC section 5.7).
pub const HIDE_SETTLE: std::time::Duration = std::time::Duration::from_millis(200);

/// What the UI, the hotkeys and the tray ask the controller to do (SPEC 6.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppCommand {
    /// Capture in `mode`, waiting `delay_seconds` after hiding the window.
    ///
    /// Menus pass their Delay setting; hotkeys and the tray pass zero, because
    /// SPEC 5.7 says hotkey-triggered captures start immediately.
    Capture {
        mode: CaptureMode,
        delay_seconds: u8,
    },
    /// Bring the main window back without capturing.
    ShowMainWindow,
    /// The tray's Settings row: open the settings dialog.
    OpenSettings,
    /// Quit, after the app has asked about unsaved tabs (the close prompts are
    /// UI state, so this only asks).
    Quit,
    /// New capture settings from the Settings dialog (SPEC section 10).
    Configure(CaptureSettings),
}

/// The capture-side settings the controller thread owns (SPEC section 10).
///
/// The UI thread reads them from the settings file and sends them here; the
/// controller cannot read the file itself, because it must work while the
/// window is hidden and the UI is not running frames (SPEC 6.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureSettings {
    /// Hide the window while a capture is taken.
    pub hide_while_capturing: bool,
    /// Include the mouse cursor in captures.
    pub include_cursor: bool,
    /// What to do with values above SDR white on an HDR monitor.
    pub hdr_highlights: HdrHighlights,
    /// SDR white level override in nits; `None` is "automatic".
    pub sdr_white_nits: Option<f32>,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            hide_while_capturing: true,
            include_cursor: false,
            hdr_highlights: HdrHighlights::Clip,
            sdr_white_nits: None,
        }
    }
}

impl CaptureSettings {
    /// The options one capture runs with.
    #[must_use]
    pub fn options(self) -> CaptureOptions {
        CaptureOptions {
            include_cursor: self.include_cursor,
            hdr_highlights: self.hdr_highlights,
            sdr_white_override_nits: self.sdr_white_nits,
        }
    }
}

/// What the controller sends back to the UI thread (SPEC 6.2).
#[derive(Debug)]
pub enum UiEvent {
    /// A finished capture, to be opened in a new tab (SPEC 6.3).
    NewCapture(image::RgbaImage),
    /// A capture did not produce an image: cancelled, or failed.
    Status(String),
    /// Something the user should be told about without blocking, such as a
    /// hotkey that could not be registered (SPEC 6.1).
    Warning(String),
    /// The tray's Settings row: the UI opens the dialog.
    ShowSettings,
    /// The tray's Quit: the app decides what "leaving" means.
    QuitRequested,
}

/// The main window, reached from the controller thread through egui's
/// thread-safe viewport commands.
#[derive(Debug)]
pub struct WindowGate {
    context: OnceLock<egui::Context>,
    /// Whether the window should currently be visible.
    ///
    /// `show` and `hide` are the only writers, and they are the only way the
    /// window's visibility changes (including `hide_to_tray`). The controller
    /// has no other way to know: a capture that is cancelled must not pull the
    /// window out of the tray (G4). Starts `true` — the window is created
    /// visible.
    visible: AtomicBool,
}

impl Default for WindowGate {
    fn default() -> Self {
        Self {
            context: OnceLock::new(),
            visible: AtomicBool::new(true),
        }
    }
}

impl WindowGate {
    /// Hands the gate the context of the running UI. Called once, when the
    /// eframe app is created.
    pub fn attach(&self, context: egui::Context) {
        if self.context.set(context).is_err() {
            tracing::warn!("the window gate was given a second context; keeping the first");
        }
    }

    /// True once the UI exists and can be reached. Tests use this; the
    /// controller only needs [`WindowGate::show`] and [`WindowGate::hide`].
    #[cfg(test)]
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.context.get().is_some()
    }

    /// Whether the last show/hide asked for a visible window.
    ///
    /// The controller reads it before a capture, so a cancel can put the window
    /// back where it was instead of always showing it (G4).
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed)
    }

    /// Shows and focuses the main window, and makes sure a frame runs so the
    /// UI can drain its event queue (SPEC 6.3).
    pub fn show(&self) {
        // Set before the early return: the flag is what a cancelled capture
        // restores (G4), and tests drive it without an attached UI.
        self.visible.store(true, Ordering::Relaxed);
        let Some(context) = self.context.get() else {
            tracing::warn!("asked to show a window that is not attached to a UI yet");
            return;
        };
        context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        context.send_viewport_cmd(egui::ViewportCommand::Focus);
        context.request_repaint();
    }

    /// Hides the main window, so it is not in the screenshot (SPEC 5.7).
    pub fn hide(&self) {
        self.visible.store(false, Ordering::Relaxed);
        let Some(context) = self.context.get() else {
            return;
        };
        context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        context.request_repaint();
    }

    /// Makes sure a frame runs, without touching the window's visibility.
    ///
    /// The UI drains the controller's event queue at the top of every frame,
    /// and no frame runs while the window is hidden; events the tray produces
    /// while the window is hidden would otherwise sit in the queue until
    /// something else woke the UI (G2).
    pub fn wake(&self) {
        let Some(context) = self.context.get() else {
            return;
        };
        context.request_repaint();
    }
}

/// The UI's end of the controller: commands out, events in.
pub struct Controller {
    commands: Sender<AppCommand>,
    events: Receiver<UiEvent>,
    /// A second handle to the event channel.
    ///
    /// It keeps the channel alive even if the controller thread ends, and tests
    /// inject what the thread would have sent through it with
    /// [`Controller::inject_event`].
    _events: Sender<UiEvent>,
    /// Test support: the receiving end of the command channel when there is no
    /// worker thread, so a test can check what the UI sent.
    #[cfg(test)]
    commands_rx: std::sync::Mutex<Option<Receiver<AppCommand>>>,
    window: Arc<WindowGate>,
}

impl Controller {
    /// Starts the controller thread.
    ///
    /// It ends when the UI end is dropped, which is what happens when the
    /// window is closed for good.
    #[must_use]
    pub fn spawn(
        platform: Arc<dyn Platform>,
        window: Arc<WindowGate>,
        settings: CaptureSettings,
    ) -> Self {
        let (command_sender, command_receiver) = std::sync::mpsc::channel();
        let (event_sender, event_receiver) = std::sync::mpsc::channel();

        let worker = Worker {
            platform,
            window: Arc::clone(&window),
            events: event_sender.clone(),
            settings,
            last_region: None,
        };

        let spawned = std::thread::Builder::new()
            .name("capture-controller".to_owned())
            .spawn(move || run_worker(worker, command_receiver));
        if let Err(err) = spawned {
            // The UI still works; captures just never produce anything.
            tracing::error!(error = %err, "could not start the controller thread");
        }

        Self {
            commands: command_sender,
            events: event_receiver,
            _events: event_sender,
            #[cfg(test)]
            commands_rx: std::sync::Mutex::new(None),
            window,
        }
    }

    /// A controller with nothing behind it, for tests: the command channel has
    /// no receiver and the event channel no sender, so commands are dropped and
    /// no event ever arrives.
    #[cfg(test)]
    #[must_use]
    pub fn detached() -> Self {
        let (commands, commands_rx) = std::sync::mpsc::channel();
        let (events, incoming) = std::sync::mpsc::channel();
        Self {
            commands,
            events: incoming,
            _events: events,
            commands_rx: std::sync::Mutex::new(Some(commands_rx)),
            window: Arc::new(WindowGate::default()),
        }
    }

    /// Everything the UI has sent this controller, for tests.
    ///
    /// The real controller's channel ends at the worker thread; the detached
    /// one keeps its receiving end here so a test can see the commands the UI
    /// produced.
    #[cfg(test)]
    #[must_use]
    pub fn take_commands(&self) -> Vec<AppCommand> {
        self.commands_rx
            .lock()
            .ok()
            .and_then(|receiver| {
                receiver
                    .as_ref()
                    .map(|receiver| receiver.try_iter().collect())
            })
            .unwrap_or_default()
    }

    /// Injects an event as if the controller thread had sent it.
    ///
    /// Test support: unit tests do not run the real thread — it would capture
    /// the agent's desktop — so this is how the tray's Quit and Settings reach
    /// the UI in a test.
    #[cfg(test)]
    pub fn inject_event(&self, event: UiEvent) {
        if self._events.send(event).is_err() {
            tracing::warn!("an injected test event was dropped");
        }
    }

    /// Sends a command. A closed controller (the thread is gone) is not an
    /// error worth showing: the window is closing anyway.
    pub fn send(&self, command: AppCommand) {
        if self.commands.send(command).is_err() {
            tracing::warn!(?command, "the controller is gone; command dropped");
        }
    }

    /// A sender for the hotkey and tray callbacks, which only send.
    #[must_use]
    pub fn command_sender(&self) -> Sender<AppCommand> {
        self.commands.clone()
    }

    /// Everything the controller has said since the last frame.
    #[must_use]
    pub fn take_events(&self) -> Vec<UiEvent> {
        self.events.try_iter().collect()
    }

    /// The window gate, so the UI can show and hide its own window too.
    #[must_use]
    pub fn window(&self) -> &WindowGate {
        &self.window
    }

    /// The window gate as a shared handle, for the save worker's wake-up (H6).
    #[must_use]
    pub fn window_handle(&self) -> Arc<WindowGate> {
        Arc::clone(&self.window)
    }
}

/// The controller thread's loop: one command at a time, in order.
fn run_worker(mut worker: Worker, commands: Receiver<AppCommand>) {
    while let Ok(command) = commands.recv() {
        match command {
            AppCommand::Capture {
                mode,
                delay_seconds,
            } => {
                worker.capture(mode, delay_seconds);
                // G7: captures requested while this one ran — the overlay is
                // blocking, so a Print Screen press queues up behind it — are
                // stale: the user has already moved on. Drop them and say how
                // many; everything else still runs, in order.
                let (queued, dropped) = split_queued(commands.try_iter());
                if dropped > 0 {
                    tracing::info!(
                        dropped,
                        "dropped capture requests queued while another capture was running"
                    );
                }
                for command in queued {
                    // `split_queued` removed the captures, so this always
                    // handles the command.
                    let _handled = run_side_command(&mut worker, command);
                }
            }
            command => {
                let _handled = run_side_command(&mut worker, command);
            }
        }
    }
    tracing::debug!("the controller thread is done");
}

/// Splits the commands queued while a capture ran: capture requests are stale
/// and counted (G7), everything else is kept in order for the caller to run.
fn split_queued(queued: impl IntoIterator<Item = AppCommand>) -> (Vec<AppCommand>, usize) {
    let mut keep = Vec::new();
    let mut dropped = 0;
    for command in queued {
        match command {
            AppCommand::Capture { .. } => dropped += 1,
            other => keep.push(other),
        }
    }
    (keep, dropped)
}

/// Runs a command that is not a capture: showing the window, and the tray's
/// Quit and Settings. Returns `false` for a capture request, which the caller
/// handles itself — the main loop runs it, the drain above drops it.
fn run_side_command(worker: &mut Worker, command: AppCommand) -> bool {
    match command {
        AppCommand::Capture { .. } => false,
        AppCommand::ShowMainWindow => {
            worker.window.show();
            true
        }
        AppCommand::Quit => {
            // G2: the emit also wakes the UI, which is what makes this work
            // while the window is hidden.
            worker.emit(UiEvent::QuitRequested);
            true
        }
        AppCommand::OpenSettings => {
            worker.emit(UiEvent::ShowSettings);
            worker.window.show();
            true
        }
        AppCommand::Configure(settings) => {
            tracing::info!(?settings, "capture settings updated");
            worker.settings = settings;
            true
        }
    }
}

/// What the pure mode selection needs to know about the machine (SPEC 5.7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureContext {
    /// The cursor, in virtual-desktop physical pixels.
    pub cursor: Option<(i32, i32)>,
    /// The foreground window's rectangle, if there is one.
    pub foreground: Option<RectI>,
    /// The region the user dragged out last time.
    pub last_region: Option<RectI>,
}

/// The virtual-desktop rectangle a capture mode crops to, for every mode that
/// does not need the overlay (SPEC section 5.7).
///
/// This is a pure function with table tests on purpose: "which monitor contains
/// the cursor" and "is the last region still on a monitor" are decisions the
/// agent cannot check by clicking (NEXT_STEPS section 4.4).
///
/// # Errors
///
/// A message for the status bar when the mode cannot produce a region.
pub fn region_for(
    mode: CaptureMode,
    capture: &VirtualCapture,
    context: &CaptureContext,
) -> Result<RectI, String> {
    let bounds = capture.bounds();
    match mode {
        CaptureMode::AllMonitors => Ok(bounds),
        CaptureMode::CurrentMonitor => Ok(monitor_with_cursor(capture, context.cursor)?.info.rect),
        CaptureMode::ActiveWindow => {
            let Some(rect) = context.foreground else {
                return Err("No foreground window to capture.".to_owned());
            };
            let rect = rect.intersection(bounds);
            if rect.is_empty() {
                return Err("The active window is not on any monitor.".to_owned());
            }
            Ok(rect)
        }
        CaptureMode::RepeatLastRegion => {
            let Some(last) = context.last_region else {
                return Err("No region has been dragged out yet.".to_owned());
            };
            if !bounds.contains_rect(last) {
                return Err("The last region is no longer on a monitor; pick a new one.".to_owned());
            }
            Ok(last)
        }
        // Region goes through the overlay, not through this function.
        CaptureMode::Region => Err("The region overlay is not built into this path.".to_owned()),
    }
}

/// The monitor the cursor is on, or the primary one when it is on none (a
/// cursor can sit in the gap between monitors, and there is always a primary).
fn monitor_with_cursor(
    capture: &VirtualCapture,
    cursor: Option<(i32, i32)>,
) -> Result<&MonitorFrame, String> {
    if let Some((x, y)) = cursor {
        if let Some(frame) = capture
            .frames
            .iter()
            .find(|frame| frame.info.rect.contains(x, y))
        {
            return Ok(frame);
        }
        tracing::warn!(
            cursor_x = x,
            cursor_y = y,
            "the cursor is not on any monitor; using the primary one"
        );
    }
    capture
        .frames
        .iter()
        .find(|frame| frame.info.is_primary)
        .or_else(|| capture.frames.first())
        .ok_or_else(|| "The desktop reported no monitors.".to_owned())
}

/// The controller thread's state: one capture at a time, in order.
struct Worker {
    platform: Arc<dyn Platform>,
    window: Arc<WindowGate>,
    events: Sender<UiEvent>,
    /// What the Settings dialog last asked for (SPEC section 10).
    settings: CaptureSettings,
    /// The region the last successful selection produced, for "repeat last
    /// region" (SPEC 5.7).
    last_region: Option<RectI>,
}

impl Worker {
    /// Runs one capture: hide, wait, capture, pick the region, crop, report.
    ///
    /// Every exit path either restores the window's previous visibility (a
    /// cancelled region selection — G4) or shows it, because a failure has to
    /// be seen and SPEC 6.3 wants the new tab in front.
    fn capture(&mut self, mode: CaptureMode, delay_seconds: u8) {
        tracing::info!(
            mode = mode.label(),
            delay_seconds,
            hide = self.settings.hide_while_capturing,
            "capture requested"
        );
        // G4: remember where the window was before the capture. Pressing Print
        // Screen while the app lives in the tray and then pressing Esc must not
        // pop the window up.
        let was_visible = self.window.is_visible();
        if self.settings.hide_while_capturing {
            self.window.hide();
        }

        // SPEC 5.7: the delay is honoured after hiding, and the desktop gets a
        // fixed 200 ms on top so our own window is really gone from the screen.
        // Without hiding there is nothing to wait for.
        let delay = std::time::Duration::from_secs(u64::from(delay_seconds));
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        if self.settings.hide_while_capturing {
            std::thread::sleep(HIDE_SETTLE);
        }

        let started = std::time::Instant::now();
        let capture = match self.platform.capture_all(&self.settings.options()) {
            Ok(capture) => {
                // SPEC 12 M7's "hotkey to overlay visible" budget is this plus
                // the overlay's own `visible_ms` (and the few hundred
                // microseconds between the hotkey and this line, which the log
                // timestamps show).
                tracing::info!(
                    mode = mode.label(),
                    capture_ms = started.elapsed().as_secs_f64() * 1000.0,
                    "frames captured"
                );
                capture
            }
            Err(err) => {
                self.fail(format!("Capture failed: {err}"));
                return;
            }
        };

        let region = match mode {
            CaptureMode::Region => {
                tracing::info!(
                    mode = mode.label(),
                    to_overlay_ms = started.elapsed().as_secs_f64() * 1000.0,
                    "opening the region overlay"
                );
                match self.select_region(&capture) {
                    Some(region) => region,
                    None => {
                        tracing::info!("the region selection was cancelled");
                        self.emit(UiEvent::Status("Capture cancelled.".to_owned()));
                        if was_visible {
                            self.window.show();
                        } else {
                            tracing::debug!("the window stays hidden after the cancelled capture");
                        }
                        return;
                    }
                }
            }
            mode => {
                let context = CaptureContext {
                    cursor: self.platform.cursor_pos().ok(),
                    foreground: self.platform.foreground_window_rect().ok().flatten(),
                    last_region: self.last_region,
                };
                match region_for(mode, &capture, &context) {
                    Ok(region) => region,
                    Err(message) => {
                        self.fail(message);
                        return;
                    }
                }
            }
        };

        if region.is_empty() {
            self.fail("That capture would have no pixels in it.".to_owned());
            return;
        }

        let image = capture.crop(region);
        let (width, height) = (image.width(), image.height());
        tracing::info!(
            mode = mode.label(),
            region = ?region,
            size = format!("{width}x{height}"),
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "capture finished"
        );

        // SPEC 6.3: the new tab, the clipboard copy and the focus are the UI's
        // (it owns the document) — the controller only hands the image over and
        // brings the window back.
        self.emit(UiEvent::NewCapture(image));
        self.window.show();
    }

    /// Sends an event to the UI and makes sure a frame runs to drain it (G2).
    ///
    /// No frame runs while the window is hidden, so without the wake the tray's
    /// Quit and Settings rows did nothing until something else woke the UI.
    fn emit(&mut self, event: UiEvent) {
        if self.events.send(event).is_err() {
            tracing::warn!("the UI is gone; the event was dropped");
        }
        self.window.wake();
    }

    /// The blocking overlay, with the region remembered for "repeat last".
    fn select_region(&mut self, capture: &VirtualCapture) -> Option<RectI> {
        match self.platform.select_region(capture) {
            Ok(Some(region)) => {
                self.last_region = Some(region);
                Some(region)
            }
            Ok(None) => None,
            Err(err) => {
                self.fail(format!("Region selection failed: {err}"));
                None
            }
        }
    }

    /// Reports a failure and brings the window back, so the warning is seen.
    fn fail(&mut self, message: String) {
        tracing::warn!("{message}");
        self.emit(UiEvent::Warning(message));
        self.window.show();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbaImage;
    use snp_ligase_platform::{CaptureApi, FrameDiagnostics, MonitorInfo};

    /// A synthetic desktop: `(rect, primary)` pairs become monitors with a
    /// one-pixel image, so only the geometry matters.
    fn desktop(monitors: &[(RectI, bool)]) -> VirtualCapture {
        VirtualCapture {
            frames: monitors
                .iter()
                .enumerate()
                .map(|(index, &(rect, is_primary))| MonitorFrame {
                    info: MonitorInfo {
                        id: format!("\\\\.\\DISPLAY{}", index + 1),
                        rect,
                        scale_factor: 1.0,
                        hdr_active: false,
                        sdr_white_nits: None,
                        is_primary,
                    },
                    image: RgbaImage::new(1, 1),
                    diagnostics: FrameDiagnostics {
                        api_used: CaptureApi::Wgc,
                        pixel_format: "Bgra8".to_owned(),
                        width: 1,
                        height: 1,
                        row_pitch: 4,
                        max_channel_value: 0.0,
                        sdr_white_nits: 200.0,
                        elapsed_ms: 0.0,
                    },
                })
                .collect(),
        }
    }

    /// Two monitors: a primary at the origin and one to its left at a negative
    /// origin, which is the layout that catches sign mistakes.
    fn two_monitors() -> VirtualCapture {
        desktop(&[
            (
                RectI {
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                },
                true,
            ),
            (
                RectI {
                    x: -2560,
                    y: 0,
                    w: 2560,
                    h: 1440,
                },
                false,
            ),
        ])
    }

    fn context() -> CaptureContext {
        CaptureContext::default()
    }

    #[test]
    fn current_monitor_uses_the_monitor_the_cursor_is_on() {
        let capture = two_monitors();
        let left = region_for(
            CaptureMode::CurrentMonitor,
            &capture,
            &CaptureContext {
                cursor: Some((-100, 500)),
                ..context()
            },
        )
        .expect("the cursor is on the left monitor");
        assert_eq!(left.x, -2560);

        let right = region_for(
            CaptureMode::CurrentMonitor,
            &capture,
            &CaptureContext {
                cursor: Some((1000, 500)),
                ..context()
            },
        )
        .expect("the cursor is on the primary monitor");
        assert_eq!(
            right,
            RectI {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080
            }
        );
    }

    #[test]
    fn current_monitor_falls_back_to_the_primary_when_the_cursor_is_in_a_gap() {
        // A cursor can sit in a gap, or there may be no cursor reading at all.
        let capture = two_monitors();
        for cursor in [Some((5000, 5000)), None] {
            let region = region_for(
                CaptureMode::CurrentMonitor,
                &capture,
                &CaptureContext {
                    cursor,
                    ..context()
                },
            )
            .expect("there is a primary monitor");
            assert_eq!(region.x, 0, "cursor {cursor:?}");
            assert!(region.contains(0, 0));
        }
    }

    #[test]
    fn all_monitors_is_the_union_of_the_monitors() {
        let region = region_for(CaptureMode::AllMonitors, &two_monitors(), &context())
            .expect("the desktop has monitors");
        assert_eq!(
            region,
            RectI {
                x: -2560,
                y: 0,
                w: 4480,
                h: 1440
            }
        );
    }

    #[test]
    fn active_window_is_clipped_to_the_desktop() {
        let capture = two_monitors();

        // A window fully on the primary monitor.
        let inside = region_for(
            CaptureMode::ActiveWindow,
            &capture,
            &CaptureContext {
                foreground: Some(RectI {
                    x: 100,
                    y: 100,
                    w: 800,
                    h: 600,
                }),
                ..context()
            },
        )
        .expect("the window is on a monitor");
        assert_eq!(
            inside,
            RectI {
                x: 100,
                y: 100,
                w: 800,
                h: 600
            }
        );

        // A maximized window whose invisible border sticks out by a few pixels
        // is clipped instead of producing transparent edges.
        let clipped = region_for(
            CaptureMode::ActiveWindow,
            &capture,
            &CaptureContext {
                foreground: Some(RectI {
                    x: -2560,
                    y: -8,
                    w: 2570,
                    h: 1448,
                }),
                ..context()
            },
        )
        .expect("the window is on a monitor");
        assert_eq!(clipped.y, 0);
        assert_eq!(clipped.x, -2560);
    }

    #[test]
    fn a_mode_that_cannot_work_says_why() {
        let capture = two_monitors();
        let no_window = region_for(CaptureMode::ActiveWindow, &capture, &context())
            .expect_err("there is no foreground window");
        assert!(no_window.contains("foreground"), "{no_window}");

        let no_region = region_for(CaptureMode::RepeatLastRegion, &capture, &context())
            .expect_err("no region has been selected");
        assert!(no_region.contains("No region"), "{no_region}");

        let off_screen = region_for(
            CaptureMode::RepeatLastRegion,
            &capture,
            &CaptureContext {
                last_region: Some(RectI {
                    x: 5000,
                    y: 0,
                    w: 100,
                    h: 100,
                }),
                ..context()
            },
        )
        .expect_err("the region is not on a monitor any more");
        assert!(
            off_screen.contains("no longer on a monitor"),
            "{off_screen}"
        );

        let window_off_screen = region_for(
            CaptureMode::ActiveWindow,
            &capture,
            &CaptureContext {
                foreground: Some(RectI {
                    x: 5000,
                    y: 5000,
                    w: 100,
                    h: 100,
                }),
                ..context()
            },
        )
        .expect_err("the window is not on a monitor");
        assert!(window_off_screen.contains("not on any monitor"));
    }

    #[test]
    fn repeat_last_region_reuses_a_region_that_still_exists() {
        let capture = two_monitors();
        let last = RectI {
            x: -2000,
            y: 10,
            w: 300,
            h: 200,
        };
        let region = region_for(
            CaptureMode::RepeatLastRegion,
            &capture,
            &CaptureContext {
                last_region: Some(last),
                ..context()
            },
        )
        .expect("the region is on the left monitor");
        assert_eq!(region, last);
    }

    #[test]
    fn the_controller_survives_being_used_without_a_thread() {
        // The detached controller is what the tests and the headless commands
        // use: sending must not panic, and nothing comes back.
        let controller = Controller::detached();
        controller.send(AppCommand::ShowMainWindow);
        assert!(controller.take_events().is_empty());
        assert!(!controller.window().is_attached());
    }

    // ---------------------------------------------------------------------
    // Step 3b: the window gate, cancelled captures and queued captures
    // ---------------------------------------------------------------------

    /// What the fake platform's region selection does.
    #[derive(Debug, Clone, Copy, Default)]
    enum Select {
        /// The user cancelled (Esc or a right click).
        #[default]
        Cancel,
        /// The user dragged out this rectangle.
        Finish(RectI),
        /// The overlay itself failed.
        Fail,
    }

    /// A platform for the controller tests: it counts captures and returns a
    /// fixed result from `select_region`, so every path of [`Worker::capture`]
    /// can be driven without touching the real desktop.
    struct FakePlatform {
        captures: std::sync::atomic::AtomicUsize,
        select: Select,
    }

    impl FakePlatform {
        fn new(select: Select) -> Self {
            Self {
                captures: std::sync::atomic::AtomicUsize::new(0),
                select,
            }
        }

        fn captures(&self) -> usize {
            self.captures.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Platform for FakePlatform {
        fn monitors(&self) -> snp_ligase_platform::Result<Vec<MonitorInfo>> {
            Err(snp_ligase_platform::Error::Capture {
                message: "the fake platform has no monitors".to_owned(),
            })
        }

        fn capture_all(
            &self,
            _opts: &CaptureOptions,
        ) -> snp_ligase_platform::Result<VirtualCapture> {
            self.captures
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(two_monitors())
        }

        fn select_region(
            &self,
            _capture: &VirtualCapture,
        ) -> snp_ligase_platform::Result<Option<RectI>> {
            match self.select {
                Select::Cancel => Ok(None),
                Select::Finish(rect) => Ok(Some(rect)),
                Select::Fail => Err(snp_ligase_platform::Error::Capture {
                    message: "the fake overlay refused".to_owned(),
                }),
            }
        }

        fn foreground_window_rect(&self) -> snp_ligase_platform::Result<Option<RectI>> {
            Ok(None)
        }

        fn cursor_pos(&self) -> snp_ligase_platform::Result<(i32, i32)> {
            Ok((0, 0))
        }
    }

    /// A worker wired to a fake platform and the given window gate.
    fn worker(
        platform: Arc<dyn Platform>,
        window: &Arc<WindowGate>,
    ) -> (Worker, Receiver<UiEvent>) {
        let (events, incoming) = std::sync::mpsc::channel();
        (
            Worker {
                platform,
                window: Arc::clone(window),
                events,
                settings: CaptureSettings::default(),
                last_region: None,
            },
            incoming,
        )
    }

    #[test]
    fn a_cancelled_capture_leaves_a_hidden_window_hidden() {
        // G4: pressing Print Screen while the app lives in the tray and then
        // pressing Esc must not pop the window up.
        let platform: Arc<dyn Platform> = Arc::new(FakePlatform::new(Select::Cancel));
        let window = Arc::new(WindowGate::default());
        window.hide();
        let (mut worker, events) = worker(platform, &window);

        worker.capture(CaptureMode::Region, 0);

        assert!(
            !window.is_visible(),
            "the cancelled capture brought the window back from the tray"
        );
        let events: Vec<UiEvent> = events.try_iter().collect();
        assert!(
            matches!(events.as_slice(), [UiEvent::Status(message)] if message.contains("cancelled")),
            "the UI should be told the capture was cancelled: {events:?}"
        );
    }

    #[test]
    fn a_cancelled_capture_brings_a_visible_window_back() {
        // ...but a cancel must still restore the window the user was looking
        // at before the capture started: the menu-driven captures hide it, and
        // leaving it hidden would strand the app in the tray.
        let platform: Arc<dyn Platform> = Arc::new(FakePlatform::new(Select::Cancel));
        let window = Arc::new(WindowGate::default());
        assert!(window.is_visible(), "the gate starts visible");
        let (mut worker, _events) = worker(platform, &window);

        worker.capture(CaptureMode::Region, 0);

        assert!(
            window.is_visible(),
            "the cancelled capture left the window hidden"
        );
    }

    #[test]
    fn a_finished_capture_shows_the_window() {
        let platform: Arc<dyn Platform> = Arc::new(FakePlatform::new(Select::Finish(RectI {
            x: 100,
            y: 100,
            w: 10,
            h: 6,
        })));
        let window = Arc::new(WindowGate::default());
        window.hide();
        let (mut worker, events) = worker(platform, &window);

        worker.capture(CaptureMode::Region, 0);

        assert!(window.is_visible(), "the window was not shown");
        let events: Vec<UiEvent> = events.try_iter().collect();
        assert!(
            matches!(events.as_slice(), [UiEvent::NewCapture(image)] if (image.width(), image.height()) == (10, 6)),
            "the capture did not arrive: {events:?}"
        );
    }

    #[test]
    fn a_failed_capture_shows_the_window_so_the_warning_is_seen() {
        let platform: Arc<dyn Platform> = Arc::new(FakePlatform::new(Select::Fail));
        let window = Arc::new(WindowGate::default());
        window.hide();
        let (mut worker, events) = worker(platform, &window);

        worker.capture(CaptureMode::Region, 0);

        assert!(
            window.is_visible(),
            "the failure was reported into a hidden window"
        );
        let events: Vec<UiEvent> = events.try_iter().collect();
        assert!(
            events.iter().any(
                |event| matches!(event, UiEvent::Warning(message) if message.contains("failed"))
            ),
            "the failure was not reported: {events:?}"
        );
    }

    #[test]
    fn sending_an_event_wakes_the_ui() {
        // G2: nothing runs a frame while the window is hidden, so an event the
        // tray produces there must ask for one, or the UI never drains it.
        let gate = Arc::new(WindowGate::default());
        let context = egui::Context::default();
        gate.attach(context.clone());
        // egui asks for a couple of frames at the start by itself; run passes
        // until the context is settled, so the assertion below is about the
        // event and nothing else.
        for _ in 0..5 {
            if !context.has_requested_repaint() {
                break;
            }
            let mut settle = context.run_ui(egui::RawInput::default(), |_ui| {});
            settle.textures_delta.clear();
        }
        assert!(
            !context.has_requested_repaint(),
            "the context never settled"
        );
        let platform: Arc<dyn Platform> = Arc::new(FakePlatform::new(Select::Cancel));
        let (mut worker, _events) = worker(platform, &gate);

        worker.emit(UiEvent::Status("test".to_owned()));

        assert!(
            context.has_requested_repaint(),
            "the event did not wake the UI"
        );
    }

    #[test]
    fn queued_captures_are_dropped_and_other_commands_keep_their_order() {
        // G7: only captures are stale while a capture ran.
        let (kept, dropped) = split_queued([
            AppCommand::Capture {
                mode: CaptureMode::Region,
                delay_seconds: 0,
            },
            AppCommand::ShowMainWindow,
            AppCommand::Capture {
                mode: CaptureMode::AllMonitors,
                delay_seconds: 5,
            },
            AppCommand::Quit,
        ]);
        assert_eq!(dropped, 2);
        assert_eq!(kept, vec![AppCommand::ShowMainWindow, AppCommand::Quit]);
    }

    #[test]
    fn a_capture_queued_while_another_ran_never_runs() {
        // The behavioural half of G7: pressing Print Screen again while the
        // overlay is open must not start a second capture when the first one
        // finishes. Both commands are in the queue before the worker starts,
        // so this is deterministic.
        let platform = Arc::new(FakePlatform::new(Select::Cancel));
        let gate = Arc::new(WindowGate::default());
        let (commands, incoming) = std::sync::mpsc::channel();
        for _ in 0..2 {
            commands
                .send(AppCommand::Capture {
                    mode: CaptureMode::Region,
                    delay_seconds: 0,
                })
                .expect("the queue should accept the command");
        }
        drop(commands);
        let (worker, _events) = worker(platform.clone(), &gate);

        run_worker(worker, incoming);

        assert_eq!(platform.captures(), 1, "the queued capture was not dropped");
    }
}
