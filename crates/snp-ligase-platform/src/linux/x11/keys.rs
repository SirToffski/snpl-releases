//! The X11 keyboard state (LINUX_SUPPORT.md section 4, phase L1c).
//!
//! `XQueryKeymap` is the X11 answer to Windows' `GetAsyncKeyState`: the server
//! returns one bitmap of every keycode that is down anywhere on the desktop,
//! whether or not this process has a focused window. That is what makes the
//! Ctrl+V that egui swallowed reachable, and what lets the hotkey recorder see
//! a key that a registered hotkey took first.
//!
//! The bitmap is keycodes; the app speaks Windows virtual keys (the same table
//! `global-hotkey` and `crate::windows::keys` use), so a [`Keymap`] built from
//! the server's keyboard mapping translates one into the other. The
//! translation is pure and tested with hand-written keymaps; only the cached
//! live connection is not.

use std::sync::{Mutex, OnceLock};

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto;
use x11rb::rust_connection::RustConnection;

/// The keyboard's state as X11 sees it: the four modifier flags the recorder
/// needs, and the virtual keys of everything else that is down.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct KeyState {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    /// Windows virtual keys, ascending, with no duplicates.
    pub keys: Vec<u16>,
}

/// A key that only contributes a modifier flag, never a recorded key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Win,
}

/// What one keycode means for a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyEntry {
    /// A key nothing can name: a dead key, a non-Latin letter on the user's
    /// layout, a vendor key.
    None,
    /// A modifier: it sets a flag and is never "the key".
    Modifier(Modifier),
    /// A normal key, by Windows virtual key.
    Key(u16),
}

/// The server's keyboard mapping, reduced to what the app asks for.
///
/// One entry per keycode from `first_keycode` (the server's minimum) upward.
/// Only the **first** keysym of a keycode is classified: it is the unshifted
/// one, so `?` on a US layout is still the `/` key, which is what a hotkey
/// recorded as `?` or `/` should register as.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Keymap {
    first_keycode: u8,
    entries: Vec<KeyEntry>,
}

impl Keymap {
    /// Builds the map from an `XGetKeyboardMapping` reply.
    ///
    /// `keysyms` is the flat reply: `keysyms_per_keycode` keysyms for each
    /// keycode starting at `first_keycode`. A zero `per` is an empty mapping
    /// rather than a panic.
    fn from_keysyms(first_keycode: u8, keysyms_per_keycode: u8, keysyms: &[u32]) -> Self {
        let per = usize::from(keysyms_per_keycode);
        let entries = if per == 0 {
            Vec::new()
        } else {
            keysyms
                .chunks(per)
                .map(|keysyms| classify(recording_keysym(keysyms)))
                .collect()
        };
        Self {
            first_keycode,
            entries,
        }
    }

    /// The state of the keyboard, from `XQueryKeymap`'s 32-byte bitmap.
    fn state(&self, bitmap: &[u8; 32]) -> KeyState {
        let mut state = KeyState::default();
        for (offset, entry) in self.entries.iter().enumerate() {
            let Ok(offset) = u8::try_from(offset) else {
                break;
            };
            let Some(keycode) = self.first_keycode.checked_add(offset) else {
                break;
            };
            if !key_down(bitmap, keycode) {
                continue;
            }
            match entry {
                KeyEntry::None => {}
                KeyEntry::Modifier(Modifier::Ctrl) => state.ctrl = true,
                KeyEntry::Modifier(Modifier::Shift) => state.shift = true,
                KeyEntry::Modifier(Modifier::Alt) => state.alt = true,
                KeyEntry::Modifier(Modifier::Win) => state.win = true,
                KeyEntry::Key(vk) => state.keys.push(*vk),
            }
        }
        state.keys.sort_unstable();
        state.keys.dedup();
        state
    }
}

/// The keysym a recording names a keycode by: normally its first (unshifted)
/// one, but a keypad key's **digit** when the keycode carries one.
///
/// The keypad's first keysym is the NumLock-off one (`KP_Home KP_7`,
/// `KP_Delete KP_Decimal` on a US layout, as `xmodmap -pke` shows), which maps
/// to no key. Without this, the recorder could not record a numpad key on X11
/// (the L1c review). The digit is what Windows' `VK_NUMPAD0`–`9` and
/// `global-hotkey`'s `Numpad0`–`9` name too.
fn recording_keysym(keysyms: &[u32]) -> u32 {
    const KP_DECIMAL: u32 = 0xffae;
    const KP_0: u32 = 0xffb0;
    const KP_9: u32 = 0xffb9;
    keysyms
        .iter()
        .copied()
        .find(|keysym| (KP_0..=KP_9).contains(keysym) || *keysym == KP_DECIMAL)
        .unwrap_or_else(|| keysyms.first().copied().unwrap_or(0))
}

