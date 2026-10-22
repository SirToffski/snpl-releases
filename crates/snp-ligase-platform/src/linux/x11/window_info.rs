//! The pointer and the active window's rectangle (SPEC section 5.7,
//! LINUX_SUPPORT.md section 4).
//!
//! Both are "ask the server right now" calls on the same connection the
//! captures use, so they see the same pixels. The active window comes from the
//! EWMH property `_NET_ACTIVE_WINDOW` on the root, which is how a window
//! manager publishes that window, plus the frame extents the window itself
//! carries: `_NET_FRAME_EXTENTS` (the window manager's border and title bar)
//! or `_GTK_FRAME_EXTENTS` (client-side decorations and shadow).
//!
//! Windows disappear while they are being asked about, so every reply here has
//! a "the window is gone" path: that is `Ok(None)` and a debug line, not a
//! failure — the active-window capture is then just not available for that
//! capture (SPEC section 5.7).

use x11rb::errors::ReplyError;
use x11rb::protocol::xproto;

use super::X11Connection;
use crate::{Error, RectI, Result};

/// Where the pointer is, in the root window's pixels.
pub(crate) fn cursor_pos(conn: &X11Connection) -> Result<(i32, i32)> {
    let reply = xproto::query_pointer(&conn.conn, conn.root)
        .map_err(|err| os_error("QueryPointer", &err))?
        .reply()
        .map_err(|err| os_error("QueryPointer", &err))?;

    if !reply.same_screen {
        // One X screen is the normal case; with several, the reply's
        // coordinates belong to the screen the pointer is on.
        tracing::debug!("the pointer is on another X screen than the one being captured");
    }
    Ok((i32::from(reply.root_x), i32::from(reply.root_y)))
}

/// The foreground window's rectangle, with the frame the user sees.
pub(crate) fn foreground_window_rect(conn: &X11Connection) -> Result<Option<RectI>> {
    let Some(window) = active_window(conn)? else {
        tracing::debug!("the root window does not name an active window");
        return Ok(None);
    };

    let geometry =
        xproto::get_geometry(&conn.conn, window).map_err(|err| os_error("GetGeometry", &err))?;
    let Some(geometry) = gone_is_none(geometry.reply(), "GetGeometry", window)? else {
        return Ok(None);
    };

    let cookie = xproto::translate_coordinates(&conn.conn, window, conn.root, 0, 0)
        .map_err(|err| os_error("TranslateCoordinates", &err))?;
    let Some(translated) = gone_is_none(cookie.reply(), "TranslateCoordinates", window)? else {
        return Ok(None);
    };
    if !translated.same_screen {
        tracing::debug!(
            window,
            "the active window is on another X screen; no rectangle"
        );
        return Ok(None);
    }

    // The client area: where the window is, and the size it asked for.
    let client = RectI::new(
        i32::from(translated.dst_x),
        i32::from(translated.dst_y),
        i32::from(geometry.width),
        i32::from(geometry.height),
    );

    let net = extents_property(conn, window, conn.net_frame_extents)?;
    let gtk = extents_property(conn, window, conn.gtk_frame_extents)?;
    let (rect, source) = window_rect(client, net, gtk);

    // Which property was used is exactly what a wrong rectangle is diagnosed
    // from (LINUX_SUPPORT.md section 10).
    tracing::debug!(
        window,
        client = ?client,
        net_extents = ?net,
        gtk_extents = ?gtk,
        source,
        rect = ?rect,
        "the active window's rectangle"
    );
    Ok(Some(rect))
}

/// The window `_NET_ACTIVE_WINDOW` names, if any.
fn active_window(conn: &X11Connection) -> Result<Option<xproto::Window>> {
    let cookie = xproto::get_property(
        &conn.conn,
        false,
        conn.root,
        conn.net_active_window,
        xproto::AtomEnum::WINDOW,
        0,
        1,
    )
    .map_err(|err| os_error("GetProperty(_NET_ACTIVE_WINDOW)", &err))?;
    let reply = cookie
        .reply()
        .map_err(|err| os_error("GetProperty(_NET_ACTIVE_WINDOW)", &err))?;

    // The property is a single WINDOW, so one 32-bit value is the whole of it.
    let window = reply.value32().and_then(|mut values| values.next());
    Ok(window.filter(|window| *window != 0))
}

/// What a window's frame extents property says, when it says anything.
fn extents_property(
    conn: &X11Connection,
    window: xproto::Window,
    property: xproto::Atom,
) -> Result<Option<Extents>> {
    let cookie = xproto::get_property(
        &conn.conn,
        false,
        window,
        property,
        xproto::AtomEnum::CARDINAL,
        0,
        // Four CARDINALs; the reply says if there were more.
        4,
    )
    .map_err(|err| os_error("GetProperty(frame extents)", &err))?;
    let Some(reply) = gone_is_none(cookie.reply(), "GetProperty(frame extents)", window)? else {
        return Ok(None);
    };
    let values: Vec<u32> = reply.value32().map(Iterator::collect).unwrap_or_default();
    Ok(extents_from(&values))
}

/// A request whose window may have gone away between the calls.
fn gone_is_none<T>(
    reply: std::result::Result<T, ReplyError>,
    what: &str,
    window: xproto::Window,
) -> Result<Option<T>> {
    match reply {
        Ok(reply) => Ok(Some(reply)),
        // BadWindow (and BadDrawable) means the window was destroyed while it
        // was being asked about: a window manager switching windows does that
        // constantly, and it is not worth an error.
        Err(ReplyError::X11Error(error)) => {
            tracing::debug!(window, what, error = ?error, "the window went away");
            Ok(None)
        }
        Err(ReplyError::ConnectionError(error)) => Err(os_error(what, &error)),
    }
}

