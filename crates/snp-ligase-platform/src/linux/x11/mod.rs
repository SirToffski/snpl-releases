//! The X11 backend (LINUX_SUPPORT.md section 4, phase L1a and L1b).
//!
//! What this adds over the stub: the monitor list from RandR 1.5, a real
//! capture (MIT-SHM, with plain `GetImage` as the fallback), the cursor
//! position, the active window's rectangle, the region overlay as a child
//! process ([`overlay`]) and — with L1c — the keyboard state the paste path
//! and the hotkey recorder read ([`keys`]). The hotkeys themselves are the
//! platform-independent `global-hotkey` manager in the app.
//!
//! X11's model is the same as Windows': one shared pixel space across every
//! monitor (LINUX_SUPPORT.md section 2), so [`crate::RectI`] means the same
//! thing here, and `scale_factor` — from `Xft.dpi` — exists for logging only.
//!
//! Everything the backend learns about the server at startup is logged once
//! (LINUX_SUPPORT.md section 10): the X11 bug reports are read from those
//! lines, not from a debugger.

mod capture;
mod monitors;
pub mod overlay;
mod pixels;
mod shm;
mod window_info;

/// The X11 key-state poll (`XQueryKeymap`) the app's paste and hotkey recorder
/// use (LINUX_SUPPORT.md section 4, phase L1c).
pub mod keys;

use x11rb::connection::Connection as _;
use x11rb::protocol::shm as mit_shm;
use x11rb::protocol::{randr, xproto, xtest};
use x11rb::rust_connection::RustConnection;

use crate::{
    Capabilities, CaptureOptions, HotkeySupport, MonitorInfo, RectI, Result, VirtualCapture,
};

/// What the X11 backend can really do after L1c (LINUX_SUPPORT.md section 4).
///
/// `overlay` is L1b's child-process overlay; `hide_main_window` is the
/// controller's wiring, which is platform-neutral, and `global_hotkeys` is
/// `global-hotkey`'s X11 backend, whose handler thread the app's binding table
/// now matches (L1c). The key state behind Ctrl+V and the hotkey recorder is
/// [`keys`], also L1c.
pub const CAPABILITIES: Capabilities = Capabilities {
    silent_capture: true,
    overlay: true,
    active_window_rect: true,
    cursor_position: true,
    capture_cursor: false,
    compositor_targets: false,
    hide_main_window: true,
    global_hotkeys: HotkeySupport::Native,
    tray: false,
    capture_reason: None,
};

/// The X11 platform: one connection, used by every call.
///
/// The connection is made once at startup, because almost everything else
/// needs it and a session without a usable one has a stub in its place
/// ([`crate::linux::LinuxPlatform`]).
#[derive(Debug)]
pub struct X11Platform {
    connection: X11Connection,
}

impl X11Platform {
    /// Connects to the X server `$DISPLAY` names and reads what this backend
    /// needs to know about it.
    ///
    /// # Errors
    ///
    /// The display cannot be connected to, or the root window's visual has no
    /// usable pixel format. The caller keeps the message and runs the stub
    /// instead of refusing to start.
    pub fn new() -> std::result::Result<Self, String> {
        Ok(Self {
            connection: X11Connection::connect()?,
        })
    }

    /// What this backend can do (see [`CAPABILITIES`]).
    #[must_use]
    pub const fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    /// Fresh monitor list, enumerated now rather than cached (SPEC 5.3).
    pub fn monitors(&self) -> Result<Vec<MonitorInfo>> {
        monitors::enumerate(&self.connection)
    }

    /// One frame from every monitor.
    pub fn capture_all(&self, opts: &CaptureOptions) -> Result<VirtualCapture> {
        capture::capture_all(&self.connection, opts)
    }

