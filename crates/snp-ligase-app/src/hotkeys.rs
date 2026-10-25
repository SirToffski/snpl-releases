//! Global hotkeys (SPEC section 6.1).
//!
//! The manager has to be created on a thread with a message loop, so it is
//! created on the main thread, before the window — the same thread that later
//! pumps messages for winit. The handler that
//! `global_hotkey` calls from that pump does one thing: send an
//! [`AppCommand::Capture`] over the controller's channel (SPEC 6.2). It never
//! captures, hides or shows anything itself.
//!
//! Registration can fail — another program may already own the key, and on
//! Windows 11 the Print Screen key is often wired to Snipping Tool. Failures
//! are returned as warnings for the status bar, never as a reason to stop
//! starting.
//!
//! The keys themselves are settings (SPEC section 10's "Hotkeys for the four
//! capture modes"), so the bindings are parsed from [`Settings`] and the guard
//! can be re-bound when the Settings dialog is confirmed. Two modes on the same
//! key are reported as a conflict before anything is registered.

use std::sync::mpsc::Sender;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::controller::AppCommand;
use crate::panels::CaptureMode;
use crate::settings::Settings;

/// One key with what it captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub hotkey: HotKey,
    pub mode: CaptureMode,
}

/// The four modes with a global hotkey (SPEC 6.1), in the order the settings
/// file stores them.
pub const HOTKEY_MODES: [CaptureMode; 4] = [
    CaptureMode::Region,
    CaptureMode::CurrentMonitor,
    CaptureMode::ActiveWindow,
    CaptureMode::AllMonitors,
];

/// The SPEC 6.1 defaults: Print Screen is Region, and the three modifiers pick
/// the other three modes.
#[must_use]
pub fn default_bindings() -> Vec<Binding> {
    const BINDINGS: [(Option<Modifiers>, Code, CaptureMode); 4] = [
        (None, Code::PrintScreen, CaptureMode::Region),
        (
            Some(Modifiers::CONTROL),
            Code::PrintScreen,
            CaptureMode::CurrentMonitor,
        ),
        (
            Some(Modifiers::SHIFT),
            Code::PrintScreen,
            CaptureMode::ActiveWindow,
        ),
        (
            Some(Modifiers::CONTROL.union(Modifiers::SHIFT)),
            Code::PrintScreen,
            CaptureMode::AllMonitors,
        ),
    ];

    BINDINGS
        .iter()
        .map(|&(mods, key, mode)| Binding {
            hotkey: HotKey::new(mods, key),
            mode,
        })
        .collect()
}

/// The defaults as the strings the settings file stores, in
/// [`HOTKEY_MODES`] order.
#[must_use]
pub fn default_hotkey_strings() -> [String; 4] {
    let mut out = [String::new(), String::new(), String::new(), String::new()];
    for binding in default_bindings() {
        if let Some(index) = HOTKEY_MODES.iter().position(|mode| *mode == binding.mode) {
            out[index] = display_hotkey(&binding.hotkey);
        }
    }
    out
}

/// A key combination written the way the Settings dialog shows it:
/// `Ctrl+Shift+Print Screen` rather than the crate's `shift+control+PrintScreen`.
#[must_use]
pub fn display_hotkey(hotkey: &HotKey) -> String {
    let mut parts: Vec<String> = Vec::new();
    if hotkey.mods.contains(Modifiers::CONTROL) {
        parts.push("Ctrl".to_owned());
    }
    if hotkey.mods.contains(Modifiers::SHIFT) {
        parts.push("Shift".to_owned());
    }
    if hotkey.mods.contains(Modifiers::ALT) {
        parts.push("Alt".to_owned());
    }
    if hotkey.mods.contains(Modifiers::SUPER) {
        parts.push("Super".to_owned());
    }
    parts.push(friendly_key_name(hotkey.key));
    parts.join("+")
}

/// The name the Settings dialog shows for one key: "P" rather than "KeyP",
/// "1" rather than "Digit1", "Up" rather than "ArrowUp".
///
/// Every name this produces is also accepted by [`parse_hotkey`], which a
/// table test pins for every key a recording can produce — so the text a
/// recording writes into the field always parses back to the same combination.
#[must_use]
fn friendly_key_name(code: Code) -> String {
    match code {
        Code::Escape => "Esc".to_owned(),
        Code::PrintScreen => "Print Screen".to_owned(),
        Code::PageUp => "Page Up".to_owned(),
        Code::PageDown => "Page Down".to_owned(),
        Code::CapsLock => "Caps Lock".to_owned(),
        Code::NumLock => "Num Lock".to_owned(),
        Code::ScrollLock => "Scroll Lock".to_owned(),
        Code::Numpad0 => "Numpad 0".to_owned(),
        Code::Numpad1 => "Numpad 1".to_owned(),
        Code::Numpad2 => "Numpad 2".to_owned(),
        Code::Numpad3 => "Numpad 3".to_owned(),
        Code::Numpad4 => "Numpad 4".to_owned(),
        Code::Numpad5 => "Numpad 5".to_owned(),
        Code::Numpad6 => "Numpad 6".to_owned(),
        Code::Numpad7 => "Numpad 7".to_owned(),
        Code::Numpad8 => "Numpad 8".to_owned(),
        Code::Numpad9 => "Numpad 9".to_owned(),
        Code::NumpadAdd => "Numpad Add".to_owned(),
        Code::NumpadSubtract => "Numpad Subtract".to_owned(),
        Code::NumpadMultiply => "Numpad Multiply".to_owned(),
        Code::NumpadDivide => "Numpad Divide".to_owned(),
        Code::NumpadDecimal => "Numpad Decimal".to_owned(),
        Code::NumpadEnter => "Numpad Enter".to_owned(),
        Code::NumpadEqual => "Numpad Equal".to_owned(),
        Code::AudioVolumeDown => "Volume Down".to_owned(),
        Code::AudioVolumeUp => "Volume Up".to_owned(),
        Code::AudioVolumeMute => "Volume Mute".to_owned(),
        // Letters, digits and arrows: the `Code` name without its prefix
        // ("KeyP" -> "P", "Digit1" -> "1", "ArrowUp" -> "Up"). The parser
        // accepts both spellings.
        other => {
            let name = other.to_string();
            for prefix in ["Key", "Digit", "Arrow"] {
                if let Some(rest) = name.strip_prefix(prefix) {
                    return rest.to_owned();
                }
            }
            name
        }
    }
}

