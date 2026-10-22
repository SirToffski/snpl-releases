//! Monitor enumeration through RandR 1.5's `GetMonitors` (LINUX_SUPPORT.md
//! section 4).
//!
//! X11 has one shared coordinate space across every monitor, exactly like
//! Windows' virtual desktop, and physical pixels are the only real space
//! (LINUX_SUPPORT.md section 2), so nothing here applies a scale factor. What
//! the scale factor is used for is the log line.
//!
//! `scale_factor` comes from `Xft.dpi` / 96. There is no HDR on X11, so
//! `hdr_active` is false and `sdr_white_nits` is `None`: the capture path has
//! nothing to tone-map.

use x11rb::protocol::randr;
use x11rb::protocol::xproto;
use x11rb::rust_connection::RustConnection;

use super::X11Connection;
use crate::{Error, MonitorInfo, RectI, Result};

/// Every monitor RandR reports, in the server's order.
pub(crate) fn enumerate(conn: &X11Connection) -> Result<Vec<MonitorInfo>> {
    let Some(version) = conn.randr_version else {
        // No RandR 1.5 (an old server, or a nested one without it): the whole
        // screen is the monitor. Saying so beats reporting nothing, and the
        // log line says which case this is.
        tracing::warn!(
            screen = conn.screen,
            "RandR 1.5 is not available; reporting the whole screen as one monitor"
        );
        return Ok(vec![screen_monitor(conn)]);
    };

    let reply = randr::get_monitors(&conn.conn, conn.root, true)
        .map_err(|err| Error::Os {
            message: format!("RandR GetMonitors failed: {err}"),
        })?
        .reply()
        .map_err(|err| Error::Os {
            message: format!("RandR GetMonitors failed: {err}"),
        })?;

    tracing::debug!(
        version = %format!("{}.{}", version.0, version.1),
        monitors = reply.monitors.len(),
        "RandR monitors"
    );

    let mut names = Vec::with_capacity(reply.monitors.len());
    for monitor in &reply.monitors {
        names.push(atom_name(conn, monitor.name));
    }

    let infos = monitors_or_whole_screen(
        monitor_infos(&reply.monitors, &names, conn.scale_factor),
        screen_monitor(conn),
    );
    for info in &infos {
        tracing::info!(
            monitor = %info.id,
            rect = ?info.rect,
            primary = info.is_primary,
            scale = info.scale_factor,
            "monitor"
        );
    }
    Ok(infos)
}

/// The monitor list to report: the server's, or the whole screen when the
/// server reports none.
///
/// RandR 1.5's monitor list is empty when there is no CRTC with an output — a
/// nested server, a driver with no outputs, a machine whose only monitors were
/// made by hand and then removed with `xrandr --delmonitor`. Reporting nothing
/// would take every capture and the overlay with it, so the whole screen is
/// reported as the one monitor, exactly like a server without RandR 1.5.
#[must_use]
fn monitors_or_whole_screen(
    monitors: Vec<MonitorInfo>,
    whole_screen: MonitorInfo,
) -> Vec<MonitorInfo> {
    if monitors.is_empty() {
        tracing::warn!(
            id = %whole_screen.id,
            rect = ?whole_screen.rect,
            "the server reports no RandR monitors; reporting the whole screen as one"
        );
        vec![whole_screen]
    } else {
        monitors
    }
}

/// The whole screen as one monitor, for a server without RandR 1.5.
fn screen_monitor(conn: &X11Connection) -> MonitorInfo {
    MonitorInfo {
        id: format!("screen{}", conn.screen),
        rect: RectI::new(
            0,
            0,
            i32::from(conn.root_width),
            i32::from(conn.root_height),
        ),
        scale_factor: conn.scale_factor,
        hdr_active: false,
        sdr_white_nits: None,
        is_primary: true,
    }
}

/// The name of a monitor's atom, `None` when there is no name or the server
/// cannot answer. Monitors created by hand (`xrandr --setmonitor` without a
/// name) have none.
fn atom_name(conn: &X11Connection, atom: xproto::Atom) -> Option<String> {
    if atom == 0 {
        return None;
    }
    match xproto::get_atom_name(&conn.conn, atom)
        .map_err(|err| err.to_string())
        .and_then(|cookie| cookie.reply().map_err(|err| err.to_string()))
    {
        Ok(reply) => {
            let name = String::from_utf8_lossy(&reply.name).into_owned();
            (!name.is_empty()).then_some(name)
        }
        Err(err) => {
            tracing::warn!(atom, error = %err, "could not read a monitor's name");
            None
        }
    }
}

/// The reply's monitors as [`MonitorInfo`]s: pure, so the mapping is a test
/// with the reply built by hand.
///
/// A monitor the server had no name for gets a positional one, so two monitors
/// are never both called the same thing in a log or a file name.
#[must_use]
fn monitor_infos(
    monitors: &[randr::MonitorInfo],
    names: &[Option<String>],
    scale_factor: f32,
) -> Vec<MonitorInfo> {
    monitors
        .iter()
        .enumerate()
        .map(|(index, monitor)| MonitorInfo {
            id: names
                .get(index)
                .cloned()
                .flatten()
                .unwrap_or_else(|| format!("monitor{index}")),
            rect: RectI::new(
                i32::from(monitor.x),
                i32::from(monitor.y),
                i32::from(monitor.width),
                i32::from(monitor.height),
            ),
            scale_factor,
            // LINUX_SUPPORT.md section 2: X11 has no HDR capture.
            hdr_active: false,
            sdr_white_nits: None,
            is_primary: monitor.primary,
        })
        .collect()
}