    /// The region overlay (LINUX_SUPPORT.md section 4, phase L1b): a child
    /// process, `snp-ligase --overlay <dir>`, over the frozen frames in
    /// `capture`.
    ///
    /// The pointer's position decides which monitor's window takes the
    /// keyboard focus, the way the Win32 overlay brings the window under the
    /// cursor forward. Failing to read it is not fatal: the primary monitor
    /// takes the focus instead and the whole desktop is covered either way.
    pub fn select_region(&self, capture: &VirtualCapture) -> Result<Option<RectI>> {
        let cursor = self.cursor_pos().unwrap_or_else(|err| {
            let fallback = capture
                .frames
                .iter()
                .find(|frame| frame.info.is_primary)
                .or_else(|| capture.frames.first())
                .map_or((0, 0), |frame| {
                    (
                        frame.info.rect.x + frame.info.rect.w / 2,
                        frame.info.rect.y + frame.info.rect.h / 2,
                    )
                });
            tracing::warn!(
                error = %err,
                fallback = ?fallback,
                "the cursor position could not be read; the overlay focuses the primary monitor"
            );
            fallback
        });
        overlay::select_region(capture, cursor)
    }

    /// The active window's rectangle, from `_NET_ACTIVE_WINDOW` and the
    /// frame extents (SPEC section 5.7).
    pub fn foreground_window_rect(&self) -> Result<Option<RectI>> {
        window_info::foreground_window_rect(&self.connection)
    }

    /// Where the pointer is, in pixels of the shared X11 screen space.
    pub fn cursor_pos(&self) -> Result<(i32, i32)> {
        window_info::cursor_pos(&self.connection)
    }
}

/// The connection and the facts about the server this backend needs.
#[derive(Debug)]
pub(crate) struct X11Connection {
    pub(crate) conn: RustConnection,
    /// The screen `$DISPLAY` names (usually 0; `:0.1` picks another).
    pub(crate) screen: usize,
    /// That screen's root window: the drawable every capture reads.
    pub(crate) root: xproto::Window,
    /// The root window's size, which bounds every capture rectangle.
    pub(crate) root_width: u16,
    pub(crate) root_height: u16,
    /// The visual's pixel layout: channel masks, bits per pixel, byte order
    /// and row padding ([`pixels`]).
    pub(crate) layout: pixels::PixelLayout,
    /// MIT-SHM version, `None` when the server does not offer it.
    pub(crate) shm_version: Option<(u16, u16)>,
    /// RandR version, `None` when it is missing or older than 1.5 (monitor
    /// enumeration then falls back to the whole screen).
    pub(crate) randr_version: Option<(u32, u32)>,
    /// `Xft.dpi` / 96, for logs only (LINUX_SUPPORT.md section 4).
    pub(crate) scale_factor: f32,
    /// Interned `_NET_ACTIVE_WINDOW` (the root property naming the active
    /// window).
    pub(crate) net_active_window: xproto::Atom,
    /// Interned `_NET_FRAME_EXTENTS` (xfwm4 and other window managers).
    pub(crate) net_frame_extents: xproto::Atom,
    /// Interned `_GTK_FRAME_EXTENTS` (client-side decorations).
    pub(crate) gtk_frame_extents: xproto::Atom,
}

impl X11Connection {
    /// Connects to `$DISPLAY` and gathers the facts the backend logs.
    fn connect() -> std::result::Result<Self, String> {
        let (conn, screen) = x11rb::connect(None)
            .map_err(|err| format!("could not connect to the X display: {err}"))?;

        let setup = conn.setup();
        let display_name = std::env::var("DISPLAY").unwrap_or_else(|_| "-".to_owned());
        let vendor = String::from_utf8_lossy(&setup.vendor).into_owned();

        let screen_info = setup.roots.get(screen).ok_or_else(|| {
            format!(
                "the display {display_name} has no screen {screen} (X screens: {})",
                setup.roots.len()
            )
        })?;
        let root = screen_info.root;
        let depth = screen_info.root_depth;

        let visual = screen_info
            .allowed_depths
            .iter()
            .filter(|allowed| allowed.depth == depth)
            .flat_map(|allowed| &allowed.visuals)
            .find(|visual| visual.visual_id == screen_info.root_visual)
            .ok_or_else(|| {
                format!(
                    "the root window's visual {:#x} (depth {depth}) is not in the screen's \
                     depth list",
                    screen_info.root_visual
                )
            })?;
        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .ok_or_else(|| format!("the X server lists no pixmap format for depth {depth}"))?;

        let layout = pixels::PixelLayout {
            depth,
            bits_per_pixel: format.bits_per_pixel,
            scanline_pad: format.scanline_pad,
            byte_order: setup.image_byte_order,
            class: visual.class,
            red_mask: visual.red_mask,
            green_mask: visual.green_mask,
            blue_mask: visual.blue_mask,
        };

        let shm_version = shm_version(&conn);
        let randr_version = randr_version(&conn);
        let xtest_version = xtest::get_version(&conn, 2, 2)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| (reply.major_version, reply.minor_version));
        let scale_factor =
            pixels::scale_from_dpi(monitors::query_xft_dpi(&conn, root).unwrap_or(None));

