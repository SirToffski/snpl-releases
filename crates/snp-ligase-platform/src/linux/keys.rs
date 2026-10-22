//! The Linux stand-in for the Windows keyboard poll (`windows::keys`).
//!
//! Windows can ask `GetAsyncKeyState` for any key's state, which is how the
//! app notices the Ctrl+V that egui swallowed and how the hotkey recorder
//! reads a key no window sees. On X11 the same question is `XQueryKeymap`,
//! answered by [`x11::keys`] from the server's own key bitmap (LINUX_SUPPORT.md
//! section 4, phase L1c).
//!
//! A Wayland or headless session still reports **nothing held**: Wayland is
//! not allowed to know (phase L3), and there is no server to ask. The app's
//! paste then falls back to the egui events (which work for a text clipboard)
//! and the hotkey recorder cannot finish there.

use std::sync::OnceLock;

use super::x11::keys::{KeyState, keyboard_state};
use super::{SessionEnv, SessionKind};

/// The virtual key code for `V`, the key Ctrl+V needs (the Windows one, like
/// every code in the snapshot).
const VK_V: u16 = 0x56;

/// A snapshot of the keyboard, in the same shape the Windows one has, so the
/// app's hotkey recorder compiles and is tested unchanged.
///
/// The field list must stay in step with `windows::keys`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyboardSnapshot {
    /// Either Ctrl is down.
    pub ctrl: bool,
    /// Either Shift is down.
    pub shift: bool,
    /// Either Alt is down.
    pub alt: bool,
    /// Either Super is down.
    pub win: bool,
    /// The non-modifier keys that are down right now, ascending.
    pub keys: Vec<u16>,
}

impl KeyboardSnapshot {
    /// The Windows-shaped snapshot of an X11 keyboard state.
    fn from_state(state: KeyState) -> Self {
        Self {
            ctrl: state.ctrl,
            shift: state.shift,
            alt: state.alt,
            win: state.win,
            keys: state.keys,
        }
    }
}

/// Which session this process runs in, read once: the answer cannot change
/// while the app runs, and the poll happens every frame while recording.
fn session_kind() -> SessionKind {
    static KIND: OnceLock<SessionKind> = OnceLock::new();
    *KIND.get_or_init(|| SessionEnv::read().kind())
}

/// The snapshot for `kind`, asking `read` only when there is an X server.
///
/// The `read` closure is the seam the tests use: a Wayland or headless session
/// must not open a connection or touch the keyboard at all.
fn snapshot_for(kind: SessionKind, read: impl FnOnce() -> KeyState) -> KeyboardSnapshot {
    match kind {
        SessionKind::X11 => KeyboardSnapshot::from_state(read()),
        SessionKind::Wayland | SessionKind::Headless => KeyboardSnapshot::default(),
    }
}

/// Reads the keyboard's current state, for the hotkey recorder.
#[must_use]
pub fn keyboard_snapshot() -> KeyboardSnapshot {
    snapshot_for(session_kind(), keyboard_state)
}

/// Whether a snapshot has Ctrl+V held: both keys down at once.
fn paste_is_held(snapshot: &KeyboardSnapshot) -> bool {
    snapshot.ctrl && snapshot.keys.contains(&VK_V)
}

/// Whether Ctrl+V is being held: both keys down at once.
///
/// This is how the app notices a paste that egui never reported.
#[must_use]
pub fn paste_held() -> bool {
    paste_is_held(&keyboard_snapshot())
}

/// Whether either Shift key is down right now.
///
/// The Ctrl+Shift+V fallback needs it: egui-winit emits no event at all for an
/// image-only clipboard, so the two paste shortcuts have to be told apart from
/// the key state.
#[must_use]
pub fn shift_down() -> bool {
    keyboard_snapshot().shift
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_state() -> KeyState {
        KeyState {
            ctrl: true,
            shift: true,
            alt: false,
            win: false,
            keys: vec![0x41, VK_V],
        }
    }

    #[test]
    fn a_session_without_x11_never_asks_for_a_keyboard_state() {
        // The output of this closure is never read, so panicking proves the
        // connection is never opened on Wayland or with no display server.
        let snapshot = snapshot_for(SessionKind::Wayland, || panic!("Wayland was asked"));
        assert_eq!(snapshot, KeyboardSnapshot::default());
        assert!(!snapshot.ctrl && !snapshot.shift && !snapshot.alt && !snapshot.win);
        assert!(snapshot.keys.is_empty());

        let snapshot = snapshot_for(SessionKind::Headless, || panic!("headless was asked"));
        assert_eq!(snapshot, KeyboardSnapshot::default());
    }

    #[test]
    fn an_x11_state_keeps_the_windows_shape() {
        let snapshot = snapshot_for(SessionKind::X11, key_state);
        assert!(snapshot.ctrl);
        assert!(snapshot.shift);
        assert!(!snapshot.alt);
        assert!(!snapshot.win);
        assert_eq!(snapshot.keys, vec![0x41, VK_V]);
    }

    #[test]
    fn paste_needs_ctrl_and_v_at_the_same_time() {
        let mut snapshot = KeyboardSnapshot {
            ctrl: true,
            keys: vec![VK_V],
            ..KeyboardSnapshot::default()
        };
        assert!(paste_is_held(&snapshot));
        // V alone, or Ctrl alone, is not a paste.
        snapshot.ctrl = false;
        assert!(!paste_is_held(&snapshot));
        snapshot.ctrl = true;
        snapshot.keys.clear();
        assert!(!paste_is_held(&snapshot));
        // Ctrl+Shift+V is still Ctrl+V for this question; the app adds Shift
        // itself when it has to tell the two apart.
        snapshot.keys.push(VK_V);
        snapshot.shift = true;
        assert!(paste_is_held(&snapshot));
    }
}
