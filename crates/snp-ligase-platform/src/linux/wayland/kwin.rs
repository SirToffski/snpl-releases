//! KDE ScreenShot2 v5: native output pixels through an fd, never a PNG.
//! Protocol checked against KWin v6.7.5 screenshotdbusinterface2.cpp.
//! The complete bus call and pipe read run on the 120 s capture worker.
use std::{collections::HashMap, io::Read, sync::Once, time::Instant};

use ashpd::{
    zbus,
    zvariant::{Fd, OwnedValue, Value},
};
use image::RgbaImage;

use super::{layout, monitor_info};
use crate::{CaptureApi, Error, FrameDiagnostics, MonitorFrame, Result, VirtualCapture};

fn error(message: impl std::fmt::Display) -> Error {
    Error::Capture {
        message: format!("KWin ScreenShot2: {message}"),
    }
}

pub(super) fn preferred(desktop: Option<&str>) -> bool {
    desktop.is_some_and(|desktop| {
        desktop
            .split(':')
            .any(|part| part.eq_ignore_ascii_case("KDE"))
    })
}

/// Retry the native service each capture: permission/output changes are live.
/// A partial native capture is discarded before the portal takes the desktop.
pub(super) fn with_fallback<T>(
    prefer_native: bool,
    native: impl FnOnce() -> Result<T>,
    portal: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if prefer_native {
        match native() {
            Ok(capture) => return Ok(capture),
            Err(error) => {
                static REFUSAL: Once = Once::new();
                REFUSAL.call_once(|| tracing::warn!(%error, "KWin native capture unavailable; falling back to Screenshot portal"));
                tracing::debug!(%error, "native capture attempt failed for this capture");
                return portal().map_err(|fallback| Error::Capture {
                    message: format!("{error}; Screenshot portal fallback also failed: {fallback}"),
                });
            }
        }
    }
    portal()
}

pub(super) fn capture(
    layout: &layout::VirtualLayout,
    started: Instant,
    include_cursor: bool,
) -> Result<VirtualCapture> {
    async_io::block_on(async {
        let connection = zbus::Connection::session().await.map_err(error)?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.kde.KWin",
            "/org/kde/KWin/ScreenShot2",
            "org.kde.KWin.ScreenShot2",
        )
        .await
        .map_err(error)?;
        let mut frames = Vec::with_capacity(layout.monitors.len());
        for monitor in &layout.monitors {
            let output_started = Instant::now();
            let (mut reader, writer) = std::io::pipe().map_err(error)?;
            let options = capture_options(include_cursor);
            let metadata: HashMap<String, OwnedValue> = proxy
                .call(
                    "CaptureScreen",
                    &(&monitor.name, options, Fd::from(&writer)),
                )
                .await
                .map_err(error)?;
            drop(writer);
            let width = number(&metadata, "width")?;
            let height = number(&metadata, "height")?;
            let stride = number(&metadata, "stride")?;
            let format = number(&metadata, "format")?;
            let expected = expected_size(monitor);
            validate(width, height, stride, format, expected)?;
            let length = u64::from(stride) * u64::from(height);
            let mut bytes = Vec::new();
            // Bound allocation even if a faulty service never closes its fd.
            reader
                .by_ref()
                .take(length + 1)
                .read_to_end(&mut bytes)
                .map_err(error)?;
            let image = decode(width, height, stride, format, expected, &bytes)?;
            tracing::info!(monitor = %monitor.name, ?metadata, ms = output_started.elapsed().as_millis(), native_scale = 1.0, rect = ?monitor.rect, "KWin native monitor frame read");
            frames.push(native_frame(
                monitor,
                image,
                stride,
                format,
                output_started.elapsed(),
            ));
        }
        tracing::info!(
            ms = started.elapsed().as_millis(),
            monitors = frames.len(),
            "KWin native capture complete"
        );
        Ok(VirtualCapture { frames })
    })
}

