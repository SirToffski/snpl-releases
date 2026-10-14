//! Monitor enumeration with DPI, HDR state and the SDR white level
//! (SPEC section 5.3).
//!
//! Three separate sources have to be stitched together, which is the fiddly
//! part of getting captures right on a mixed setup:
//!
//! - `EnumDisplayMonitors` / `GetMonitorInfoW` give the rectangle, the primary
//!   flag and the device name.
//! - `GetDpiForMonitor` gives the scale factor, which exists for logging only.
//! - DXGI outputs give the HDR state, matched to monitors by `HMONITOR`.
//! - `QueryDisplayConfig` gives the SDR white level, matched by device name.
//!
//! Everything is re-enumerated on every capture: monitors move, dock, and
//! switch HDR on and off while the program is running.

use std::collections::HashMap;
use std::mem;

use ::windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use ::windows::Win32::Foundation::{LPARAM, RECT, TRUE};
use ::windows::Win32::Graphics::Dxgi::{
    Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, CreateDXGIFactory1, IDXGIFactory1,
    IDXGIOutput6,
};
use ::windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use ::windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use ::windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use ::windows::core::{BOOL, Interface};

use crate::{MonitorInfo, RectI};

/// One monitor as Win32 reports it, with the handle the capture path still
/// needs (`HMONITOR` is what turns a monitor into a capture item).
///
/// # Safety
///
/// `RawMonitor` is sent to the per-monitor capture threads. An `HMONITOR` is an
/// opaque handle to a display, not a pointer into memory we dereference: the
/// only things done with it are Win32 calls on the owning thread, which is
/// exactly what each of those threads is for. `windows-capture` makes the same
/// judgement for its `Monitor` type.
#[derive(Debug, Clone)]
pub(crate) struct RawMonitor {
    pub(crate) hmonitor: HMONITOR,
    pub(crate) device_name: String,
    pub(crate) rect: RectI,
    pub(crate) primary: bool,
}

/// Safety: see the note on [`RawMonitor`]. An `HMONITOR` is an opaque handle,
/// and this type only hands it to the thread that uses it for Win32 calls.
unsafe impl Send for RawMonitor {}

/// Enumerates the desktop's monitors.
///
/// Called before every capture, not once at startup (SPEC section 5.3).
pub(crate) fn enumerate() -> crate::Result<Vec<RawMonitor>> {
    let mut monitors: Vec<RawMonitor> = Vec::new();

    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enumerate_callback),
            LPARAM((&raw mut monitors).cast::<std::ffi::c_void>() as isize),
        )
        .ok()?;
    }

    Ok(monitors)
}

/// Collects one monitor per callback call.
///
/// # Safety
///
/// Called by Win32 with `state` pointing at the `Vec` that [`enumerate`] passed
/// in. It only pushes to that vector.
unsafe extern "system" fn enumerate_callback(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    state: LPARAM,
) -> BOOL {
    let monitors = unsafe { &mut *(state.0 as *mut Vec<RawMonitor>) };

    if let Some(monitor) = raw_from_handle(hmonitor) {
        monitors.push(monitor);
    }

    TRUE
}