/// `Xft.dpi` from the root window's `RESOURCE_MANAGER` property.
///
/// The property is the merged xrdb string, where `Xft.dpi:\t96` is one line
/// among many. It is read for logging only (LINUX_SUPPORT.md section 4).
pub(crate) fn query_xft_dpi(conn: &RustConnection, root: xproto::Window) -> Option<Option<f32>> {
    let cookie = xproto::get_property(
        &conn,
        false,
        root,
        xproto::AtomEnum::RESOURCE_MANAGER,
        xproto::AtomEnum::STRING,
        0,
        // 1024 32-bit units is far more than an xrdb string needs; the reply
        // says if it was cut short.
        1024,
    )
    .ok()?;
    let reply = cookie.reply().ok()?;
    if reply.bytes_after > 0 {
        tracing::debug!("the RESOURCE_MANAGER property was longer than one reply");
    }
    Some(parse_xft_dpi(&String::from_utf8_lossy(&reply.value)))
}

/// `Xft.dpi` inside a `RESOURCE_MANAGER` string (an `xrdb` file's contents).
///
/// The last assignment wins, the way xrdb resolves a repeated resource.
#[must_use]
fn parse_xft_dpi(text: &str) -> Option<f32> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("Xft.dpi:"))
        .filter_map(|value| value.trim().parse::<f32>().ok())
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: u32, x: i16, y: i16, w: u16, h: u16, primary: bool) -> randr::MonitorInfo {
        randr::MonitorInfo {
            name,
            primary,
            automatic: false,
            x,
            y,
            width: w,
            height: h,
            width_in_millimeters: 0,
            height_in_millimeters: 0,
            outputs: Vec::new(),
        }
    }

    #[test]
    fn monitors_keep_their_rectangles_names_and_the_primary_flag() {
        let monitors = [
            monitor(1, 0, 0, 1920, 1080, true),
            monitor(2, 1920, -200, 1080, 1920, false),
        ];
        let names = [Some("HDMI-1".to_owned()), Some("DP-2".to_owned())];

        let infos = monitor_infos(&monitors, &names, 1.0);

        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].id, "HDMI-1");
        assert_eq!(
            infos[0].rect,
            RectI {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080
            }
        );
        assert!(infos[0].is_primary, "the first monitor is the primary one");
        assert_eq!(infos[1].id, "DP-2");
        assert_eq!(infos[1].rect.x, 1920);
        assert_eq!(infos[1].rect.y, -200, "a monitor above the origin");
        assert!(!infos[1].is_primary);

        // LINUX_SUPPORT.md section 4: X11 has no HDR, and the scale factor is
        // only ever logged.
        for info in &infos {
            assert!(!info.hdr_active);
            assert_eq!(info.sdr_white_nits, None);
            assert_eq!(info.scale_factor, 1.0);
        }
    }

    #[test]
    fn a_monitor_without_a_name_gets_a_positional_one() {
        // `xrandr --setmonitor` without a name, and a name the server could not
        // answer, both arrive as `None`.
        let monitors = [
            monitor(0, 0, 0, 800, 600, true),
            monitor(7, 0, 600, 800, 600, false),
        ];
        let names = [None, None];

        let infos = monitor_infos(&monitors, &names, 1.25);

        assert_eq!(infos[0].id, "monitor0");
        assert_eq!(infos[1].id, "monitor1");
        assert_eq!(infos[1].rect.y, 600);
        assert_eq!(infos[0].scale_factor, 1.25, "the session's scale factor");
    }

    #[test]
    fn a_short_name_list_still_names_every_monitor() {
        // Defensive: the reply and the name list come from two calls, and a
        // monitor added in between must not shift the names.
        let monitors = [
            monitor(1, 0, 0, 100, 100, true),
            monitor(2, 100, 0, 100, 100, false),
        ];
        let infos = monitor_infos(&monitors, &[Some("eDP-1".to_owned())], 1.0);

        assert_eq!(infos[0].id, "eDP-1");
        assert_eq!(infos[1].id, "monitor1");
    }

    #[test]
    fn a_server_with_no_monitors_reports_the_whole_screen() {
        // `xrandr --delmonitor` on a machine whose monitors were all made by
        // hand leaves RandR 1.5 with an empty list; reporting nothing would
        // take the capture and the overlay with it.
        let whole = MonitorInfo {
            id: "screen0".to_owned(),
            rect: RectI::new(0, 0, 1920, 1080),
            scale_factor: 1.0,
            hdr_active: false,
            sdr_white_nits: None,
            is_primary: true,
        };
        let reported = monitors_or_whole_screen(Vec::new(), whole.clone());
        assert_eq!(reported, vec![whole.clone()]);

        // A server that does have monitors keeps them as they are.
        let monitors = monitor_infos(&[monitor(1, 0, 0, 800, 600, true)], &[None], 1.0);
        assert_eq!(
            monitors_or_whole_screen(monitors.clone(), whole),
            monitors,
            "the fallback must not replace a real list"
        );
    }

    #[test]
    fn xft_dpi_is_read_out_of_the_resource_manager_string() {
        let resources = "Xft.antialias:\t1\nXft.hintstyle:\thintfull\nXft.rgba:\tnone\n\
                         Xft.dpi:\t96\nXcursor.theme:\t\nXcursor.size:\t0\n";
        assert_eq!(parse_xft_dpi(resources), Some(96.0));

        // The last assignment wins, like xrdb.
        assert_eq!(parse_xft_dpi("Xft.dpi:\t96\nXft.dpi: 144\n"), Some(144.0));

        // Nothing to read is not a value, and a broken one does not stop a
        // later good one.
        assert_eq!(parse_xft_dpi(""), None);
        assert_eq!(parse_xft_dpi("Xft.rgba: none\n"), None);
        assert_eq!(
            parse_xft_dpi("Xft.dpi: not-a-number\nXft.dpi: 120\n"),
            Some(120.0)
        );
    }
}
