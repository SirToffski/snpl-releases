//! Capture: one Windows.Graphics.Capture session per monitor, with a GDI
//! fallback (SPEC section 5.4).
//!
//! The shape of this module comes from three rules in SPEC:
//!
//! 1. Every monitor is captured on its own thread, so all frames come from
//!    nearly the same instant.
//! 2. HDR monitors are captured as FP16 (`Rgba16F`) and converted through
//!    [`snp_ligase_core::color`]; SDR monitors stay 8-bit so their pixels are
//!    bit-exact.
//! 3. If no frame arrives within a second, or the API fails, fall back to GDI
//!    `BitBlt` and warn that colours may be wrong on HDR.
//!
//! The `windows-capture` crate owns the D3D11 plumbing and gives us the raw
//! bytes; we own everything above that: pixel format choice, pitch handling,
//! conversion and diagnostics.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ::windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HDC, HGDIOBJ,
    ReleaseDC, SRCCOPY, SelectObject,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON, ICONINFO,
};
use half::f16;
use image::RgbaImage;
use snp_ligase_core::color;
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use super::monitors::RawMonitor;
use crate::{
    CaptureApi, CaptureOptions, Error, FrameDiagnostics, HdrHighlights, MonitorFrame, MonitorInfo,
    RectI, Result, VirtualCapture,
};

/// How long to wait for a Windows.Graphics.Capture frame before falling back to
/// GDI (SPEC section 5.4).
const WGC_TIMEOUT: Duration = Duration::from_millis(1000);

/// How long a capture waits for the previous WGC worker on the same monitor to
/// finish its teardown before giving up on WGC for that monitor. A worker sends
/// its frame before the session closes, so back-to-back captures can find it
/// still alive for a moment; falling back to GDI then would cost an HDR monitor
/// its colours for no reason.
const WGC_REUSE_GRACE: Duration = Duration::from_millis(250);

// Keep the actual thread handle, not the result channel: a worker can send a
// frame and then hang during session teardown. Only thread exit allows reuse.
static WGC_WORKERS: LazyLock<Mutex<HashMap<String, thread::JoinHandle<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn start_wgc(
    workers: &Mutex<HashMap<String, thread::JoinHandle<()>>>,
    id: &str,
    grace: Duration,
    capture: impl FnOnce(Sender<Outcome>) + Send + 'static,
) -> std::result::Result<Option<Receiver<Outcome>>, String> {
    let deadline = Instant::now() + grace;
    let mut workers = loop {
        let workers = workers
            .lock()
            .map_err(|err| format!("WGC worker lock: {err}"))?;
        let busy = workers
            .get(id)
            .is_some_and(|previous| !previous.is_finished());
        if !busy {
            break workers;
        }
        drop(workers);
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(5));
    };
    if let Some(previous) = workers.remove(id)
        && previous.join().is_err()
    {
        tracing::warn!(monitor = id, "previous WGC worker panicked");
    }
    let (sender, receiver) = mpsc::channel();
    let worker = thread::Builder::new()
        .name(format!("capture-{id}"))
        .spawn(move || capture(sender))
        .map_err(|err| format!("could not start a capture thread: {err}"))?;
    workers.insert(id.to_owned(), worker);
    Ok(Some(receiver))
}

/// What one capture thread hands back: pixels, or a reason it could not.
///
/// Deliberately not the crate's `Result`, because the failure is a message for
/// the log rather than an error the caller should propagate.
type Outcome = std::result::Result<MonitorPixels, String>;

/// Pixels taken out of one frame, before they are wrapped in a `MonitorFrame`.
struct MonitorPixels {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    color_format: ColorFormat,
    /// Bytes from the start of one row to the next (SPEC section 5.4 step 2).
    row_pitch: u32,
    max_channel_value: f32,
    sdr_white_nits: f32,
}

