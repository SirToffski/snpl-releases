//! The overlay child itself: `snp-ligase --overlay <dir>` on a Wayland
//! session (LINUX_SUPPORT.md section 6, L3b).
//!
//! The run directory's input ([`Layout`], the frozen frames) is the shared
//! protocol with the X11 child; only the windows are Wayland's:
//!
//! - **One window per output, matched by name.** A
//!   `Fullscreen::Borderless(Some(handle))` window is opened for each layout
//!   monitor, the handle found by `MonitorHandle::name()` against the
//!   `xdg_output` name — never by position, which winit's monitor geometry
//!   gets wrong under mixed scale (S1). A layout monitor whose output cannot
//!   be found by name is a failure, not a guess: a window on the wrong
//!   output would answer positions in the wrong coordinate frame.
//! - **The windows are created in reverse layout order**, so the layout's
//!   first monitor's window is created last and lands with initial focus —
//!   S2's "focus starts on the last-created window", aimed at the monitor
//!   whose rectangle contains the parent's cursor.
//! - **No position, no inner size, no grabs.** The compositor places a
//!   fullscreen window exactly over its output (S2); Wayland has no
//!   active-input grab to arrange and no X server to ask for geometry.
//!   Events route the compositor's way, and S2 says that is enough (an
//!   implicit grab keeps a drag flowing to the pressed window past its
//!   bounds).
//! - **The pointer is mapped, not trusted**: window-local physical pixels go
//!   through [`map::virtual_from_local`], which is the compositor's own
//!   placement relation run backwards, so an extrapolated coordinate past
//!   the pressed window's edge still lands on the right virtual pixel.
//!
//! The frames are pre-scaled to their monitor's native resolution when the
//! child starts (never grown — the portal composes at the desktop's largest
//! scale, so a frame can only shrink), and painted by [`paint`] per window.

use std::io::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use super::map;
use super::paint::{self, Frame};
use crate::RectI;
use crate::downscale_rgba;
use crate::linux::x11::overlay::paint::{dim, pixels_from_rgba};
use crate::linux::x11::overlay::state::{Outcome, SelectionState};
use crate::linux::x11::overlay::{
    Answer, Layout, LayoutMonitor, VisibleMarker, read_layout, unix_ms,
};

/// How often a window that would not paint is asked again before the child
/// gives up on it. A window that never becomes paintable is a broken run,
/// and a run that keeps retrying would spin the event loop — the X11
/// child's same budget.
const MAX_PAINT_ATTEMPTS: u32 = 50;

/// Runs the overlay child and returns the process's exit code.
///
/// 0 covers both answers — the selection and the cancel — because the
/// answer itself is the JSON line on stdout. A non-zero exit code means the
/// run broke before it could answer, and stderr says why (the parent keeps
/// it in `stderr.log` and reports it with the error).
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
    let layout: Layout = read_layout(dir)?;
    let frames = read_frames(dir, &layout)?;
    let deadline = Instant::now() + Duration::from_millis(layout.timeout_ms);
    tracing::info!(
        monitors = layout.monitors.len(),
        timeout_ms = layout.timeout_ms,
        cursor = ?layout.cursor,
        wayland_display = ?std::env::var("WAYLAND_DISPLAY").ok(),
        "the Wayland overlay child starts"
    );

    let event_loop =
        EventLoop::new().map_err(|err| format!("the overlay cannot open its event loop: {err}"))?;
    event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));

    let mut child = WaylandOverlayChild {
        dir: dir.to_path_buf(),
        layout,
        frames,
        state: SelectionState::new(),
        deadline,
        context: None,
        windows: Vec::new(),
        failure: None,
        activation: None,
        activation_token: None,
        visible_written: false,
    };
    let looped = event_loop.run_app(&mut child);
    // The windows go first and the answer second: the parent's next step is
    // a drag on the desktop, and an overlay window may not still be in the
    // way when the line arrives.
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
    tracing::info!(selection = ?answer.selection, has_token = answer.activation_token.is_some(), "the overlay child answers");
    Ok(())
}