        let net_active_window = intern(&conn, b"_NET_ACTIVE_WINDOW")?;
        let net_frame_extents = intern(&conn, b"_NET_FRAME_EXTENTS")?;
        let gtk_frame_extents = intern(&conn, b"_GTK_FRAME_EXTENTS")?;

        let root_width = screen_info.width_in_pixels;
        let root_height = screen_info.height_in_pixels;

        // LINUX_SUPPORT.md section 10: the session, the server and the
        // extensions, once, so a report can be read from the log alone.
        tracing::info!(
            display = %display_name,
            vendor = %vendor,
            release = setup.release_number,
            protocol = %format!("{}.{}", setup.protocol_major_version, setup.protocol_minor_version),
            screens = setup.roots.len(),
            screen,
            root = format!("{root:#x}"),
            root_size = %format!("{root_width}x{root_height}"),
            depth,
            bits_per_pixel = layout.bits_per_pixel,
            format = %layout.name(),
            byte_order = ?setup.image_byte_order,
            shm = ?shm_version,
            randr = ?randr_version,
            xtest = ?xtest_version,
            scale_factor = scale_factor,
            "connected to the X server"
        );

        Ok(Self {
            conn,
            screen,
            root,
            root_width,
            root_height,
            layout,
            shm_version,
            randr_version,
            scale_factor,
            net_active_window,
            net_frame_extents,
            gtk_frame_extents,
        })
    }
}

/// The MIT-SHM version when the server offers the extension.
fn shm_version(conn: &RustConnection) -> Option<(u16, u16)> {
    let cookie = mit_shm::query_version(conn).ok()?;
    let reply = cookie.reply().ok()?;
    Some((reply.major_version, reply.minor_version))
}

/// The RandR version, and only when it is at least 1.5, which is what
/// `GetMonitors` needs (LINUX_SUPPORT.md section 4).
fn randr_version(conn: &RustConnection) -> Option<(u32, u32)> {
    let cookie = randr::query_version(conn, 1, 5).ok()?;
    let reply = cookie.reply().ok()?;
    let version = (reply.major_version, reply.minor_version);
    if version < (1, 5) {
        tracing::warn!(
            major = version.0,
            minor = version.1,
            "RandR is older than 1.5; monitors fall back to the whole screen"
        );
        return None;
    }
    Some(version)
}