/// Every key a recording can produce, as (Windows virtual key, [`Code`]).
///
/// The virtual keys are the ones `global-hotkey` 0.8.0's Windows backend
/// registers for the same `Code` (`global-hotkey-0.8.0/src/platform_impl/windows/mod.rs`,
/// `key_to_vk`), so a combination recorded here is a combination that fires.
/// Three `Code`s share a virtual key with a partner and are handled by
/// [`code_to_vk`]: `NumpadEnter` (Windows reports `VK_RETURN` for both),
/// `MediaPause` (there is no separate media-pause key) and `NumpadEqual`
/// (`global-hotkey` maps it to the E key, a crate bug; this table keeps the
/// real numpad key, 0x92, and `DECISIONS.md` records the trap).
const VIRTUAL_KEYS: &[(u16, Code)] = &[
    // Editing and navigation.
    (0x08, Code::Backspace),
    (0x09, Code::Tab),
    (0x0D, Code::Enter),
    (0x13, Code::Pause),
    (0x14, Code::CapsLock),
    (0x1B, Code::Escape),
    (0x20, Code::Space),
    (0x21, Code::PageUp),
    (0x22, Code::PageDown),
    (0x23, Code::End),
    (0x24, Code::Home),
    (0x25, Code::ArrowLeft),
    (0x26, Code::ArrowUp),
    (0x27, Code::ArrowRight),
    (0x28, Code::ArrowDown),
    (0x2C, Code::PrintScreen),
    (0x2D, Code::Insert),
    (0x2E, Code::Delete),
    // The main block: digits, letters, then the punctuation keys.
    (0x30, Code::Digit0),
    (0x31, Code::Digit1),
    (0x32, Code::Digit2),
    (0x33, Code::Digit3),
    (0x34, Code::Digit4),
    (0x35, Code::Digit5),
    (0x36, Code::Digit6),
    (0x37, Code::Digit7),
    (0x38, Code::Digit8),
    (0x39, Code::Digit9),
    (0x41, Code::KeyA),
    (0x42, Code::KeyB),
    (0x43, Code::KeyC),
    (0x44, Code::KeyD),
    (0x45, Code::KeyE),
    (0x46, Code::KeyF),
    (0x47, Code::KeyG),
    (0x48, Code::KeyH),
    (0x49, Code::KeyI),
    (0x4A, Code::KeyJ),
    (0x4B, Code::KeyK),
    (0x4C, Code::KeyL),
    (0x4D, Code::KeyM),
    (0x4E, Code::KeyN),
    (0x4F, Code::KeyO),
    (0x50, Code::KeyP),
    (0x51, Code::KeyQ),
    (0x52, Code::KeyR),
    (0x53, Code::KeyS),
    (0x54, Code::KeyT),
    (0x55, Code::KeyU),
    (0x56, Code::KeyV),
    (0x57, Code::KeyW),
    (0x58, Code::KeyX),
    (0x59, Code::KeyY),
    (0x5A, Code::KeyZ),
    (0xBA, Code::Semicolon),
    (0xBB, Code::Equal),
    (0xBC, Code::Comma),
    (0xBD, Code::Minus),
    (0xBE, Code::Period),
    (0xBF, Code::Slash),
    (0xC0, Code::Backquote),
    (0xDB, Code::BracketLeft),
    (0xDC, Code::Backslash),
    (0xDD, Code::BracketRight),
    (0xDE, Code::Quote),
    // Function keys.
    (0x70, Code::F1),
    (0x71, Code::F2),
    (0x72, Code::F3),
    (0x73, Code::F4),
    (0x74, Code::F5),
    (0x75, Code::F6),
    (0x76, Code::F7),
    (0x77, Code::F8),
    (0x78, Code::F9),
    (0x79, Code::F10),
    (0x7A, Code::F11),
    (0x7B, Code::F12),
    (0x7C, Code::F13),
    (0x7D, Code::F14),
    (0x7E, Code::F15),
    (0x7F, Code::F16),
    (0x80, Code::F17),
    (0x81, Code::F18),
    (0x82, Code::F19),
    (0x83, Code::F20),
    (0x84, Code::F21),
    (0x85, Code::F22),
    (0x86, Code::F23),
    (0x87, Code::F24),
    // The numpad.
    (0x60, Code::Numpad0),
    (0x61, Code::Numpad1),
    (0x62, Code::Numpad2),
    (0x63, Code::Numpad3),
    (0x64, Code::Numpad4),
    (0x65, Code::Numpad5),
    (0x66, Code::Numpad6),
    (0x67, Code::Numpad7),
    (0x68, Code::Numpad8),
    (0x69, Code::Numpad9),
    (0x6A, Code::NumpadMultiply),
    (0x6B, Code::NumpadAdd),
    (0x6D, Code::NumpadSubtract),
    (0x6E, Code::NumpadDecimal),
    (0x6F, Code::NumpadDivide),
    (0x90, Code::NumLock),
    (0x91, Code::ScrollLock),
    (0x92, Code::NumpadEqual),
    // Media and volume keys.
    (0xAD, Code::AudioVolumeMute),
    (0xAE, Code::AudioVolumeDown),
    (0xAF, Code::AudioVolumeUp),
    (0xB0, Code::MediaTrackNext),
    (0xB1, Code::MediaTrackPrevious),
    (0xB2, Code::MediaStop),
    (0xB3, Code::MediaPlayPause),
    (0xFA, Code::MediaPlay),
];

/// The [`Code`] for a Windows virtual key, if a recording can name it.
#[must_use]
pub fn vk_to_code(vk: u16) -> Option<Code> {
    VIRTUAL_KEYS
        .iter()
        .find(|(key, _)| *key == vk)
        .map(|(_, code)| *code)
}

/// The Windows virtual key for a [`Code`], if it has one.
///
/// The three codes that share a virtual key with a partner
/// ([`Code::NumpadEnter`], [`Code::MediaPause`], [`Code::NumpadEqual`]) map to
/// the key Windows reports for them; see [`VIRTUAL_KEYS`].
#[must_use]
pub fn code_to_vk(code: Code) -> Option<u16> {
    match code {
        // `GetAsyncKeyState` reports VK_RETURN for the numpad Enter too, and
        // there is no separate media-pause virtual key; a recording produces
        // the partner. The reverse mapping still names them, for hotkeys typed
        // by hand.
        Code::NumpadEnter => Some(0x0D),
        Code::MediaPause => Some(0x13),
        other => VIRTUAL_KEYS
            .iter()
            .find(|(_, kept)| *kept == other)
            .map(|(vk, _)| *vk),
    }
}

/// Parses a hotkey as the settings file or the dialog writes it.
///
/// Both spellings are accepted: the friendly one this app writes
/// (`Ctrl+Shift+Print Screen`) and the crate's own (`shift+control+PrintScreen`).
/// An empty string means "no hotkey for this mode", which is a valid choice.
///
/// # Errors
///
/// A message for the dialog and the status bar when the text is not a key
/// combination.
pub fn parse_hotkey(text: &str) -> Result<Option<HotKey>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut normalized = trimmed
        .to_lowercase()
        .replace("print screen", "printscreen");
    normalized.retain(|character| !character.is_whitespace());
    normalized = normalized.replace("ctrl", "control");
    match normalized.parse::<HotKey>() {
        Ok(hotkey) => Ok(Some(hotkey)),
        Err(err) => Err(format!(
            "\"{text}\" is not a key combination I can use ({err})"
        )),
    }
}