/// Every monitor's frame, pre-scaled to the monitor's native resolution.
fn read_frames(dir: &Path, layout: &Layout) -> Result<Vec<FramePixels>, String> {
    let mut frames = Vec::with_capacity(layout.monitors.len());
    for monitor in &layout.monitors {
        // The parent always writes `frame_<index>.rgba`; anything else in the
        // layout is a mistake worth refusing instead of joining (a name with
        // a `/` in it would leave the run directory).
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
        let expected = monitor.frame_w as usize * monitor.frame_h as usize * 4;
        if bytes.len() != expected {
            return Err(format!(
                "{}: a {}x{} frame is {expected} bytes of RGBA, not {}",
                path.display(),
                monitor.frame_w,
                monitor.frame_h,
                bytes.len()
            ));
        }

        // The frame from the parent is the portal image's crop, which can
        // only be *bigger* than the output's own pixels (the compose happens
        // at the desktop's largest scale). A layout whose scales say
        // otherwise is broken, and a broken layout answering clicks is worse
        // than no answer at all.
        let (native_w, native_h) = map::native_frame_size(monitor);
        if native_w > monitor.frame_w || native_h > monitor.frame_h {
            return Err(format!(
                "{}: the native size {native_w}x{native_h} is bigger than the frame {}x{}; \
                 the layout's scales disagree with its frames",
                path.display(),
                monitor.frame_w,
                monitor.frame_h,
            ));
        }
        let pixels = if native_w == monitor.frame_w && native_h == monitor.frame_h {
            bytes
        } else {
            let scaled =
                downscale_rgba(&bytes, monitor.frame_w, monitor.frame_h, native_w, native_h);
            tracing::info!(
                monitor = %monitor.name,
                from = %format!("{}x{}", monitor.frame_w, monitor.frame_h),
                to = %format!("{native_w}x{native_h}"),
                "a frame is pre-scaled to its monitor's native resolution"
            );
            scaled
        };

        let original = pixels_from_rgba(&pixels, native_w, native_h)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        let dimmed = dim(&original);
        frames.push(FramePixels {
            original,
            dimmed,
            width: native_w,
            height: native_h,
        });
    }
    Ok(frames)
}

/// One monitor's frozen frame, at its native resolution.
struct FramePixels {
    original: Vec<u32>,
    dimmed: Vec<u32>,
    width: u32,
    height: u32,
}

/// One overlay window: the monitor it covers, its native frame, and the last
/// pointer position seen in it.
struct OverlayWindow {
    surface: Surface<OwnedDisplayHandle, Window>,
    monitor: LayoutMonitor,
    frame: FramePixels,
    /// The last pointer position, in the window's own pixels.
    pointer: (f64, f64),
    presented: bool,
    attempts: u32,
}

/// The child's state between events.
struct WaylandOverlayChild {
    dir: PathBuf,
    layout: Layout,
    frames: Vec<FramePixels>,
    state: SelectionState,
    deadline: Instant,
    context: Option<Context<OwnedDisplayHandle>>,
    windows: Vec<OverlayWindow>,
    /// The reason this run is over before it decided anything.
    failure: Option<String>,
    activation: Option<crate::linux::wayland::activation::OverlayActivation>,
    activation_token: Option<String>,
    /// Whether the first present has already been reported to the parent.
    visible_written: bool,
}

impl WaylandOverlayChild {
    /// The desktop's union in virtual pixels, computed fresh from the
    /// layout: where mapped pointers clamp.
    fn union(&self) -> RectI {
        map::desktop_union(&self.layout.monitors)
    }

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

    /// Asks every window for a repaint.
    ///
    /// The selection can cover more than one monitor, so a move repaints all
    /// of them; the X11 and Windows overlays do the same.
    fn redraw(&self) {
        for window in &self.windows {
            window.surface.window().request_redraw();
        }
    }