/// The entry for one keysym.
fn classify(keysym: u32) -> KeyEntry {
    if let Some(modifier) = modifier_of(keysym) {
        KeyEntry::Modifier(modifier)
    } else if let Some(vk) = keysym_to_vk(keysym) {
        KeyEntry::Key(vk)
    } else {
        KeyEntry::None
    }
}

/// The modifier a keysym stands for, if it is one.
///
/// AltGr is `Mode_switch` or `ISO_Level3_Shift` on most layouts and is reported
/// as Alt, which is what a Windows recording of AltGr produces. Meta is Alt on
/// an X11 keyboard too, and Super is the Windows key.
fn modifier_of(keysym: u32) -> Option<Modifier> {
    match keysym {
        0xffe1 | 0xffe2 => Some(Modifier::Shift),
        0xffe3 | 0xffe4 => Some(Modifier::Ctrl),
        0xffe7 | 0xffe8 | 0xffe9 | 0xffea | 0xff7e | 0xfe03 => Some(Modifier::Alt),
        0xffeb..=0xffee => Some(Modifier::Win),
        _ => None,
    }
}

/// Whether the bitmap says `keycode` is down.
///
/// `XQueryKeymap` numbers the bits by keycode: byte `keycode / 8`, bit
/// `keycode % 8`. Keycodes 0–7 are not real keys and are never set.
#[must_use]
fn key_down(bitmap: &[u8; 32], keycode: u8) -> bool {
    let byte = usize::from(keycode) / 8;
    let bit = 1u8 << (keycode % 8);
    bitmap.get(byte).is_some_and(|byte| byte & bit != 0)
}

/// The Windows virtual key for an X11 keysym, when a recording can name it.
///
/// The list is the one `crate::hotkeys::VIRTUAL_KEYS` knows, in keysym terms:
/// a key that maps here is a key the recorder can turn into a `Code` and
/// `global-hotkey` can register. Shifted spellings (`?`, `:`, `%`, …) map to
/// the same key as their unshifted partner, because the recorder reads the
/// physical key, not the character the layout produces.
#[must_use]
fn keysym_to_vk(keysym: u32) -> Option<u16> {
    Some(match keysym {
        // Space.
        0x20 => 0x20,
        // Letters: either case is the same key.
        0x61..=0x7a => 0x41 + u16::try_from(keysym - 0x61).ok()?,
        0x41..=0x5a => u16::try_from(keysym).ok()?,
        // Digits.
        0x30..=0x39 => u16::try_from(keysym).ok()?,
        // Punctuation, both spellings of each key.
        0x27 | 0x22 => 0xDE, // ' "
        0x2c | 0x3c => 0xBC, // , <
        0x2d | 0x5f => 0xBD, // - _
        0x2e | 0x3e => 0xBE, // . >
        0x2f | 0x3f => 0xBF, // / ?
        0x3b | 0x3a => 0xBA, // ; :
        0x3d | 0x2b => 0xBB, // = +
        0x5b | 0x7b => 0xDB, // [ {
        0x5c | 0x7c => 0xDC, // \ |
        0x5d | 0x7d => 0xDD, // ] }
        0x60 | 0x7e => 0xC0, // ` ~
        // The shifted digit row: the key is the digit.
        0x21 => 0x31, // !
        0x40 => 0x32, // @
        0x23 => 0x33, // #
        0x24 => 0x34, // $
        0x25 => 0x35, // %
        0x5e => 0x36, // ^
        0x26 => 0x37, // &
        0x2a => 0x38, // *
        0x28 => 0x39, // (
        0x29 => 0x30, // )
        // Editing and navigation.
        0xff08 => 0x08,                                                 // BackSpace
        0xfe20 | 0xff09 => 0x09,                                        // ISO_Left_Tab, Tab
        0xff0d | 0xff8d => 0x0D,                                        // Return, KP_Enter
        0xff13 => 0x13,                                                 // Pause
        0xff14 => 0x91,                                                 // Scroll_Lock
        0xff15 | 0xff61 => 0x2C,                                        // Sys_Req, Print
        0xff1b => 0x1B,                                                 // Escape
        0xffff => 0x2E,                                                 // Delete
        0xff63 => 0x2D,                                                 // Insert
        0xff50 => 0x24,                                                 // Home
        0xff51 => 0x25,                                                 // Left
        0xff52 => 0x26,                                                 // Up
        0xff53 => 0x27,                                                 // Right
        0xff54 => 0x28,                                                 // Down
        0xff55 => 0x21,                                                 // Page_Up
        0xff56 => 0x22,                                                 // Page_Down
        0xff57 => 0x23,                                                 // End
        0xffe5 => 0x14,                                                 // Caps_Lock
        0xffbe..=0xffd5 => 0x70 + u16::try_from(keysym - 0xffbe).ok()?, // F1–F24
        // The keypad.
        0xffb0..=0xffb9 => 0x60 + u16::try_from(keysym - 0xffb0).ok()?,
        0xffaa => 0x6A, // KP_Multiply
        0xffab => 0x6B, // KP_Add
        0xffad => 0x6D, // KP_Subtract
        0xffae => 0x6E, // KP_Decimal
        0xffaf => 0x6F, // KP_Divide
        0xffbd => 0x92, // KP_Equal
        0xff7f => 0x90, // Num_Lock
        // Media and volume keys.
        0x1008ff12 => 0xAD, // XF86AudioMute
        0x1008ff11 => 0xAE, // XF86AudioLowerVolume
        0x1008ff13 => 0xAF, // XF86AudioRaiseVolume
        0x1008ff17 => 0xB0, // XF86AudioNext
        0x1008ff16 => 0xB1, // XF86AudioPrev
        0x1008ff15 => 0xB2, // XF86AudioStop
        0x1008ff14 => 0xB3, // XF86AudioPlay
        0x1008ff31 => 0x13, // XF86AudioPause, read as the Pause key
        0x1008ff32 => 0xFA, // XF86AudioMedia
        _ => return None,
    })
}