/// Captures every monitor, each on its own thread, so all frames come from
/// nearly the same instant (SPEC section 5.4).
///
/// `raw` carries the handles the capture API needs; `infos` matches it one to
/// one and says which monitors are HDR.
pub(crate) fn capture_all(
    raw: &[RawMonitor],
    infos: &[MonitorInfo],
    opts: &CaptureOptions,
) -> Result<VirtualCapture> {
    let started = Instant::now();
    let deadline = started + WGC_TIMEOUT;

    let mut receivers = Vec::with_capacity(infos.len());
    for (monitor, info) in raw.iter().zip(infos.iter()) {
        let thread_monitor = monitor.clone();
        let thread_info = info.clone();
        let opts = *opts;

        let receiver = match start_wgc(&WGC_WORKERS, &info.id, WGC_REUSE_GRACE, move |sender| {
            capture_monitor(thread_monitor, thread_info, opts, sender);
        }) {
            Ok(Some(receiver)) => Some(receiver),
            Ok(None) => {
                tracing::warn!(monitor = %info.id,
                    "previous WGC thread is still alive; skipping WGC and using GDI");
                None
            }
            Err(err) => {
                tracing::warn!(monitor = %info.id, error = %err, "using the GDI fallback");
                None
            }
        };

        receivers.push(receiver);
    }

    let mut frames = Vec::with_capacity(infos.len());
    for (info, receiver) in infos.iter().zip(receivers) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let outcome = match receiver.map(|receiver| receiver.recv_timeout(remaining)) {
            None => Err(String::new()),
            Some(Ok(outcome)) => outcome,
            Some(Err(RecvTimeoutError::Timeout)) => {
                tracing::warn!(
                    monitor = %info.id,
                    timeout_ms = WGC_TIMEOUT.as_millis(),
                    "no Windows.Graphics.Capture frame in time; falling back to GDI"
                );
                Err(String::new())
            }
            Some(Err(RecvTimeoutError::Disconnected)) => {
                tracing::warn!(
                    monitor = %info.id,
                    "the capture thread ended without reporting; falling back to GDI"
                );
                Err(String::new())
            }
        };

        let frame = match outcome {
            Ok(pixels) => frame_from_pixels(info, pixels, started),
            Err(message) => {
                if !message.is_empty() {
                    tracing::warn!(monitor = %info.id, error = %message, "capture failed; falling back to GDI");
                }
                gdi_frame(raw, info, opts.include_cursor, started)?
            }
        };
        frames.push(frame);
    }

    Ok(VirtualCapture { frames })
}

/// Runs one monitor's Windows.Graphics.Capture session on this thread.
///
/// The elapsed time is measured by the parent thread, which owns the deadline
/// for the GDI fallback.
fn capture_monitor(
    monitor: RawMonitor,
    info: MonitorInfo,
    opts: CaptureOptions,
    sender: Sender<Outcome>,
) {
    // HDR monitors come back as FP16 and are converted here; SDR monitors stay
    // 8-bit so their pixels are bit-exact (SPEC section 5.4).
    let color_format = if info.hdr_active {
        ColorFormat::Rgba16F
    } else {
        ColorFormat::Bgra8
    };
    let cursor = if opts.include_cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };

    let sdr_white_nits = opts.sdr_white_override_nits.unwrap_or_else(|| {
        info.sdr_white_nits.unwrap_or_else(|| {
            tracing::warn!(
                monitor = %info.id,
                default_nits = color::DEFAULT_SDR_WHITE_NITS,
                "no SDR white level reported; using the default"
            );
            color::DEFAULT_SDR_WHITE_NITS
        })
    });
    if let Some(override_nits) = opts.sdr_white_override_nits {
        tracing::info!(
            monitor = %info.id,
            override_nits,
            reported_nits = info.sdr_white_nits,
            "the SDR white level override is in force (SPEC section 10)"
        );
    }

    // SPEC 5.5's highlight policy is chosen by the caller; M7 implemented
    // RollOff, so there is no fallback left to map.
    let highlight = match opts.hdr_highlights {
        HdrHighlights::Clip => color::HighlightMode::Clip,
        HdrHighlights::RollOff => color::HighlightMode::RollOff,
    };

    let settings = Settings::new(
        Monitor::from_raw_hmonitor(monitor.hmonitor.0),
        cursor,
        // No border: the yellow capture rectangle would end up in every frame
        // (SPEC section 5.4).
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        color_format,
        Flags {
            sender: sender.clone(),
            info: info.clone(),
            sdr_white_nits,
            highlight,
        },
    );

    if let Err(err) = OneShot::start(settings) {
        // The handler normally reports through the channel; a start failure
        // means it never ran, so report it here instead of making the parent
        // wait for the timeout.
        let _ = sender.send(Err(format!("capture session failed: {err}")));
    }
}

