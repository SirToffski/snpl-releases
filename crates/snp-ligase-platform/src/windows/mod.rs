//! The Windows implementation of [`crate::Platform`].
//!
//! Everything in here is Windows-specific: the `windows` and `windows-capture`
//! crates, `unsafe`, Win32 handles. Nothing outside this module knows any of
//! that (SPEC section 0, rule 7).

pub mod autostart;
pub mod capture;
pub mod keys;
pub mod monitors;
pub mod overlay;
pub mod single_instance;
pub mod synth;
pub mod window_info;

use crate::{CaptureOptions, Error, MonitorInfo, Platform, RectI, Result, VirtualCapture};

/// The Windows platform.
///
/// Holds no state on purpose: monitors are re-enumerated on every capture
/// because they change while the program runs (SPEC section 5.3).
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsPlatform;

impl WindowsPlatform {
    /// Creates the platform implementation.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Fresh monitor list, enumerated now rather than cached.
    fn fresh_monitors(&self) -> Result<(Vec<monitors::RawMonitor>, Vec<MonitorInfo>)> {
        let raw = monitors::enumerate()?;
        if raw.is_empty() {
            return Err(Error::Capture {
                message: "the desktop reported no monitors".to_owned(),
            });
        }
        let infos = monitors::details(&raw)?;
        Ok((raw, infos))
    }
}

impl Platform for WindowsPlatform {
    fn capabilities(&self) -> crate::Capabilities {
        crate::Capabilities::FULL
    }

    fn environment(&self) -> crate::PlatformEnvironment {
        crate::PlatformEnvironment::WINDOWS
    }

    fn monitors(&self) -> Result<Vec<MonitorInfo>> {
        let raw = monitors::enumerate()?;
        monitors::details(&raw)
    }

    fn capture_all(&self, opts: &CaptureOptions) -> Result<VirtualCapture> {
        let (raw, infos) = self.fresh_monitors()?;

        tracing::info!(
            monitors = infos.len(),
            cursor = opts.include_cursor,
            "capturing every monitor"
        );

        let capture = capture::capture_all(&raw, &infos, opts)?;

        // Log one line per frame, because a wrong capture has to be diagnosable
        // from the log file alone (SPEC section 0, rule 11).
        for frame in &capture.frames {
            tracing::info!(
                monitor = %frame.info.id,
                rect = ?frame.info.rect,
                scale = frame.info.scale_factor,
                hdr = frame.info.hdr_active,
                sdr_white_nits = frame.info.sdr_white_nits,
                api = %frame.diagnostics.api_used,
                format = %frame.diagnostics.pixel_format,
                size = format!("{}x{}", frame.diagnostics.width, frame.diagnostics.height),
                pitch = frame.diagnostics.row_pitch,
                max_channel = frame.diagnostics.max_channel_value,
                elapsed_ms = frame.diagnostics.elapsed_ms,
                "frame captured"
            );
        }

        Ok(capture)
    }

    fn select_region(&self, capture: &VirtualCapture) -> Result<Option<RectI>> {
        overlay::select_region(capture)
    }

    fn foreground_window_rect(&self) -> Result<Option<RectI>> {
        window_info::foreground_window_rect()
    }

    fn cursor_pos(&self) -> Result<(i32, i32)> {
        window_info::cursor_pos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Capabilities, HotkeySupport};

    #[test]
    fn windows_reports_what_it_has_today() {
        // LINUX_SUPPORT.md section 3, item 2: `WindowsPlatform` reports what
        // it has today, and this is the one list that says so.
        let capabilities = WindowsPlatform::new().capabilities();
        assert!(capabilities.silent_capture, "capture");
        assert!(capabilities.overlay, "overlay");
        assert!(capabilities.active_window_rect, "active window");
        assert!(capabilities.cursor_position, "cursor");
        assert!(capabilities.hide_main_window, "hide window");
        assert_eq!(
            capabilities.global_hotkeys,
            HotkeySupport::Native,
            "hotkeys"
        );
        assert!(capabilities.tray, "tray");
        assert_eq!(capabilities, Capabilities::FULL);
    }
}
