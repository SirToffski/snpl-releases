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

#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, OnceLock};

use snp_ligase_platform::{
    CaptureOptions, HdrHighlights, MonitorFrame, Platform, RectI, VirtualCapture,
};

use crate::panels::CaptureMode;

/// Show the permission-wait status after this long without capture frames.
pub const CAPTURE_WAIT_NOTICE: std::time::Duration = std::time::Duration::from_secs(2);

/// Desktop redraw time after hiding on native backends (SPEC 5.7).
pub const HIDE_SETTLE: std::time::Duration = std::time::Duration::from_millis(200);

/// What the UI, the hotkeys and the tray ask the controller to do (SPEC 6.2).
#[derive(Debug, Clone, PartialEq)]
pub enum AppCommand {
    /// A capture with the Wayland activation token that came with it (a
    /// portal shortcut or a forwarded `--capture`), so the editor can be
    /// brought back afterwards (LINUX_SUPPORT section 7b, W2). Only the Linux
    /// paths send it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ActivatedCapture {
        mode: CaptureMode,
        delay_seconds: u8,
        token: Option<String>,
    },
    /// Capture in `mode`, waiting `delay_seconds` after hiding the window.
    ///
    /// Menus pass their Delay setting; hotkeys and the tray pass zero, because
    /// SPEC 5.7 says hotkey-triggered captures start immediately.
    Capture {
        mode: CaptureMode,
        delay_seconds: u8,
    },
    /// Bring the main window back without capturing.
    ///
    /// Sent by the Windows tray; on Linux the tray is off (LINUX_SUPPORT.md
    /// section 1) until the instance channel and close dialog arrive (L0d).
    #[cfg_attr(not(windows), allow(dead_code))]
    ShowMainWindow,
    /// The tray's Settings row: open the settings dialog.
    #[cfg_attr(not(windows), allow(dead_code))]
    OpenSettings,
    /// Quit, after the app has asked about unsaved tabs (the close prompts are
    /// UI state, so this only asks).
    #[cfg_attr(not(windows), allow(dead_code))]
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
    pub copy_new_captures: bool,
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
            copy_new_captures: true,
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
    /// Clipboard is complete; a later event completes the optional background save.
    NewCaptureSaved {
        image: image::RgbaImage,
        copy: Result<(), String>,
        id: u64,
        saving: bool,
    },
    /// Save completion is correlated with the original capture, even after tab switches.
    CaptureSaved {
        id: u64,
        result: Result<std::path::PathBuf, String>,
    },
    /// A capture did not produce an image: cancelled, or failed.
    Status(String),
    /// Something the user should be told about without blocking, such as a
    /// hotkey that could not be registered (SPEC 6.1).
    Warning(String),
    /// The tray's Settings row: the UI opens the dialog.
    ShowSettings,
    /// The tray's Quit: the app decides what "leaving" means.
    QuitRequested,
    /// Image files another launch asked this instance to open
    /// (LINUX_SUPPORT.md section 3, item 9). The instance channel posts this
    /// itself through [`Controller::event_sender`]; no capture is involved.
    ///
    /// Unix for now: the Windows launch path has no channel to forward files
    /// through (SPEC P2), so nothing there constructs it.
    #[cfg_attr(not(unix), allow(dead_code))]
    OpenFiles(Vec<std::path::PathBuf>),
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
    wayland: AtomicBool,
    focused: AtomicBool,
    pub clipboard: crate::clipboard::ClipboardHandle,
    auto_save: std::sync::Mutex<Option<crate::settings::Settings>>,
    pub save_claims: std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
    #[cfg(target_os = "linux")]
    surface:
        std::sync::Mutex<Option<snp_ligase_platform::linux::wayland::activation::EditorSurface>>,
    /// How many times a hide was asked for. `is_visible` cannot tell a hide
    /// that was undone from a hide that never happened, and a test needs that
    /// difference (LINUX_SUPPORT.md section 3, item 2).
    #[cfg(test)]
    hides: AtomicUsize,
}

impl Default for WindowGate {
    fn default() -> Self {
        Self {
            context: OnceLock::new(),
            visible: AtomicBool::new(true),
            wayland: AtomicBool::new(false),
            focused: AtomicBool::new(false),
            clipboard: crate::clipboard::ClipboardHandle::default(),
            auto_save: std::sync::Mutex::new(None),
            save_claims: std::sync::Mutex::default(),
            #[cfg(target_os = "linux")]
            surface: std::sync::Mutex::new(None),
            #[cfg(test)]
            hides: AtomicUsize::new(0),
        }
    }
}