/// Reads the rectangle, device name and primary flag for one `HMONITOR`.
fn raw_from_handle(hmonitor: HMONITOR) -> Option<RawMonitor> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: u32::try_from(mem::size_of::<MONITORINFOEXW>()).ok()?,
            rcMonitor: RECT::default(),
            rcWork: RECT::default(),
            dwFlags: 0,
        },
        szDevice: [0; 32],
    };

    let ok = unsafe { GetMonitorInfoW(hmonitor, (&raw mut info).cast()) };
    if !ok.as_bool() {
        tracing::warn!("GetMonitorInfoW failed for a monitor; skipping it");
        return None;
    }

    let rc = info.monitorInfo.rcMonitor;
    Some(RawMonitor {
        hmonitor,
        device_name: wide_to_string(&info.szDevice),
        rect: RectI {
            x: rc.left,
            y: rc.top,
            w: rc.right - rc.left,
            h: rc.bottom - rc.top,
        },
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

/// Adds scale factor, HDR state and SDR white level to the raw monitors.
///
/// The two extra queries are *advisory*: SPEC section 13 says a failure to read
/// a monitor's HDR state or SDR white level only means that monitor is treated
/// as SDR with the default white level, never that there is no screenshot at
/// all. Either error is logged and [`crate::platform::merge`] fills in the
/// defaults.
pub(crate) fn details(raw: &[RawMonitor]) -> crate::Result<Vec<MonitorInfo>> {
    let hdr = hdr_state();
    let sdr_white = sdr_white_levels();

    if let Err(err) = &hdr {
        tracing::warn!(
            error = %err,
            "could not read the HDR state; every monitor is treated as SDR"
        );
    }
    if let Err(err) = &sdr_white {
        tracing::warn!(
            error = %err,
            "could not read the SDR white levels; using the default where it matters"
        );
    }

    Ok(merge(raw, hdr, sdr_white))
}

/// Stitches the raw monitor rectangles together with the HDR map and the SDR
/// white level map, degrading to the safe default for anything missing.
///
/// A pure function on purpose: it is what the OS calls are allowed to fail
/// around, and it can be tested without a desktop.
#[must_use]
pub(crate) fn merge(
    raw: &[RawMonitor],
    hdr: crate::Result<std::collections::HashMap<isize, bool>>,
    sdr_white: crate::Result<std::collections::HashMap<String, f32>>,
) -> Vec<MonitorInfo> {
    let hdr = hdr.unwrap_or_default();
    let sdr_white = sdr_white.unwrap_or_default();

    raw.iter()
        .map(|monitor| {
            let sdr_white_nits = sdr_white.get(&monitor.device_name).copied();
            let hdr_active = hdr
                .get(&(monitor.hmonitor.0 as isize))
                .copied()
                .unwrap_or(false);

            if hdr_active && sdr_white_nits.is_none() {
                tracing::warn!(
                    monitor = %monitor.device_name,
                    default_nits = snp_ligase_core::color::DEFAULT_SDR_WHITE_NITS,
                    "HDR monitor reported no SDR white level; using the default"
                );
            }

            MonitorInfo {
                id: monitor.device_name.clone(),
                rect: monitor.rect,
                // For logging and the UI only. The scale factor is never used
                // to multiply or divide a coordinate (SPEC section 5.2).
                scale_factor: effective_scale(monitor.hmonitor),
                hdr_active,
                sdr_white_nits,
                is_primary: monitor.primary,
            }
        })
        .collect()
}

/// The display scale, e.g. 1.5 for 150%. Defaults to 1.0 when the call fails.
fn effective_scale(hmonitor: HMONITOR) -> f32 {
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;

    match unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &raw mut dpi_x, &raw mut dpi_y) } {
        Ok(()) if dpi_x > 0 => dpi_x as f32 / 96.0,
        Ok(()) => 1.0,
        Err(err) => {
            tracing::debug!(error = %err, "GetDpiForMonitor failed; assuming 100% scale");
            1.0
        }
    }
}

/// Which monitors are currently driven in HDR, keyed by raw `HMONITOR`.
///
/// HDR is decided by the output's colour space (SPEC section 5.3); the output
/// description carries the `HMONITOR` it drives, which is how the two worlds
/// are matched up.
fn hdr_state() -> crate::Result<HashMap<isize, bool>> {
    let mut state = HashMap::new();
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };

    let mut adapter_index = 0u32;
    loop {
        let Ok(adapter) = (unsafe { factory.EnumAdapters(adapter_index) }) else {
            break; // no more adapters
        };
        adapter_index += 1;

        let mut output_index = 0u32;
        loop {
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                break; // no more outputs on this adapter
            };
            output_index += 1;

            let output = match output.cast::<IDXGIOutput6>() {
                Ok(output) => output,
                Err(err) => {
                    tracing::debug!(error = %err, "DXGI output does not expose GetDesc1; skipping");
                    continue;
                }
            };
            let Ok(desc) = (unsafe { output.GetDesc1() }) else {
                tracing::debug!("IDXGIOutput6::GetDesc1 failed; skipping the output");
                continue;
            };

            if !desc.AttachedToDesktop.as_bool() {
                continue; // a disconnected output, not a monitor
            }

            state.insert(
                desc.Monitor.0 as isize,
                desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
            );
        }
    }

    Ok(state)
}