/// Interns a property name once, so the hot paths do not ask the server again.
fn intern(conn: &RustConnection, name: &[u8]) -> std::result::Result<xproto::Atom, String> {
    let cookie = xproto::intern_atom(conn, false, name)
        .map_err(|err| format!("could not intern {}: {err}", String::from_utf8_lossy(name)))?;
    cookie
        .reply()
        .map_err(|err| format!("could not intern {}: {err}", String::from_utf8_lossy(name)))
        .map(|reply| reply.atom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_reports_what_l1a_to_l1c_built() {
        // LINUX_SUPPORT.md section 4: silent capture, the cursor, the active
        // window (L1a), the region overlay (L1b) and hotkeys plus the key-state
        // poll (L1c). The hide-and-capture wiring is platform-neutral in the
        // controller, so it is real here too.
        let capabilities = CAPABILITIES;
        assert!(capabilities.silent_capture, "capture");
        assert!(capabilities.active_window_rect, "active window");
        assert!(capabilities.cursor_position, "cursor");
        assert!(capabilities.hide_main_window, "hide window");
        assert!(capabilities.overlay, "the region overlay is L1b");
        assert_eq!(
            capabilities.global_hotkeys,
            HotkeySupport::Native,
            "hotkeys (L1c)"
        );
        assert!(
            !capabilities.tray,
            "no tray on Linux (LINUX_SUPPORT.md section 1)"
        );
        assert_ne!(capabilities, Capabilities::NONE);
    }

    /// The real X server this machine runs: connecting, enumerating monitors
    /// and reading the pixel layout are exactly the calls the backend is
    /// judged by (L1a, plus L1b's empty-capture rule).
    #[test]
    fn this_machines_x_server_answers_the_backend_calls() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skipped: no DISPLAY");
            return;
        }
        let platform = X11Platform::new().expect("the X server should be reachable");
        let connection = &platform.connection;

        assert!(connection.root_width > 0 && connection.root_height > 0);
        assert!(connection.layout.bits_per_pixel >= 8);
        assert!(
            connection.randr_version.is_some(),
            "RandR 1.5 is what monitor enumeration is built on"
        );

        let monitors = platform.monitors().expect("monitors should enumerate");
        assert!(!monitors.is_empty());
        for monitor in &monitors {
            assert!(monitor.rect.w <= i32::from(connection.root_width));
            assert!(monitor.rect.h <= i32::from(connection.root_height));
        }

        let (x, y) = platform
            .cursor_pos()
            .expect("the pointer should be findable");
        assert!(
            x >= 0 && x < i32::from(connection.root_width),
            "cursor x {x}"
        );
        assert!(
            y >= 0 && y < i32::from(connection.root_height),
            "cursor y {y}"
        );

        // A real capture of this machine's desktop: one frame per monitor, the
        // image exactly the size of the monitor's rectangle, and the pixels
        // opaque like every other capture path's (SPEC section 5.4).
        let capture = platform
            .capture_all(&CaptureOptions::default())
            .expect("this session should capture");
        assert_eq!(capture.frames.len(), monitors.len());
        for frame in &capture.frames {
            assert_eq!(
                (frame.image.width(), frame.image.height()),
                (
                    u32::try_from(frame.info.rect.w).unwrap_or(0),
                    u32::try_from(frame.info.rect.h).unwrap_or(0)
                ),
                "{:?}",
                frame.info
            );
            assert_eq!(frame.image.get_pixel(0, 0).0[3], 255, "alpha");
            assert!(
                matches!(
                    frame.diagnostics.api_used,
                    crate::CaptureApi::X11Shm | crate::CaptureApi::X11Get
                ),
                "{:?}",
                frame.diagnostics
            );
            assert_eq!(frame.diagnostics.sdr_white_nits, 0.0, "no HDR on X11");
            assert_eq!(
                frame.diagnostics.row_pitch,
                u32::try_from(connection.layout.pitch(frame.image.width())).unwrap_or(0)
            );
        }

        // A capture with `include_cursor` on: X11 cannot draw the pointer into
        // the frame (LINUX_SUPPORT.md section 4), but asking for it must not
        // fail the capture either — it warns and captures without it.
        let with_cursor = platform
            .capture_all(&CaptureOptions {
                include_cursor: true,
                ..CaptureOptions::default()
            })
            .expect("include_cursor must not fail an X11 capture");
        assert_eq!(with_cursor.frames.len(), capture.frames.len());

        // The active window's rectangle, when this session has a focused one
        // (a bare X server without a window manager may not).
        if let Some(rect) = platform
            .foreground_window_rect()
            .expect("reading the active window should not fail")
        {
            assert!(rect.w > 0 && rect.h > 0, "{rect:?}");
            tracing::debug!(?rect, "the active window");
        }

        // The region overlay needs something to select from: an empty capture
        // is refused before any window could open. (A real capture would open
        // the overlay child, which in a test binary is not the app; that is
        // what `--debug-overlay` is for.)
        let error = platform
            .select_region(&VirtualCapture::default())
            .expect_err("an empty capture has nothing to select");
        assert!(error.to_string().contains("nothing to select"), "{error}");
    }
}