/// The bindings a settings object asks for, plus a warning per key that could
/// not be parsed and per conflict.
///
/// A key two modes both claim is registered once, for the first mode that
/// asked: Windows would refuse the second registration anyway, and the conflict
/// warning says what happened.
#[must_use]
pub fn bindings_from(settings: &Settings) -> (Vec<Binding>, Vec<String>) {
    let mut parsed: Vec<Binding> = Vec::new();
    let mut warnings = Vec::new();
    for mode in HOTKEY_MODES {
        match parse_hotkey(settings.hotkey_text(mode)) {
            Ok(Some(hotkey)) => parsed.push(Binding { hotkey, mode }),
            // Unbound on purpose: the mode is still on the menu and the tray.
            Ok(None) => {}
            Err(message) => {
                warnings.push(format!("The {} hotkey is off: {message}", mode.label()));
            }
        }
    }
    warnings.extend(conflict_warnings(&parsed));

    let mut bindings: Vec<Binding> = Vec::new();
    for binding in parsed {
        if !bindings.iter().any(|kept| kept.hotkey == binding.hotkey) {
            bindings.push(binding);
        }
    }
    (bindings, warnings)
}

/// Every pair of bindings that use the same key (SPEC section 10's "conflict
/// warnings").
#[must_use]
pub fn conflict_warnings(bindings: &[Binding]) -> Vec<String> {
    let mut warnings = Vec::new();
    for (index, binding) in bindings.iter().enumerate() {
        for other in &bindings[index + 1..] {
            if binding.hotkey == other.hotkey {
                warnings.push(format!(
                    "{} and {} are both {}; only one of them can work.",
                    binding.mode.label(),
                    other.mode.label(),
                    display_hotkey(&binding.hotkey)
                ));
            }
        }
    }
    warnings
}

/// The conflicts in a settings draft, for the Settings dialog to show while the
/// user is typing.
#[must_use]
pub fn conflicts_in_settings(settings: &Settings) -> Vec<String> {
    let mut parsed = Vec::new();
    for mode in HOTKEY_MODES {
        if let Ok(Some(hotkey)) = parse_hotkey(settings.hotkey_text(mode)) {
            parsed.push(Binding { hotkey, mode });
        }
    }
    conflict_warnings(&parsed)
}

/// What the platform's keyboard poll saw this frame (NEXT_STEPS step 7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecorderInput {
    /// Either Ctrl is down.
    pub ctrl: bool,
    /// Either Shift is down.
    pub shift: bool,
    /// Either Alt is down.
    pub alt: bool,
    /// Either Windows key is down.
    pub win: bool,
    /// The virtual keys of the non-modifier keys that are down.
    pub keys: Vec<u16>,
}

impl RecorderInput {
    /// From the platform's keyboard snapshot.
    #[must_use]
    pub fn from_snapshot(snapshot: &snp_ligase_platform::keys::KeyboardSnapshot) -> Self {
        Self {
            ctrl: snapshot.ctrl,
            shift: snapshot.shift,
            alt: snapshot.alt,
            win: snapshot.win,
            keys: snapshot.keys.clone(),
        }
    }

    /// The modifiers as the hotkey crate names them.
    #[must_use]
    fn modifiers(&self) -> Modifiers {
        let mut mods = Modifiers::empty();
        if self.ctrl {
            mods |= Modifiers::CONTROL;
        }
        if self.shift {
            mods |= Modifiers::SHIFT;
        }
        if self.alt {
            mods |= Modifiers::ALT;
        }
        if self.win {
            mods |= Modifiers::SUPER;
        }
        mods
    }
}

/// What a finished recording decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    /// A named key the native hotkey backend cannot register correctly.
    #[cfg_attr(not(windows), allow(dead_code))]
    Rejected(&'static str),
    /// The first non-modifier key went down: use this combination.
    Hotkey(HotKey),
    /// Backspace or Delete alone: the field becomes empty ("None").
    Clear,
    /// Esc: leave the field as it is.
    Cancel,
}

/// The recorder's state machine (NEXT_STEPS step 7).
///
/// Fed one [`RecorderInput`] per frame. Recording finishes on the first
/// non-modifier key **going down**, with the modifiers held at that moment; a
/// key already down when recording starts (or already seen) does not count
/// until it is released and pressed again. Modifier-only frames keep waiting.
/// Esc cancels; Backspace or Delete alone clears the field; a real key ends it
/// with the held modifiers.
pub struct Recorder {
    /// The non-modifier keys that were down the last time the recorder looked.
    down: Vec<u16>,
}

impl Recorder {
    /// A recorder that ignores every key already down — `already_down` is the
    /// snapshot taken when recording started, so a key being held at that
    /// moment waits for a release and a fresh press.
    #[must_use]
    pub fn new(already_down: Vec<u16>) -> Self {
        Self { down: already_down }
    }

    /// Feeds one keyboard snapshot. `Some` ends the recording.
    ///
    /// When two keys go down between two polls, the first one in the
    /// snapshot's (ascending virtual-key) order wins.
    pub fn update(&mut self, input: &RecorderInput) -> Option<Recorded> {
        let mut outcome = None;
        for vk in &input.keys {
            if self.down.contains(vk) {
                continue;
            }
            // A key with no name (Break, a vendor key) cannot be written
            // down; it is ignored rather than ending the recording with
            // nothing.
            let Some(code) = vk_to_code(*vk) else {
                continue;
            };
            outcome = Some(match code {
                #[cfg(windows)]
                Code::NumpadEqual => Recorded::Rejected(
                    "Numpad Equal cannot be used on Windows: the hotkey backend registers it as the E key. Choose another key.",
                ),
                Code::Escape => Recorded::Cancel,
                Code::Backspace | Code::Delete if input.modifiers().is_empty() => Recorded::Clear,
                other => Recorded::Hotkey(HotKey::new(Some(input.modifiers()), other)),
            });
            break;
        }
        self.down.clone_from(&input.keys);
        outcome
    }
}

/// The bindings the installed global-hotkey event handler matches against.
///
/// `global-hotkey` 0.8.0 keeps the handler in a `OnceCell`
/// (`global-hotkey-0.8.0/src/lib.rs:89`): the first `set_event_handler`
/// call wins and later ones are silently ignored (`lib.rs:118-124`). The
/// handler therefore cannot capture one rebind's bindings; it reads this
/// table, which every rebind replaces. Without it, a hotkey recorded in
/// Settings is registered but its events are dropped by the stale startup
/// handler — found by the Step 7 real run (the `hotkey registered` line
/// without a later `hotkey pressed`).
///
/// A **process-wide** table, not a thread-local: on Windows the handler runs
/// on the main thread's message pump, but on X11 `global-hotkey` calls it from
/// a thread of its own (`global-hotkey-0.8.0/src/platform_impl/x11/mod.rs`,
/// `std::thread::spawn` in `GlobalHotKeyManager::new`), where a thread-local
/// would always be empty — the keys would register and never fire
/// (LINUX_SUPPORT.md section 4).
static HANDLER_BINDINGS: std::sync::Mutex<Vec<Binding>> = std::sync::Mutex::new(Vec::new());