/// SDR white levels in nits, keyed by GDI device name (`\\.\DISPLAY1`).
///
/// The value Windows reports is in thousandths of a nit
/// (`DISPLAYCONFIG_SDR_WHITE_LEVEL::SDRWhiteLevel`), so 200 nits arrives as
/// 200 000 and `nits = SDRWhiteLevel / 1000 * 80` (SPEC section 5.3).
///
/// `QueryDisplayConfig` can legitimately answer `ERROR_INSUFFICIENT_BUFFER`
/// when the display topology changes between the size query and the real one —
/// a documented race, not a reason to lose the screenshot. The size-and-query
/// pair is retried a few times, and the buffers are truncated to the counts the
/// query really filled in.
fn sdr_white_levels() -> crate::Result<HashMap<String, f32>> {
    const ATTEMPTS: usize = 3;

    for attempt in 1..=ATTEMPTS {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        unsafe {
            GetDisplayConfigBufferSizes(
                QDC_ONLY_ACTIVE_PATHS,
                &raw mut path_count,
                &raw mut mode_count,
            )
        }
        .ok()?;

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // The query writes back how many entries it actually used, which can be
        // fewer than were asked for when the topology changed.
        let mut paths_filled = path_count;
        let mut modes_filled = mode_count;

        let outcome = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &raw mut paths_filled,
                paths.as_mut_ptr(),
                &raw mut modes_filled,
                modes.as_mut_ptr(),
                None,
            )
        }
        .ok();

        match outcome {
            Ok(()) => {
                paths.truncate(paths_filled as usize);
                modes.truncate(modes_filled as usize);
                return Ok(white_levels_for(&paths));
            }
            Err(err) if attempt < ATTEMPTS => {
                tracing::debug!(
                    attempt,
                    error = %err,
                    "the display topology changed while being queried; retrying"
                );
            }
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "QueryDisplayConfig failed; SDR white levels stay unknown"
                );
                return Err(crate::Error::from(err));
            }
        }
    }

    Err(crate::Error::Capture {
        message: "the display configuration could not be read".to_owned(),
    })
}

/// Reads the SDR white level for every path of a completed query.
fn white_levels_for(paths: &[DISPLAYCONFIG_PATH_INFO]) -> HashMap<String, f32> {
    let mut levels = HashMap::new();
    for path in paths {
        // Step 1: which GDI device name does this path's source drive?
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: header(
                DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                path.sourceInfo.adapterId,
                path.sourceInfo.id,
                mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            ),
            viewGdiDeviceName: [0; 32],
        };
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut source.header) } != 0 {
            tracing::debug!("DisplayConfigGetDeviceInfo(GET_SOURCE_NAME) failed");
            continue;
        }
        let device_name = wide_to_string(&source.viewGdiDeviceName);

        // Step 2: that same path's target carries the SDR white level.
        let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: header(
                DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                path.targetInfo.adapterId,
                path.targetInfo.id,
                mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
            ),
            SDRWhiteLevel: 0,
        };
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut white.header) } != 0 {
            tracing::debug!(
                monitor = %device_name,
                "DisplayConfigGetDeviceInfo(GET_SDR_WHITE_LEVEL) failed; SDR white stays unknown"
            );
            continue;
        }

        // The value arrives in thousandths of a nit, so 200 nits is 200 000.
        let nits = white.SDRWhiteLevel as f32 / 1000.0 * 80.0;
        if nits > 0.0 {
            tracing::debug!(monitor = %device_name, nits, "SDR white level");
            levels.insert(device_name, nits);
        }
    }

    levels
}