/// Data the capture handler needs, passed in through `Settings::flags`.
struct Flags {
    /// Kept in sync with the sender in [`capture_monitor`], so both paths can
    /// report back.
    sender: Sender<Outcome>,
    info: MonitorInfo,
    sdr_white_nits: f32,
    /// The highlight policy the caller asked for, resolved once per capture
    /// (R26).
    highlight: color::HighlightMode,
}

/// A one-shot Windows.Graphics.Capture session: take the first frame, hand it
/// over, stop.
struct OneShot {
    flags: Flags,
    taken: bool,
}

impl GraphicsCaptureApiHandler for OneShot {
    type Flags = Flags;
    type Error = Error;

    fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            flags: ctx.flags,
            taken: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        let result = if self.taken {
            Ok(())
        } else {
            self.grab_first_frame(frame)
        };

        // Stop after the first frame whatever happened: a screenshot never
        // needs a second one, and the session has to end for the thread to
        // return at all.
        capture_control.stop();
        self.taken = true;
        result
    }

    fn on_closed(&mut self) -> std::result::Result<(), Self::Error> {
        Ok(())
    }
}

impl OneShot {
    /// Copies the first frame's pixels out and sends them to the parent.
    ///
    /// The row pitch is respected here, because capture APIs pad rows and a
    /// screenshot with sheared rows is the classic symptom of ignoring it.
    fn grab_first_frame(&mut self, frame: &mut Frame) -> std::result::Result<(), crate::Error> {
        let mut buffer = frame.buffer().map_err(|err| Error::Capture {
            message: err.to_string(),
        })?;

        let width = buffer.width();
        let height = buffer.height();
        let pitch = buffer.row_pitch() as usize;
        let color_format = buffer.color_format();
        let raw = buffer.as_raw_buffer();

        let (pixels, max_channel_value) = match color_format {
            ColorFormat::Rgba16F => {
                let scaled = color::rgba16f_to_rgba8(
                    raw,
                    width,
                    height,
                    pitch,
                    self.flags.sdr_white_nits,
                    self.flags.highlight,
                )?;
                (
                    scaled,
                    max_linear_value(raw, width, height, pitch, self.flags.sdr_white_nits),
                )
            }
            ColorFormat::Rgba8 | ColorFormat::Bgra8 => {
                (reorder_to_rgba8(raw, width, height, pitch), 1.0)
            }
        };

        tracing::debug!(
            monitor = %self.flags.info.id,
            width,
            height,
            pitch,
            format = ?color_format,
            max_channel_value,
            "captured a frame"
        );

        let _ = self.flags.sender.send(Ok(MonitorPixels {
            pixels,
            width,
            height,
            color_format,
            row_pitch: u32::try_from(pitch).unwrap_or(u32::MAX),
            max_channel_value,
            sdr_white_nits: self.flags.sdr_white_nits,
        }));

        Ok(())
    }
}

/// Highest linear channel value in an FP16 frame, scaled so that 1.0 is SDR
/// white. This is what tells the human, from the log file alone, whether a
/// capture clipped everything or washed it out.
///
/// RGB only: the alpha channel is always 1.0 and would hide a real peak below
/// it (R26).
fn max_linear_value(raw: &[u8], width: u32, height: u32, pitch: usize, sdr_white_nits: f32) -> f32 {
    const BYTES_PER_PIXEL: usize = 8; // four f16 channels
    let row_bytes = width as usize * BYTES_PER_PIXEL;
    let scale = color::sdr_white_scale(sdr_white_nits);
    let mut max = 0.0f32;

    for y in 0..height as usize {
        let start = y * pitch;
        let Some(row) = raw.get(start..start + row_bytes) else {
            break;
        };
        for pixel in row.as_chunks::<BYTES_PER_PIXEL>().0 {
            for channel in 0..3 {
                let bytes = [pixel[channel * 2], pixel[channel * 2 + 1]];
                let value = f16::from_le_bytes(bytes).to_f32() / scale;
                if value > max {
                    max = value;
                }
            }
        }
    }

    max
}