/// Replaces the table the event handler reads.
fn set_handler_bindings(bindings: Vec<Binding>) {
    // A poisoned lock means a panic happened while the table was held; the
    // table itself is still a valid `Vec`, so the hotkeys keep working.
    let mut table = HANDLER_BINDINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *table = bindings;
}

/// The table the event handler reads.
fn handler_bindings() -> Vec<Binding> {
    HANDLER_BINDINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The mode an event id belongs to, from the bindings that are registered.
fn mode_for_event(bindings: &[Binding], event_id: u32) -> Option<CaptureMode> {
    bindings
        .iter()
        .find(|binding| binding.hotkey.id() == event_id)
        .map(|binding| binding.mode)
}

/// The registered hotkeys. Keep this alive for as long as the hotkeys should
/// work: dropping it unregisters them.
pub struct Hotkeys {
    /// The manager owns the message-only window the registrations live on;
    /// dropping it unregisters every key.
    manager: Option<GlobalHotKeyManager>,
    /// What is registered, so a rebind can replace it.
    bindings: Vec<Binding>,
    /// True for [`Hotkeys::none`]: a guard that must never touch the real
    /// keyboard, which is what a test gets.
    inert: bool,
    #[cfg(target_os = "linux")]
    portal: Option<snp_ligase_platform::linux::wayland::shortcuts::Shortcuts>,
    /// One entry per `rebind` call, in order. Only written in tests: it is how
    /// a test proves that a recording's guard put the stored bindings back on
    /// every exit path without ever touching the real keyboard.
    #[cfg(test)]
    rebind_log: Vec<Vec<Binding>>,
    /// What each `release` saw in `bindings`, whether or not a manager was
    /// live. A rebind must hand back the **old** keys, not the new ones.
    #[cfg(test)]
    release_log: Vec<Vec<Binding>>,
}

impl Hotkeys {
    /// No hotkeys at all, and no way to bind any: for tests, so a test can
    /// never take the human's keys.
    #[cfg(test)]
    #[must_use]
    pub fn none() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            portal: None,
            manager: None,
            bindings: Vec::new(),
            inert: true,
            rebind_log: Vec::new(),
            release_log: Vec::new(),
        }
    }

    /// A guard with nothing registered that can still bind later: what
    /// [`Hotkeys::register`] starts from, so a startup failure can be retried
    /// from the Settings dialog.
    fn empty() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            portal: None,
            manager: None,
            bindings: Vec::new(),
            inert: false,
            #[cfg(test)]
            rebind_log: Vec::new(),
            #[cfg(test)]
            release_log: Vec::new(),
        }
    }

    /// Registers `bindings` and turns presses into capture commands.
    ///
    /// Returns the guard and one warning line per hotkey that could not be
    /// registered (SPEC 6.1: a non-blocking warning in the status bar).
    #[must_use]
    pub fn register(commands: Sender<AppCommand>, bindings: Vec<Binding>) -> (Self, Vec<String>) {
        let mut hotkeys = Self::empty();
        let warnings = hotkeys.rebind(commands, bindings);
        (hotkeys, warnings)
    }

    /// The desktop owns these bindings; Settings never re-registers them.
    #[cfg(target_os = "linux")]
    pub fn register_portal(
        capabilities: snp_ligase_platform::Capabilities,
        commands: Sender<AppCommand>,
        events: Sender<crate::controller::UiEvent>,
        window: std::sync::Arc<crate::controller::WindowGate>,
    ) -> (Self, Vec<String>) {
        use snp_ligase_platform::linux::wayland::shortcuts::{self, Event};
        let requested = portal_requests(&capabilities);
        let mut guard = Self::empty();
        // Inert even after failure: no accidental global-hotkey/XWayland fallback.
        guard.inert = true;
        match shortcuts::start(requested, move |event| match event {
            Event::Activated { id, token } => {
                if let Some(mode) = [
                    CaptureMode::Region,
                    CaptureMode::CurrentMonitor,
                    CaptureMode::ActiveWindow,
                    CaptureMode::AllMonitors,
                    CaptureMode::RepeatLastRegion,
                ]
                .into_iter()
                .find(|mode| portal_id(*mode) == id)
                {
                    let _ = commands.send(AppCommand::ActivatedCapture {
                        mode,
                        delay_seconds: 0,
                        token,
                    });
                }
            }
            Event::Status(message) => {
                let _ = events.send(crate::controller::UiEvent::Warning(message));
                window.wake();
            }
        }) {
            Ok(portal) => {
                guard.portal = Some(portal);
                (guard, Vec::new())
            }
            Err(err) => (guard, vec![format!("Global hotkeys unavailable: {err}")]),
        }
    }

    /// Replaces the registered keys with `bindings` (the Settings dialog's OK).
    ///
    /// Returns the same warnings [`Hotkeys::register`] does.
    pub fn rebind(&mut self, commands: Sender<AppCommand>, bindings: Vec<Binding>) -> Vec<String> {
        #[cfg(target_os = "linux")]
        if self.portal.is_some() {
            return Vec::new();
        }
        #[cfg(test)]
        self.rebind_log.push(bindings.clone());
        // Hand the old keys back **before** the table names the new ones:
        // `release` unregisters what `self.bindings` says, and those are the
        // keys the manager actually holds. On X11 dropping the manager only
        // *asks* its own thread to close the connection (up to a 50 ms poll
        // later), so the explicit unregister is what keeps a rebind from
        // registering against grabs the old manager still holds; without it
        // every line of Settings > OK reads "could not be registered".
        self.release();
        self.bindings = bindings;
        // The handler is installed once for the life of the process (see
        // `HANDLER_BINDINGS`); this is the table it reads, so it is replaced
        // even when this guard is inert.
        set_handler_bindings(self.bindings.clone());
        if self.inert {
            tracing::debug!("this guard never registers hotkeys (a test, or none)");
            return Vec::new();
        }
        let mut warnings = Vec::new();

        let manager = match GlobalHotKeyManager::new() {
            Ok(manager) => manager,
            Err(err) => {
                let warning = format!(
                    "Global hotkeys are off: the keyboard manager could not start ({err})."
                );
                tracing::warn!("{warning}");
                return vec![warning];
            }
        };

        for binding in &self.bindings {
            if let Err(err) = manager.register(binding.hotkey) {
                let warning = registration_warning(binding, &err.to_string());
                tracing::warn!("{warning}");
                warnings.push(warning);
            } else {
                tracing::info!(
                    hotkey = %display_hotkey(&binding.hotkey),
                    mode = binding.mode.label(),
                    "hotkey registered"
                );
            }
        }

        // The handler runs on the main thread's message pump, which is exactly
        // why it only sends (SPEC 6.2). It reads the bindings table rather than
        // capturing this rebind's list: `global-hotkey` ignores every
        // `set_event_handler` call after the first (see `HANDLER_BINDINGS`).
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.state() != HotKeyState::Pressed {
                return;
            }
            let Some(mode) = mode_for_event(&handler_bindings(), event.id()) else {
                return;
            };
            // SPEC 5.7: hotkey captures start immediately, so no delay.
            let command = AppCommand::Capture {
                mode,
                delay_seconds: 0,
            };
            tracing::info!(mode = mode.label(), "hotkey pressed");
            if commands.send(command).is_err() {
                tracing::warn!("the controller is gone; the hotkey did nothing");
            }
        }));

        self.manager = Some(manager);
        warnings
    }

    /// Unregisters every key while a recording is in progress (NEXT_STEPS
    /// step 7).
    ///
    /// The recorder reads the keyboard with `QueryKeymap`/`GetAsyncKeyState`,
    /// which sees a press even while one of our own hotkeys owns it — but the
    /// handler would still fire a capture behind the Settings dialog. Handing
    /// the keys back takes every one of them out of the way; the recording
    /// guard re-registers the bindings it suspended when the recording ends.
    pub fn suspend(&mut self) {
        if self.inert {
            return;
        }
        if self.manager.is_some() {
            self.release();
            tracing::info!("global hotkeys unregistered while a hotkey is recorded");
        }
    }

    /// Hands every registered key back to the platform and drops the manager.
    ///
    /// The explicit `unregister` calls are what make a rebind work on X11:
    /// dropping the manager only *asks* its own thread to close its X
    /// connection, which is when the server releases the grabs, and the next
    /// registration can happen before that thread has woken up.
    fn release(&mut self) {
        #[cfg(test)]
        self.release_log.push(self.bindings.clone());
        if let Some(manager) = self.manager.take() {
            for binding in &self.bindings {
                if let Err(err) = manager.unregister(binding.hotkey) {
                    // A key that failed to register is not registered now
                    // either; that is not worth a warning line.
                    tracing::debug!(
                        hotkey = %display_hotkey(&binding.hotkey),
                        error = %err,
                        "a hotkey could not be handed back"
                    );
                }
            }
        }
    }

    /// The bindings every `rebind` call was asked for, in order. Tests use it
    /// to prove the recording guard restored the stored keys.
    #[cfg(test)]
    #[must_use]
    pub fn rebind_log(&self) -> &[Vec<Binding>] {
        &self.rebind_log
    }

    /// What each `release` saw, in order. Tests use it to prove that a rebind
    /// handed back the old keys rather than the ones it is replacing them
    /// with.
    #[cfg(test)]
    #[must_use]
    fn release_log(&self) -> &[Vec<Binding>] {
        &self.release_log
    }
}