/// Builds the request header every `DisplayConfigGetDeviceInfo` call needs.
///
/// `size` is the size of the whole request struct, and Windows refuses the
/// request if it does not match, which is why it is passed in rather than
/// guessed.
fn header(
    request: windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_TYPE,
    adapter_id: windows::Win32::Foundation::LUID,
    id: u32,
    size: u32,
) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: request,
        size,
        adapterId: adapter_id,
        id,
    }
}

/// Converts a fixed-size wide string to a `String`, ignoring anything past the
/// terminator. Device names are plain ASCII, so lossy conversion is safe.
fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header size has to match the struct it is embedded in, or Windows
    /// quietly refuses the request.
    #[test]
    fn header_sizes_match_the_structs() {
        assert_eq!(
            mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(),
            mem::size_of::<DISPLAYCONFIG_DEVICE_INFO_HEADER>() + 64
        );
        assert_eq!(
            mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>(),
            mem::size_of::<DISPLAYCONFIG_DEVICE_INFO_HEADER>() + 4
        );
    }

    #[test]
    fn device_names_are_read_up_to_the_terminator() {
        let mut wide = [0u16; 32];
        for (index, character) in "\\\\.\\DISPLAY1".encode_utf16().enumerate() {
            wide[index] = character;
        }
        assert_eq!(wide_to_string(&wide), "\\\\.\\DISPLAY1");
    }

    #[test]
    fn an_all_zero_device_name_is_empty() {
        assert_eq!(wide_to_string(&[0; 32]), "");
    }

    /// A monitor the OS calls did not describe. `slot` makes its `HMONITOR`
    /// unique, which is the key the HDR map is matched on.
    fn raw(name: &str, x: i32, slot: usize) -> RawMonitor {
        RawMonitor {
            hmonitor: HMONITOR(slot as *mut std::ffi::c_void),
            device_name: name.to_owned(),
            rect: RectI {
                x,
                y: 0,
                w: 10,
                h: 10,
            },
            primary: false,
        }
    }

    fn hdr_of(slot: usize) -> HashMap<isize, bool> {
        HashMap::from([(slot as isize, true)])
    }

    #[test]
    fn a_failed_hdr_query_leaves_the_monitor_sdr() {
        // SPEC section 13: a query that fails degrades, it does not abort.
        let monitors = [raw("\\\\.\\DISPLAY1", 0, 1)];
        let merged = merge(
            &monitors,
            Err(crate::Error::Capture {
                message: "no DXGI factory".to_owned(),
            }),
            Ok(HashMap::new()),
        );

        assert_eq!(merged.len(), 1);
        assert!(!merged[0].hdr_active, "a failed query invented HDR");
        assert_eq!(merged[0].sdr_white_nits, None);
        assert_eq!(merged[0].id, "\\\\.\\DISPLAY1");
    }

    #[test]
    fn a_failed_sdr_white_query_leaves_the_level_unknown() {
        let monitors = [raw("\\\\.\\DISPLAY2", 0, 2)];
        let merged = merge(
            &monitors,
            Ok(hdr_of(2)),
            Err(crate::Error::Capture {
                message: "QueryDisplayConfig failed".to_owned(),
            }),
        );

        assert!(merged[0].hdr_active, "the HDR map was ignored");
        // `None` is the existing "use the default" path in `capture.rs`.
        assert_eq!(merged[0].sdr_white_nits, None);
    }

    #[test]
    fn the_two_maps_are_stitched_by_device_name() {
        let monitors = [raw("\\\\.\\DISPLAY1", -10, 1), raw("\\\\.\\DISPLAY2", 0, 2)];
        let mut white = HashMap::new();
        white.insert("\\\\.\\DISPLAY2".to_owned(), 240.0_f32);

        let merged = merge(&monitors, Ok(hdr_of(2)), Ok(white));

        assert!(!merged[0].hdr_active, "the left monitor is not HDR");
        assert!(merged[1].hdr_active, "the right monitor is HDR");
        assert_eq!(merged[1].sdr_white_nits, Some(240.0));
        assert_eq!(merged[0].rect.x, -10, "the rectangle survived");
    }
}