impl WindowGate {
    pub fn set_auto_save(&self, settings: crate::settings::Settings) {
        if let Ok(mut saved) = self.auto_save.lock() {
            *saved = Some(settings);
        }
    }

    pub fn set_wayland(&self, enabled: bool) {
        self.wayland.store(enabled, Ordering::Relaxed);
    }

    #[cfg(target_os = "linux")]
    pub fn attach_surface(&self, frame: &eframe::Frame) {
        if !self.is_wayland() {
            return;
        }
        if self.surface.lock().is_ok_and(|surface| surface.is_some()) {
            return;
        }
        // Setup may wait for a bounded registry roundtrip. The UI owns the
        // window here; do not hold the surface lock that teardown needs.
        match snp_ligase_platform::linux::wayland::activation::EditorSurface::capture(frame) {
            Ok(handles) => {
                if let Ok(mut surface) = self.surface.lock() {
                    *surface = Some(handles);
                    tracing::info!("editor Wayland handles attached");
                }
            }
            Err(error) => tracing::debug!(%error, "editor Wayland handles unavailable"),
        }
    }

    #[cfg(target_os = "linux")]
    pub fn detach_surface(&self) {
        if let Ok(mut surface) = self.surface.lock() {
            *surface = None;
        }
    }

    fn activate_wayland(&self, token: Option<String>) {
        #[cfg(target_os = "linux")]
        if let Some(token) = token
            && let Ok(surface) = self.surface.lock()
            && let Some(surface) = surface.as_ref()
        {
            match surface.activate(token) {
                Ok(()) => return,
                Err(error) => tracing::warn!(%error, "controller activation failed"),
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = token;
        tracing::info!("no usable activation token/surface; taskbar attention is the fallback");
    }

    pub fn is_wayland(&self) -> bool {
        self.wayland.load(Ordering::Relaxed)
    }

    pub fn record_focus(&self, focused: bool) {
        self.focused.store(focused, Ordering::Relaxed);
    }

    fn should_hide(&self) -> bool {
        !self.is_wayland() || self.focused.load(Ordering::Relaxed)
    }

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

    /// How many times a hide was asked for, undoing or not. Tests use this;
    /// the controller never needs it.
    #[cfg(test)]
    #[must_use]
    pub fn hide_count(&self) -> usize {
        self.hides.load(Ordering::Relaxed)
    }

    /// Shows and focuses the main window, and makes sure a frame runs so the
    /// UI can drain its event queue (SPEC 6.3).
    pub fn show(&self) {
        self.show_activated(None);
    }

    pub fn show_activated(&self, token: Option<String>) {
        // Set before the early return: the flag is what a cancelled capture
        // restores (G4), and tests drive it without an attached UI.
        self.visible.store(true, Ordering::Relaxed);
        let Some(context) = self.context.get() else {
            tracing::warn!("asked to show a window that is not attached to a UI yet");
            return;
        };
        if self.is_wayland() {
            if token.is_some() {
                self.activate_wayland(token);
            }
            // Flushing activate is not confirmation of focus (KDE's real CLI
            // token run stayed minimized). Queue attention only after the
            // capture/show request, and only while the editor lacks focus.
            if !self.focused.load(Ordering::Relaxed) {
                context.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                    egui::UserAttentionType::Informational,
                ));
            }
        }
        context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        context.send_viewport_cmd(egui::ViewportCommand::Focus);
        context.request_repaint();
    }