/// Reorders an 8-bit capture buffer to straight RGBA, top-down, alpha 255.
///
/// Windows hands over BGRA (or RGBA) and pads rows, so both the channel order
/// and the pitch are dealt with here.
fn reorder_to_rgba8(src: &[u8], width: u32, height: u32, pitch: usize) -> Vec<u8> {
    let row_bytes = width as usize * 4;
    let mut out = vec![0u8; row_bytes * height as usize];

    for y in 0..height as usize {
        let start = y * pitch;
        let Some(row) = src.get(start..start + row_bytes) else {
            break;
        };
        let dst = &mut out[y * row_bytes..y * row_bytes + row_bytes];
        for (pixel, out_pixel) in row
            .as_chunks::<4>()
            .0
            .iter()
            .zip(dst.as_chunks_mut::<4>().0.iter_mut())
        {
            out_pixel[0] = pixel[2];
            out_pixel[1] = pixel[1];
            out_pixel[2] = pixel[0];
            out_pixel[3] = 255;
        }
    }

    out
}

/// Builds the frame for a successful capture.
fn frame_from_pixels(info: &MonitorInfo, pixels: MonitorPixels, started: Instant) -> MonitorFrame {
    // SPEC section 5.4 step 5: when the frame's size is not the monitor's size,
    // say so with both numbers and use the frame as-is at the rectangle origin.
    let monitor_size = (info.rect.w.max(0) as u32, info.rect.h.max(0) as u32);
    if size_mismatch((pixels.width, pixels.height), monitor_size) {
        tracing::error!(
            monitor = %info.id,
            frame_size = format!("{}x{}", pixels.width, pixels.height),
            monitor_size = format!("{}x{}", monitor_size.0, monitor_size.1),
            "captured frame is not the size of the monitor rectangle"
        );
    }

    // A size that does not match the pixel count should not be possible, but
    // this is OS data: degrade to a blank frame instead of panicking.
    let image =
        RgbaImage::from_raw(pixels.width, pixels.height, pixels.pixels).unwrap_or_else(|| {
            tracing::error!(
                monitor = %info.id,
                width = pixels.width,
                height = pixels.height,
                "captured pixels did not fit the reported size; using a blank frame"
            );
            RgbaImage::new(pixels.width, pixels.height)
        });

    let diagnostics = FrameDiagnostics {
        api_used: CaptureApi::Wgc,
        pixel_format: format!("{:?}", pixels.color_format),
        width: pixels.width,
        height: pixels.height,
        row_pitch: pixels.row_pitch,
        max_channel_value: pixels.max_channel_value,
        sdr_white_nits: pixels.sdr_white_nits,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    };

    MonitorFrame {
        info: info.clone(),
        image,
        diagnostics,
        // Windows frames hold their own pixels: 1 frame px = 1 virtual px.
        stored_pixels: crate::StoredPixels::Virtual,
        native_scale_x: 1.0,
        native_scale_y: 1.0,
    }
}

/// True when a captured frame does not match the monitor it came from, which
/// SPEC section 5.4 step 5 says must be logged with both sizes (the frame is
/// still used as-is, positioned at the rectangle origin).
#[must_use]
pub(crate) fn size_mismatch(frame: (u32, u32), monitor: (u32, u32)) -> bool {
    frame != monitor
}

