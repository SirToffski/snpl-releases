//! The Linux backend (LINUX_SUPPORT.md).
//!
//! L0a shipped a **stub** so the workspace compiles and tests on Linux. L1a
//! adds the **X11 backend**: monitors through RandR, capture through MIT-SHM
//! with a plain `GetImage` fallback, the cursor and the active window; L1b adds
//! the region overlay, which runs as a child process; L1c adds the X11 key
//! state behind Ctrl+V and the hotkey recorder ([`keys`]). What is left of the
//! stub answers Wayland sessions (phase L3) and sessions with no display server
//! at all, where nothing can be captured.
//!
//! Which one runs is decided once, at startup, from the session's environment
//! variables ([`SessionEnv`]) — logged either way, because a Linux bug report
//! starts with "which session was this?".

pub mod keys;
mod overlay_parent;
pub mod wayland;
pub mod x11;

use crate::{
    Capabilities, CaptureOptions, Error, MonitorInfo, Platform, PlatformEnvironment, RectI, Result,
    VirtualCapture,
};

/// Which display server a session runs on, and so which backend can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// An X11 session: this machine's backend (LINUX_SUPPORT.md section 4).
    X11,
    /// A Wayland session, where the portal backend (phase L3) is not built
    /// yet.
    Wayland,
    /// No display server: `$DISPLAY` and `$WAYLAND_DISPLAY` are both unset.
    Headless,
}

/// The session's environment variables, read once at startup.
///
/// A plain struct so the detection and the environment report are pure
/// functions of it and every case is a test, with no test touching the real
/// environment (LINUX_SUPPORT.md sections 4 and 10).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionEnv {
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    /// `$XDG_SESSION_TYPE`, as the session manager describes it.
    pub session_type: Option<String>,
    /// `$XDG_CURRENT_DESKTOP`, e.g. `XFCE` or `KDE`.
    pub current_desktop: Option<String>,
}

impl SessionEnv {
    /// The variables as this process sees them.
    #[must_use]
    pub fn read() -> Self {
        fn var(name: &str) -> Option<String> {
            // An empty variable is not a value: `DISPLAY=` would otherwise
            // look like a session that is there.
            std::env::var(name).ok().filter(|value| !value.is_empty())
        }
        Self {
            display: var("DISPLAY"),
            wayland_display: var("WAYLAND_DISPLAY"),
            session_type: var("XDG_SESSION_TYPE"),
            current_desktop: var("XDG_CURRENT_DESKTOP"),
        }
    }

    /// Which backend can run here.
    ///
    /// `$WAYLAND_DISPLAY` wins over `$DISPLAY`: a Wayland session also sets
    /// `$DISPLAY` for XWayland, and its XX root window is not the screen the
    /// user sees (LINUX_SUPPORT.md section 6: the portal backend is chosen
    /// when `$WAYLAND_DISPLAY` is set).
    #[must_use]
    pub fn kind(&self) -> SessionKind {
        if self.wayland_display.is_some() {
            SessionKind::Wayland
        } else if self.display.is_some() {
            SessionKind::X11
        } else {
            SessionKind::Headless
        }
    }

    /// What a bug report needs to know about this session.
    #[must_use]
    pub fn environment(&self, backend: &'static str) -> PlatformEnvironment {
        // The session manager's own word when there is one, else what the
        // variables say: the report should not claim more than the session did.
        let session_type = self.session_type.clone().or_else(|| match self.kind() {
            SessionKind::X11 => Some("x11".to_owned()),
            SessionKind::Wayland => Some("wayland".to_owned()),
            SessionKind::Headless => None,
        });
        PlatformEnvironment {
            backend,
            session_type,
            desktop: self.current_desktop.clone(),
        }
    }
}

/// Why a session has no [`crate::Platform`] backend behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StubReason {
    /// No display server at all.
    Headless,
    /// `$DISPLAY` is set, but the X11 server cannot be reached (a stale
    /// variable, a dead session, no permission).
    NoX11(String),
}