fn os_error(what: &str, error: &dyn std::fmt::Display) -> Error {
    Error::Os {
        message: format!("{what} failed: {error}"),
    }
}

/// A frame's overhang: how far its border reaches left, right, above and below
/// the client area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extents {
    pub(crate) left: i32,
    pub(crate) right: i32,
    pub(crate) top: i32,
    pub(crate) bottom: i32,
}

/// The extents a property holds: `(left, right, top, bottom)` in the order
/// both EWMH and GTK publish them.
///
/// A property that is missing, has the wrong number of values, has a value too
/// large for a rectangle, or is all zero (which is how a window manager says
/// "no frame") is `None`.
#[must_use]
fn extents_from(values: &[u32]) -> Option<Extents> {
    let [left, right, top, bottom] = values else {
        return None;
    };
    if values.iter().all(|value| *value == 0) {
        return None;
    }
    Some(Extents {
        left: i32::try_from(*left).ok()?,
        right: i32::try_from(*right).ok()?,
        top: i32::try_from(*top).ok()?,
        bottom: i32::try_from(*bottom).ok()?,
    })
}

/// The rectangle the user sees, from the client area and the two kinds of
/// extents (SPEC section 5.7).
///
/// `_NET_FRAME_EXTENTS` is the frame the window manager draws *around* the
/// client, so the rectangle grows by it, and it is what wins when both
/// properties exist. `_GTK_FRAME_EXTENTS` is the decoration and resize shadow
/// the client draws as part of its own surface, so the rectangle shrinks by it;
/// a GTK inset that would leave nothing falls back to the client area itself.
/// The second value names what was used, for the log.
#[must_use]
fn window_rect(client: RectI, net: Option<Extents>, gtk: Option<Extents>) -> (RectI, &'static str) {
    if let Some(extents) = net {
        return (
            RectI::new(
                client.x - extents.left,
                client.y - extents.top,
                client.w + extents.left + extents.right,
                client.h + extents.top + extents.bottom,
            ),
            "_NET_FRAME_EXTENTS",
        );
    }
    if let Some(extents) = gtk {
        let inset = RectI::new(
            client.x + extents.left,
            client.y + extents.top,
            client.w - extents.left - extents.right,
            client.h - extents.top - extents.bottom,
        );
        if !inset.is_empty() {
            return (inset, "_GTK_FRAME_EXTENTS");
        }
        tracing::warn!(
            client = ?client,
            extents = ?extents,
            "the GTK frame extents cover the whole window; using the client area"
        );
    }
    (client, "the client window, which has no frame extents")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_rect() -> RectI {
        RectI::new(100, 200, 800, 600)
    }

    #[test]
    fn a_window_manager_frame_grows_the_client_area() {
        // The measured numbers from this machine's xfwm4: a 4-pixel border and
        // a 28-pixel title bar (left, right, top, bottom).
        let extents = extents_from(&[4, 4, 28, 4]).expect("four values are extents");
        let (rect, source) = window_rect(client_rect(), Some(extents), None);

        assert_eq!(rect, RectI::new(96, 172, 808, 632));
        assert_eq!(source, "_NET_FRAME_EXTENTS");
    }

    #[test]
    fn a_gtk_client_side_decoration_shrinks_the_client_area() {
        // GTK's extents are the shadow and resize border inside the surface.
        let extents = extents_from(&[20, 20, 60, 60]).expect("four values are extents");
        let (rect, source) = window_rect(client_rect(), None, Some(extents));

        assert_eq!(rect, RectI::new(120, 260, 760, 480));
        assert_eq!(source, "_GTK_FRAME_EXTENTS");
    }

    #[test]
    fn a_window_manager_frame_wins_over_the_gtk_one() {
        // Both properties are published for some windows (a GTK client under
        // xfwm4); the frame the window manager draws is the one on screen.
        let net = extents_from(&[4, 4, 28, 4]).expect("extents");
        let gtk = extents_from(&[1, 1, 1, 1]).expect("extents");

        let (rect, source) = window_rect(client_rect(), Some(net), Some(gtk));

        assert_eq!(rect, RectI::new(96, 172, 808, 632));
        assert_eq!(source, "_NET_FRAME_EXTENTS");
    }

    #[test]
    fn a_window_without_extents_is_its_client_area() {
        let (rect, source) = window_rect(client_rect(), None, None);
        assert_eq!(rect, client_rect());
        assert!(source.contains("client"), "{source}");

        // All-zero properties are the same as missing ones.
        assert_eq!(extents_from(&[0, 0, 0, 0]), None);
        // And so is a property with the wrong number of values.
        assert_eq!(extents_from(&[1, 1, 1]), None);
        assert_eq!(extents_from(&[]), None);
    }

    #[test]
    fn a_gtk_inset_that_leaves_nothing_falls_back_to_the_client_area() {
        // A window smaller than its own shadow: the visible frame is the client
        // area, never a zero-sized or inverted rectangle.
        let extents = extents_from(&[400, 400, 300, 300]).expect("extents");
        let (rect, _) = window_rect(client_rect(), None, Some(extents));

        assert_eq!(rect, client_rect());
    }

    #[test]
    fn a_value_too_large_for_a_rectangle_is_not_extents() {
        assert_eq!(extents_from(&[u32::MAX, 0, 0, 0]), None);
        assert_eq!(extents_from(&[0, 0, u32::MAX, 0]), None);
    }
}
