//! The Screenshot portal: the Wayland backend's cross-desktop capture path
//! and fallback when KDE native capture is unavailable (L3b/L4a).
//!
//! Two rules from the spec:
//!
//! - **Every capture has one deadline** (120 s including layout/native/fallback;
//!   15 s for the startup probe). Service work runs on a worker, bounded by
//!   its caller's receive deadline. Late answers are discarded.
//! - **Log the latency and the portal's response code** on every call (SPEC
//!   section 0, rule 11): the latency was the spike's headline, and the
//!   response code says whether a failure was the user or the portal.
//!
//! The portal answers with a URI to a PNG it keeps in a hidden file under
//! the session directory (about 20 MB for the VM's 5441×1620 image). The
//! file is decoded here and **deleted immediately**: leaving six of those
//! around a day of captures would fill `$XDG_RUNTIME_DIR`.

use std::time::{Duration, Instant};

use crate::{CaptureApi, Error, FrameDiagnostics, Result};

use ashpd::desktop::ResponseError;
use ashpd::desktop::screenshot::{ScreenshotOptions, ScreenshotProxy};
use image::RgbaImage;
/// The entire Wayland capture budget, including layout, native and fallback.
pub(crate) const PORTAL_DEADLINE: Duration = Duration::from_secs(120);

/// The portal session: whether `org.freedesktop.portal.Screenshot` is there,
/// and which version it speaks.
///
/// Probed once when the backend starts (LINUX_SUPPORT.md section 6); a
/// definitively missing portal disables capture only without KDE native support.
/// A transient probe failure permits a fresh service connection on capture.
/// Layout is re-probed per capture because monitors move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PortalSession {
    /// The interface version when the portal answered, e.g. 2.
    version: Option<u32>,
    /// Why there is no portal, for [`crate::Capabilities::capture_reason`],
    /// when the interface was definitively absent (not a transient failure).
    missing_reason: Option<&'static str>,
}

/// Why a missing Screenshot portal disables capture (SPEC 8.3's reason in the
/// hover text); also the shape the tests pin.
pub const NO_PORTAL_REASON: &str = "the desktop's Screenshot portal is not available";

impl PortalSession {
    /// Asks the session bus whether the Screenshot portal exists.
    ///
    /// Bound by a 15 s probe deadline: a session bus that hangs is
    /// a hung startup in the making, never that.
    pub(crate) fn probe() -> Self {
        let answer = portal_thread_with_deadline(
            "probing for the Screenshot portal",
            Duration::from_secs(15),
            |deadline_started| match async_io::block_on(ScreenshotProxy::new()) {
                Ok(proxy) => Ok(Some(proxy.version())),
                Err(ashpd::Error::PortalNotFound(_)) => Ok(None),
                Err(error) => Err(portal_error(error, deadline_started)),
            },
        );
        Self::from_probe(answer)
    }

    fn from_probe(answer: Result<Option<u32>>) -> Self {
        match answer {
            Ok(version) => Self {
                version,
                missing_reason: version.is_none().then_some(NO_PORTAL_REASON),
            },
            Err(reason) => {
                tracing::warn!(%reason, "Screenshot portal probe failed transiently; retry on capture");
                Self {
                    version: None,
                    missing_reason: None,
                }
            }
        }
    }

    /// What the portal can be named in logs and hover text.
    pub(crate) fn version(&self) -> Option<u32> {
        self.version
    }

    /// The reason the Screenshot interface is unavailable, independently of KDE.
    pub(crate) fn missing_reason(&self) -> Option<&'static str> {
        self.missing_reason
    }

    /// One silent full-desktop capture: the portal image and its facts.
    ///
    /// The call is the portal's, one at at time; KDE composes the whole
    /// desktop at the largest output's scale, so this is the only image the
    /// backend ever reads (LINUX_SUPPORT.md section 6, L3b).
    pub(crate) fn capture(&self, deadline_started: Instant) -> Result<PortalFrame> {
        let elapsed_started = Instant::now();
        let proxy_result = async_io::block_on(async {
            let proxy = ScreenshotProxy::new().await?;
            let request = proxy
                .screenshot(
                    // No target: KDE rejects `Screen` ("Unavailable
                    // screenshot target 1", Spike S1), and `None`
                    // is the whole desktop, silently.
                    None,
                    ScreenshotOptions::default().set_interactive(false),
                )
                .await?;
            // `response()` reads the already-
            // started request, not an async call: ashpd 0.13's
            // `Request::response` returns a plain Result.
            let response = request.response()?;
            Ok::<ashpd::desktop::screenshot::Screenshot, ashpd::Error>(response)
        });
        let response = proxy_result.map_err(|err| portal_error(err, elapsed_started))?;
        let elapsed_call = elapsed_started.elapsed();
        let uri = response.uri().as_str().to_owned();
        let image = load_portal_image(&uri)?;
        tracing::info!(
            ms = elapsed_call.as_millis(),
            capture_elapsed_ms = deadline_started.elapsed().as_millis(),
            response_code = 0,
            size = format!("{}x{}", image.width(), image.height()),
            uri = %uri,
            "portal image decoded"
        );

        Ok(PortalFrame {
            image,
            elapsed: elapsed_call,
        })
    }
}