/// Target frames have their own native pixel coordinates, never desktop crop coordinates.
pub(super) fn capture_target(
    target: crate::CaptureTarget,
    started: Instant,
    include_cursor: bool,
) -> Result<(VirtualCapture, crate::RectI)> {
    let method = target_method(target)?;
    async_io::block_on(async {
        let connection = zbus::Connection::session().await.map_err(error)?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.kde.KWin",
            "/org/kde/KWin/ScreenShot2",
            "org.kde.KWin.ScreenShot2",
        )
        .await
        .map_err(error)?;
        let (mut reader, writer) = std::io::pipe().map_err(error)?;
        let mut options = capture_options(include_cursor);
        options.insert("include-decoration", Value::from(true));
        options.insert("include-shadow", Value::from(false));
        let metadata: HashMap<String, OwnedValue> = proxy
            .call(method, &(options, Fd::from(&writer)))
            .await
            .map_err(error)?;
        drop(writer);
        let width = number(&metadata, "width")?;
        let height = number(&metadata, "height")?;
        let stride = number(&metadata, "stride")?;
        let format = number(&metadata, "format")?;
        validate(width, height, stride, format, (width, height))?;
        let mut bytes = Vec::new();
        reader
            .by_ref()
            .take(u64::from(stride) * u64::from(height) + 1)
            .read_to_end(&mut bytes)
            .map_err(error)?;
        let image = decode(width, height, stride, format, (width, height), &bytes)?;
        let rect = crate::RectI {
            x: 0,
            y: 0,
            w: width as i32,
            h: height as i32,
        };
        tracing::info!(
            ?target,
            ?metadata,
            elapsed_ms = started.elapsed().as_millis(),
            "KWin native target captured before editor minimization"
        );
        let frame = MonitorFrame {
            info: crate::MonitorInfo {
                id: metadata
                    .get("screen")
                    .or_else(|| metadata.get("windowId"))
                    .and_then(|value| <&str>::try_from(value).ok())
                    .unwrap_or(method)
                    .to_owned(),
                rect,
                scale_factor: metadata
                    .get("scale")
                    .and_then(|value| f64::try_from(value).ok())
                    .unwrap_or(1.0) as f32,
                hdr_active: false,
                sdr_white_nits: None,
                is_primary: false,
            },
            diagnostics: FrameDiagnostics {
                api_used: CaptureApi::Kwin,
                pixel_format: format!("QImage({format}) BGRA8"),
                width,
                height,
                row_pitch: stride,
                max_channel_value: 0.0,
                sdr_white_nits: 0.0,
                elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            },
            image,
            stored_pixels: crate::StoredPixels::Native,
            native_scale_x: 1.0,
            native_scale_y: 1.0,
        };
        Ok((
            VirtualCapture {
                frames: vec![frame],
            },
            rect,
        ))
    })
}

fn target_method(target: crate::CaptureTarget) -> Result<&'static str> {
    match target {
        crate::CaptureTarget::CurrentMonitor => Ok("CaptureActiveScreen"),
        crate::CaptureTarget::ActiveWindow => Ok("CaptureActiveWindow"),
        crate::CaptureTarget::AllMonitors => Err(error("all monitors require per-output capture")),
    }
}

pub(super) fn expected_size(monitor: &layout::VirtualMonitor) -> (u32, u32) {
    (
        (monitor.rect.w as f64 * monitor.native_scale_x).round() as u32,
        (monitor.rect.h as f64 * monitor.native_scale_y).round() as u32,
    )
}

fn native_frame(
    monitor: &layout::VirtualMonitor,
    image: RgbaImage,
    stride: u32,
    format: u32,
    elapsed: std::time::Duration,
) -> MonitorFrame {
    MonitorFrame {
        info: monitor_info(monitor),
        diagnostics: FrameDiagnostics {
            api_used: CaptureApi::Kwin,
            pixel_format: format!("QImage({format}) BGRA8"),
            width: image.width(),
            height: image.height(),
            row_pitch: stride,
            max_channel_value: 0.0,
            sdr_white_nits: 0.0,
            elapsed_ms: elapsed.as_secs_f64() * 1000.0,
        },
        image,
        stored_pixels: crate::StoredPixels::Native,
        native_scale_x: 1.0,
        native_scale_y: 1.0,
    }
}

fn number(metadata: &HashMap<String, OwnedValue>, key: &str) -> Result<u32> {
    metadata
        .get(key)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| error(format!("missing or invalid {key}")))
}

fn validate(width: u32, height: u32, stride: u32, format: u32, expected: (u32, u32)) -> Result<()> {
    if width == 0 || height == 0 || (width, height) != expected || width > 32768 || height > 32768 {
        return Err(error(format!(
            "unexpected size {width}x{height}; expected {expected:?}"
        )));
    }
    // QImage RGB32, ARGB32, ARGB32_Premultiplied: native-endian 0xAARRGGBB.
    // Unknown/HDR formats fall back rather than silently corrupting colours.
    if !cfg!(target_endian = "little")
        || !matches!(format, 4..=6)
        || stride < width * 4
        || stride > width * 4 + 4096
    {
        return Err(error(format!(
            "unsupported format {format} / stride {stride}"
        )));
    }
    Ok(())
}