/// GDI capture of one monitor rectangle (SPEC section 5.4).
///
/// The screen DC spans the whole virtual desktop, so a monitor at negative
/// coordinates is just a `BitBlt` at that origin.
fn capture_with_gdi(rect: RectI, include_cursor: bool) -> Result<(Vec<u8>, u32, u32)> {
    if rect.w <= 0 || rect.h <= 0 {
        return Err(Error::Capture {
            message: format!("monitor rectangle {rect:?} is empty"),
        });
    }

    let width = rect.w;
    let height = rect.h;
    let byte_count = width as usize * height as usize * 4;

    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        return Err(Error::Capture {
            message: "GetDC(NULL) failed".to_owned(),
        });
    }

    let memory = unsafe { CreateCompatibleDC(Some(screen)) };
    if memory.is_invalid() {
        unsafe { ReleaseDC(None, screen) };
        return Err(Error::Capture {
            message: "CreateCompatibleDC failed".to_owned(),
        });
    }

    let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
    if bitmap.is_invalid() {
        unsafe {
            let _ = DeleteDC(memory);
            let _ = ReleaseDC(None, screen);
        }
        return Err(Error::Capture {
            message: "CreateCompatibleBitmap failed".to_owned(),
        });
    }

    // The closure keeps every failure on the common resource-cleanup path.
    let captured = (|| -> Result<Vec<u8>> {
        let previous = checked_selection(unsafe { SelectObject(memory, bitmap.into()) })?;
        unsafe {
            BitBlt(
                memory,
                0,
                0,
                width,
                height,
                Some(screen),
                rect.x,
                rect.y,
                // CAPTUREBLT so layered windows (menus, tooltips) are included in
                // the fallback frame (R26, SPEC M5's human check).
                SRCCOPY | CAPTUREBLT,
            )
        }
        .map_err(|err| Error::Capture {
            message: format!("BitBlt failed: {err}"),
        })?;
        // A cursor that cannot be drawn costs the cursor, never the frame: GDI
        // is the last way to get a screenshot at all.
        if include_cursor && let Err(err) = draw_cursor(memory, rect) {
            tracing::warn!(error = %err, "could not draw the cursor into the GDI frame; capturing without it");
        }

        // GetDIBits requires this bitmap to be deselected from every DC.
        checked_selection(unsafe { SelectObject(memory, previous) })?;

        let mut pixels = vec![0u8; byte_count];
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(0),
                biWidth: width,
                biHeight: -height, // negative: rows come back top-down, no flipping
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: byte_count as u32,
                ..Default::default()
            },
            bmiColors: [Default::default(); 1],
        };
        let lines = unsafe {
            // Catch regressions in the Win32 contract in debug/self-check runs.
            debug_assert_ne!(
                ::windows::Win32::Graphics::Gdi::GetCurrentObject(
                    memory,
                    ::windows::Win32::Graphics::Gdi::OBJ_BITMAP
                ),
                bitmap.into(),
                "GetDIBits requires a deselected bitmap",
            );
            GetDIBits(
                memory,
                bitmap,
                0,
                height as u32,
                Some(pixels.as_mut_ptr().cast()),
                &raw mut header,
                DIB_RGB_COLORS,
            )
        };

        if lines != height {
            return Err(Error::Capture {
                message: format!("GetDIBits copied {lines} of {height} rows"),
            });
        }
        Ok(pixels)
    })();

    // Destroy the memory DC first: even a failed restoration or draw must not
    // leave the bitmap selected when DeleteObject runs.
    unsafe {
        let _ = DeleteDC(memory);
        let _ = DeleteObject(bitmap.into());
        let _ = ReleaseDC(None, screen);
    }
    let mut pixels = captured?;

    // GetDIBits hands back BGRA with an unused alpha byte.
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }

    Ok((pixels, width as u32, height as u32))
}

fn checked_selection(previous: HGDIOBJ) -> Result<HGDIOBJ> {
    if previous.is_invalid() {
        Err(Error::Capture {
            message: "SelectObject failed in GDI capture".to_owned(),
        })
    } else {
        Ok(previous)
    }
}

fn cursor_origin(position: (i32, i32), hotspot: (u32, u32), rect: RectI) -> (i32, i32) {
    (
        position.0 - rect.x - hotspot.0 as i32,
        position.1 - rect.y - hotspot.1 as i32,
    )
}

