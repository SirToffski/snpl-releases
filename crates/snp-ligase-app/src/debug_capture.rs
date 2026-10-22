//! `snip-ligase --debug-capture <dir>` (SPEC section 6.1).
//!
//! The main tool for diagnosing HDR and DPI problems: capture every monitor
//! without opening a window, write `monitor_<n>.png` for each, plus
//! `capture.json` holding every `MonitorInfo` and `FrameDiagnostics`.
//!
//! Everything a capture does is written to both the log file and, when a
//! console is attached, stdout, because this command is often run from a
//! terminal on a machine where the GUI app cannot be used.

use std::fs;
use std::path::Path;

use image::{ExtendedColorType, ImageEncoder as _, RgbaImage, codecs::png::PngEncoder};
use serde::Serialize;
use snp_ligase_platform::{
    CaptureOptions, CurrentPlatform, FrameDiagnostics, MonitorInfo, Platform, PlatformEnvironment,
};

/// The `capture.json` document: one entry per monitor.
#[derive(Debug, Serialize)]
struct CaptureReport {
    /// Program version that produced the capture.
    version: &'static str,
    /// Which backend answered, on which session, in which desktop
    /// (LINUX_SUPPORT.md sections 4 and 10): the first thing a Linux bug
    /// report is read for.
    platform: PlatformEnvironment,
    /// The options the capture ran with.
    options: CaptureOptions,
    /// One entry per monitor, in enumeration order.
    monitors: Vec<MonitorReport>,
}

/// One monitor's half of `capture.json`.
#[derive(Debug, Serialize)]
struct MonitorReport {
    index: usize,
    info: MonitorInfo,
    diagnostics: FrameDiagnostics,
    /// The file this monitor's pixels were written to, relative to the output
    /// directory.
    file: String,
}

/// Captures every monitor and writes the report.
///
/// The folders the program writes to are printed and logged first
/// (LINUX_SUPPORT.md section 3, item 4): this command is the one that is run on
/// a machine whose GUI cannot be used, and on a platform where the capture is
/// not there yet it is the only thing it can answer.
///
/// # Errors
///
/// If the directory cannot be created or written, or the capture itself fails.
pub fn run(directory: &Path, paths: &crate::paths::Paths) -> anyhow::Result<()> {
    let platform = CurrentPlatform::new();
    let environment = platform.environment();

    println!("{}", paths.summary());
    println!(
        "platform: backend {}, session {}, desktop {}",
        environment.backend,
        environment.session_type.as_deref().unwrap_or("-"),
        environment.desktop.as_deref().unwrap_or("-"),
    );
    tracing::info!(
        config_dir = %paths.config_dir.display(),
        log_dir = %paths.log_dir.display(),
        backend = environment.backend,
        session_type = environment.session_type.as_deref().unwrap_or("-"),
        desktop = environment.desktop.as_deref().unwrap_or("-"),
        "debug capture"
    );

    let monitors = platform.monitors()?;
    print_monitors(&monitors);

    let capture = platform.capture_all(&CaptureOptions::default())?;
    if capture.frames.is_empty() {
        anyhow::bail!("the capture produced no frames");
    }

    fs::create_dir_all(directory)
        .map_err(|err| anyhow::anyhow!("could not create {}: {err}", directory.display()))?;

    let mut reports = Vec::with_capacity(capture.frames.len());
    for (index, frame) in capture.frames.iter().enumerate() {
        let file = format!("monitor_{index}.png");
        let path = directory.join(&file);
        write_png(&path, &frame.image)?;

        println!(
            "monitor {index}: {} {}x{} via {} (HDR {}, SDR white {:?} nits) -> {}",
            frame.info.id,
            frame.diagnostics.width,
            frame.diagnostics.height,
            frame.diagnostics.api_used,
            frame.info.hdr_active,
            frame.info.sdr_white_nits,
            path.display()
        );
        println!(
            "    format {} peak value {:.3} elapsed {:.0} ms",
            frame.diagnostics.pixel_format,
            frame.diagnostics.max_channel_value,
            frame.diagnostics.elapsed_ms
        );

        reports.push(MonitorReport {
            index,
            info: frame.info.clone(),
            diagnostics: frame.diagnostics.clone(),
            file,
        });
    }

    let report = CaptureReport {
        version: env!("CARGO_PKG_VERSION"),
        platform: environment,
        options: CaptureOptions::default(),
        monitors: reports,
    };
    let json_path = directory.join("capture.json");
    let json = serde_json::to_string_pretty(&report)?;
    fs::write(&json_path, json)
        .map_err(|err| anyhow::anyhow!("could not write {}: {err}", json_path.display()))?;

    println!("wrote {}", json_path.display());
    Ok(())
}