/// What to say when a key could not be registered (SPEC 6.1).
///
/// The Print Screen case is the common one on Windows 11: the key opens
/// Snipping Tool unless that is turned off, and the fix is in Settings. The
/// binding carries the combination the user actually asked for, so the message
/// names that rather than the SPEC default.
#[must_use]
fn registration_warning(binding: &Binding, reason: &str) -> String {
    let name = format!(
        "{} ({})",
        binding.mode.label(),
        display_hotkey(&binding.hotkey)
    );
    let base = format!("{name} could not be registered: {reason}.");
    if binding.hotkey.key == Code::PrintScreen {
        format!(
            "{base} If another program owns the key, turn it off there; on Windows 11 the \
             Snipping Tool takes Print Screen unless you disable \"Use the Print screen key to \
             open Snipping Tool\" under Settings > Accessibility > Keyboard."
        )
    } else {
        format!("{base} Another program is probably already using it.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The binding table is process-wide, so the tests that touch it must not
    /// run at the same time as each other: every one of them takes this lock.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The lock, unpoisoned: one failing test must not break the others.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn the_defaults_are_the_four_spec_modes() {
        let bindings = default_bindings();
        assert_eq!(bindings.len(), 4);

        let modes: Vec<CaptureMode> = bindings.iter().map(|binding| binding.mode).collect();
        assert_eq!(
            modes,
            vec![
                CaptureMode::Region,
                CaptureMode::CurrentMonitor,
                CaptureMode::ActiveWindow,
                CaptureMode::AllMonitors
            ]
        );

        // All four are Print Screen, and the modifier tells them apart.
        for binding in &bindings {
            assert_eq!(binding.hotkey.key, Code::PrintScreen);
        }
        assert_eq!(bindings[0].hotkey.mods, Modifiers::empty());
        assert_eq!(bindings[1].hotkey.mods, Modifiers::CONTROL);
        assert_eq!(bindings[2].hotkey.mods, Modifiers::SHIFT);
        assert_eq!(
            bindings[3].hotkey.mods,
            Modifiers::CONTROL.union(Modifiers::SHIFT)
        );

        // The ids are distinct, which is what the event handler matches on.
        let mut ids: Vec<u32> = bindings.iter().map(|b| b.hotkey.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn a_failed_print_screen_registration_mentions_the_snipping_tool() {
        let binding = Binding {
            hotkey: HotKey::new(None, Code::PrintScreen),
            mode: CaptureMode::Region,
        };
        let warning = registration_warning(&binding, "already registered");
        assert!(warning.contains("Region"), "{warning}");
        assert!(warning.contains("Print Screen"), "{warning}");
        assert!(warning.contains("Snipping Tool"), "{warning}");
        assert!(
            warning.contains("Settings > Accessibility > Keyboard"),
            "{warning}"
        );
    }

    #[test]
    fn a_failed_registration_of_another_key_names_the_users_combination() {
        // The message names the mode and the combination the user asked for —
        // not the SPEC default — and points at the usual reason (another
        // program owns it).
        let binding = Binding {
            hotkey: HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyP),
            mode: CaptureMode::Region,
        };
        let warning = registration_warning(&binding, "already registered");
        assert!(warning.contains("Region"), "{warning}");
        assert!(warning.contains("Ctrl+Alt+P"), "{warning}");
        assert!(warning.contains("already registered"), "{warning}");
        assert!(warning.contains("Another program"), "{warning}");
        assert!(!warning.contains("Snipping Tool"), "{warning}");
    }

    /// The settings file stores the defaults as text; this pins that text to
    /// the real default bindings, so the two cannot drift.
    #[test]
    fn the_default_strings_parse_back_to_the_default_bindings() {
        let strings = default_hotkey_strings();
        assert_eq!(
            strings,
            [
                "Print Screen",
                "Ctrl+Print Screen",
                "Shift+Print Screen",
                "Ctrl+Shift+Print Screen"
            ]
        );
        for (index, mode) in HOTKEY_MODES.iter().enumerate() {
            let parsed = parse_hotkey(&strings[index])
                .expect("the default text parses")
                .expect("the default text is bound");
            let expected = default_bindings()
                .into_iter()
                .find(|binding| binding.mode == *mode)
                .expect("the mode has a default");
            assert_eq!(parsed, expected.hotkey, "{mode:?}");
        }
    }

    #[test]
    fn both_spellings_of_a_hotkey_parse_to_the_same_key() {
        let friendly = parse_hotkey("Ctrl+Shift+Print Screen").expect("parses");
        let crate_style = parse_hotkey("shift+control+PrintScreen").expect("parses");
        assert_eq!(friendly, crate_style);
        assert_eq!(
            friendly.map(|hotkey| display_hotkey(&hotkey)).as_deref(),
            Some("Ctrl+Shift+Print Screen"),
            "the display form is not the friendly one"
        );

        // Case and spacing do not matter.
        for text in [
            "ctrl+printscreen",
            " CTRL + PrintScreen ",
            "Control+Print Screen",
        ] {
            let parsed = parse_hotkey(text).expect("parses");
            assert_eq!(
                parsed.map(|hotkey| hotkey.key),
                Some(Code::PrintScreen),
                "{text}"
            );
        }

        // A letter key works too.
        let letter = parse_hotkey("Ctrl+Alt+K").expect("parses").expect("bound");
        assert_eq!(letter.key, Code::KeyK);
        assert!(letter.mods.contains(Modifiers::CONTROL));
        assert!(letter.mods.contains(Modifiers::ALT));
    }

    #[test]
    fn an_empty_string_means_no_hotkey_and_rubbish_is_an_error() {
        assert_eq!(parse_hotkey("").expect("empty is allowed"), None);
        assert_eq!(parse_hotkey("   ").expect("blank is allowed"), None);

        let error = parse_hotkey("banana").expect_err("not a key");
        assert!(error.contains("banana"), "{error}");
        let error = parse_hotkey("Ctrl+").expect_err("no key");
        assert!(error.contains("Ctrl+"), "{error}");
    }

    #[test]
    fn two_modes_on_the_same_key_are_a_conflict() {
        let settings = Settings {
            hotkey_current_monitor: "Ctrl+Print Screen".to_owned(),
            hotkey_active_window: "Ctrl+Print Screen".to_owned(),
            ..Settings::default()
        };
        let (bindings, warnings) = bindings_from(&settings);
        assert_eq!(bindings.len(), 3, "the duplicate should not register twice");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("Current Monitor"), "{warnings:?}");
        assert!(warnings[0].contains("Active Window"), "{warnings:?}");
        assert!(warnings[0].contains("Ctrl+Print Screen"), "{warnings:?}");

        assert_eq!(conflicts_in_settings(&settings), warnings);
    }

    #[test]
    fn a_settings_file_with_a_bad_hotkey_reports_it_and_keeps_the_rest() {
        let settings = Settings {
            hotkey_region: "banana".to_owned(),
            hotkey_current_monitor: String::new(),
            ..Settings::default()
        };
        let (bindings, warnings) = bindings_from(&settings);

        // Region is off and Current Monitor is unbound on purpose.
        assert_eq!(bindings.len(), 2);
        assert!(
            bindings
                .iter()
                .all(|binding| binding.mode != CaptureMode::Region)
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("Region"), "{warnings:?}");
    }

    /// Every key name `global-hotkey` 0.8.0's parser accepts
    /// (`global-hotkey-0.8.0/src/hotkey.rs`, `parse_key`), one canonical
    /// spelling per `Code`. The friendly spellings ("P", "Esc", "Up") are
    /// covered by the display round-trip test.
    const PARSER_KEYS: &[&str] = &[
        "Backquote",
        "Backslash",
        "BracketLeft",
        "BracketRight",
        "Comma",
        "Equal",
        "Minus",
        "Period",
        "Quote",
        "Semicolon",
        "Slash", //
        "Digit0",
        "Digit1",
        "Digit2",
        "Digit3",
        "Digit4",
        "Digit5",
        "Digit6",
        "Digit7",
        "Digit8",
        "Digit9", //
        "KeyA",
        "KeyB",
        "KeyC",
        "KeyD",
        "KeyE",
        "KeyF",
        "KeyG",
        "KeyH",
        "KeyI",
        "KeyJ",
        "KeyK",
        "KeyL",
        "KeyM",
        "KeyN",
        "KeyO",
        "KeyP",
        "KeyQ",
        "KeyR",
        "KeyS",
        "KeyT",
        "KeyU",
        "KeyV",
        "KeyW",
        "KeyX",
        "KeyY",
        "KeyZ", //
        "Backspace",
        "CapsLock",
        "Delete",
        "End",
        "Enter",
        "Escape",
        "Home",
        "Insert",
        "NumLock",
        "PageDown",
        "PageUp",
        "Pause",
        "PrintScreen",
        "ScrollLock",
        "Space",
        "Tab", //
        "ArrowDown",
        "ArrowLeft",
        "ArrowRight",
        "ArrowUp", //
        "Numpad0",
        "Numpad1",
        "Numpad2",
        "Numpad3",
        "Numpad4",
        "Numpad5",
        "Numpad6",
        "Numpad7",
        "Numpad8",
        "Numpad9", //
        "NumpadAdd",
        "NumpadDecimal",
        "NumpadDivide",
        "NumpadEnter",
        "NumpadEqual",
        "NumpadMultiply",
        "NumpadSubtract", //
        "F1",
        "F2",
        "F3",
        "F4",
        "F5",
        "F6",
        "F7",
        "F8",
        "F9",
        "F10",
        "F11",
        "F12",
        "F13",
        "F14",
        "F15",
        "F16",
        "F17",
        "F18",
        "F19",
        "F20",
        "F21",
        "F22",
        "F23",
        "F24", //
        "AudioVolumeDown",
        "AudioVolumeUp",
        "AudioVolumeMute", //
        "MediaPlay",
        "MediaPause",
        "MediaPlayPause",
        "MediaStop",
        "MediaTrackNext",
        "MediaTrackPrevious",
    ];

    /// The table test NEXT_STEPS step 7 asks for: every key the parser accepts
    /// has a virtual key, and the virtual key maps back to the same `Code` —
    /// except the two Windows cannot tell apart with `GetAsyncKeyState`.
    #[test]
    fn every_key_the_parser_accepts_has_a_virtual_key_and_round_trips() {
        for name in PARSER_KEYS {
            let code = parse_hotkey(name)
                .unwrap_or_else(|err| panic!("{name} does not parse: {err}"))
                .unwrap_or_else(|| panic!("{name} parsed as unbound"))
                .key;
            let vk = code_to_vk(code)
                .unwrap_or_else(|| panic!("{name} has no virtual key in the table"));
            let back =
                vk_to_code(vk).unwrap_or_else(|| panic!("{name}: {vk:#x} maps back to no code"));
            // NumpadEnter shares VK_RETURN with Enter, and there is no
            // separate media-pause virtual key; a recording produces the
            // partner.
            let expected = match code {
                Code::NumpadEnter => Code::Enter,
                Code::MediaPause => Code::Pause,
                other => other,
            };
            assert_eq!(back, expected, "{name} round-tripped to {back:?}");
        }
    }

    #[test]
    fn the_virtual_key_table_names_each_key_once() {
        let mut vks: Vec<u16> = VIRTUAL_KEYS.iter().map(|(vk, _)| *vk).collect();
        let count = vks.len();
        vks.sort_unstable();
        vks.dedup();
        assert_eq!(vks.len(), count, "two codes share a virtual key");

        for (index, (_, code)) in VIRTUAL_KEYS.iter().enumerate() {
            assert!(
                !VIRTUAL_KEYS[index + 1..]
                    .iter()
                    .any(|(_, other)| other == code),
                "{code:?} is in the table twice"
            );
        }
    }

    /// The text a recording writes into the field is parsed again when the
    /// dialog is applied, so every displayed name must parse back to its code.
    #[test]
    fn every_key_the_parser_accepts_displays_as_text_that_parses_back() {
        for name in PARSER_KEYS {
            let code = parse_hotkey(name).expect("parses").expect("bound").key;
            let shown = display_hotkey(&HotKey::new(None, code));
            let parsed = parse_hotkey(&shown)
                .unwrap_or_else(|err| panic!("{name} displays as \"{shown}\": {err}"))
                .unwrap_or_else(|| panic!("{name} displays as \"{shown}\", which is unbound"));
            assert_eq!(
                parsed.key, code,
                "\"{shown}\" parses back to a different key"
            );
        }
    }

    #[test]
    fn a_recorded_combination_displays_the_way_a_person_writes_it() {
        let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyP);
        assert_eq!(display_hotkey(&hotkey), "Ctrl+Alt+P");
        assert_eq!(
            parse_hotkey("Ctrl+Alt+P").expect("parses"),
            Some(hotkey),
            "the displayed form does not parse back"
        );
    }

    fn input(keys: &[u16], ctrl: bool, shift: bool, alt: bool, win: bool) -> RecorderInput {
        RecorderInput {
            ctrl,
            shift,
            alt,
            win,
            keys: keys.to_vec(),
        }
    }

    #[test]
    #[cfg(windows)]
    fn win3_numpad_equal_is_refused_with_a_reason() {
        let mut recorder = Recorder::new(Vec::new());
        let result = recorder.update(&input(&[0x92], true, false, false, false));
        assert!(matches!(result, Some(Recorded::Rejected(reason))
            if reason.contains("Numpad Equal") && reason.contains("E key")));
    }

    #[test]
    fn modifier_only_presses_keep_the_recorder_waiting() {
        let mut recorder = Recorder::new(Vec::new());
        assert_eq!(
            recorder.update(&input(&[], true, false, false, false)),
            None
        );
        assert_eq!(recorder.update(&input(&[], true, true, false, false)), None);
        assert_eq!(recorder.update(&input(&[], false, false, true, true)), None);
        assert_eq!(
            recorder.update(&input(&[], false, false, false, false)),
            None
        );
    }

    #[test]
    fn the_first_real_key_ends_the_recording_with_the_held_modifiers() {
        let mut recorder = Recorder::new(Vec::new());
        // Ctrl and Alt go down first: still waiting.
        assert_eq!(recorder.update(&input(&[], true, false, true, false)), None);
        // Then P: done, with both modifiers held at that moment.
        assert_eq!(
            recorder.update(&input(&[0x50], true, false, true, false)),
            Some(Recorded::Hotkey(HotKey::new(
                Some(Modifiers::CONTROL | Modifiers::ALT),
                Code::KeyP
            )))
        );
    }

    #[test]
    fn a_key_already_down_when_recording_starts_waits_for_a_new_press() {
        // The snapshot taken when the recording starts includes a held key; it
        // must not end the recording until it is released and pressed again.
        let mut recorder = Recorder::new(vec![0x50]);
        assert_eq!(
            recorder.update(&input(&[0x50], false, false, false, false)),
            None
        );
        assert_eq!(
            recorder.update(&input(&[], false, false, false, false)),
            None
        );
        assert_eq!(
            recorder.update(&input(&[0x50], false, false, false, false)),
            Some(Recorded::Hotkey(HotKey::new(None, Code::KeyP)))
        );
    }

    #[test]
    fn escape_cancels_and_bare_backspace_or_delete_clears() {
        let mut recorder = Recorder::new(Vec::new());
        assert_eq!(
            recorder.update(&input(&[0x1B], false, false, false, false)),
            Some(Recorded::Cancel)
        );
        for vk in [0x08, 0x2E] {
            let mut recorder = Recorder::new(Vec::new());
            assert_eq!(
                recorder.update(&input(&[vk], false, false, false, false)),
                Some(Recorded::Clear)
            );
        }
    }

    #[test]
    fn a_modifier_makes_backspace_and_delete_recordable() {
        // Ctrl+Backspace is a hotkey; the "clear the field" rule is for the
        // bare key only.
        let mut recorder = Recorder::new(Vec::new());
        assert_eq!(
            recorder.update(&input(&[0x08], true, false, false, false)),
            Some(Recorded::Hotkey(HotKey::new(
                Some(Modifiers::CONTROL),
                Code::Backspace
            )))
        );
        let mut recorder = Recorder::new(Vec::new());
        assert_eq!(
            recorder.update(&input(&[0x2E], false, false, true, false)),
            Some(Recorded::Hotkey(HotKey::new(
                Some(Modifiers::ALT),
                Code::Delete
            )))
        );
    }

    #[test]
    fn an_unnamed_key_is_ignored_and_waits_for_a_nameable_one() {
        // 0x03 (Ctrl+Break) has no `Code` in the table; the recorder must not
        // end with nothing.
        let mut recorder = Recorder::new(Vec::new());
        assert_eq!(
            recorder.update(&input(&[0x03], false, false, false, false)),
            None
        );
        assert_eq!(
            recorder.update(&input(&[0x03, 0x50], false, false, false, false)),
            Some(Recorded::Hotkey(HotKey::new(None, Code::KeyP)))
        );
    }

    /// LINUX_SUPPORT.md section 4: on X11 `global-hotkey` calls the handler
    /// from a thread of its own, so the table a rebind installs has to be
    /// visible from another thread. A thread-local table would be empty there
    /// and every hotkey would register without ever firing.
    #[test]
    fn the_binding_table_is_process_wide_so_the_x11_handler_thread_sees_it() {
        let _guard = test_lock();
        let binding = Binding {
            hotkey: HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyP),
            mode: CaptureMode::Region,
        };

        // What a rebind does, on the main thread...
        set_handler_bindings(vec![binding]);
        // ...has to be what a second thread — where the X11 handler runs —
        // reads.
        let seen = std::thread::scope(|scope| {
            scope
                .spawn(handler_bindings)
                .join()
                .expect("the reader thread")
        });
        assert_eq!(
            seen,
            vec![binding],
            "the handler thread sees an empty table"
        );

        // The other direction too: the table a rebind replaces is gone for
        // every thread.
        set_handler_bindings(Vec::new());
        assert!(handler_bindings().is_empty());
    }

    #[test]
    fn suspending_does_not_register_anything_and_the_bindings_can_go_back() {
        let _guard = test_lock();
        let (sender, _receiver) = std::sync::mpsc::channel();
        let mut hotkeys = Hotkeys::none();
        let bindings = default_bindings();
        hotkeys.rebind(sender.clone(), bindings.clone());
        hotkeys.suspend();
        assert_eq!(hotkeys.rebind_log().len(), 1, "suspend must not register");

        hotkeys.rebind(sender, bindings.clone());
        let log = hotkeys.rebind_log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[1], bindings, "the bindings were not put back");
    }

    #[test]
    fn a_rebind_hands_back_the_keys_the_old_manager_holds() {
        let _guard = test_lock();
        let (sender, _receiver) = std::sync::mpsc::channel();
        let mut hotkeys = Hotkeys::none();
        let old = Binding {
            hotkey: HotKey::new(None, Code::PrintScreen),
            mode: CaptureMode::Region,
        };
        let new = Binding {
            hotkey: HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyP),
            mode: CaptureMode::Region,
        };

        hotkeys.rebind(sender.clone(), vec![old]);
        hotkeys.rebind(sender, vec![new]);

        // The first rebind has nothing to hand back; the second must hand back
        // the key the first one registered. Before this was ordered correctly
        // it handed back the *new* key, which the manager did not hold — and
        // on X11 the old grab stayed until the manager's thread happened to
        // close its connection.
        assert_eq!(
            hotkeys.release_log(),
            &[Vec::new(), vec![old]],
            "a rebind handed back the wrong keys"
        );
    }

    /// The bug the Step 7 real run found: `global-hotkey` installs only the
    /// first event handler and ignores every later `set_event_handler` call
    /// (`global-hotkey-0.8.0/src/lib.rs:89`, 118-124), so a handler that
    /// captures one rebind's bindings goes stale. The installed handler must
    /// read the table a rebind replaces; this pins the lookup it uses.
    #[test]
    fn the_event_handler_reads_the_bindings_a_rebind_put_in_place() {
        let _guard = test_lock();
        let (sender, _receiver) = std::sync::mpsc::channel();
        let mut hotkeys = Hotkeys::none();
        let print_screen = Binding {
            hotkey: HotKey::new(None, Code::PrintScreen),
            mode: CaptureMode::Region,
        };
        let recorded = Binding {
            hotkey: HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyP),
            mode: CaptureMode::Region,
        };

        hotkeys.rebind(sender.clone(), vec![print_screen]);
        assert_eq!(
            mode_for_event(&handler_bindings(), print_screen.hotkey.id()),
            Some(CaptureMode::Region)
        );

        // The Settings dialog's OK rebinds; the handler must now match the new
        // key and no longer the old one.
        hotkeys.rebind(sender, vec![recorded]);
        assert_eq!(
            mode_for_event(&handler_bindings(), recorded.hotkey.id()),
            Some(CaptureMode::Region),
            "the recorded hotkey's event is not matched"
        );
        assert_eq!(
            mode_for_event(&handler_bindings(), print_screen.hotkey.id()),
            None,
            "the handler still matches the registration it replaced"
        );
    }
}