fn draw_cursor(memory: HDC, rect: RectI) -> Result<()> {
    let mut cursor = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetCursorInfo(&mut cursor) }.map_err(|err| Error::Capture {
        message: format!("GetCursorInfo failed: {err}"),
    })?;
    if cursor.flags.0 & CURSOR_SHOWING.0 == 0 {
        return Ok(());
    }
    let mut icon = ICONINFO::default();
    unsafe { GetIconInfo(HICON(cursor.hCursor.0), &mut icon) }.map_err(|err| Error::Capture {
        message: format!("GetIconInfo failed: {err}"),
    })?;
    let (x, y) = cursor_origin(
        (cursor.ptScreenPos.x, cursor.ptScreenPos.y),
        (icon.xHotspot, icon.yHotspot),
        rect,
    );
    // DrawIconEx clips to this monitor's bitmap, including a cursor straddling
    // an edge. GetIconInfo creates bitmaps owned by us; the cursor is borrowed.
    let drawn = unsafe {
        DrawIconEx(
            memory,
            x,
            y,
            HICON(cursor.hCursor.0),
            0,
            0,
            0,
            None,
            DI_NORMAL,
        )
    };
    unsafe {
        if !icon.hbmMask.is_invalid() {
            let _ = DeleteObject(icon.hbmMask.into());
        }
        if !icon.hbmColor.is_invalid() {
            let _ = DeleteObject(icon.hbmColor.into());
        }
    }
    drawn.map_err(|err| Error::Capture {
        message: format!("DrawIconEx failed: {err}"),
    })
}