/// Writes one frame as a PNG, creating the file only when it is complete so a
/// failed write never leaves a half-written image behind (SPEC section 9).
fn write_png(path: &Path, image: &RgbaImage) -> anyhow::Result<()> {
    let temporary = path.with_extension("png.part");
    {
        let file = fs::File::create(&temporary)
            .map_err(|err| anyhow::anyhow!("could not create {}: {err}", temporary.display()))?;
        let encoder = PngEncoder::new(file);
        encoder
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgba8,
            )
            .map_err(|err| anyhow::anyhow!("could not encode {}: {err}", temporary.display()))?;
    }

    fs::rename(&temporary, path)
        .map_err(|err| anyhow::anyhow!("could not move into {}: {err}", path.display()))?;
    Ok(())
}

/// Prints the monitor list before capturing, so a wrong enumeration is obvious
/// even before the pixels are examined.
fn print_monitors(monitors: &[MonitorInfo]) {
    println!("{} monitor(s) detected:", monitors.len());
    for monitor in monitors {
        let marker = if monitor.is_primary {
            "primary"
        } else {
            "       "
        };
        println!(
            "  {} {} at ({}, {}) {}x{}, scale {:.2}, HDR {}, SDR white {:?} nits",
            marker,
            monitor.id,
            monitor.rect.x,
            monitor.rect.y,
            monitor.rect.w,
            monitor.rect.h,
            monitor.scale_factor,
            monitor.hdr_active,
            monitor.sdr_white_nits,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report has to stay valid JSON for the human's tooling.
    #[test]
    fn the_report_serialises() {
        let report = CaptureReport {
            version: "0.1.0",
            platform: PlatformEnvironment {
                backend: "x11",
                session_type: Some("x11".to_owned()),
                desktop: Some("XFCE".to_owned()),
            },
            options: CaptureOptions::default(),
            monitors: vec![MonitorReport {
                index: 0,
                info: MonitorInfo {
                    id: "\\\\.\\DISPLAY1".to_owned(),
                    rect: snp_ligase_platform::RectI {
                        x: 0,
                        y: 0,
                        w: 4,
                        h: 4,
                    },
                    scale_factor: 1.0,
                    hdr_active: false,
                    sdr_white_nits: Some(200.0),
                    is_primary: true,
                },
                diagnostics: FrameDiagnostics {
                    api_used: snp_ligase_platform::CaptureApi::Wgc,
                    pixel_format: "Bgra8".to_owned(),
                    width: 4,
                    height: 4,
                    row_pitch: 16,
                    max_channel_value: 1.0,
                    sdr_white_nits: 200.0,
                    elapsed_ms: 12.0,
                },
                file: "monitor_0.png".to_owned(),
            }],
        };

        let json = serde_json::to_string(&report).expect("report is serialisable");
        assert!(json.contains("monitor_0.png"), "{json}");
        assert!(json.contains("\"hdr_active\":false"), "{json}");
        assert!(json.contains("\"api_used\":\"WGC\""), "{json}");
        // LINUX_SUPPORT.md section 4: which backend and which session produced
        // the capture is the first line of a Linux bug report.
        assert!(json.contains("\"backend\":\"x11\""), "{json}");
        assert!(json.contains("\"session_type\":\"x11\""), "{json}");
        assert!(json.contains("\"desktop\":\"XFCE\""), "{json}");
    }
}