/// The cached connection, opened on the first poll and reopened after a
/// connection error.
static READER: Mutex<Option<Reader>> = Mutex::new(None);

/// Whether the "no X11 keyboard state" warning was already printed. Polling
/// happens every frame, so the reason is logged once, not sixty times a
/// second.
static WARNED: OnceLock<()> = OnceLock::new();

/// The keyboard's state, or "nothing is down" when X11 cannot be asked.
///
/// Never fails: the caller is a per-frame poll, and a session that cannot read
/// the keyboard is the Wayland/headless case the stub exists for. A connection
/// that broke is dropped so the next poll opens a fresh one.
pub(crate) fn keyboard_state() -> KeyState {
    let mut guard = READER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.is_none() {
        match Reader::connect() {
            Ok(reader) => *guard = Some(reader),
            Err(err) => {
                if WARNED.set(()).is_ok() {
                    tracing::warn!(
                        error = %err,
                        "the X11 keyboard state cannot be read; paste-by-key and the hotkey \
                         recorder fall back to egui's events"
                    );
                }
                return KeyState::default();
            }
        }
    }
    let Some(reader) = guard.as_mut() else {
        return KeyState::default();
    };
    match reader.state() {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "the X11 keyboard state could not be read; reconnecting"
            );
            *guard = None;
            KeyState::default()
        }
    }
}

/// One X connection and the keymap read once from it.
struct Reader {
    conn: RustConnection,
    keymap: Keymap,
}

impl Reader {
    /// Connects and reads the keyboard mapping.
    fn connect() -> Result<Self, String> {
        let (conn, _screen) = x11rb::connect(None)
            .map_err(|err| format!("could not connect to the X display: {err}"))?;
        let setup = conn.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - setup.min_keycode + 1;
        let reply = xproto::get_keyboard_mapping(&conn, first, count)
            .map_err(|err| err.to_string())?
            .reply()
            .map_err(|err| err.to_string())?;
        let keymap = Keymap::from_keysyms(first, reply.keysyms_per_keycode, &reply.keysyms);
        tracing::debug!(
            first_keycode = first,
            keysyms_per_keycode = reply.keysyms_per_keycode,
            "the X11 keyboard mapping is read for the key-state poll"
        );
        Ok(Self { conn, keymap })
    }

