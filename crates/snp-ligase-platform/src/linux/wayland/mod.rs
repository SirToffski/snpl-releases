//! The Wayland backend — KDE native pixels with a Screenshot portal fallback, an `xdg_output`
//! layout and one overlay window per output (LINUX_SUPPORT.md section 6,
//! the backend design).
//!
//! The pieces:
//!
//! - [`layout`]: the compositor's outputs, probed on a short-lived Wayland
//!   connection, and the pure build that turns them plus the portal image's
//!   size into the virtual layout the rest of the program works on.
//! - [`portal`]: the Screenshot portal behind a 120 s capture deadline, one composed
//!   image of the whole desktop.
//! - [`overlay`]: the region overlay's own child process — the X11 design
//!   ported to Wayland windows. The parent side is shared with X11
//!   (run directory, answer line), so only this module is Wayland-specific.
//!
//! The virtual space is the portal image's pixel space: monitor rects are
//! positions inside that image, each [`crate::MonitorFrame`] is that crop,
//! and a selection inside one output still comes out at the output's own
//! physical resolution through the frame's native scale (`crop`'s one-output
//! rule).

pub mod activation;
mod kwin;
pub mod layout;
pub mod overlay;
pub mod portal;
mod roundtrip;
pub mod shortcuts;

use std::time::Duration;

use image::RgbaImage;

use crate::{
    Capabilities, CaptureOptions, Error, HotkeySupport, MonitorFrame, MonitorInfo, Platform,
    PlatformEnvironment, RectI, Result, VirtualCapture,
};

use portal::PortalSession;

/// What the Wayland backend can really do today (LINUX_SUPPORT.md section 6).
///
/// The portal gives silent capture and the overlay windows work, but the
/// session cursor, the active window and the tray are out of reach for the
/// backend as designed. Global hotkeys belong to the desktop's GlobalShortcuts
/// portal, and hiding uses minimization. `capture_reason` is set per instance in
/// [`WaylandPortalPlatform::capabilities`] when the portal is missing, so a
/// disabled menu row can name it (SPEC 8.3).
pub const CAPABILITIES: Capabilities = Capabilities {
    silent_capture: true,
    overlay: true,
    active_window_rect: false,
    cursor_position: false,
    capture_cursor: false,
    compositor_targets: false,
    hide_main_window: true,
    global_hotkeys: HotkeySupport::Portal,
    tray: false,
    capture_reason: None,
};

/// How long the Wayland overlay child may take to answer.
///
/// Half of X11's [`super::x11::overlay::TIMEOUT`]: on Wayland a stuck overlay
/// is invisible to the trapping key machinery that would cancel it anyway,
/// and the parent has no X server to raise the main window over it, so it
/// clears itself sooner rather than covering the desktop longer.
pub const SELECTION_TIMEOUT: Duration = Duration::from_secs(60);

/// The Wayland platform: KDE ScreenShot2 when allowed, Screenshot portal otherwise.
///
/// Native KDE eligibility and the startup portal probe are independent.
/// A transient portal failure is retried by a fresh connection on capture.
/// The layout is re-probed per capture because monitors move.
#[derive(Debug)]
pub struct WaylandPortalPlatform {
    portal: PortalSession,
    native_kde: bool,
}

impl WaylandPortalPlatform {
    /// Probes for the Screenshot portal.
    ///
    /// Never fails: without the portal, KDE still attempts its native service;
    /// other desktops name a definitively missing portal in the disabled menu.
    #[must_use]
    pub fn new() -> Self {
        let portal = PortalSession::probe();
        let native_kde = kwin::preferred(super::SessionEnv::read().current_desktop.as_deref());
        if portal.missing_reason().is_some() {
            tracing::warn!(
                native_kde,
                "no Screenshot portal; native KDE eligibility is independent"
            );
        } else {
            tracing::info!(
                version = portal.version(),
                native_kde_preferred = native_kde,
                "Wayland capture uses native KWin when allowed on KDE, Screenshot portal otherwise"
            );
        }
        Self { portal, native_kde }
    }