    /// Maps a window-local position to a virtual one, on this window's
    /// monitor, clamped into the desktop's union.
    fn virtual_pos(&self, monitor: &LayoutMonitor, local: (f64, f64)) -> (i32, i32) {
        map::virtual_from_local(local, monitor, self.union())
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
            let inner = window.surface.window().inner_size();
            let (Some(width), Some(height)) = (
                NonZeroU32::new(inner.width.max(1)),
                NonZeroU32::new(inner.height.max(1)),
            ) else {
                return;
            };

            // The label belongs to the window that owns the selection's
            // top-left corner, and names the size the answer comes out at.
            let label = selection
                .filter(|rect| window.monitor.rect().contains(rect.x, rect.y))
                .map(|rect| map::label_text(rect, &self.layout.monitors));

            let result = (|| -> Result<(), softbuffer::SoftBufferError> {
                window.surface.resize(width, height)?;
                let mut buffer = window.surface.buffer_mut()?;
                paint::paint_window_physical(
                    &mut buffer,
                    (width.get(), height.get()),
                    &Frame {
                        original: &window.frame.original,
                        dimmed: &window.frame.dimmed,
                        width: window.frame.width,
                        height: window.frame.height,
                    },
                    selection,
                    &window.monitor,
                    label.as_deref(),
                );
                buffer.present()
            })();

            match result {
                Ok(()) => {
                    let first = !window.presented;
                    window.presented = true;
                    window.attempts = 0;
                    if first {
                        tracing::info!(
                            monitor = %window.monitor.name,
                            window_virtual = ?map::window_virtual(
                                (inner.width, inner.height),
                                &window.monitor,
                            ),
                            native_scale = %format!("{:.4}", window.monitor.native_scale_x),
                            "the first window is on screen, at its output's own resolution"
                        );
                    }
                    first
                }
                Err(err) => {
                    window.attempts += 1;
                    if window.attempts <= MAX_PAINT_ATTEMPTS {
                        tracing::warn!(
                            monitor = %window.monitor.name,
                            attempt = window.attempts,
                            error = %err,
                            "an overlay window could not be painted; asking again"
                        );
                        window.surface.window().request_redraw();
                    } else {
                        tracing::error!(
                            monitor = %window.monitor.name,
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
                tracing::info!(visible_ms, "the overlay's first frame is on screen");
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

    /// A pointer move: map, feed, and repaint everything while dragging.
    fn pointer_moved(&mut self, index: usize) {
        let Some(window) = self.windows.get(index) else {
            return;
        };
        let virtual_pos = self.virtual_pos(&window.monitor, window.pointer);
        // Only a move during a drag changes any pixel.
        let dragging = self.state.selection().is_some();
        self.state.pointer_moved(virtual_pos.0, virtual_pos.1);
        if dragging {
            self.redraw();
        }
    }

    /// The user decided: Esc, a right click, a finished drag or the timeout.
    fn decide(&mut self, event_loop: &ActiveEventLoop) {
        match self.state.outcome() {
            Outcome::Selected(rect) => tracing::info!(?rect, "the overlay ended with a selection"),
            Outcome::Cancelled => tracing::info!("the overlay was cancelled"),
            Outcome::Waiting => tracing::debug!("the overlay is still waiting"),
        }
        event_loop.exit();
    }

    /// Closes every window, then says what happened.
    ///
    /// The windows are dropped — which destroys them — so the desktop is
    /// clear before the answer reaches the parent.
    fn close(&mut self) -> Result<Answer, String> {
        self.activation = None;
        self.windows.clear();
        self.context = None;
        if let Some(failure) = self.failure.take() {
            return Err(failure);
        }
        match self.state.outcome() {
            Outcome::Selected(rect) => {
                let mut answer = Answer::selected(rect);
                answer.activation_token = self.activation_token.take();
                Ok(answer)
            }
            Outcome::Cancelled => Ok(Answer::cancelled()),
            Outcome::Waiting => Err(
                "the overlay ended without a selection and without a cancel; \
                 the event loop stopped for no reason it can name"
                    .to_owned(),
            ),
        }
    }
}

impl ApplicationHandler for WaylandOverlayChild {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.context.is_some() {
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

        // Reverse layout order: the layout's first monitor's window is
        // created last, which is the one the compositor gives focus (S2).
        let monitors: Vec<LayoutMonitor> = self.layout.monitors.clone();
        let frames = std::mem::take(&mut self.frames);
        let mut windows = Vec::with_capacity(monitors.len());
        for (monitor, frame) in windows_in_focus_order(monitors, frames) {
            let handle = event_loop
                .available_monitors()
                .find(|available| available.name().as_deref() == Some(monitor.name.as_str()));
            let Some(handle) = handle else {
                return self.fail(
                    event_loop,
                    format!(
                        "no output named {:?} exists; the overlay matches windows by output name",
                        monitor.name
                    ),
                );
            };
            let attributes = Window::default_attributes()
                .with_title("snp-ligase overlay")
                .with_decorations(false)
                .with_fullscreen(Some(Fullscreen::Borderless(Some(handle))));
            let window = match event_loop.create_window(attributes) {
                Ok(window) => window,
                Err(err) => {
                    return self.fail(
                        event_loop,
                        format!(
                            "the overlay window on {} could not be opened: {err}",
                            monitor.name
                        ),
                    );
                }
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
            tracing::info!(monitor = %monitor.name, rect = ?monitor.rect(), "an overlay window is on its output");

            // Winit tells us where the pointer is on the first enter; before
            // that anchor, a press falls on the window's middle.
            let pointer = (frame.width as f64 / 2.0, frame.height as f64 / 2.0);
            windows.push(OverlayWindow {
                surface,
                monitor,
                frame,
                pointer,
                presented: false,
                attempts: 0,
            });
        }
        self.context = Some(context);
        if let Some(window) = windows.first() {
            match crate::linux::wayland::activation::OverlayActivation::capture(
                window.surface.window(),
            ) {
                Ok(activation) => self.activation = Some(activation),
                Err(error) => tracing::warn!(%error, "overlay activation setup unavailable"),
            }
        }
        self.windows = windows;
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

            // The compositor sized the window (the request may still be in
            // flight from the fullscreen match): repaint at its real size.
            WindowEvent::Resized(_) => {
                if let Some(window) = self.windows.get(index) {
                    window.surface.window().request_redraw();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if let Some(window) = self.windows.get_mut(index) {
                    window.pointer = (position.x, position.y);
                }
                self.pointer_moved(index);
            }

            WindowEvent::MouseInput { state, button, .. } => {
                // The press needs this window's own monitor: during the
                // implicit grab the pressed window keeps being told where the
                // pointer is, so its coordinates are the frame to map.
                let Some(window) = self.windows.get(index) else {
                    return;
                };
                let virtual_pos = self.virtual_pos(&window.monitor, window.pointer);
                match (button, state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        self.state.press(virtual_pos.0, virtual_pos.1);
                        self.redraw();
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        if self.state.release(virtual_pos.0, virtual_pos.1) {
                            if let Some(activation) = &mut self.activation {
                                match activation.finish(self.windows[index].surface.window()) {
                                    Ok(token) => self.activation_token = Some(token),
                                    Err(error) => {
                                        tracing::warn!(%error, "fresh overlay activation unavailable")
                                    }
                                }
                            }
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
                let escape = matches!(event.logical_key, Key::Named(NamedKey::Escape))
                    || event.physical_key == PhysicalKey::Code(KeyCode::Escape);
                // Repeats count: once the outcome is decided, the state
                // machine ignores everything further anyway.
                if escape && event.state == ElementState::Pressed {
                    self.state.escape();
                    self.decide(event_loop);
                }
            }

            // The compositor asked us to close: the user is done with the
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
        if self.state.outcome() != Outcome::Waiting {
            return;
        }
        if Instant::now() >= self.deadline {
            // The safety timeout cancels, exactly like Esc does.
            tracing::warn!(
                timeout_ms = self.layout.timeout_ms,
                "the overlay reached its deadline; cancelling"
            );
            self.state.timed_out();
            self.decide(event_loop);
            return;
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.deadline));
    }
}

/// Create the focused window last, keeping every frame with its own output.
fn windows_in_focus_order(
    monitors: Vec<LayoutMonitor>,
    frames: Vec<FramePixels>,
) -> impl Iterator<Item = (LayoutMonitor, FramePixels)> {
    monitors.into_iter().zip(frames).rev()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reversed_window_order_keeps_each_outputs_own_pixels() {
        let monitor = |name: &str| LayoutMonitor {
            name: name.into(),
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            frame_w: 1,
            frame_h: 1,
            file: String::new(),
            primary: false,
            native_scale_x: 1.0,
            native_scale_y: 1.0,
        };
        let frame = |pixel| FramePixels {
            original: vec![pixel],
            dimmed: vec![pixel],
            width: 1,
            height: 1,
        };
        let rows: Vec<_> = windows_in_focus_order(
            vec![monitor("left"), monitor("right")],
            vec![frame(0x112233), frame(0xaabbcc)],
        )
        .map(|(monitor, frame)| (monitor.name, frame.original[0]))
        .collect();
        assert_eq!(
            rows,
            [("right".into(), 0xaabbcc), ("left".into(), 0x112233)]
        );
    }
}