impl StubReason {
    /// What every stubbed call says, so a log line explains what is missing.
    fn message(&self) -> String {
        match self {
            Self::Headless => "there is no display server to capture from \
                 (both DISPLAY and WAYLAND_DISPLAY are unset)"
                .to_owned(),
            Self::NoX11(reason) => {
                format!("the X11 display cannot be used: {reason}")
            }
        }
    }

    /// The error every call of a stubbed session returns.
    fn unsupported(&self) -> Error {
        Error::Unsupported {
            message: self.message(),
        }
    }
}

/// The Linux platform: the X11 backend on an X11 session (LINUX_SUPPORT.md
/// phase L1), the stub everywhere else.
#[derive(Debug)]
pub struct LinuxPlatform {
    backend: Backend,
    session: SessionEnv,
}

/// Which backend answers this session's calls.
#[derive(Debug)]
enum Backend {
    /// Boxed: the connection and the facts about the server are much bigger
    /// than the stub's reason, and this enum is carried in the platform the
    /// app holds (clippy's `large_enum_variant`).
    X11(Box<x11::X11Platform>),
    /// The Screenshot portal backend (LINUX_SUPPORT.md section 6).
    Wayland(Box<wayland::WaylandPortalPlatform>),
    Stub(StubReason),
}

impl Backend {
    fn capabilities(&self) -> Capabilities {
        match self {
            Self::X11(platform) => platform.capabilities(),
            Self::Wayland(platform) => platform.capabilities(),
            // The stub promises nothing: every menu row and Settings control
            // that needs a capability is disabled.
            Self::Stub(_) => Capabilities::NONE,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::X11(_) => "x11",
            Self::Wayland(_) => "wayland-portal",
            Self::Stub(_) => "stub",
        }
    }
}

impl LinuxPlatform {
    /// Picks the backend for this session and connects to it.
    ///
    /// Never fails: a session that cannot be used is a stub with a reason, so
    /// the app still starts and says why (LINUX_SUPPORT.md sections 3 and 4).
    #[must_use]
    pub fn new() -> Self {
        let session = SessionEnv::read();
        let kind = session.kind();
        tracing::info!(
            session = %format!("{kind:?}"),
            display = session.display.as_deref().unwrap_or("-"),
            wayland_display = session.wayland_display.as_deref().unwrap_or("-"),
            xdg_session_type = session.session_type.as_deref().unwrap_or("-"),
            desktop = session.current_desktop.as_deref().unwrap_or("-"),
            "linux session"
        );

        let backend = match kind {
            SessionKind::X11 => match x11::X11Platform::new() {
                Ok(platform) => Backend::X11(Box::new(platform)),
                Err(reason) => {
                    tracing::warn!(reason = %reason, "falling back to the stub");
                    Backend::Stub(StubReason::NoX11(reason))
                }
            },
            SessionKind::Wayland => Backend::Wayland(Box::default()),
            SessionKind::Headless => Backend::Stub(StubReason::Headless),
        };

        Self { backend, session }
    }
}

impl Default for LinuxPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl Platform for LinuxPlatform {
    fn capabilities(&self) -> Capabilities {
        self.backend.capabilities()
    }

    fn environment(&self) -> PlatformEnvironment {
        self.session.environment(self.backend.name())
    }

    fn monitors(&self) -> Result<Vec<MonitorInfo>> {
        match &self.backend {
            Backend::X11(platform) => platform.monitors(),
            Backend::Wayland(platform) => platform.monitors(),
            Backend::Stub(reason) => Err(reason.unsupported()),
        }
    }

    fn capture_target(
        &self,
        target: crate::CaptureTarget,
        opts: &CaptureOptions,
    ) -> Result<(VirtualCapture, RectI)> {
        match &self.backend {
            Backend::X11(_) => crate::capture_native_target(self, target, opts),
            Backend::Wayland(platform) => platform.capture_target(target, opts),
            Backend::Stub(reason) => Err(reason.unsupported()),
        }
    }