/// Builds the GDI fallback frame for one monitor.
fn gdi_frame(
    raw: &[RawMonitor],
    info: &MonitorInfo,
    include_cursor: bool,
    started: Instant,
) -> Result<MonitorFrame> {
    let monitor = raw
        .iter()
        .find(|monitor| monitor.device_name == info.id)
        .ok_or_else(|| Error::Capture {
            message: format!("no monitor named {}", info.id),
        })?;

    let (pixels, width, height) = capture_with_gdi(monitor.rect, include_cursor)?;
    let image = RgbaImage::from_raw(width, height, pixels).ok_or_else(|| Error::Capture {
        message: "GDI pixels did not fit the monitor rectangle".to_owned(),
    })?;

    tracing::warn!(
        monitor = %info.id,
        hdr = info.hdr_active,
        include_cursor,
        "using the GDI fallback; colours may be wrong on an HDR monitor"
    );

    let diagnostics = FrameDiagnostics {
        api_used: CaptureApi::Gdi,
        pixel_format: "Bgra8".to_owned(),
        width,
        height,
        // GDI rows are tightly packed, so there is no pitch to report.
        row_pitch: 0,
        max_channel_value: 0.0,
        sdr_white_nits: info.sdr_white_nits.unwrap_or(0.0),
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    };

    Ok(MonitorFrame {
        info: info.clone(),
        image,
        diagnostics,
        // Windows frames hold their own pixels: 1 frame px = 1 virtual px.
        stored_pixels: crate::StoredPixels::Virtual,
        native_scale_x: 1.0,
        native_scale_y: 1.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win3_timed_out_wgc_worker_stays_registered_until_thread_exit() {
        let workers = Mutex::new(HashMap::new());
        let (finish, wait) = mpsc::channel();
        let receiver = start_wgc(&workers, "timed-out", Duration::ZERO, move |_sender| {
            wait.recv().unwrap();
        })
        .unwrap()
        .unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(10)),
            Err(RecvTimeoutError::Timeout)
        ));
        assert!(
            start_wgc(&workers, "timed-out", Duration::ZERO, |_| panic!(
                "second live session"
            ))
            .unwrap()
            .is_none()
        );
        finish.send(()).unwrap();
        for (_, handle) in workers.into_inner().unwrap() {
            handle.join().unwrap();
        }
    }

    #[test]
    fn win3_live_wgc_worker_blocks_replacement_even_after_sending_a_result() {
        let workers = Mutex::new(HashMap::new());
        let (finish, wait) = mpsc::channel();
        let first = start_wgc(&workers, "monitor-a", Duration::ZERO, move |sender| {
            sender.send(Err("first result".to_owned())).unwrap();
            wait.recv().unwrap(); // Model a session stuck in teardown.
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            first
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .err()
                .as_deref(),
            Some("first result")
        );
        for _ in 0..8 {
            assert!(
                start_wgc(&workers, "monitor-a", Duration::ZERO, |_| panic!(
                    "second live session"
                ))
                .unwrap()
                .is_none()
            );
        }
        let other = start_wgc(&workers, "monitor-b", Duration::ZERO, |sender| {
            sender.send(Err("other monitor".to_owned())).unwrap();
        })
        .unwrap()
        .unwrap();
        assert!(other.recv_timeout(Duration::from_secs(2)).is_ok());
        finish.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !workers.lock().unwrap()["monitor-a"].is_finished() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        let next = start_wgc(&workers, "monitor-a", Duration::ZERO, |sender| {
            sender.send(Err("fresh session".to_owned())).unwrap();
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            next.recv_timeout(Duration::from_secs(2))
                .unwrap()
                .err()
                .as_deref(),
            Some("fresh session")
        );
        for (_, handle) in workers.into_inner().unwrap() {
            handle.join().unwrap();
        }
    }

    #[test]
    fn a_worker_finishing_its_teardown_within_the_grace_is_reused() {
        let workers = Mutex::new(HashMap::new());
        let first = start_wgc(&workers, "monitor-a", Duration::ZERO, |sender| {
            sender.send(Err("first".to_owned())).unwrap();
            thread::sleep(Duration::from_millis(50)); // Session teardown.
        })
        .unwrap()
        .unwrap();
        assert!(first.recv_timeout(Duration::from_secs(2)).is_ok());
        let next = start_wgc(&workers, "monitor-a", Duration::from_secs(5), |sender| {
            sender.send(Err("second".to_owned())).unwrap();
        })
        .unwrap()
        .expect("a worker that finished within the grace must not force GDI");
        assert_eq!(
            next.recv_timeout(Duration::from_secs(2))
                .unwrap()
                .err()
                .as_deref(),
            Some("second")
        );
        for (_, handle) in workers.into_inner().unwrap() {
            handle.join().unwrap();
        }
    }

    #[test]
    fn win3_gdi_rejects_both_select_object_failure_values() {
        assert!(checked_selection(HGDIOBJ::default()).is_err());
        assert!(checked_selection(HGDIOBJ(-1isize as _)).is_err());
        assert!(checked_selection(HGDIOBJ(1usize as _)).is_ok());
    }

    #[test]
    fn win3_cursor_uses_monitor_origin_and_hotspot_including_negative_origins() {
        assert_eq!(
            cursor_origin((-1900, 100), (5, 7), RectI::new(-1920, 0, 1920, 1080)),
            (15, 93)
        );
        // Keep negative local coordinates: DrawIconEx clips cursors on seams.
        assert_eq!(
            cursor_origin((1921, 2), (5, 7), RectI::new(1920, 0, 1920, 1080)),
            (-4, -5)
        );
    }

    #[test]
    #[ignore = "reads the real desktop; run explicitly for WIN3"]
    fn win3_real_gdi_capture_with_and_without_cursor() {
        let raw = super::super::monitors::enumerate().unwrap();
        assert!(!raw.is_empty());
        for monitor in raw {
            for include_cursor in [false, true] {
                let (pixels, width, height) =
                    capture_with_gdi(monitor.rect, include_cursor).unwrap();
                assert_eq!(
                    (width, height),
                    (monitor.rect.w as u32, monitor.rect.h as u32)
                );
                assert_eq!(pixels.len(), width as usize * height as usize * 4);
                assert!(
                    pixels
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .all(|pixel| pixel[3] == 255)
                );
                assert!(
                    pixels
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|pixel| pixel[..3] != [0, 0, 0])
                );
                println!(
                    "GDI {} {}x{} cursor={include_cursor}: valid opaque pixels",
                    monitor.device_name, width, height
                );
            }
        }
    }

    #[test]
    fn a_frame_of_the_monitors_own_size_is_not_a_mismatch() {
        assert!(!size_mismatch((1920, 1080), (1920, 1080)));
    }

    #[test]
    fn a_smaller_frame_is_a_mismatch() {
        assert!(size_mismatch((1920, 1080), (2560, 1440)));
        assert!(size_mismatch((0, 0), (1920, 1080)));
    }
}