    /// One `XQueryKeymap` round trip.
    fn state(&self) -> Result<KeyState, String> {
        let reply = xproto::query_keymap(&self.conn)
            .map_err(|err| err.to_string())?
            .reply()
            .map_err(|err| err.to_string())?;
        Ok(self.keymap.state(&reply.keys))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A keymap whose keycodes start at 8, the server's usual minimum.
    fn map_of(keysyms: &[u32]) -> Keymap {
        Keymap::from_keysyms(8, 1, keysyms)
    }

    /// A bitmap with `keycodes` down.
    fn bitmap(keycodes: &[u8]) -> [u8; 32] {
        let mut bitmap = [0u8; 32];
        for keycode in keycodes {
            bitmap[usize::from(*keycode) / 8] |= 1 << (keycode % 8);
        }
        bitmap
    }

    #[test]
    fn letters_digits_and_punctuation_map_to_the_windows_virtual_keys() {
        // Lower and upper case are one key; the recorder reads keys, not
        // characters.
        assert_eq!(keysym_to_vk(0x61), Some(0x41), "a");
        assert_eq!(keysym_to_vk(0x41), Some(0x41), "A");
        assert_eq!(keysym_to_vk(0x7a), Some(0x5A), "z");
        assert_eq!(keysym_to_vk(0x30), Some(0x30), "0");
        assert_eq!(keysym_to_vk(0x39), Some(0x39), "9");
        for (keysym, vk) in [
            (0x3b, 0xBA), // ;
            (0x3a, 0xBA), // :
            (0x27, 0xDE), // '
            (0x22, 0xDE), // "
            (0x2f, 0xBF), // /
            (0x3f, 0xBF), // ?
            (0x5b, 0xDB), // [
            (0x7b, 0xDB), // {
            (0x60, 0xC0), // `
            (0x7e, 0xC0), // ~
        ] {
            assert_eq!(keysym_to_vk(keysym), Some(vk), "{keysym:#x}");
        }
        // The shifted digit row is the digit key.
        assert_eq!(keysym_to_vk(0x21), Some(0x31), "!");
        assert_eq!(keysym_to_vk(0x29), Some(0x30), ")");
    }

    #[test]
    fn escape_function_and_keypad_keys_map_to_their_virtual_keys() {
        assert_eq!(keysym_to_vk(0xff1b), Some(0x1B), "Escape");
        assert_eq!(keysym_to_vk(0xff08), Some(0x08), "BackSpace");
        assert_eq!(keysym_to_vk(0xffff), Some(0x2E), "Delete");
        assert_eq!(keysym_to_vk(0xff51), Some(0x25), "Left");
        assert_eq!(keysym_to_vk(0xffbe), Some(0x70), "F1");
        assert_eq!(keysym_to_vk(0xffd5), Some(0x87), "F24");
        assert_eq!(keysym_to_vk(0xffb0), Some(0x60), "KP_0");
        assert_eq!(keysym_to_vk(0xffaf), Some(0x6F), "KP_Divide");
        assert_eq!(keysym_to_vk(0x1008ff13), Some(0xAF), "volume up");
        // A key the app has no name for is not guessed at.
        assert_eq!(keysym_to_vk(0x0100_0000), None);
        assert_eq!(keysym_to_vk(0x6c1), None, "Cyrillic");
    }

    #[test]
    fn modifiers_are_flags_and_never_recorded_keys() {
        // Keycodes 8–11 are the four modifiers; 12 is `a`.
        let keymap = map_of(&[0xffe1, 0xffe3, 0xffe9, 0xffeb, 0x61]);
        let state = keymap.state(&bitmap(&[8, 9, 10, 11, 12]));
        assert!(state.ctrl, "Control_L");
        assert!(state.shift, "Shift_L");
        assert!(state.alt, "Alt_L");
        assert!(state.win, "Super_L");
        assert_eq!(state.keys, vec![0x41], "only `a` is a key");

        // AltGr is Alt, the way Windows reports it.
        let keymap = map_of(&[0xfe03, 0x61]);
        let state = keymap.state(&bitmap(&[8, 9]));
        assert!(state.alt, "ISO_Level3_Shift");
        assert_eq!(state.keys, vec![0x41]);
    }

    #[test]
    fn only_the_keycodes_in_the_bitmap_are_down() {
        let keymap = map_of(&[0x61, 0x62, 0x63]);
        // Keycode 9 is `b`; 8 and 10 are up.
        let state = keymap.state(&bitmap(&[9]));
        assert_eq!(state.keys, vec![0x42]);
        assert!(!state.ctrl && !state.shift && !state.alt && !state.win);

        // The bitmap's bit order: keycode 8 is byte 1 bit 0, not byte 0.
        assert!(key_down(&bitmap(&[8]), 8));
        assert!(!key_down(&[0u8; 32], 0), "nothing is down");
        assert!(!key_down(&bitmap(&[8]), 9));
        assert!(key_down(&bitmap(&[255]), 255), "the last keycode");
    }

    #[test]
    fn the_first_keysym_of_each_keycode_is_the_one_that_counts() {
        // Two keysyms per keycode, as a real server reports: unshifted then
        // shifted. Keycode 8 is `1`/`!`, keycode 9 is `2`/`@`.
        let keymap = Keymap::from_keysyms(8, 2, &[0x31, 0x21, 0x32, 0x40]);
        assert_eq!(
            keymap.state(&bitmap(&[8, 9])).keys,
            vec![0x31, 0x32],
            "the unshifted keysyms, one virtual key each"
        );

        // A shifted spelling in the first slot still maps to its key, and a
        // zero keysyms-per-keycode reply is empty rather than a panic.
        let keymap = Keymap::from_keysyms(8, 1, &[0x3f, 0x61]);
        assert_eq!(keymap.state(&bitmap(&[8, 9])).keys, vec![0x41, 0xBF]);
        let empty = Keymap::from_keysyms(8, 0, &[0x61]);
        assert_eq!(empty.state(&[0xff; 32]), KeyState::default());
    }

    /// The L1c review: a keypad keycode's first keysym is the NumLock-off one,
    /// which names no key, so numpad keys could not be recorded. These are
    /// the Mac mini's real keycodes 79, 90, 91 and 86 (`xmodmap -pke`, US
    /// layout, four keysyms per keycode).
    #[test]
    fn keypad_keys_are_recorded_by_their_digit() {
        let keymap = Keymap::from_keysyms(
            79,
            4,
            &[
                0xff95, 0xffb7, 0xff95, 0xffb7, // 79: KP_Home KP_7
                0xff97, 0xffb8, 0xff97, 0xffb8, // 80: KP_Up KP_8
                0xff9a, 0xffb9, 0xff9a, 0xffb9, // 81: KP_Prior KP_9
                0xffad, 0xffad, 0xffad, 0xffad, // 82: KP_Subtract
                0xff96, 0xffb4, 0xff96, 0xffb4, // 83: KP_Left KP_4
                0xff9d, 0xffb5, 0xff9d, 0xffb5, // 84: KP_Begin KP_5
                0xff98, 0xffb6, 0xff98, 0xffb6, // 85: KP_Right KP_6
                0xffab, 0xffab, 0xffab, 0xffab, // 86: KP_Add
                0xff9c, 0xffb1, 0xff9c, 0xffb1, // 87: KP_End KP_1
                0xff99, 0xffb2, 0xff99, 0xffb2, // 88: KP_Down KP_2
                0xff9b, 0xffb3, 0xff9b, 0xffb3, // 89: KP_Next KP_3
                0xff9e, 0xffb0, 0xff9e, 0xffb0, // 90: KP_Insert KP_0
                0xff9f, 0xffae, 0xff9f, 0xffae, // 91: KP_Delete KP_Decimal
            ],
        );
        assert_eq!(keymap.state(&bitmap(&[79])).keys, vec![0x67], "KP_7");
        assert_eq!(keymap.state(&bitmap(&[90])).keys, vec![0x60], "KP_0");
        assert_eq!(keymap.state(&bitmap(&[91])).keys, vec![0x6E], "KP_Decimal");
        // Keys without a digit keep their first keysym.
        assert_eq!(keymap.state(&bitmap(&[86])).keys, vec![0x6B], "KP_Add");
        // All ten digits.
        let digits = keymap.state(&bitmap(&[79, 80, 81, 83, 84, 85, 87, 88, 89, 90]));
        assert_eq!(digits.keys, (0x60..=0x69).collect::<Vec<u16>>());
    }

    #[test]
    fn duplicate_virtual_keys_are_reported_once() {
        // Return and KP_Enter are both VK_RETURN; holding both must not make
        // the recorder see two keys.
        let keymap = map_of(&[0xff0d, 0xff8d]);
        let state = keymap.state(&bitmap(&[8, 9]));
        assert_eq!(state.keys, vec![0x0D]);

        // The bitmap can be wider than the keymap; unmapped keycodes are not
        // read past the entries.
        assert_eq!(keymap.state(&bitmap(&[200, 201])).keys, Vec::<u16>::new());
    }
}