fn decode(
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
    expected: (u32, u32),
    bytes: &[u8],
) -> Result<RgbaImage> {
    validate(width, height, stride, format, expected)?;
    if bytes.len() as u64 != u64::from(stride) * u64::from(height) {
        return Err(error("pipe length differs from metadata"));
    }
    Ok(RgbaImage::from_fn(width, height, |x, y| {
        let i = y as usize * stride as usize + x as usize * 4;
        let alpha = if format == 4 { 255 } else { bytes[i + 3] };
        let channel = |c: u8| {
            if format == 6 {
                if alpha == 0 {
                    0
                } else {
                    ((u32::from(c) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255) as u8
                }
            } else {
                c
            }
        };
        image::Rgba([
            channel(bytes[i + 2]),
            channel(bytes[i + 1]),
            channel(bytes[i]),
            alpha,
        ])
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_frame_has_unit_scale_and_keeps_virtual_rectangle() {
        let monitor = layout::VirtualMonitor {
            name: "native".into(),
            rect: crate::RectI::new(-6, 0, 6, 6),
            native_scale_x: 2.0 / 3.0,
            native_scale_y: 2.0 / 3.0,
            scale_factor: 1.0,
        };
        let image = RgbaImage::new(4, 4);
        let frame = native_frame(&monitor, image, 16, 6, std::time::Duration::ZERO);
        assert_eq!((frame.native_scale_x, frame.native_scale_y), (1.0, 1.0));
        assert_eq!(frame.info.rect, monitor.rect);
        assert_eq!(frame.image.dimensions(), (4, 4));
        assert_eq!(frame.diagnostics.api_used, CaptureApi::Kwin);
        assert_eq!(frame.stored_pixels, crate::StoredPixels::Native);
    }
    #[test]
    fn choice_retries_each_capture_and_falls_back_only_after_failure() {
        let calls = std::cell::RefCell::new(Vec::new());
        let run = |prefer, allowed| {
            with_fallback(
                prefer,
                || {
                    calls.borrow_mut().push("native");
                    if allowed {
                        Ok(1)
                    } else {
                        Err(error("refused"))
                    }
                },
                || {
                    calls.borrow_mut().push("portal");
                    Ok(2)
                },
            )
            .unwrap()
        };
        assert_eq!(run(true, false), 2);
        assert_eq!(run(true, true), 1);
        assert_eq!(run(false, true), 2);
        assert_eq!(*calls.borrow(), ["native", "portal", "native", "portal"]);
        assert!(preferred(Some("KDE")));
        assert!(preferred(Some("other:KDE")));
        assert!(!preferred(Some("GNOME")));
        assert!(!preferred(None));
        assert!(
            with_fallback(
                true,
                || Err::<(), _>(error("refused")),
                || Err(error("portal failed"))
            )
            .is_err()
        );
    }
    #[test]
    fn native_pixels_decode_padding_premultiplication_and_reject_bad_metadata() {
        let bytes = [0, 32, 64, 128, 255, 255, 255, 255, 9, 9, 9, 9];
        let image = decode(2, 1, 12, 6, (2, 1), &bytes).unwrap();
        assert_eq!(image.dimensions(), (2, 1));
        assert_eq!(image.get_pixel(0, 0).0, [128, 64, 0, 128]);
        assert_eq!(image.get_pixel(1, 0).0, [255, 255, 255, 255]);
        assert!(decode(2, 1, 12, 6, (2, 1), &bytes[..8]).is_err());
        assert!(decode(2, 1, 4, 6, (2, 1), &bytes).is_err());
        assert!(decode(2, 1, 12, 16, (2, 1), &bytes).is_err());
        assert!(decode(2, 1, 12, 6, (3, 1), &bytes).is_err());
    }
}

fn capture_options(include_cursor: bool) -> HashMap<&'static str, Value<'static>> {
    HashMap::from([
        ("native-resolution", Value::from(true)),
        ("include-cursor", Value::from(include_cursor)),
        ("hide-caller-windows", Value::from(false)),
    ])
}

#[cfg(test)]
#[test]
fn w2_kwin_cursor_option_tracks_setting() {
    for enabled in [false, true] {
        let options = capture_options(enabled);
        assert_eq!(bool::try_from(&options["include-cursor"]).unwrap(), enabled);
        assert!(bool::try_from(&options["native-resolution"]).unwrap());
        assert!(!bool::try_from(&options["hide-caller-windows"]).unwrap());
    }
}

#[cfg(test)]
#[test]
fn w3_kwin_target_methods_are_explicit_and_never_fall_back_to_desktop() {
    use crate::CaptureTarget;
    assert_eq!(
        target_method(CaptureTarget::CurrentMonitor).unwrap(),
        "CaptureActiveScreen"
    );
    assert_eq!(
        target_method(CaptureTarget::ActiveWindow).unwrap(),
        "CaptureActiveWindow"
    );
    assert!(target_method(CaptureTarget::AllMonitors).is_err());
}