fn local_file_path(uri: &str) -> Result<std::path::PathBuf> {
    let invalid = || Error::Capture {
        message: format!("the portal answered an invalid local file URI: {uri}"),
    };
    let url = url::Url::parse(uri).map_err(|_| invalid())?;
    if url.scheme() != "file"
        || url.query().is_some()
        || url.fragment().is_some()
        || url
            .host_str()
            .is_some_and(|host| !host.is_empty() && host != "localhost")
    {
        return Err(invalid());
    }
    url.to_file_path().map_err(|_| invalid())
}

fn load_portal_image(uri: &str) -> Result<RgbaImage> {
    let path = local_file_path(uri)?;
    // Keep the read/decode error even when cleanup also fails. Once the URI
    // identifies a local file, cleanup is attempted on every path.
    let result = (|| {
        let png = std::fs::read(&path).map_err(|error| Error::Capture {
            message: format!(
                "the portal's image at {} could not be read: {error}",
                path.display()
            ),
        })?;
        image::load_from_memory(&png)
            .map(|image| image.into_rgba8())
            .map_err(|error| Error::Capture {
                message: format!("the portal's image is not a decodable picture: {error}"),
            })
    })();
    match std::fs::remove_file(&path) {
        Ok(()) => {
            tracing::info!(path = %path.display(), decoded = result.is_ok(), "portal temporary image removed")
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, decoded = result.is_ok(), "portal temporary image could not be removed")
        }
    }
    result
}

/// One portal answer: the composed desktop's pixels and how long it took.
#[derive(Debug)]
pub(crate) struct PortalFrame {
    /// Straight RGBA; the portal's PNG is opaque, alpha comes out 255.
    pub image: RgbaImage,
    /// The portal call's own latency, for the frame's diagnostics.
    pub elapsed: Duration,
}

impl PortalFrame {
    /// The diagnostics to log alongside the frame (SPEC section 5.4).
    pub(crate) fn diagnostics(&self) -> FrameDiagnostics {
        FrameDiagnostics {
            api_used: CaptureApi::Portal,
            pixel_format: "Rgba8".to_owned(),
            width: self.image.width(),
            height: self.image.height(),
            row_pitch: self.image.width() * 4,
            max_channel_value: 0.0,
            sdr_white_nits: 0.0,
            elapsed_ms: self.elapsed.as_secs_f64() * 1000.0,
        }
    }
}

/// Runs service work on one worker with a bounded caller wait. A stuck
/// service may retain its detached worker; late answers are discarded.
pub(super) fn portal_thread_with_deadline<T: Send + 'static>(
    what: &'static str,
    deadline: Duration,
    work: impl FnOnce(Instant) -> Result<T> + Send + 'static,
) -> Result<T> {
    let started = Instant::now();
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("snp-portal".to_owned())
        .spawn(move || {
            // A panic in the portal plumbing is a caller's error text, not a
            // process death: the caller's UI keeps working.
            let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(started)))
                .unwrap_or_else(|_panic| {
                    Err(Error::Capture {
                        message: format!("the portal call panicked: {what}"),
                    })
                });
            // Nobody is listening after a deadline: dropped.
            let _ = sender.send(answer);
        })
        .map_err(|err| Error::Capture {
            message: format!("the portal call thread could not be started: {err}"),
        })?;

    match receiver.recv_timeout(deadline.saturating_sub(started.elapsed())) {
        Ok(answer) => {
            tracing::debug!(
                ms = started.elapsed().as_millis(),
                what,
                "the portal call answered"
            );
            answer
        }
        Err(_) => {
            // The JoinHandle's drop detaches the thread: its zbus session
            // still ends itself eventually, and its answer is dropped.
            drop(thread);
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            tracing::warn!(
                ms,
                deadline_s = deadline.as_secs(),
                what,
                "the portal call was abandoned past the deadline; its thread keeps running detached"
            );
            Err(Error::Capture {
                message: "the desktop's screenshot service did not answer".to_owned(),
            })
        }
    }
}