/// KDE preferred-trigger spelling (the portal does not accept XDG angle brackets).
#[cfg(test)]
fn kde_trigger(key: &HotKey) -> String {
    let mut parts = Vec::new();
    for (flag, name) in [
        (Modifiers::CONTROL, "CTRL"),
        (Modifiers::ALT, "ALT"),
        (Modifiers::SHIFT, "SHIFT"),
        (Modifiers::SUPER, "META"),
    ] {
        if key.mods.contains(flag) {
            parts.push(name.to_owned());
        }
    }
    let name = match key.key {
        Code::PrintScreen => "Print".to_owned(),

        _ => format!("{}", key.key)
            .trim_start_matches("Key")
            .trim_start_matches("Digit")
            .to_owned(),
    };
    parts.push(name);
    parts.join("+")
}

#[cfg(test)]
mod portal_tests {
    use super::*;
    #[test]
    fn kde_triggers_cover_defaults_and_custom_binding() {
        let actual: Vec<_> = default_bindings()
            .iter()
            .map(|b| kde_trigger(&b.hotkey))
            .collect();
        assert_eq!(
            actual,
            ["Print", "CTRL+Print", "SHIFT+Print", "CTRL+SHIFT+Print"]
        );
        assert_eq!(
            kde_trigger(&parse_hotkey("Ctrl+Alt+P").unwrap().unwrap()),
            "CTRL+ALT+P"
        );
    }
}