    /// What this session can do, with the portal's absence named when it is
    /// absent.
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        match self.portal.missing_reason() {
            Some(reason) if !self.native_kde => Capabilities {
                silent_capture: false,
                capture_reason: Some(reason),
                ..CAPABILITIES
            },
            _ => Capabilities {
                capture_cursor: self.native_kde,
                compositor_targets: self.native_kde,
                ..CAPABILITIES
            },
        }
    }

    fn capture_portal(
        &self,
        outputs: &[layout::ProbedOutput],
        started: std::time::Instant,
    ) -> Result<VirtualCapture> {
        let portal_frame = self.portal.capture(started)?;
        let image = &portal_frame.image;
        let layout = layout::build(outputs, Some((image.width(), image.height())))?;
        tracing::info!(
            scale_x = layout.scale_x,
            scale_y = layout.scale_y,
            monitors = layout.monitors.len(),
            image = %format!("{}x{}", image.width(), image.height()),
            "the composed portal image belongs to this layout"
        );

        let mut frames = Vec::with_capacity(layout.monitors.len());
        for monitor in &layout.monitors {
            let frame_image = monitor_image(image, monitor.rect);
            tracing::info!(
                monitor = %monitor.name,
                rect = %format!(
                    "{} {} {}x{}",
                    monitor.rect.x, monitor.rect.y, monitor.rect.w, monitor.rect.h
                ),
                image = %format!("{}x{}", frame_image.width(), frame_image.height()),
                native_scale = %format!("{:.4}", monitor.native_scale_x),
                scale_factor = monitor.scale_factor,
                "monitor frame cut from the portal image"
            );
            frames.push(MonitorFrame {
                diagnostics: crate::FrameDiagnostics {
                    width: frame_image.width(),
                    height: frame_image.height(),
                    ..portal_frame.diagnostics()
                },
                image: frame_image,
                info: monitor_info(monitor),
                stored_pixels: crate::StoredPixels::Virtual,
                native_scale_x: monitor.native_scale_x,
                native_scale_y: monitor.native_scale_y,
            });
        }
        Ok(VirtualCapture { frames })
    }
    /// Probes the session's layout and logs its warnings.
    ///
    /// The layout is re-probed per capture because monitors move; the portal
    /// image's size fixes the measured scale `S` when there is one.
    fn current_layout(&self, image: Option<(u32, u32)>) -> Result<layout::VirtualLayout> {
        let probe = layout::probe()?;
        let layout = layout::build(&probe, image)?;
        for warning in &layout.warnings {
            tracing::warn!("{warning}");
        }
        Ok(layout)
    }
}

impl Default for WaylandPortalPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl Platform for WaylandPortalPlatform {
    fn capabilities(&self) -> Capabilities {
        self.capabilities()
    }

    fn environment(&self) -> PlatformEnvironment {
        // The backend's caller in `linux` names it; nothing Wayland-specific
        // beyond the session type the session manager already reports.
        PlatformEnvironment {
            backend: "wayland-portal",
            session_type: Some("wayland".to_owned()),
            desktop: super::SessionEnv::read().current_desktop,
        }
    }

    fn monitors(&self) -> Result<Vec<MonitorInfo>> {
        let layout = self.current_layout(None)?;
        Ok(layout.monitors.iter().map(monitor_info).collect())
    }

    /// One native frame per output on KDE when allowed; composed portal crops otherwise.
    ///
    /// Frames are the *image-space* crops: a 100% output next to a 150% one
    /// arrives upscaled by the composition, and its frame's native scale says
    /// so, so [`crate::VirtualCapture::crop`] can still hand back the output's
    /// own pixels (LINUX_SUPPORT.md section 6).
    fn capture_target(
        &self,
        target: crate::CaptureTarget,
        opts: &CaptureOptions,
    ) -> Result<(VirtualCapture, RectI)> {
        if target == crate::CaptureTarget::AllMonitors {
            let capture = self.capture_all(opts)?;
            let bounds = capture.bounds();
            return Ok((capture, bounds));
        }
        if !self.native_kde {
            return Err(Error::Unsupported {
                message: "This Wayland desktop has no native screen/window capture target.".into(),
            });
        }
        let include_cursor = opts.include_cursor;
        capture_with_deadline(portal::PORTAL_DEADLINE, move |started, _| {
            kwin::capture_target(target, started, include_cursor)
        })
    }