    /// Hides the main window, so it is not in the screenshot (SPEC 5.7).
    pub fn hide(&self) {
        self.visible.store(false, Ordering::Relaxed);
        #[cfg(test)]
        self.hides.fetch_add(1, Ordering::Relaxed);
        let Some(context) = self.context.get() else {
            return;
        };
        if self.wayland.load(Ordering::Relaxed) {
            context.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        } else {
            context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
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
            last_overlay_closed: None,
            #[cfg(test)]
            save_pause: None,
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

    /// A sender for the UI's event queue, for the parts of the shell that are
    /// not the controller: the instance channel (LINUX_SUPPORT.md section 3,
    /// item 9) posts [`UiEvent::OpenFiles`] through it while the app runs.
    ///
    /// The caller asks for a frame itself ([`WindowGate::wake`]) — this only
    /// puts the event in the queue, exactly like the controller's own emits.
    ///
    /// Unix for now: the instance channel is the only caller
    /// (LINUX_SUPPORT.md section 3, item 9).
    #[cfg_attr(not(unix), allow(dead_code))]
    #[must_use]
    pub fn event_sender(&self) -> Sender<UiEvent> {
        self._events.clone()
    }

    /// Sends a command. A closed controller (the thread is gone) is not an
    /// error worth showing: the window is closing anyway.
    pub fn send(&self, command: AppCommand) {
        if self.commands.send(command).is_err() {
            tracing::warn!("the controller is gone; command dropped");
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
        let (command, token) = match command {
            AppCommand::ActivatedCapture {
                mode,
                delay_seconds,
                token,
            } => (
                AppCommand::Capture {
                    mode,
                    delay_seconds,
                },
                token,
            ),
            other => (other, None),
        };
        match command {
            AppCommand::Capture {
                mode,
                delay_seconds,
            } => {
                worker.capture_activated(mode, delay_seconds, token);
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
            AppCommand::Capture { .. } | AppCommand::ActivatedCapture { .. } => dropped += 1,
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
        AppCommand::Capture { .. } | AppCommand::ActivatedCapture { .. } => false,
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

/// Dropping the sender cancels the waiting notice, even on an early return.
fn waiting_status(
    events: Sender<UiEvent>,
    window: Arc<WindowGate>,
    after: std::time::Duration,
) -> Sender<()> {
    let (done, receiver) = std::sync::mpsc::channel();
    if let Err(error) = std::thread::Builder::new()
        .name("capture-wait-status".into())
        .spawn(move || {
            if matches!(
                receiver.recv_timeout(after),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                tracing::info!("Waiting for the desktop to allow the screenshot…");
                let _ = events.send(UiEvent::Status(
                    "Waiting for the desktop to allow the screenshot…".into(),
                ));
                window.wake();
            }
        })
    {
        tracing::warn!(%error, "could not start capture waiting notice");
    }
    done
}

/// Wayland closing animations must finish before freezing the next frame.
fn overlay_capture_gap(
    wayland: bool,
    since_closed: Option<std::time::Duration>,
) -> std::time::Duration {
    if wayland {
        since_closed.map_or(std::time::Duration::ZERO, |elapsed| {
            std::time::Duration::from_millis(300).saturating_sub(elapsed)
        })
    } else {
        std::time::Duration::ZERO
    }
}

/// Publish the tab first; encoding and atomic IO must never hold the controller.
fn save_capture_off_thread(
    id: u64,
    events: Sender<UiEvent>,
    window: Arc<WindowGate>,
    save: impl FnOnce() -> Result<std::path::PathBuf, String> + Send + 'static,
) {
    let completion = events.clone();
    let wake = Arc::clone(&window);
    if let Err(error) = std::thread::Builder::new()
        .name("capture-save".into())
        .spawn(move || {
            let result = save();
            let _ = completion.send(UiEvent::CaptureSaved { id, result });
            wake.wake();
        })
    {
        tracing::error!(%error, id, "could not start capture save worker");
        let _ = events.send(UiEvent::CaptureSaved {
            id,
            result: Err(error.to_string()),
        });
        window.wake();
    }
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
    last_overlay_closed: Option<std::time::Instant>,
    #[cfg(test)]
    save_pause: Option<Arc<std::sync::Barrier>>,
}

impl Worker {
    /// Runs one capture: hide, wait, capture, pick the region, crop, report.
    ///
    /// Every exit path either restores the window's previous visibility (a
    /// cancelled region selection — G4) or shows it, because a failure has to
    /// be seen and SPEC 6.3 wants the new tab in front.
    #[cfg(test)]
    fn capture(&mut self, mode: CaptureMode, delay_seconds: u8) {
        self.capture_activated(mode, delay_seconds, None);
    }

    fn capture_activated(
        &mut self,
        mode: CaptureMode,
        delay_seconds: u8,
        mut token: Option<String>,
    ) {
        let hotkey_started = std::time::Instant::now();
        tracing::info!(
            mode = mode.label(),
            delay_seconds,
            hide = self.settings.hide_while_capturing,
            "capture requested"
        );
        // LINUX_SUPPORT.md section 3, item 2: the controller reads the
        // platform's capabilities before it hides anything. A mode this
        // machine cannot run is refused with the reason instead of a failure
        // after the window has already been hidden.
        let capabilities = self.platform.capabilities();
        if let Some(reason) = mode.blocked(&capabilities) {
            tracing::warn!(mode = mode.label(), reason, "capture refused");
            self.fail(format!("Capture failed: {reason}"));
            return;
        }
        // Hiding is a capability of its own: a platform that cannot hide the
        // window (Wayland, LINUX_SUPPORT.md section 2) captures anyway if the
        // rest works.
        let hide = self.settings.hide_while_capturing
            && capabilities.hide_main_window
            && self.window.should_hide()
            // KWin picks these targets itself; minimizing the editor first
            // would change which window is active (W3).
            && !(capabilities.compositor_targets
                && matches!(
                    mode,
                    CaptureMode::CurrentMonitor | CaptureMode::ActiveWindow
                ));
        tracing::info!(
            hide,
            focused = self.window.focused.load(Ordering::Relaxed),
            "capture window decision"
        );
        // G4: remember where the window was before the capture. Pressing Print
        // Screen while the app lives in the tray and then pressing Esc must not
        // pop the window up.
        let was_visible = self.window.is_visible();
        if hide {
            self.window.hide();
        }

        // SPEC 5.7: the delay is honoured after hiding, and the desktop gets a
        // fixed 200 ms on top so our own window is really gone from the screen.
        // Without hiding there is nothing to wait for.
        let delay = std::time::Duration::from_secs(u64::from(delay_seconds));
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        if hide {
            // KDE animates minimization. Its fading editor still appeared in
            // a real capture with the native backends' 200 ms settle.
            // L4b: five frame-checked runs each at 300/400 ms were clean.
            let settle = if self.window.wayland.load(Ordering::Relaxed) {
                std::time::Duration::from_millis(300)
            } else {
                HIDE_SETTLE
            };
            tracing::info!(
                settle_ms = settle.as_millis(),
                "waiting for the editor to leave the desktop"
            );
            std::thread::sleep(settle);
        }

        let gap = overlay_capture_gap(
            self.window.is_wayland(),
            self.last_overlay_closed.map(|closed| closed.elapsed()),
        );
        if !gap.is_zero() {
            tracing::info!(
                gap_ms = gap.as_millis(),
                "waiting for the previous overlay to leave the desktop"
            );
            std::thread::sleep(gap);
        }
        let started = std::time::Instant::now();
        let waiting = self.window.is_wayland().then(|| {
            waiting_status(
                self.events.clone(),
                Arc::clone(&self.window),
                CAPTURE_WAIT_NOTICE,
            )
        });
        // Only a compositor that picks the target itself (KWin) is asked for
        // one. Windows and X11 capture everything and pick the region below
        // with `region_for`, exactly as before W3, messages included.
        let compositor_target = capabilities.compositor_targets
            && matches!(
                mode,
                CaptureMode::CurrentMonitor | CaptureMode::ActiveWindow
            );
        let target = match mode {
            CaptureMode::CurrentMonitor if compositor_target => {
                snp_ligase_platform::CaptureTarget::CurrentMonitor
            }
            CaptureMode::ActiveWindow if compositor_target => {
                snp_ligase_platform::CaptureTarget::ActiveWindow
            }
            _ => snp_ligase_platform::CaptureTarget::AllMonitors,
        };
        let captured = self
            .platform
            .capture_target(target, &self.settings.options());
        drop(waiting);
        let (capture, target_region) = match captured {
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
                    to_overlay_ms = hotkey_started.elapsed().as_secs_f64() * 1000.0,
                    "opening the region overlay"
                );
                match self.select_region(&capture, &mut token) {
                    Some(region) => region,
                    None => {
                        tracing::info!("the region selection was cancelled");
                        self.emit(UiEvent::Status("Capture cancelled.".to_owned()));
                        if was_visible && !self.window.is_wayland() {
                            self.window.show();
                        } else {
                            tracing::debug!("the window stays hidden after the cancelled capture");
                        }
                        return;
                    }
                }
            }
            CaptureMode::CurrentMonitor | CaptureMode::ActiveWindow if compositor_target => {
                target_region
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

        if token.is_some() {
            self.window.show_activated(token.take());
        }

        // Wayland may have no further frames. Copy here before publishing
        // the tab; native backends retain the UI-side retry path.
        if self.window.is_wayland() {
            let result = if self.settings.copy_new_captures {
                self.window.clipboard.clone().copy_capture_blocking(&image)
            } else {
                Ok(())
            };
            let copy_status = match &result {
                Err(error) => format!("Clipboard copy failed: {error}."),
                Ok(()) if self.settings.copy_new_captures => "Copied capture to clipboard.".into(),
                Ok(()) => "Capture ready (clipboard copy disabled).".into(),
            };
            let settings = self
                .window
                .auto_save
                .lock()
                .ok()
                .and_then(|settings| settings.clone());
            static NEXT_CAPTURE: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(1);
            let id = NEXT_CAPTURE.fetch_add(1, Ordering::Relaxed);
            let saving = settings.as_ref().is_some_and(|s| s.auto_save_new_captures);
            let save_image = saving.then(|| image.clone());
            self.emit(UiEvent::NewCaptureSaved {
                image,
                copy: result,
                id,
                saving,
            });
            self.emit(UiEvent::Status(format!(
                "{copy_status} If the editor stays minimized, its tab appears when shown."
            )));
            if let (Some(settings), Some(image)) = (settings, save_image) {
                let window = Arc::clone(&self.window);
                #[cfg(test)]
                let pause = self.save_pause.clone();
                save_capture_off_thread(id, self.events.clone(), Arc::clone(&window), move || {
                    #[cfg(test)]
                    if let Some(pause) = pause {
                        pause.wait();
                    }
                    crate::save::capture_to_disk(&settings, &image, &window.save_claims)
                        .unwrap_or_else(|| Err("Auto-save was disabled before submission".into()))
                });
            }
        } else {
            self.emit(UiEvent::NewCapture(image));
        }
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
    fn select_region(
        &mut self,
        capture: &VirtualCapture,
        token: &mut Option<String>,
    ) -> Option<RectI> {
        // Input in the overlay invalidates any older launch permission.
        *token = None;
        let selected = self.platform.select_region_activated(capture);
        self.last_overlay_closed = Some(std::time::Instant::now());
        tracing::info!("region overlay closed");
        match selected {
            Ok(snp_ligase_platform::RegionSelection {
                region: Some(region),
                activation_token,
            }) => {
                *token = activation_token;
                self.window.show_activated(token.take());
                self.last_region = Some(region);
                Some(region)
            }
            Ok(_) => None,
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
    #[test]
    fn overlay_gap_is_wayland_only_and_expires_at_300_ms() {
        use std::time::Duration as D;
        for (wayland, elapsed, expected) in [
            (true, None, 0),
            (true, Some(0), 300),
            (true, Some(115), 185),
            (true, Some(299), 1),
            (true, Some(300), 0),
            (true, Some(500), 0),
            (false, Some(0), 0),
            (false, Some(115), 0),
            (false, None, 0),
        ] {
            assert_eq!(
                super::overlay_capture_gap(wayland, elapsed.map(D::from_millis)),
                D::from_millis(expected)
            );
        }
    }

    use super::*;
    use image::RgbaImage;
    use snp_ligase_platform::{Capabilities, CaptureApi, FrameDiagnostics, MonitorInfo};

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
                    stored_pixels: snp_ligase_platform::StoredPixels::Virtual,
                    native_scale_x: 1.0,
                    native_scale_y: 1.0,
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
    fn l3d_minimizes_only_focused_wayland_editor() {
        let window = WindowGate::default();
        for wayland in [false, true] {
            window.set_wayland(wayland);
            for focused in [false, true] {
                window.record_focus(focused);
                assert_eq!(window.should_hide(), !wayland || focused);
            }
        }
    }

    #[test]
    fn l3d_controller_copies_without_frames_and_ui_receives_completed_copy() {
        let fake = crate::clipboard::FakeClipboard::default();
        fake.refuse_copies(1);
        let mut window = WindowGate::default();
        window.set_wayland(true);
        window.clipboard = crate::clipboard::ClipboardHandle::for_test(Box::new(fake.clone()));
        let window = Arc::new(window);
        let (mut worker, events) = worker(Arc::new(FakePlatform::new(Select::Cancel)), &window);
        worker.capture(CaptureMode::AllMonitors, 0);
        assert_eq!(fake.copied().len(), 1);
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, UiEvent::NewCaptureSaved { copy: Ok(()), .. }))
        );
        assert_eq!(window.hide_count(), 0);
    }

    #[test]
    fn w3_compositor_targets_capture_without_minimizing_editor() {
        for mode in [CaptureMode::CurrentMonitor, CaptureMode::ActiveWindow] {
            let window = Arc::new(WindowGate::default());
            window.set_wayland(true);
            window.focused.store(true, Ordering::Relaxed);
            let mut platform = FakePlatform::new(Select::Cancel).reporting(Capabilities {
                compositor_targets: true,
                ..Capabilities::FULL
            });
            platform.foreground = Some(RectI::new(0, 0, 2, 2));
            platform.capture_window = Some(Arc::clone(&window));
            let (mut worker, events) = worker(Arc::new(platform), &window);
            worker.settings.copy_new_captures = false;
            worker.capture(mode, 0);
            assert!(
                events
                    .try_iter()
                    .any(|e| matches!(e, UiEvent::NewCaptureSaved { .. }))
            );
            assert_eq!(window.hide_count(), 0);
        }
    }

    #[test]
    fn w3_native_target_selection_keeps_cursor_and_foreground_semantics() {
        use snp_ligase_platform::CaptureTarget;
        let mut platform = FakePlatform::new(Select::Cancel);
        platform.foreground = Some(RectI::new(0, 0, 2, 2));
        platform.cursor = (-100, 100);
        let opts = CaptureOptions::default();
        let (all, rect) = platform
            .capture_target(CaptureTarget::AllMonitors, &opts)
            .unwrap();
        assert_eq!(rect, all.bounds());
        let (capture, rect) = platform
            .capture_target(CaptureTarget::CurrentMonitor, &opts)
            .unwrap();
        assert_eq!(
            rect,
            monitor_with_cursor(&capture, Some((-100, 100)))
                .unwrap()
                .info
                .rect
        );
        let (_, rect) = platform
            .capture_target(CaptureTarget::ActiveWindow, &opts)
            .unwrap();
        assert_eq!(rect, RectI::new(0, 0, 2, 2));
        platform.foreground = None;
        assert!(
            platform
                .capture_target(CaptureTarget::ActiveWindow, &opts)
                .is_err()
        );
    }

    #[test]
    fn w3_slow_encode_delays_neither_tab_nor_next_capture() {
        let folder = std::env::temp_dir().join(format!("snp-w3-slow-{}", std::process::id()));
        let window = Arc::new(WindowGate::default());
        window.set_wayland(true);
        window.set_auto_save(crate::settings::Settings {
            auto_save_new_captures: true,
            save_folder: Some(folder.clone()),
            ..Default::default()
        });
        let (mut worker, events) = worker(Arc::new(FakePlatform::new(Select::Cancel)), &window);
        worker.settings.copy_new_captures = false;
        let pause = Arc::new(std::sync::Barrier::new(3));
        worker.save_pause = Some(Arc::clone(&pause));
        std::thread::spawn(move || {
            worker.capture(CaptureMode::AllMonitors, 0);
            worker.capture(CaptureMode::AllMonitors, 0);
        });
        // The barrier, not the clock, is what proves the point: the encodes
        // cannot finish until it is released below, so both tabs arriving
        // first means encoding did not block the controller. The timeouts only
        // keep a broken build from hanging, and are generous because the real
        // encodes after the release are two large PNGs in an unoptimised test
        // build on whatever machine runs the tests (a shared CI runner took
        // more than 3 s).
        let mut tabs = 0;
        while tabs < 2 {
            match events
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("encoding blocked the capture controller")
            {
                UiEvent::NewCaptureSaved { saving: true, .. } => tabs += 1,
                UiEvent::CaptureSaved { .. } => panic!("the fake encode finished before release"),
                _ => {}
            }
        }
        pause.wait();
        let mut saved = 0;
        while saved < 2 {
            if let UiEvent::CaptureSaved { result, .. } = events
                .recv_timeout(std::time::Duration::from_secs(120))
                .expect("a released save never finished")
            {
                result.unwrap();
                saved += 1;
            }
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn w2_auto_save_finishes_without_any_ui_frame_and_preserves_claims() {
        let folder = std::env::temp_dir().join(format!("snp-w2-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let window = Arc::new(WindowGate::default());
        window.set_wayland(true);
        window.set_auto_save(crate::settings::Settings {
            auto_save_new_captures: true,
            save_folder: Some(folder.clone()),
            filename_template: "capture".into(),
            ..crate::settings::Settings::default()
        });
        window
            .save_claims
            .lock()
            .unwrap()
            .insert(folder.join("capture.png"));
        let (mut worker, events) = worker(Arc::new(FakePlatform::new(Select::Cancel)), &window);
        worker.settings.copy_new_captures = false;
        for _ in 0..2 {
            worker.capture(CaptureMode::AllMonitors, 0);
        }
        let mut paths = Vec::new();
        while paths.len() < 2 {
            if let UiEvent::CaptureSaved {
                result: Ok(path), ..
            } = events
                // Two real PNG encodes in an unoptimised test build: generous,
                // so a slow CI runner does not fail it (it only bounds a hang).
                .recv_timeout(std::time::Duration::from_secs(120))
                .expect("an auto-save never finished")
            {
                paths.push(path);
            }
        }
        paths.sort();
        assert_eq!(
            paths,
            vec![folder.join("capture-1.png"), folder.join("capture-2.png")]
        );
        for path in paths {
            assert!(image::open(path).is_ok());
        }
        assert!(!window.is_attached());
        assert!(!folder.join("capture.png").exists());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn l3d_disabled_copy_does_not_touch_shared_clipboard() {
        let fake = crate::clipboard::FakeClipboard::default();
        let mut window = WindowGate::default();
        window.set_wayland(true);
        window.clipboard = crate::clipboard::ClipboardHandle::for_test(Box::new(fake.clone()));
        let window = Arc::new(window);
        let (mut worker, events) = worker(Arc::new(FakePlatform::new(Select::Cancel)), &window);
        worker.settings.copy_new_captures = false;
        worker.capture(CaptureMode::AllMonitors, 0);
        assert!(fake.copied().is_empty());
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, UiEvent::NewCaptureSaved { copy: Ok(()), .. }))
        );
    }

    #[test]
    fn l3d_waiting_notice_is_delayed_and_cancelled_after_fast_capture() {
        assert_eq!(CAPTURE_WAIT_NOTICE, std::time::Duration::from_secs(2));
        let (events, received) = std::sync::mpsc::channel();
        let window = Arc::new(WindowGate::default());
        // 500 ms, not the real 2 s, to keep the test quick; long enough that a
        // briefly descheduled test thread still sees "nothing yet" first.
        let done = waiting_status(
            events.clone(),
            window.clone(),
            std::time::Duration::from_millis(500),
        );
        assert!(received.try_recv().is_err());
        assert!(
            matches!(received.recv_timeout(std::time::Duration::from_secs(10)).unwrap(), UiEvent::Status(message) if message == "Waiting for the desktop to allow the screenshot…")
        );
        drop(done);
        drop(waiting_status(
            events,
            window,
            std::time::Duration::from_millis(20),
        ));
        assert!(
            received
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
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
        /// What this fake backend can do; `FULL` unless a test asks for less
        /// (LINUX_SUPPORT.md section 3, item 2).
        capabilities: Capabilities,
        foreground: Option<RectI>,
        capture_window: Option<Arc<WindowGate>>,
        cursor: (i32, i32),
    }

    impl FakePlatform {
        fn new(select: Select) -> Self {
            Self {
                captures: std::sync::atomic::AtomicUsize::new(0),
                select,
                capabilities: Capabilities::FULL,
                foreground: None,
                capture_window: None,
                cursor: (0, 0),
            }
        }

        /// The same fake, reporting `capabilities` instead of a full backend.
        fn reporting(mut self, capabilities: Capabilities) -> Self {
            self.capabilities = capabilities;
            self
        }

        fn captures(&self) -> usize {
            self.captures.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Platform for FakePlatform {
        fn capabilities(&self) -> Capabilities {
            self.capabilities
        }

        fn environment(&self) -> snp_ligase_platform::PlatformEnvironment {
            snp_ligase_platform::PlatformEnvironment::WINDOWS
        }

        fn monitors(&self) -> snp_ligase_platform::Result<Vec<MonitorInfo>> {
            Err(snp_ligase_platform::Error::Capture {
                message: "the fake platform has no monitors".to_owned(),
            })
        }

        fn capture_all(
            &self,
            _opts: &CaptureOptions,
        ) -> snp_ligase_platform::Result<VirtualCapture> {
            if let Some(window) = &self.capture_window {
                assert_eq!(
                    window.hide_count(),
                    0,
                    "minimizing changed the compositor target"
                );
            }
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
            Ok(self.foreground)
        }

        fn cursor_pos(&self) -> snp_ligase_platform::Result<(i32, i32)> {
            Ok(self.cursor)
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
                last_overlay_closed: None,
                #[cfg(test)]
                save_pause: None,
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

    /// W3 review: Windows and X11 pick Current Monitor and Active Window with
    /// `region_for`, as before W3, so their messages are unchanged. Only a
    /// compositor-target backend (KWin) is asked for the target itself.
    #[test]
    fn native_backends_keep_the_region_for_messages() {
        let warnings = |foreground: Option<RectI>| {
            let mut fake = FakePlatform::new(Select::Cancel);
            fake.foreground = foreground;
            let window = Arc::new(WindowGate::default());
            let (mut worker, events) = worker(Arc::new(fake), &window);
            worker.capture(CaptureMode::ActiveWindow, 0);
            events
                .try_iter()
                .filter_map(|event| match event {
                    UiEvent::Warning(message) => Some(message),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            warnings(None),
            vec!["No foreground window to capture.".to_owned()]
        );
        assert_eq!(
            warnings(Some(RectI::new(100_000, 100_000, 50, 50))),
            vec!["The active window is not on any monitor.".to_owned()]
        );
    }

    /// LINUX_SUPPORT.md section 3, item 2: the controller reads the
    /// capabilities before anything happens. A mode this machine cannot run is
    /// refused with the reason, before the window is hidden and before the
    /// capture API is touched.
    #[test]
    fn a_capture_without_the_capability_is_refused_before_it_starts() {
        let fake = Arc::new(FakePlatform::new(Select::Cancel).reporting(Capabilities::NONE));
        let window = Arc::new(WindowGate::default());
        let (mut worker, events) = worker(fake.clone(), &window);

        worker.capture(CaptureMode::AllMonitors, 0);

        assert_eq!(fake.captures(), 0, "the capture API was called anyway");
        assert!(
            window.is_visible(),
            "the window was hidden for a capture that can never run"
        );
        let events: Vec<UiEvent> = events.try_iter().collect();
        assert!(
            events.iter().any(|event| matches!(
                event,
                UiEvent::Warning(message) if message.contains(crate::panels::CAPTURE_UNAVAILABLE)
            )),
            "the refusal was not reported: {events:?}"
        );
    }

    /// The hide is a capability of its own (LINUX_SUPPORT.md section 2:
    /// Wayland cannot hide a window): the capture still runs, it just does not
    /// hide the window for it.
    #[test]
    fn a_platform_that_cannot_hide_is_not_asked_to_hide() {
        let capabilities = Capabilities {
            hide_main_window: false,
            ..Capabilities::FULL
        };
        let fake = Arc::new(
            FakePlatform::new(Select::Finish(RectI {
                x: 100,
                y: 100,
                w: 10,
                h: 6,
            }))
            .reporting(capabilities),
        );
        let window = Arc::new(WindowGate::default());
        let (mut worker, events) = worker(fake, &window);

        worker.capture(CaptureMode::AllMonitors, 0);

        assert_eq!(
            window.hide_count(),
            0,
            "the window was hidden on a platform that cannot hide"
        );
        assert!(
            window.is_visible(),
            "the window was not left visible after a capture on a platform that cannot hide"
        );
        let events: Vec<UiEvent> = events.try_iter().collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, UiEvent::NewCapture(_))),
            "the capture did not happen: {events:?}"
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
        assert_eq!(
            window.hide_count(),
            2,
            "the test's own hide plus the capture's hide should both be counted"
        );
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

#[cfg(test)]
#[test]
fn w2_queued_capture_drops_its_activation_permission() {
    let request = AppCommand::ActivatedCapture {
        mode: CaptureMode::Region,
        delay_seconds: 0,
        token: Some("stale".into()),
    };
    let (kept, dropped) = split_queued([request, AppCommand::ShowMainWindow]);
    assert_eq!(dropped, 1);
    assert_eq!(kept, vec![AppCommand::ShowMainWindow]);
}