    fn capture_all(&self, opts: &CaptureOptions) -> Result<VirtualCapture> {
        match &self.backend {
            Backend::X11(platform) => platform.capture_all(opts),
            Backend::Wayland(platform) => platform.capture_all(opts),
            Backend::Stub(reason) => {
                let error = reason.unsupported();
                tracing::warn!(%error, "a capture was asked for, and {error}");
                Err(error)
            }
        }
    }

    fn select_region(&self, capture: &VirtualCapture) -> Result<Option<RectI>> {
        match &self.backend {
            Backend::X11(platform) => platform.select_region(capture),
            Backend::Wayland(platform) => platform.select_region(capture),
            Backend::Stub(reason) => Err(reason.unsupported()),
        }
    }

    fn select_region_activated(&self, capture: &VirtualCapture) -> Result<crate::RegionSelection> {
        match &self.backend {
            Backend::Wayland(platform) => platform.select_region_activated(capture),
            _ => self
                .select_region(capture)
                .map(|region| crate::RegionSelection {
                    region,
                    activation_token: None,
                }),
        }
    }

    fn foreground_window_rect(&self) -> Result<Option<RectI>> {
        match &self.backend {
            Backend::X11(platform) => platform.foreground_window_rect(),
            Backend::Wayland(_) => Err(Error::Unsupported {
                message: "the foreground window's rectangle is not available on Wayland".to_owned(),
            }),
            Backend::Stub(reason) => Err(reason.unsupported()),
        }
    }