/// Turns an ashpd error into the message the log and the UI see, with the
/// portal's response code spelled out (LINUX_SUPPORT.md section 6: log the
/// response code on every call).
pub(crate) fn portal_error(err: ashpd::Error, started: Instant) -> Error {
    let ms = started.elapsed().as_millis();
    let message = match &err {
        ashpd::Error::Response(ResponseError::Cancelled) => {
            format!("the portal answered cancelled (response code 1) after {ms} ms")
        }
        ashpd::Error::Response(ResponseError::Other) => {
            format!("the portal answered with no details (response code 2) after {ms} ms")
        }
        ashpd::Error::PortalNotFound(name) => {
            format!("the desktop has no {name} portal to capture from")
        }
        other => format!("the portal call failed after {ms} ms: {other}"),
    };
    tracing::info!(ms, %err, reason = %message, "a portal call failed");
    Error::Capture { message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::wayland::{CAPABILITIES, WaylandPortalPlatform};

    #[test]
    fn w1_file_uris_decode_local_paths_and_reject_remote_authorities() {
        for uri in [
            "file:///tmp/Screen%20Shot%23%25+%C3%A9.png",
            "file://localhost/tmp/Screen%20Shot%23%25+%C3%A9.png",
        ] {
            assert_eq!(
                local_file_path(uri).unwrap(),
                std::path::Path::new("/tmp/Screen Shot#%+é.png")
            );
        }
        for uri in [
            "file://remote/tmp/x.png",
            "https://localhost/tmp/x.png",
            "file:///tmp/x?query",
            "file:///tmp/x#fragment",
            "file://user@localhost/tmp/x",
        ] {
            assert!(local_file_path(uri).is_err(), "{uri}");
        }
    }

    #[test]
    fn w1_portal_file_is_removed_on_decode_failure_and_success() {
        let path =
            std::env::temp_dir().join(format!("snp-w1-uri-{} Screen Shot.png", std::process::id()));
        let uri = url::Url::from_file_path(&path).unwrap().to_string();
        std::fs::write(&path, b"not an image").unwrap();
        assert!(
            load_portal_image(&uri)
                .unwrap_err()
                .to_string()
                .contains("not a decodable")
        );
        assert!(!path.exists());
        RgbaImage::new(2, 3).save(&path).unwrap();
        assert_eq!(load_portal_image(&uri).unwrap().dimensions(), (2, 3));
        assert!(!path.exists());
        assert!(
            load_portal_image(&uri)
                .unwrap_err()
                .to_string()
                .contains("could not be read")
        );
    }

    #[test]
    fn w3_kde_target_capability_is_independent_of_cursor_rect_access() {
        for native_kde in [false, true] {
            let platform = WaylandPortalPlatform {
                native_kde,
                portal: PortalSession::from_probe(Ok(Some(2))),
            };
            let capabilities = platform.capabilities();
            assert_eq!(capabilities.compositor_targets, native_kde);
            assert!(!capabilities.cursor_position);
            assert!(!capabilities.active_window_rect);
        }
    }

    #[test]
    fn w1_native_kde_is_allowed_without_a_portal_and_transient_probe_retries() {
        let missing = WaylandPortalPlatform {
            native_kde: true,
            portal: PortalSession::from_probe(Ok(None)),
        };
        assert!(missing.capabilities().silent_capture);
        assert_eq!(missing.capabilities().capture_reason, None);
        let native = super::super::kwin::with_fallback(
            missing.native_kde,
            || Ok(7),
            || panic!("native must be tried without portal"),
        );
        assert_eq!(native.unwrap(), 7);
        let transient = WaylandPortalPlatform {
            native_kde: false,
            portal: PortalSession::from_probe(Err(Error::Capture {
                message: "temporary bus failure".into(),
            })),
        };
        assert!(
            transient.capabilities().silent_capture,
            "capture retries a transient startup failure"
        );
    }

    #[test]
    fn a_missing_portal_disables_capture_with_its_named_reason() {
        let missing = WaylandPortalPlatform {
            native_kde: false,
            portal: PortalSession {
                version: None,
                missing_reason: Some(NO_PORTAL_REASON),
            },
        };
        let capabilities = missing.capabilities();
        assert!(!capabilities.silent_capture);
        assert_eq!(capabilities.capture_reason, Some(NO_PORTAL_REASON));

        let available = WaylandPortalPlatform {
            native_kde: false,
            portal: PortalSession {
                version: Some(2),
                missing_reason: None,
            },
        };
        assert_eq!(available.capabilities(), CAPABILITIES);
    }
}

#[cfg(test)]
#[test]
fn l3d_capture_deadline_is_two_minutes_and_stalled_calls_fail() {
    assert_eq!(PORTAL_DEADLINE, Duration::from_secs(120));
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    let result = portal_thread_with_deadline("test stall", Duration::from_millis(10), move |_| {
        let _ = blocked.recv();
        Ok(())
    });
    assert!(result.unwrap_err().to_string().contains("did not answer"));
    drop(release);
    assert!(portal_thread_with_deadline("test success", Duration::from_secs(1), |_| Ok(7)).is_ok());
}