/// Stable desktop ids preserve bindings across versions and key changes.
pub fn portal_id(mode: CaptureMode) -> &'static str {
    match mode {
        CaptureMode::Region => "region",
        CaptureMode::CurrentMonitor => "monitor",
        CaptureMode::ActiveWindow => "window",
        CaptureMode::AllMonitors => "all",
        CaptureMode::RepeatLastRegion => "repeat",
    }
}

/// Portal defaults intentionally differ from SPEC 6.1 on Wayland.
#[cfg(target_os = "linux")]
fn portal_requests(
    capabilities: &snp_ligase_platform::Capabilities,
) -> Vec<(String, String, Option<String>)> {
    [
        (CaptureMode::Region, Some("CTRL+Print")),
        (CaptureMode::AllMonitors, Some("CTRL+SHIFT+Print")),
        (CaptureMode::RepeatLastRegion, None),
        (CaptureMode::CurrentMonitor, None),
        (CaptureMode::ActiveWindow, None),
    ]
    .into_iter()
    .filter(|(mode, _)| mode.blocked(capabilities).is_none())
    .map(|(mode, trigger)| {
        (
            portal_id(mode).into(),
            mode.label().into(),
            trigger.map(str::to_owned),
        )
    })
    .collect()
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn l3d_portal_registers_only_supported_modes() {
    let rows = portal_requests(&snp_ligase_platform::linux::wayland::CAPABILITIES);
    assert_eq!(
        rows.into_iter()
            .map(|(id, _, trigger)| (id, trigger))
            .collect::<Vec<_>>(),
        vec![
            ("region".into(), Some("CTRL+Print".into())),
            ("all".into(), Some("CTRL+SHIFT+Print".into())),
            ("repeat".into(), None)
        ]
    );
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn w3_kde_shortcuts_add_targets_without_preferred_triggers() {
    let capabilities = snp_ligase_platform::Capabilities {
        compositor_targets: true,
        ..snp_ligase_platform::linux::wayland::CAPABILITIES
    };
    let rows = portal_requests(&capabilities);
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[3], ("monitor".into(), "Current Monitor".into(), None));
    assert_eq!(rows[4], ("window".into(), "Active Window".into(), None));
    assert_eq!(rows[0].2.as_deref(), Some("CTRL+Print"));
    assert_eq!(rows[1].2.as_deref(), Some("CTRL+SHIFT+Print"));
}