    fn capture_all(&self, opts: &CaptureOptions) -> Result<VirtualCapture> {
        let portal = self.portal.clone();
        let prefer_native = self.native_kde;
        let include_cursor = opts.include_cursor;
        capture_with_deadline(portal::PORTAL_DEADLINE, move |started, first_error| {
            let outputs = layout::probe()?;
            let native_layout = layout::build(&outputs, None)?;
            if started.elapsed() >= portal::PORTAL_DEADLINE {
                return Err(Error::Capture {
                    message: "the capture budget was exhausted during layout discovery".into(),
                });
            }
            kwin::with_fallback(
                prefer_native,
                || {
                    kwin::capture(&native_layout, started, include_cursor).inspect_err(|error| {
                        if let Ok(mut first) = first_error.lock() {
                            *first = Some(error.to_string());
                        }
                    })
                },
                || {
                    if started.elapsed() >= portal::PORTAL_DEADLINE {
                        return Err(Error::Capture {
                            message: "the capture budget was exhausted before fallback".into(),
                        });
                    }
                    if include_cursor {
                        tracing::warn!(
                            "Screenshot portal cannot include the cursor; capturing without it"
                        );
                    }
                    Self {
                        portal,
                        native_kde: prefer_native,
                    }
                    .capture_portal(&outputs, started)
                },
            )
        })
    }

    /// The region overlay: the parent plumbing of the X11 overlay (run
    /// directory, child process, answer), whose Wayland side is
    /// [`overlay`] (a child process of its own, ported to Wayland windows).
    ///
    /// The pointer position the child starts on is the desktop union's
    /// centre: the cursor itself is unreadable on Wayland (`cursor_position`
    /// off), and the child focuses the window containing this point.
    fn select_region(&self, capture: &VirtualCapture) -> Result<Option<RectI>> {
        let visible = capture.bounds();
        let cursor = (visible.x + visible.w / 2, visible.y + visible.h / 2);
        super::x11::overlay::select_region_with_timeout(capture, cursor, SELECTION_TIMEOUT)
    }

    fn select_region_activated(&self, capture: &VirtualCapture) -> Result<crate::RegionSelection> {
        let visible = capture.bounds();
        let cursor = (visible.x + visible.w / 2, visible.y + visible.h / 2);
        super::x11::overlay::select_region_activated(capture, cursor, SELECTION_TIMEOUT)
    }

    fn foreground_window_rect(&self) -> Result<Option<RectI>> {
        Err(Error::Unsupported {
            message: "the foreground window's rectangle is not available on Wayland".to_owned(),
        })
    }

    fn cursor_pos(&self) -> Result<(i32, i32)> {
        Err(Error::Unsupported {
            message: "the cursor position is not available on Wayland".to_owned(),
        })
    }
}

/// A single receive deadline encloses layout, native work and portal fallback.
/// Remember the native failure even if the fallback never returns.
fn capture_with_deadline<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce(std::time::Instant, std::sync::Arc<std::sync::Mutex<Option<String>>>) -> Result<T>
    + Send
    + 'static,
) -> Result<T> {
    let first = std::sync::Arc::new(std::sync::Mutex::new(None));
    let worker_first = first.clone();
    portal::portal_thread_with_deadline(
        "capturing the Wayland desktop (layout, native, fallback)",
        budget,
        move |started| work(started, worker_first),
    )
    .map_err(
        |error| match first.lock().ok().and_then(|first| first.clone()) {
            Some(first) if !error.to_string().contains(&first) => Error::Capture {
                message: format!("{first}; capture failed: {error}"),
            },
            _ => error,
        },
    )
}

/// The [`MonitorInfo`] for one virtual monitor.
///
/// Wayland has no primary concept to ask about: none of these is reported
/// primary (noted in DECISIONS.md).
fn monitor_info(monitor: &layout::VirtualMonitor) -> MonitorInfo {
    MonitorInfo {
        id: monitor.name.clone(),
        rect: monitor.rect,
        scale_factor: monitor.scale_factor,
        hdr_active: false,
        sdr_white_nits: None,
        is_primary: false,
    }
}

/// Cuts a monitor's image-space crop out of the composed portal image.
///
/// The layout's rects should sit inside the image; a monitor that reaches
/// past it shrank (`build` only warns) and is clamped here with transparent
/// pixels filling the rest, degrading instead of failing a capture.
fn monitor_image(image: &RgbaImage, rect: RectI) -> RgbaImage {
    let width = image.width() as i64;
    let height = image.height() as i64;
    let left = (rect.x as i64).clamp(0, width);
    let top = (rect.y as i64).clamp(0, height);
    let right = (rect.x as i64 + rect.w as i64).clamp(0, width);
    let bottom = (rect.y as i64 + rect.h as i64).clamp(0, height);
    let w = (right - left).max(0) as u32;
    let h = (bottom - top).max(0) as u32;
    if w != rect.w as u32 || h != rect.h as u32 {
        tracing::warn!(
            wanted = %format!("{}x{}", rect.w, rect.h),
            got = %format!("{}x{}", w, h),
            at = %format!("{},{}", left, top),
            "a monitor's rect lies partly or wholly outside the portal image; clamped"
        );
    }
    let bytes = crate::copy_region(image, left as u32, top as u32, w, h);
    RgbaImage::from_raw(w, h, bytes).unwrap_or_else(|| RgbaImage::new(w, h))
}