    fn cursor_pos(&self) -> Result<(i32, i32)> {
        match &self.backend {
            Backend::X11(platform) => platform.cursor_pos(),
            Backend::Wayland(_) => Err(Error::Unsupported {
                message: "the cursor position is not available on Wayland".to_owned(),
            }),
            Backend::Stub(reason) => Err(reason.unsupported()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(display: Option<&str>, wayland: Option<&str>) -> SessionEnv {
        SessionEnv {
            display: display.map(str::to_owned),
            wayland_display: wayland.map(str::to_owned),
            ..SessionEnv::default()
        }
    }

    #[test]
    fn the_session_kind_follows_the_environment_variables() {
        // LINUX_SUPPORT.md section 6: $WAYLAND_DISPLAY wins, because a Wayland
        // session sets $DISPLAY for XWayland too and that X root window is not
        // the screen the user sees.
        let cases = [
            (env(Some(":0"), None), SessionKind::X11),
            (env(Some(":0"), Some("wayland-0")), SessionKind::Wayland),
            (env(None, Some("wayland-0")), SessionKind::Wayland),
            (env(None, None), SessionKind::Headless),
            (env(Some(":1"), None), SessionKind::X11),
        ];
        for (session, expected) in cases {
            assert_eq!(session.kind(), expected, "{session:?}");
        }
    }

    #[test]
    fn the_environment_reports_the_backend_and_the_session() {
        let mut session = env(Some(":0"), None);
        session.session_type = Some("x11".to_owned());
        session.current_desktop = Some("XFCE".to_owned());

        let environment = session.environment("x11");
        assert_eq!(environment.backend, "x11");
        assert_eq!(environment.session_type.as_deref(), Some("x11"));
        assert_eq!(environment.desktop.as_deref(), Some("XFCE"));

        // A session that does not name its type is described by what the
        // variables say, never by a better guess.
        let inferred = env(Some(":0"), None).environment("x11");
        assert_eq!(inferred.session_type.as_deref(), Some("x11"));
        assert_eq!(inferred.desktop, None);

        let wayland = env(None, Some("wayland-0")).environment("stub");
        assert_eq!(wayland.backend, "stub");
        assert_eq!(wayland.session_type.as_deref(), Some("wayland"));

        // Nothing is claimed on a machine without a session.
        let headless = env(None, None).environment("stub");
        assert_eq!(headless.session_type, None);
        assert_eq!(headless.desktop, None);

        // What Windows reports: the same shape, no session or desktop.
        assert_eq!(PlatformEnvironment::WINDOWS.backend, "windows");
        assert_eq!(PlatformEnvironment::WINDOWS.session_type, None);
    }

    #[test]
    fn the_stub_reports_no_capabilities_and_says_what_is_missing() {
        // LINUX_SUPPORT.md section 3, item 2: a stub reports nothing. Every
        // menu row and Settings control that needs a capability is disabled.
        let headless = LinuxPlatform {
            backend: Backend::Stub(StubReason::Headless),
            session: env(None, None),
        };
        assert_eq!(headless.capabilities(), Capabilities::NONE);

        let error = headless
            .capture_all(&CaptureOptions::default())
            .expect_err("a headless session must not capture");
        assert!(error.to_string().contains("no display server"), "{error}");

        // Every call of a stubbed session says so, not just the capture.
        for call in [
            headless.monitors().err(),
            headless.select_region(&VirtualCapture::default()).err(),
            headless.foreground_window_rect().err(),
            headless.cursor_pos().err(),
        ] {
            assert!(
                matches!(call, Some(Error::Unsupported { .. })),
                "{call:?} was not an Unsupported error"
            );
        }
    }

    /// The real Wayland session this machine runs, when it has one (L3b's
    /// check: the portal backend, its capabilities and its environment).
    #[test]
    fn this_machines_session_picks_the_wayland_backend() {
        let session = SessionEnv::read();
        if session.kind() != SessionKind::Wayland {
            eprintln!("skipped: not a Wayland session ({session:?})");
            return;
        }

        let platform = LinuxPlatform::new();
        assert_eq!(
            platform.environment().backend,
            "wayland-portal",
            "{session:?}"
        );

        let capabilities = platform.capabilities();
        if let Some(reason) = capabilities.capture_reason {
            tracing::warn!(reason, "the real session's portal is unavailable");
        }

        let monitors = platform
            .monitors()
            .expect("the Wayland session should enumerate its monitors");
        assert!(!monitors.is_empty(), "no monitors on a Wayland session");
        for monitor in &monitors {
            assert!(monitor.rect.w > 0 && monitor.rect.h > 0, "{monitor:?}");
            assert!(
                !monitor.hdr_active,
                "Wayland has no HDR capture: {monitor:?}"
            );
            assert!(!monitor.is_primary, "Wayland has no primary: {monitor:?}");
        }
    }

    #[test]
    fn a_broken_x11_connection_is_a_stub_that_names_the_reason() {
        let platform = LinuxPlatform {
            backend: Backend::Stub(StubReason::NoX11(
                "could not connect to the X display: display :9 is gone".to_owned(),
            )),
            session: env(Some(":9"), None),
        };
        let error = platform
            .capture_all(&CaptureOptions::default())
            .expect_err("a dead DISPLAY must not capture");
        assert!(error.to_string().contains(":9"), "{error}");
        assert_eq!(platform.environment().backend, "stub");
        assert_eq!(platform.environment().session_type.as_deref(), Some("x11"));
    }

    /// The real session this machine runs, when it has one (L1a's end-to-end
    /// check: the backend, its capabilities and its environment).
    #[test]
    fn this_machines_session_picks_the_x11_backend() {
        let session = SessionEnv::read();
        if session.kind() != SessionKind::X11 {
            eprintln!("skipped: not an X11 session ({session:?})");
            return;
        }

        let platform = LinuxPlatform::new();
        assert_eq!(platform.environment().backend, "x11", "{session:?}");

        let capabilities = platform.capabilities();
        assert_eq!(capabilities, x11::CAPABILITIES, "the X11 capabilities");

        let monitors = platform
            .monitors()
            .expect("the X11 session should enumerate its monitors");
        assert!(!monitors.is_empty(), "no monitors on an X11 session");
        for monitor in &monitors {
            assert!(monitor.rect.w > 0 && monitor.rect.h > 0, "{monitor:?}");
            assert!(!monitor.hdr_active, "X11 has no HDR capture: {monitor:?}");
        }

        let (x, y) = platform
            .cursor_pos()
            .expect("the cursor position should be readable on X11");
        let _ = (x, y);
    }
}