#[cfg(test)]
mod w1_tests {
    use super::*;
    use std::{
        os::unix::net::{UnixListener, UnixStream},
        time::Instant,
    };

    #[test]
    fn w1_stalled_compositor_layout_and_fallback_share_one_budget() {
        let path = std::env::temp_dir().join(format!("snp-w1-stall-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        std::fs::remove_file(path).unwrap();
        let (release, stop) = std::sync::mpsc::channel::<()>();
        // External watchdog bounds even a deliberately sabotaged deadline.
        let peer = std::thread::spawn(move || {
            let _ = stop.recv_timeout(Duration::from_millis(700));
            drop(server);
        });
        let (finished, worker_finished) = std::sync::mpsc::channel();
        let started = Instant::now();
        let result = capture_with_deadline(Duration::from_millis(50), move |_, first_error| {
            let result = kwin::with_fallback(
                true,
                || {
                    *first_error.lock().unwrap() = Some("native first failure".into());
                    Err(Error::Capture {
                        message: "native first failure".into(),
                    })
                },
                || {
                    let connection = wayland_client::Connection::from_socket(client).unwrap();
                    layout::probe_connection(connection)
                },
            );
            let _ = finished.send(());
            result
        });
        let elapsed = started.elapsed();
        drop(release);
        peer.join().unwrap();
        worker_finished
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let error = result.unwrap_err().to_string();
        assert!(error.contains("native first failure"), "{error}");
        assert!(error.contains("did not answer"), "{error}");
        assert!(elapsed < Duration::from_millis(400), "{elapsed:?}");
        assert_eq!(portal::PORTAL_DEADLINE, Duration::from_secs(120));
    }

    #[test]
    fn w1_native_failure_is_kept_when_portal_also_fails() {
        let error = kwin::with_fallback(
            true,
            || {
                Err::<(), _>(Error::Capture {
                    message: "native refused".into(),
                })
            },
            || {
                Err(Error::Capture {
                    message: "portal refused".into(),
                })
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("native refused"), "{error}");
        assert!(error.contains("portal refused"), "{error}");
    }

    #[test]
    fn w1_repeat_selection_keeps_pixels_across_native_portal_fallback() {
        for (x, y) in [(-16, 0), (0, -4), (3, 7)] {
            let outputs: Vec<_> = [(x, y), (x + 16, y + 4)]
                .iter()
                .enumerate()
                .map(|(i, &(x, y))| layout::ProbedOutput {
                    xdg_name: format!("output-{i}"),
                    logical_x: x,
                    logical_y: y,
                    logical_w: 16,
                    logical_h: 12,
                    physical_w: 16,
                    physical_h: 12,
                    ..Default::default()
                })
                .collect();
            let desktop =
                RgbaImage::from_fn(32, 16, |x, y| image::Rgba([x as u8, y as u8, 90, 255]));
            let make = |portal: bool| {
                let layout =
                    layout::build(&outputs, portal.then_some(desktop.dimensions())).unwrap();
                VirtualCapture {
                    frames: layout
                        .monitors
                        .iter()
                        .map(|monitor| MonitorFrame {
                            info: monitor_info(monitor),
                            image: monitor_image(&desktop, monitor.rect),
                            diagnostics: portal::PortalFrame {
                                image: RgbaImage::new(0, 0),
                                elapsed: Duration::ZERO,
                            }
                            .diagnostics(),
                            stored_pixels: if portal {
                                crate::StoredPixels::Virtual
                            } else {
                                crate::StoredPixels::Native
                            },
                            native_scale_x: 1.0,
                            native_scale_y: 1.0,
                        })
                        .collect(),
                }
            };
            let native = make(false);
            let fallback = make(true);
            for selection in [
                RectI::new(2, 2, 8, 6),
                RectI::new(18, 6, 8, 6),
                RectI::new(12, 5, 10, 6),
            ] {
                let expected = image::imageops::crop_imm(
                    &desktop,
                    selection.x as u32,
                    selection.y as u32,
                    selection.w as u32,
                    selection.h as u32,
                )
                .to_image();
                assert_eq!(native.crop(selection), expected);
                assert_eq!(
                    fallback.crop(selection),
                    expected,
                    "Repeat uses the same saved rectangle"
                );
            }
        }
    }
}
