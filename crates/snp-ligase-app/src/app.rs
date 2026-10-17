//! The eframe application: tabs, the canvas, the panels, and the actions they
//! produce (SPEC sections 7 and 8).
//!
//! The panels only collect [`Action`]s and this file runs them, so a menu item,
//! a toolbar button and a keyboard shortcut are the same thing.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use eframe::App;
use egui::{CentralPanel, Key, Panel, Ui, pos2};
use snp_ligase_core::codec;
use snp_ligase_core::geom::{Point, Rect};
use snp_ligase_core::model::{Document, Kind, Rgba};

use crate::canvas::{self, CanvasInput};
use crate::controller::{AppCommand, CaptureSettings, Controller, UiEvent};
use crate::hotkeys::{Hotkeys, Recorded, RecorderInput};
use crate::panels::{self, properties};
use crate::panels::{Action, CaptureMode, ColourField, CustomField, MenuState, StyleProperty};
use crate::settings::{Settings, SettingsStore};
use crate::shortcuts;
use crate::tab::Tab;
use crate::tool::{Tool, ToolStyle, ToolStyles};
use crate::tools::{self, Gesture};

/// Everything the shell needs to start.
///
/// A struct rather than eight arguments: the controller, the hotkeys and the
/// settings store are all created by `main` before the window exists, and
/// tests replace every one of them.
pub struct Shell {
    pub log_dir: PathBuf,
    pub files: Vec<PathBuf>,
    pub controller: Controller,
    /// The registered global hotkeys; the Settings dialog re-binds them.
    pub hotkeys: Hotkeys,
    /// The config file, with its folder injected by the caller (SPEC 10).
    pub settings: SettingsStore,
    /// Problems from startup — a hotkey that would not register — for the
    /// status bar (SPEC 6.1).
    pub warnings: Vec<String>,
    /// Whether the tray icon really exists (G3).
    pub tray_installed: bool,
    /// SPEC 10's "Start hidden in the tray": hide the window on the second
    /// frame (see [`SnpLigaseApp::start_hidden`]).
    pub start_hidden: bool,
}

/// Whether the app should start with its window hidden (SPEC 10's "Start hidden
/// in the tray", SPEC 12 M7).
///
/// Only honoured when the tray icon was really installed: without it there is no
/// way back, the same rule the close button follows (G3). `main` uses this to
/// create the window hidden, and [`SnpLigaseApp::from_shell`] uses it to report
/// it — one decision, two readers.
#[must_use]
pub fn starts_hidden(tray_installed: bool, settings: &Settings) -> bool {
    tray_installed && settings.start_hidden_in_tray
}

/// An open text editing session (SPEC 7.5).
///
/// The session owns the annotation while it is open: every keystroke is written
/// straight into the document so the canvas shows the real result, and the
/// whole session becomes one undo step when it ends. That is why the document
/// is snapshotted here instead of going through [`Tab::stage_edit`], which
/// commits on every frame the pointer is up.
struct TextSession {
    /// The annotation being edited.
    id: u64,
    /// The tab the annotation lives in (F4). Annotation ids are per document,
    /// so the same id can exist in another tab: every session operation reads
    /// and writes this index, never the current tab. Every path that adds,
    /// removes or switches tabs ends the session first, so this stays valid.
    tab: usize,
    /// True when this session created the annotation, so cancelling removes it.
    created: bool,
    /// The document as it was before the session, for its one undo step and
    /// for `Esc` (M7: cancelling restores the whole document, so a property
    /// change made during the session cannot be left behind without an undo
    /// step to reach it).
    snapshot: snp_ligase_core::model::Document,
    /// The text in the editor. Written into the annotation on every change.
    text: String,
    /// True once the editor has had keyboard focus, so losing it can commit.
    focused: bool,
}

/// What a tab-close prompt is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingClose {
    /// Close these tab indices, once the user answers. `then_exit` is set when
    /// the request came from the window's close button or File > Exit, which
    /// really exit once the answer is carried out (SPEC 6.1).
    Tabs {
        indices: Vec<usize>,
        then_exit: bool,
    },
}

/// A save the save worker is still running (H6).
///
/// Only the path matters to the UI: it blocks a second save of the same tab and
/// stops an auto-save from handing out a name the worker has not written yet.
type SaveInFlight = PathBuf;

/// A hotkey recording in progress (NEXT_STEPS step 7, "record a hotkey by
/// pressing it").
///
/// The global hotkeys are suspended while this lives, and `Drop` re-registers
/// the stored bindings — so every way a recording can end (a recorded key, Esc,
/// Backspace, the dialog closing, the window losing focus) puts them back.
/// The recorder itself is a pure state machine (in `hotkeys`) fed by the
/// platform's keyboard poll.
struct HotkeyRecording {
    /// The mode whose field the recording writes into.
    mode: CaptureMode,
    /// The state machine that decides when the recording is over.
    recorder: crate::hotkeys::Recorder,
    /// The app's hotkey guard, suspended for the recording.
    hotkeys: Rc<RefCell<Hotkeys>>,
    /// Where the resumed registration's warnings go.
    commands: std::sync::mpsc::Sender<AppCommand>,
    /// The bindings that were active when the recording started: exactly what
    /// `Drop` registers again.
    bindings: Vec<crate::hotkeys::Binding>,
}

impl Drop for HotkeyRecording {
    fn drop(&mut self) {
        let warnings = self
            .hotkeys
            .borrow_mut()
            .rebind(self.commands.clone(), self.bindings.clone());
        for warning in &warnings {
            tracing::warn!("{warning}");
        }
    }
}

/// The application.
pub struct SnpLigaseApp {
    tabs: Vec<Tab>,
    active: usize,
    tool: Tool,
    /// Each tool's remembered style (SPEC 7.7).
    styles: ToolStyles,
    gesture: Gesture,
    /// The last 8 colours used (SPEC 8.4).
    recent_colours: Vec<Rgba>,
    show_properties: bool,
    show_tool_labels: bool,
    /// The capture delay, for the Capture menu's check mark.
    delay_seconds: u8,
    /// What the status bar says.
    status: String,
    log_dir: PathBuf,
    pending_close: Option<PendingClose>,
    /// Files named on the command line, opened once the window is up.
    pending_open: Vec<PathBuf>,
    /// The canvas size from the last frame, so zoom can centre on it.
    canvas: egui::Vec2,
    /// The export rectangle computed by this frame's canvas, from the sprite
    /// cache (NEXT_STEPS step 11, J3). The status bar reads it instead of
    /// computing the same rectangle a second time.
    content_rect: Rect,
    /// The GPU's maximum texture side, read from egui every frame. The paste
    /// actions refuse an image bigger than this with a message instead of
    /// crashing on upload (SPEC 7.2, NEXT_STEPS step 11).
    max_texture_side: usize,
    /// The point handle that is picked out, if any. `Delete` removes this point
    /// rather than the whole annotation.
    active_point: Option<snp_ligase_core::edit::HandleKind>,
    /// Where the pointer is in image pixels, for the status bar.
    cursor: Option<Point>,
    /// File > Exit asks for the window to close, which needs a `Context`; the
    /// frame that owns one does the closing.
    exit: bool,
    /// Which colour the Annotation menu's picker window is choosing.
    colour_picker: Option<ColourField>,
    /// Whether the Annotation menu's "Font…" window is open.
    font_picker: bool,
    /// Whether the Annotation menu's "Next Number…" window is open.
    next_number_editor: bool,
    /// Which numeric value the Annotation menu's "Custom…" window is editing.
    custom_editor: Option<CustomField>,
    /// Whether Ctrl+V was held last frame, so the press can be noticed as an
    /// edge. egui reports no event at all for a clipboard that holds an image.
    paste_held: bool,
    /// The one font system the app owns (SPEC 7.5). It starts empty and is
    /// replaced by [`SnpLigaseApp::poll_fonts`] once the background thread has
    /// loaded the installed fonts.
    fonts: snp_ligase_core::text::Fonts,
    /// The channel the font thread reports on; `None` once it has answered.
    fonts_loading: Option<std::sync::mpsc::Receiver<snp_ligase_core::text::Fonts>>,
    /// The open text editing session, if any (SPEC 7.5).
    text_session: Option<TextSession>,
    /// True when this frame's press landed on the canvas widget (F5).
    ///
    /// The editor uses it to tell the two "clicked outside" cases apart: a
    /// click on the canvas commits the session (SPEC 7.5), anything else —
    /// a properties panel control, a menu — leaves it open.
    canvas_press: bool,
    /// The UI's end of the controller thread (SPEC 6.2): menus and the toolbar
    /// send capture commands, and the finished images arrive here.
    controller: Controller,
    /// The registered hotkeys (SPEC 6.1). The Settings dialog re-binds them.
    ///
    /// Shared with [`HotkeyRecording`], whose `Drop` re-registers the stored
    /// bindings when a recording ends, however it ends.
    hotkeys: Rc<RefCell<Hotkeys>>,
    /// A hotkey recording in progress (NEXT_STEPS step 7), if any.
    recording: Option<HotkeyRecording>,
    /// The Settings dialog's registration warnings, from its "Test" button:
    /// shown in the dialog, not only the status bar (NEXT_STEPS step 7 item 3).
    settings_warnings: Vec<String>,
    /// The config file (SPEC section 10). Injected, so tests never touch the
    /// real one.
    settings: SettingsStore,
    /// Whether the tray icon really exists (G3): without it the X exits, and
    /// the setting cannot change that.
    tray_installed: bool,
    /// The Settings dialog's working copy, while it is open. OK replaces the
    /// store with it; Cancel drops it.
    settings_draft: Option<Settings>,
    /// Whether the Keyboard Shortcuts window is open (SPEC 8.3, F1).
    shortcuts_open: bool,
    /// SPEC 10's "Start hidden in the tray": 1 on the first frame, 2 on the
    /// second (the frame the hide is sent on), 0 once done.
    ///
    /// The hide cannot happen on the first frame: eframe shows the window
    /// itself after the first painted frame
    /// (`eframe-0.36.2/src/native/epi_integration.rs:380-386`), so a hide sent
    /// before that is overridden. `main` decides with [`starts_hidden`].
    start_hidden: u8,
    /// Frame timing for SPEC 12 M7's drag budget: the frames of the gesture in
    /// progress, accumulated while it lasts and logged when it ends.
    ///
    /// `work` is what one frame of the app itself costs — the number the budget
    /// is about (under 33 ms means the app can hold 30 fps). The *period*
    /// between frames is logged as well, but it also contains the wait for the
    /// next input event, so a synthetic drag shows gaps there that a human's
    /// mouse does not produce.
    gesture_frames: u32,
    gesture_total: std::time::Duration,
    gesture_max: std::time::Duration,
    gesture_period_total: std::time::Duration,
    gesture_periods: u32,
    /// Frames whose own work took longer than 30 fps allows.
    gesture_slow: u32,
    /// When the previous frame ran, for the period between frames.
    last_frame: Option<std::time::Instant>,
    /// True when the window's X hides to the tray instead of exiting
    /// (SPEC 6.1; the setting's default). Only when the tray icon was really
    /// installed (G3): without it, hiding would strand the process with no way
    /// back, because a relaunch exits at the single-instance mutex.
    close_hides: bool,
    /// The save worker: encode and write off the UI thread (H6). AVIF at 4K
    /// takes seconds, and a frozen window in the middle of a save is what this
    /// exists to avoid.
    save_worker: crate::save::SaveWorker,
    /// The saves in flight, by tab id (H6).
    saving: std::collections::HashMap<u64, SaveInFlight>,
}

impl SnpLigaseApp {
    /// Creates the shell with a detached controller and no hotkeys: tests use
    /// this, since a real controller would capture the agent's desktop and real
    /// hotkeys would take the agent's keyboard during `cargo test`.
    #[cfg(test)]
    #[must_use]
    pub fn new(log_dir: PathBuf, files: Vec<PathBuf>) -> Self {
        // A folder no other test uses, so a settings write from a test cannot
        // collide with another test or reach the real config.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let config_dir = std::env::temp_dir().join(format!(
            "snp-ligase-app-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Self::from_shell(Shell {
            log_dir,
            files,
            controller: Controller::detached(),
            hotkeys: Hotkeys::none(),
            settings: SettingsStore::load(config_dir).0,
            warnings: Vec::new(),
            tray_installed: true,
            start_hidden: false,
        })
    }

    /// Creates the shell from what `main` (or a test) has prepared.
    #[must_use]
    pub fn from_shell(shell: Shell) -> Self {
        let Shell {
            log_dir,
            files,
            controller,
            hotkeys,
            settings,
            warnings,
            tray_installed,
            start_hidden,
        } = shell;

        // H6: the encodes run off this thread. The window gate is how the
        // worker asks for the frame that applies the result, even when the
        // window is hidden and egui would not run one by itself.
        let save_worker = crate::save::SaveWorker::spawn(controller.window_handle());

        // SPEC 7.5: the font system is loaded once, on a background thread, and
        // the Text tool stays disabled until it is ready.
        let (sender, receiver) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("fonts".to_owned())
            .spawn(move || {
                let started = std::time::Instant::now();
                let fonts = snp_ligase_core::text::Fonts::load();
                tracing::info!(
                    families = fonts.families().len(),
                    elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                    "fonts loaded"
                );
                // The receiver is gone only if the app is shutting down.
                let _ = sender.send(fonts);
            });
        let fonts_loading = match spawned {
            Ok(_) => Some(receiver),
            Err(err) => {
                tracing::error!(error = %err, "could not start the font thread");
                None
            }
        };

        // A startup warning has to be visible without blocking (SPEC 6.1):
        // the status bar is exactly that.
        let status = if warnings.is_empty() {
            "Ready. File > Open… to open a PNG, or paste an image with Ctrl+V.".to_owned()
        } else {
            for warning in &warnings {
                tracing::warn!("{warning}");
            }
            warnings.join(" ")
        };

        // The persisted editor state becomes the live state (SPEC 10's
        // "per-tool style defaults, recent colours").
        let styles = settings.settings().styles.clone();
        let recent_colours = settings.settings().recent_colours.clone();
        let show_tool_labels = settings.settings().show_tool_labels;
        let close_hides = tray_installed && settings.settings().close_hides_to_tray;
        // "Start hidden in the tray" needs a tray to come back from, exactly
        // like the close button (G3). The caller says whether it applies; the
        // window is hidden on the second frame (see `start_hidden`).
        let start_hidden = if start_hidden { 1 } else { 0 };

        Self {
            tabs: Vec::new(),
            active: 0,
            tool: Tool::Select,
            styles,
            gesture: Gesture::None,
            recent_colours,
            show_properties: true,
            show_tool_labels,
            delay_seconds: 0,
            status,
            log_dir,
            pending_close: None,
            pending_open: files,
            canvas: egui::vec2(800.0, 600.0),
            content_rect: Rect::ZERO,
            max_texture_side: 16_384,
            active_point: None,
            cursor: None,
            exit: false,
            colour_picker: None,
            font_picker: false,
            next_number_editor: false,
            custom_editor: None,
            paste_held: false,
            fonts: snp_ligase_core::text::Fonts::empty(),
            fonts_loading,
            text_session: None,
            canvas_press: false,
            controller,
            hotkeys: Rc::new(RefCell::new(hotkeys)),
            recording: None,
            settings_warnings: Vec::new(),
            settings,
            tray_installed,
            settings_draft: None,
            shortcuts_open: false,
            start_hidden,
            gesture_frames: 0,
            gesture_total: std::time::Duration::ZERO,
            gesture_max: std::time::Duration::ZERO,
            gesture_period_total: std::time::Duration::ZERO,
            gesture_periods: 0,
            gesture_slow: 0,
            last_frame: None,
            close_hides,
            save_worker,
            saving: std::collections::HashMap::new(),
        }
    }

    /// Takes the font system from the loading thread once it is ready.
    ///
    /// The first frame after it arrives drops every cached texture, so text and
    /// number sprites that could not be rasterized while the fonts were missing
    /// are built again with real glyphs (SPEC 7.5).
    fn poll_fonts(&mut self) {
        let Some(receiver) = &self.fonts_loading else {
            return;
        };
        match receiver.try_recv() {
            Ok(fonts) => {
                self.fonts = fonts;
                self.fonts_loading = None;
                for tab in &mut self.tabs {
                    tab.forget_all_textures();
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.fonts_loading = None;
                tracing::error!(
                    "the font thread ended without a result; the Text tool stays disabled"
                );
            }
        }
    }

    /// True once the installed fonts are available (SPEC 7.5).
    #[must_use]
    fn fonts_ready(&self) -> bool {
        self.fonts.is_loaded()
    }

    // ---------------------------------------------------------------------
    // The text editing session (SPEC 7.5)
    // ---------------------------------------------------------------------

    /// Opens the floating editor for a text annotation (SPEC 7.5).
    ///
    /// `snapshot` is the document as it was before the session, so the whole
    /// session is one undo step.
    fn begin_text_session(&mut self, id: u64, created: bool, snapshot: Document) {
        let tab_index = self.active;
        let Some(tab) = self.tabs.get(tab_index) else {
            return;
        };
        let Some(annotation) = tab.document.find(id) else {
            return;
        };
        let Kind::Text(text) = &annotation.kind else {
            return;
        };
        self.text_session = Some(TextSession {
            id,
            tab: tab_index,
            created,
            snapshot,
            text: text.text.clone(),
            focused: false,
        });
    }

    /// Ends the session keeping the text, as one undo step (SPEC 7.5).
    ///
    /// Committing with empty text deletes the annotation instead. The session
    /// is committed into the tab it was opened on (F4), never the current one.
    fn commit_text_session(&mut self) {
        let Some(session) = self.text_session.take() else {
            return;
        };
        if session.tab != self.active {
            // Every path that changes the active tab ends the session, so this
            // is unreachable; if it ever happens, doing nothing is safer than
            // writing into the wrong document.
            tracing::warn!(
                session_tab = session.tab,
                active = self.active,
                "a text session was committed while another tab was active"
            );
            return;
        }
        let Some(tab) = self.tabs.get_mut(session.tab) else {
            return;
        };
        if tab.document.find(session.id).is_none() {
            // The annotation went away under the session (undo, crop, a closed
            // tab): there is nothing to commit.
            return;
        }

        if session.text.trim().is_empty() {
            // SPEC 7.5: committing with empty text deletes the annotation.
            remove_text(tab, session.id);
            if !session.created {
                // Emptying an existing annotation is a delete: one undo step.
                tab.push_snapshot(session.snapshot);
            }
            // Nothing was drawn, so the tool stays put (9b).
            return;
        }

        // The whole session is one undo step, and only if something changed.
        if tab.document != session.snapshot {
            tab.push_snapshot(session.snapshot);
        }

        // SPEC section 10's "Switch to Select after drawing" (9b). The Text
        // tool's gesture ends in `Gesture::TextEdit`, which `canvas_input`
        // turns into a session, so the "a drawing gesture ended this frame"
        // condition there never sees it and the Text tool never switched. A
        // session that commits a **newly created** annotation is the moment
        // that matters; cancelling, deleting an empty one and re-editing an
        // existing text all leave the tool alone. `self.tool` is set directly:
        // going through `Action::Tool` would end the session.
        if switches_after_text_session(
            self.settings.settings().switch_to_select_after_drawing,
            session.created,
        ) {
            self.tool = Tool::Select;
            self.report("Select tool (V).".to_owned());
        }
    }

    /// Ends the session and puts the annotation back the way it was
    /// (SPEC 7.5): a new annotation is removed, an existing one restored.
    ///
    /// M7: property changes made *during* the session fold into it (they take
    /// no undo step of their own while it is open), so cancelling restores the
    /// whole pre-session document. Restoring only the annotation's text would
    /// leave those changes in the document with no undo step to reach them.
    fn cancel_text_session(&mut self) {
        let Some(session) = self.text_session.take() else {
            return;
        };
        if session.tab != self.active {
            tracing::warn!(
                session_tab = session.tab,
                active = self.active,
                "a text session was cancelled while another tab was active"
            );
            return;
        }
        let Some(tab) = self.tabs.get_mut(session.tab) else {
            return;
        };
        if tab.document != session.snapshot {
            tab.restore_document(session.snapshot);
        }
    }

    /// The floating editor (SPEC 7.5): a small window just below the
    /// annotation, with the live text and its hints.
    ///
    /// The text is written into the annotation on every change, so the canvas
    /// always shows the rasterized result and the editor can never disagree
    /// with the image.
    fn text_editor_window(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let Some((tab_index, id)) = self
            .text_session
            .as_ref()
            .map(|session| (session.tab, session.id))
        else {
            return;
        };
        // The editor's decision needs to know whether this frame's click landed
        // on the canvas, which the canvas read before this runs (F5).
        let canvas_press = self.canvas_press;
        let Some(tab) = self.tabs.get(tab_index) else {
            self.text_session = None;
            return;
        };
        let Some(annotation) = tab.document.find(id) else {
            self.text_session = None;
            return;
        };
        let Kind::Text(text) = &annotation.kind else {
            self.text_session = None;
            return;
        };
        let padding = text.padding.max(0.0);
        let zoom = tab.viewport.zoom;
        let top_left = tab.viewport.to_screen(text.pos);
        // The layout needs the font system, which is a different field of
        // `self`, so the two borrows do not overlap.
        let layout = snp_ligase_core::text::layout(text, &mut self.fonts);
        let below = pos2(
            top_left.x,
            top_left.y + (layout.height + padding * 2.0) * zoom + 4.0,
        );

        let Some(session) = self.text_session.as_mut() else {
            return;
        };
        egui::Area::new(egui::Id::new("text_editor"))
            .order(egui::Order::Foreground)
            .fixed_pos(below)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(280.0);
                    let response = ui.add(
                        egui::TextEdit::multiline(&mut session.text)
                            .desired_rows(2)
                            .desired_width(260.0)
                            .hint_text("Type the text"),
                    );
                    if !session.focused {
                        response.request_focus();
                    }
                    if response.has_focus() {
                        session.focused = true;
                    }
                    ui.label("Ctrl+Enter to finish, Esc to cancel");

                    let ctrl_enter = ui
                        .input(|input| input.modifiers.ctrl && input.key_pressed(egui::Key::Enter));
                    let escape = ui.input(|input| input.key_pressed(egui::Key::Escape));
                    match editor_keys(
                        ctrl_enter,
                        escape,
                        response.lost_focus(),
                        session.focused,
                        canvas_press,
                    ) {
                        EditorKeys::None => {}
                        EditorKeys::Commit => actions.push(Action::CommitText),
                        EditorKeys::Cancel => actions.push(Action::CancelText),
                        EditorKeys::KeepEditing => {
                            // F5: a click on a panel control or a menu takes the
                            // focus off the editor (egui surrenders it on any
                            // click outside, `SurrenderFocusOn::Clicks`), and
                            // SPEC 7.5 wants those controls usable while typing.
                            // The session stays, and the editor takes the
                            // keyboard back unless the click landed on something
                            // that wants it — a text field or a numeric field —
                            // so typing goes on where the user put the caret.
                            if ui.ctx().memory(|memory| memory.focused()).is_none() {
                                response.request_focus();
                            }
                        }
                    }
                });
            });

        // The live write: the canvas rasterizes from the document, so the text
        // the user sees is the text that will be saved (SPEC 7.5).
        let current = self
            .text_session
            .as_ref()
            .map(|session| session.text.clone());
        if let Some(current) = current
            && let Some(tab) = self.tabs.get_mut(tab_index)
            && let Some(annotation) = tab.document.find_mut(id)
            && let Kind::Text(text) = &mut annotation.kind
            && text.text != current
        {
            text.text = current;
        }
    }

    /// The active tab, mutably.
    fn active_tab(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// The active tab, immutably.
    fn active_ref(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// Says something in the status bar, and in the log so it survives a crash.
    fn report(&mut self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(target: "snp_ligase::ui", "{message}");
        self.status = message;
    }

    // ---------------------------------------------------------------------
    // Settings (SPEC section 10)
    // ---------------------------------------------------------------------

    /// Opens the Settings dialog with a copy of the stored settings.
    ///
    /// An already-open dialog keeps its draft: the tray's Settings row can fire
    /// while it is up, and replacing the draft would throw the edits away.
    fn open_settings(&mut self) {
        if self.settings_draft.is_none() {
            self.settings_draft = Some(self.settings.settings().clone());
            self.settings_warnings.clear();
            self.report("Settings.");
        }
    }

    /// Applies the Settings dialog: stores the file and every side effect.
    ///
    /// The hotkeys are re-bound first, because a key that another program owns
    /// has to be reported; the rest applies regardless (SPEC section 10's
    /// conflict warnings).
    fn apply_settings(&mut self) {
        let Some(draft) = self.settings_draft.take() else {
            return;
        };
        self.settings_warnings.clear();
        let mut warnings = Vec::new();

        let (bindings, hotkey_warnings) = crate::hotkeys::bindings_from(&draft);
        warnings.extend(hotkey_warnings);
        warnings.extend(
            self.hotkeys
                .borrow_mut()
                .rebind(self.controller.command_sender(), bindings),
        );

        self.controller.send(AppCommand::Configure(CaptureSettings {
            hide_while_capturing: draft.hide_while_capturing,
            include_cursor: draft.include_cursor,
            hdr_highlights: draft.hdr_highlights,
            sdr_white_nits: draft.sdr_white_nits,
        }));

        self.show_tool_labels = draft.show_tool_labels;
        self.close_hides = self.tray_installed && draft.close_hides_to_tray;
        self.settings.replace(draft);
        if let Err(message) = self.settings.flush() {
            warnings.push(message);
        }

        if warnings.is_empty() {
            self.report(format!(
                "Settings saved to {}.",
                self.settings.config_path().display()
            ));
        } else {
            for warning in &warnings {
                tracing::warn!("{warning}");
            }
            self.report(warnings.join(" "));
        }
    }

    /// The Settings dialog's "Browse…": a folder picker for the default save
    /// folder, written into the draft.
    fn pick_save_folder(&mut self) {
        let Some(draft) = self.settings_draft.as_mut() else {
            return;
        };
        let mut dialog = rfd::FileDialog::new().set_title("Default save folder");
        let start = draft.resolved_save_folder();
        dialog = dialog.set_directory(crate::settings::existing_start_folder(&start));
        if let Some(folder) = dialog.pick_folder() {
            // The folder takes `%` variables, so a chosen one has every percent
            // sign doubled to stay literal (NEXT_STEPS step 9c).
            draft.save_folder = Some(PathBuf::from(crate::settings::escape_percent(
                &folder.to_string_lossy(),
            )));
        }
    }

    // ---------------------------------------------------------------------
    // Recording a hotkey by pressing it (NEXT_STEPS step 7)
    // ---------------------------------------------------------------------

    /// Starts recording a hotkey for one mode: the global keys are suspended
    /// and the keyboard is polled every frame until a key is pressed.
    ///
    /// Only from the Settings dialog, and only one at a time; "repeat last
    /// region" has no global hotkey (SPEC 6.1).
    fn start_hotkey_recording(&mut self, mode: CaptureMode) {
        if self.recording.is_some() || self.settings_draft.is_none() {
            return;
        }
        if mode == CaptureMode::RepeatLastRegion {
            return;
        }
        self.settings_warnings.clear();
        let (bindings, _) = crate::hotkeys::bindings_from(self.settings.settings());
        self.hotkeys.borrow_mut().suspend();
        // A key held right now (someone clicking "Record…" with a key down)
        // must not end the recording until it is released and pressed again.
        let already_down = snp_ligase_platform::windows::keys::keyboard_snapshot().keys;
        self.recording = Some(HotkeyRecording {
            mode,
            recorder: crate::hotkeys::Recorder::new(already_down),
            hotkeys: Rc::clone(&self.hotkeys),
            commands: self.controller.command_sender(),
            bindings,
        });
        tracing::info!(mode = mode.label(), "recording a hotkey");
    }

    /// Ends a hotkey recording without a result: Esc, the dialog closing, or
    /// the window losing focus. Dropping the guard re-registers the stored
    /// bindings.
    fn end_hotkey_recording(&mut self) {
        let Some(recording) = self.recording.take() else {
            return;
        };
        tracing::info!(
            mode = recording.mode.label(),
            "the hotkey recording ended without a key"
        );
        // `recording` drops here, which re-registers the stored bindings.
    }

    /// Feeds one keyboard snapshot to the recorder, if one is running.
    ///
    /// Called once per frame while recording. When the recording finishes, the
    /// guard goes first (re-registering the stored keys) and then the outcome
    /// is written into the dialog's draft.
    fn poll_hotkey_recording(
        &mut self,
        snapshot: &snp_ligase_platform::windows::keys::KeyboardSnapshot,
    ) {
        let Some(recording) = self.recording.as_mut() else {
            return;
        };
        let input = RecorderInput::from_snapshot(snapshot);
        let Some(outcome) = recording.recorder.update(&input) else {
            return;
        };
        let mode = recording.mode;
        // The recording is over: dropping the guard re-registers the stored
        // keys (the draft is not saved until OK).
        self.recording = None;
        match outcome {
            Recorded::Hotkey(hotkey) => {
                let text = crate::hotkeys::display_hotkey(&hotkey);
                if let Some(draft) = self.settings_draft.as_mut() {
                    draft.set_hotkey_text(mode, text.clone());
                }
                // The virtual key goes in the log too: when a recorded hotkey
                // does not fire later, this says which key was registered.
                tracing::info!(
                    mode = mode.label(),
                    hotkey = %text,
                    vk = ?crate::hotkeys::code_to_vk(hotkey.key),
                    "hotkey recorded"
                );
                self.report(format!("{} hotkey recorded as {text}.", mode.label()));
            }
            Recorded::Clear => {
                if let Some(draft) = self.settings_draft.as_mut() {
                    draft.set_hotkey_text(mode, String::new());
                }
                tracing::info!(mode = mode.label(), "the hotkey field was cleared");
                self.report(format!("{} hotkey cleared.", mode.label()));
            }
            Recorded::Cancel => {
                tracing::info!(mode = mode.label(), "the hotkey recording was cancelled");
            }
        }
    }

    /// The Settings dialog's "Test": registers the draft's hotkeys for real to
    /// see whether another program owns them, keeps the failures for the
    /// dialog to show, and then puts the stored bindings back.
    fn test_hotkeys(&mut self) {
        let Some(draft) = self.settings_draft.clone() else {
            return;
        };
        let (bindings, mut warnings) = crate::hotkeys::bindings_from(&draft);
        warnings.extend(
            self.hotkeys
                .borrow_mut()
                .rebind(self.controller.command_sender(), bindings),
        );
        // Whatever the trial found, the stored keys go back: the draft is not
        // saved until OK.
        let (stored, stored_warnings) = crate::hotkeys::bindings_from(self.settings.settings());
        warnings.extend(stored_warnings);
        warnings.extend(
            self.hotkeys
                .borrow_mut()
                .rebind(self.controller.command_sender(), stored),
        );
        self.settings_warnings = warnings;
        if self.settings_warnings.is_empty() {
            self.report("Every hotkey registered; nothing else is using them.");
        } else {
            for warning in &self.settings_warnings {
                tracing::warn!("{warning}");
            }
            self.report("Some hotkeys could not be registered; see Settings.");
        }
    }

    /// Persists the editor state and writes the config file when it is due.
    ///
    /// SPEC section 10: "Save when Settings is confirmed and when tool defaults
    /// change (debounced)". The styles and the recent colours are compared here
    /// once per frame rather than at every write site, so a change cannot slip
    /// through; the store rate-limits the actual writes.
    fn settings_tick(&mut self) {
        self.settings
            .sync_editor_state(&self.styles, &self.recent_colours);
        let _wrote = self.settings.tick(std::time::Instant::now());
    }

    /// The Settings dialog, drawn from the draft.
    ///
    /// The draft is taken out for the borrow and put back afterwards; the
    /// action that runs later in the frame (Apply or Cancel) is what closes
    /// the dialog.
    fn settings_window(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let Some(mut draft) = self.settings_draft.take() else {
            return;
        };
        let config_path = self.settings.config_path();
        let recording = self.recording.as_ref().map(|recording| recording.mode);
        panels::settings::show(
            ui,
            &mut draft,
            &config_path,
            recording,
            &self.settings_warnings,
            &mut self.recent_colours,
            actions,
        );
        self.settings_draft = Some(draft);
    }

    /// The Keyboard Shortcuts window (SPEC 8.3, F1).
    ///
    /// The rows are rebuilt every frame: the capture group shows the user's own
    /// hotkeys, so changing them in Settings changes this window too.
    fn shortcuts_window(&mut self, ui: &mut Ui) {
        if !self.shortcuts_open {
            return;
        }
        let rows = shortcuts::help_rows(self.settings.settings());
        panels::shortcuts::window(ui, &rows, &mut self.shortcuts_open);
    }

    // ---------------------------------------------------------------------
    // The controller thread (SPEC 6.2)
    // ---------------------------------------------------------------------

    /// Takes everything the controller thread has produced since the last
    /// frame: finished captures, status lines and the tray's Quit.
    ///
    /// The controller cannot touch the document — it does not have one — so a
    /// capture arrives as an image and becomes a tab here (SPEC 6.3).
    fn drain_controller_events(&mut self) {
        for event in self.controller.take_events() {
            match event {
                UiEvent::NewCapture(image) => {
                    self.add_capture(image);
                    self.copy_new_capture();
                }
                UiEvent::Status(message) | UiEvent::Warning(message) => self.report(message),
                UiEvent::ShowSettings => self.run(Action::Settings),
                UiEvent::QuitRequested => self.quit_requested(),
            }
        }
    }

    /// Applies the saves the worker has finished since the last frame (H6).
    ///
    /// The tab state is only ever touched here, on the UI thread: the worker
    /// writes the file and hands back the document snapshot it wrote, so a tab
    /// that changed while the save ran stays correctly unsaved.
    fn drain_saves(&mut self) {
        for outcome in self.save_worker.take_outcomes() {
            self.saving.remove(&outcome.tab);
            let name = outcome.path.display().to_string();
            match outcome.result {
                Ok(stats) => {
                    if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id() == outcome.tab) {
                        tab.note_saved(outcome.path.clone(), outcome.document.clone());
                    } else {
                        tracing::warn!(path = %name, "a finished save's tab is gone");
                    }
                    let reported = if outcome.auto { "Auto-saved" } else { "Saved" };
                    self.report(format!(
                        "{reported} {name} ({} bytes in {:.0} ms).",
                        stats.bytes,
                        stats.encode_ms + stats.write_ms
                    ));
                }
                Err(message) => {
                    tracing::warn!(path = %name, error = %message, "the save failed");
                    self.report(message);
                }
            }
        }
    }

    /// Waits for a save of `tab_id` that is still in flight (M1), draining its
    /// result through the normal path so the tab's saved state is up to date.
    ///
    /// The running app polls the worker once per frame; a close decision has
    /// to know whether the file is written before the tab goes, so this waits
    /// here, bounded like `on_exit`. A save that failed leaves the tab dirty
    /// and the caller still has to deal with it.
    fn wait_for_save(&mut self, tab_id: u64) {
        let Some(path) = self.saving.get(&tab_id).cloned() else {
            return;
        };
        tracing::info!(path = %path.display(), "waiting for a save in flight");
        self.report(format!("{} is being saved…", path.display()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while self.saving.contains_key(&tab_id) && std::time::Instant::now() < deadline {
            self.drain_saves();
            if self.saving.contains_key(&tab_id) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        if self.saving.contains_key(&tab_id) {
            tracing::warn!(
                path = %path.display(),
                "the save in flight did not finish; going on without it"
            );
        }
    }

    /// Waits for the saves of `indices` that are still in flight and whose tab
    /// is dirty (M2), so a close decision is made against what is really
    /// unsaved and the prompt does not ask about a file that is being written
    /// right now.
    fn wait_for_dirty_saves(&mut self, indices: &[usize]) {
        let ids: Vec<u64> = indices
            .iter()
            .filter_map(|index| {
                let tab = self.tabs.get(*index)?;
                (tab.dirty() && self.saving.contains_key(&tab.id())).then_some(tab.id())
            })
            .collect();
        for id in ids {
            self.wait_for_save(id);
        }
    }

    /// Leaving because the tray's Quit asked for it (G2).
    ///
    /// The tray works while the window is hidden, and a save prompt drawn in a
    /// hidden window is invisible: with unsaved tabs the window comes back
    /// first. With none, nothing needs to be seen — `request_exit` sets `exit`
    /// and the close request that follows is let through (G1).
    fn quit_requested(&mut self) {
        if self.tabs.iter().any(Tab::dirty) {
            self.controller.window().show();
        }
        self.request_exit();
    }

    /// SPEC 6.3: a new capture goes to the clipboard when the setting is on
    /// (its default; SPEC section 10).
    fn copy_new_capture(&mut self) {
        if !self.settings.settings().copy_new_captures {
            tracing::debug!("the clipboard copy is switched off in Settings");
            return;
        }
        let Some(tab) = self.active_ref() else {
            return;
        };
        let image = std::sync::Arc::clone(&tab.document.base);
        let (width, height) = (image.width(), image.height());
        let result = arboard::Clipboard::new().and_then(|mut clipboard| {
            clipboard.set_image(arboard::ImageData {
                width: width as usize,
                height: height as usize,
                bytes: image.as_raw().clone().into(),
            })
        });
        match result {
            Ok(()) => tracing::info!(
                target: "snp_ligase::ui",
                width,
                height,
                "capture copied to the clipboard"
            ),
            Err(err) => self.report(format!("Captured, but the clipboard copy failed: {err}")),
        }
    }

    /// Hides the window to the tray (SPEC 6.1).
    fn hide_to_tray(&mut self) {
        self.controller.window().hide();
        self.report(
            "Hidden to the tray. Print Screen captures a region; the tray's Show Window brings \
             this back.",
        );
    }

    // ---------------------------------------------------------------------
    // Opening images (SPEC 7.1)
    // ---------------------------------------------------------------------

    /// Opens an image file in a new tab.
    pub fn open_file(&mut self, path: &Path) {
        match load_image(path) {
            Ok(document) => {
                let name = path.display().to_string();
                self.push_tab(Tab::from_path(document, path.to_path_buf()));
                self.report(format!("Opened {name}"));
            }
            Err(message) => self.report(format!("Could not open {}: {message}", path.display())),
        }
    }

    /// Opens every file named on the command line, once.
    fn open_pending(&mut self) {
        for path in std::mem::take(&mut self.pending_open) {
            self.open_file(&path);
        }
    }

    /// Adds a tab and makes it active.
    fn push_tab(&mut self, mut tab: Tab) {
        self.settle_edits();
        // A new tab takes the setting (NEXT_STEPS step 13). This is the one
        // place tabs enter the app, so a capture, a paste, Ctrl+O and a
        // dropped file all agree.
        tab.auto_fit = self.settings.settings().auto_fit_to_window;
        self.tabs.push(tab);
        self.set_active(self.tabs.len() - 1);
    }

    /// Makes a tab active (F4).
    ///
    /// This is the one place `active` changes. An open text session ends first:
    /// annotation ids are per document, so the same id can exist in another
    /// tab, and a session that followed a switch could type into — or delete —
    /// the wrong tab's annotation and push its snapshot onto the wrong undo
    /// stack. Every path that adds or removes tabs ends the session too, so the
    /// index the session remembers stays valid.
    fn set_active(&mut self, index: usize) {
        self.commit_text_session();
        self.active = index;
        self.gesture = Gesture::None;
        self.active_point = None;
    }

    /// Adds a tab holding `image`, titled by the clock (SPEC 6.3).
    ///
    /// `UiEvent::NewCapture` is the only caller in the program: the controller
    /// thread captures, the UI thread owns documents (SPEC 6.2). With SPEC
    /// section 10's "auto-save new captures" on, the file is written straight
    /// away as well.
    pub fn add_capture(&mut self, image: image::RgbaImage) {
        let (width, height) = (image.width(), image.height());
        let title = format!("Capture {}", clock_now());
        let tab = Tab::new(Document::new(image), title);
        self.push_tab(tab);
        self.report(format!(
            "Captured {width} × {height} px. Save As to keep it."
        ));
        self.auto_save_capture();
    }

    /// SPEC section 10's "Auto-save new captures": write the new capture into
    /// the default folder under the template's name, without a dialog.
    ///
    /// The name is numbered until it is free — on disk *or* claimed by a save
    /// that is still running (H1 + H6) — and the encode runs on the save
    /// worker, so an AVIF auto-save does not freeze the window the capture just
    /// brought back. A failure is reported when the worker answers; the tab
    /// stays open and can be saved by hand.
    fn auto_save_capture(&mut self) {
        if !self.settings.settings().auto_save_new_captures {
            return;
        }
        let settings = self.settings.settings().clone();
        // One clock reading feeds the folder and the name, so the two can
        // never disagree about which day the capture belongs to (9c).
        let now = chrono::Local::now();
        let (folder, folder_warning) = settings.resolved_save_folder_at(now);
        let (name, template_warning) = settings.file_name(now);
        for warning in [folder_warning, template_warning].into_iter().flatten() {
            tracing::warn!("{warning}");
        }
        if let Err(err) = std::fs::create_dir_all(&folder) {
            self.report(format!(
                "Could not create {} for the auto-save: {err}",
                folder.display()
            ));
            return;
        }
        let path = self.free_save_path(&folder, &name, settings.default_format().extension());
        match self.start_save(self.active, &path, true) {
            Ok(()) => self.report(format!("Auto-saving {}…", path.display())),
            Err(message) => self.report(message),
        }
    }

    /// Adds a pasted image as a new tab, titled and reported as a paste rather
    /// than a capture (R30).
    fn add_pasted(&mut self, image: image::RgbaImage) {
        let (width, height) = (image.width(), image.height());
        let title = format!("Pasted {}", clock_now());
        let tab = Tab::new(Document::new(image), title);
        self.push_tab(tab);
        self.report(format!(
            "Pasted image {width} × {height} px. Save As to keep it."
        ));
    }

    /// Adds an image pasted from the clipboard as a new tab (SPEC 7.1).
    fn action_paste(&mut self) {
        match Self::clipboard_image() {
            Ok(image) => self.add_pasted(image),
            Err(message) => self.report(message),
        }
    }

    /// The clipboard's image, with the all-zero-alpha fix for 32-bit DIBs.
    ///
    /// Shared by both paste actions: Ctrl+V (a new tab) and Ctrl+Shift+V (into
    /// the current tab, NEXT_STEPS step 11).
    fn clipboard_image() -> Result<image::RgbaImage, String> {
        let mut clipboard = arboard::Clipboard::new()
            .map_err(|err| format!("Could not reach the clipboard: {err}"))?;
        let image = clipboard
            .get_image()
            .map_err(|err| format!("The clipboard holds no image ({err})."))?;
        let (width, height) = (image.width, image.height);
        // 32-bit DIBs from other applications often carry an all-zero alpha
        // channel, which would make the whole picture transparent.
        let mut bytes = image.bytes.into_owned();
        if !bytes.is_empty() && bytes.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0) {
            for pixel in bytes.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
        }
        image::RgbaImage::from_raw(width as u32, height as u32, bytes)
            .ok_or_else(|| "The clipboard image could not be read.".to_owned())
    }

    /// File > "Paste Image into This Tab" / Ctrl+Shift+V (NEXT_STEPS step 11):
    /// add the clipboard image to the **current** tab as a movable annotation,
    /// so several screenshots can be stitched into one.
    ///
    /// With no tab open this behaves like Ctrl+V and says so in the status bar.
    fn action_paste_into_tab(&mut self) {
        if self.active_ref().is_none() {
            let before = self.tabs.len();
            self.action_paste();
            if self.tabs.len() > before {
                self.report(
                    "No image open, so the pasted image went into a new tab (Ctrl+V).".to_owned(),
                );
            }
            return;
        }
        match Self::clipboard_image() {
            Ok(image) => self.paste_image_into_tab(image),
            Err(message) => self.report(message),
        }
    }

    /// Adds one clipboard image to the current tab.
    ///
    /// The image's centre goes at the centre of the current view, on whole
    /// pixels; it becomes the selection and the tool switches to Select, so it
    /// can be dragged or nudged into place at once. One undo step.
    fn paste_image_into_tab(&mut self, image: image::RgbaImage) {
        let (width, height) = (image.width(), image.height());
        // SPEC 7.2's texture limit, for the pasted image's own sprite: a clear
        // message instead of a crash.
        let limit = self.max_texture_side;
        if width as usize > limit || height as usize > limit {
            self.report(format!(
                "This image is {width} × {height} px, larger than the {limit} px texture limit \
                 of this machine, so it cannot be shown in a tab. Ctrl+V opens it as its own tab."
            ));
            return;
        }

        let canvas = self.canvas;
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        // The centre of the visible canvas, in image pixels.
        let centre = tab.viewport.to_image(pos2(
            tab.viewport.canvas_min.x + canvas.x / 2.0,
            tab.viewport.canvas_min.y + canvas.y / 2.0,
        ));
        let pos = image_position_for_centre(centre, (width, height));

        tab.begin_edit();
        let id = tab
            .document
            .push(Kind::Image(snp_ligase_core::model::ImageA {
                pos,
                image: std::sync::Arc::new(image),
            }));
        tab.select_only(Some(id));
        self.tool = Tool::Select;
        tracing::info!(
            id,
            width,
            height,
            x = pos.x,
            y = pos.y,
            "pasted an image into the current tab"
        );
        self.report(format!(
            "Pasted image {width} × {height} px into this tab. Drag or nudge it into place."
        ));
    }

    /// File > Open: the native dialog.
    fn action_open(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Open image")
            .add_filter("PNG", &["png"])
            .add_filter("JPEG", &["jpg", "jpeg"])
            .add_filter("BMP", &["bmp"])
            .add_filter("GIF", &["gif"])
            .add_filter("WebP", &["webp"])
            .pick_file()
        else {
            return;
        };
        self.open_file(&path);
    }

    // ---------------------------------------------------------------------
    // Saving and copying (SPEC 9)
    // ---------------------------------------------------------------------

    /// File > Save, or Save As when `as_directed` or there is no path yet.
    ///
    /// The encode and the write go to the save worker (H6); the status bar says
    /// "Saving…" until the result arrives ([`SnpLigaseApp::drain_saves`]).
    fn action_save(&mut self, as_directed: bool) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.report("Nothing to save.");
            return;
        };
        if let Some(message) = self.save_blocked(tab.id()) {
            self.report(message);
            return;
        }
        let existing = tab.path().map(Path::to_path_buf);
        let path = match (as_directed, existing) {
            // SPEC 9: Save on a tab that already has a file writes to it.
            (false, Some(path)) => path,
            (_, existing) => {
                let Some(path) = save_dialog(self.settings.settings(), existing.as_deref()) else {
                    return;
                };
                path
            }
        };
        match self.start_save(self.active, &path, false) {
            Ok(()) => self.report(format!("Saving {}…", path.display())),
            Err(message) => self.report(message),
        }
    }

    /// Saves every tab that has a path and unsaved changes.
    fn action_save_all(&mut self) {
        let targets: Vec<(usize, PathBuf)> = (0..self.tabs.len())
            .filter_map(|index| {
                let tab = self.tabs.get(index)?;
                if !tab.dirty() || self.saving.contains_key(&tab.id()) {
                    return None;
                }
                tab.path().map(|path| (index, path.to_path_buf()))
            })
            .collect();
        let mut started = 0;
        let mut first_error = None;
        for (index, path) in targets {
            match self.start_save(index, &path, false) {
                Ok(()) => started += 1,
                Err(message) => first_error = first_error.or(Some(message)),
            }
        }
        // A tab with no file cannot be saved silently: say how many were left.
        let skipped = self
            .tabs
            .iter()
            .filter(|tab| tab.dirty() && tab.path().is_none())
            .count();
        match first_error {
            Some(message) => self.report(format!("Saving {started}; then {message}")),
            None if skipped > 0 => self.report(format!(
                "Saving {started} tab(s). {skipped} tab(s) have no file yet — use Save As."
            )),
            None => self.report(format!("Saving {started} tab(s).")),
        }
    }

    /// The message when this tab already has a save running, if it has (H6).
    ///
    /// A second save of the same tab would race the first one for the file, and
    /// the user would see two results for one action.
    fn save_blocked(&self, tab: u64) -> Option<String> {
        self.saving
            .get(&tab)
            .map(|path| format!("{} is already being saved…", path.display()))
    }

    /// Flattens the tab and hands the encode + write to the save worker (H6).
    ///
    /// Everything that can fail before bytes are touched — an extension this
    /// build cannot write, a tab that is gone — fails here, so a failed Save
    /// does not leave the status bar saying "Saving…". `flatten` stays on this
    /// thread: it needs the app's one `Fonts`.
    fn start_save(&mut self, index: usize, path: &Path, auto: bool) -> Result<(), String> {
        let format = crate::tab::format_for_path(path)?;
        let flatten = self.settings.settings().flatten_options();
        let Some(tab) = self.tabs.get(index) else {
            return Err("Nothing to save.".to_owned());
        };
        let job = crate::save::SaveJob {
            tab: tab.id(),
            path: path.to_path_buf(),
            document: tab.document.clone(),
            image: tab.flattened(&mut self.fonts, &flatten),
            format,
            options: self.settings.settings().encode_options(),
            auto,
        };
        self.saving.insert(job.tab, job.path.clone());
        self.save_worker.submit(job);
        Ok(())
    }

    /// A free path for a new save in `folder` (H1): `name.ext`, then
    /// `name-1.ext`, … — free on disk **and** not claimed by a save that is
    /// still running (H6). The worker writes later than the name is chosen, so
    /// the filesystem alone cannot answer this.
    fn free_save_path(&self, folder: &Path, name: &str, extension: &str) -> PathBuf {
        let in_flight = &self.saving;
        let file_name = free_file_name_with(name, extension, |candidate| {
            let path = folder.join(candidate);
            path.exists() || in_flight.values().any(|claimed| *claimed == path)
        });
        folder.join(file_name)
    }

    /// The flattened image Save or Copy of tab `index` would use, with the
    /// "annotations outside the image" setting applied (NEXT_STEPS step 10).
    ///
    /// Copy uses this; Save flattens through the same `FlattenOptions` and the
    /// same `Tab::flattened` call inside [`SnpLigaseApp::start_save`], so the
    /// clipboard and the file cannot disagree about the export.
    fn flattened_for_export(&mut self, index: usize) -> Option<image::RgbaImage> {
        let flatten = self.settings.settings().flatten_options();
        let tab = self.tabs.get(index)?;
        Some(tab.flattened(&mut self.fonts, &flatten))
    }

    /// File > Copy: the flattened image on the clipboard (SPEC 7.4 and 9).
    ///
    /// The copy uses the same "annotations outside the image" setting as Save
    /// (NEXT_STEPS step 10): what is copied is what would be written.
    fn action_copy(&mut self) {
        let Some(image) = self.flattened_for_export(self.active) else {
            self.report("Nothing to copy.");
            return;
        };
        let (width, height) = (image.width(), image.height());
        let result = arboard::Clipboard::new().and_then(|mut clipboard| {
            clipboard.set_image(arboard::ImageData {
                width: width as usize,
                height: height as usize,
                bytes: image.into_raw().into(),
            })
        });
        match result {
            Ok(()) => self.report(format!("Copied {width} × {height} px to the clipboard.")),
            Err(err) => self.report(format!("Could not copy to the clipboard: {err}")),
        }
    }

    // ---------------------------------------------------------------------
    // Closing tabs (SPEC 7.1)
    // ---------------------------------------------------------------------

    /// Closes a tab, asking first when it has unsaved changes.
    fn request_close(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.settings.settings().ask_before_closing {
            // M2: a save of this tab that is still running is waited for
            // before the prompt: the tab is only dirty until it lands.
            self.wait_for_dirty_saves(&[index]);
            if self.tabs[index].dirty() {
                self.pending_close = Some(PendingClose::Tabs {
                    indices: vec![index],
                    then_exit: false,
                });
                return;
            }
        }
        self.close_tabs(&[index]);
    }

    /// Closes every tab, asking first when any has unsaved changes.
    fn request_close_all(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        if self.settings.settings().ask_before_closing {
            let all: Vec<usize> = (0..self.tabs.len()).collect();
            self.wait_for_dirty_saves(&all);
            if self.tabs.iter().any(Tab::dirty) {
                self.pending_close = Some(PendingClose::Tabs {
                    indices: all,
                    then_exit: false,
                });
                return;
            }
        }
        self.close_tabs(&(0..self.tabs.len()).collect::<Vec<_>>());
    }

    /// Really exits, but only after asking about unsaved tabs (SPEC 6.1).
    ///
    /// Both the window's close button and File > Exit come through here, so
    /// there is one place that decides what "leaving" means. SPEC section 10's
    /// "Ask before closing unsaved tabs" off means it does not ask.
    fn request_exit(&mut self) {
        let ask = self.settings.settings().ask_before_closing;
        if ask {
            // M2: a tab whose save is still running is not unsaved yet; wait
            // for the result before deciding whether a prompt is needed.
            self.wait_for_dirty_saves(&(0..self.tabs.len()).collect::<Vec<_>>());
        }
        let dirty: Vec<usize> = (0..self.tabs.len())
            .filter(|&index| self.tabs[index].dirty())
            .collect();
        if dirty.is_empty() || !ask {
            self.exit = true;
            return;
        }
        self.pending_close = Some(PendingClose::Tabs {
            indices: dirty,
            then_exit: true,
        });
    }

    /// Carries out a close the user confirmed.
    ///
    /// `ask` is how a tab with no file gets its Save As dialog; taking it as a
    /// closure is what makes the "the user cancelled" path testable (R08).
    fn confirm_close(&mut self, save_first: bool, ask: &mut dyn FnMut(&Tab) -> Option<PathBuf>) {
        let Some(PendingClose::Tabs {
            indices: mut targets,
            then_exit,
        }) = self.pending_close.take()
        else {
            return;
        };
        targets.sort_unstable();
        targets.dedup();

        let options = self.settings.settings().encode_options();
        let flatten = self.settings.settings().flatten_options();
        if save_first {
            for index in targets.clone() {
                if !self.tabs.get(index).is_some_and(Tab::dirty) {
                    continue;
                }
                // M1: a save of this tab that is still in flight is waited for
                // and reused. Starting a second, synchronous save of the same
                // path would race it for the temporary file and the rename,
                // and would encode the same pixels twice.
                if let Some(id) = self.tabs.get(index).map(Tab::id) {
                    self.wait_for_save(id);
                }
                if !self.tabs.get(index).is_some_and(Tab::dirty) {
                    // The save in flight wrote exactly this document.
                    continue;
                }
                // A tab that has never been saved has nowhere to write to, so it
                // goes through Save As. Cancelling that cancels the whole
                // close: the old code closed the tab anyway (R08).
                let path = match self.tabs[index].path().map(Path::to_path_buf) {
                    Some(path) => Some(path),
                    None => match ask(&self.tabs[index]) {
                        Some(path) => Some(path),
                        None => {
                            self.pending_close = Some(PendingClose::Tabs {
                                indices: vec![index],
                                then_exit,
                            });
                            self.report("Save cancelled; nothing was closed.");
                            return;
                        }
                    },
                };
                let Some(path) = path else {
                    continue;
                };
                if let Some(tab) = self.tabs.get_mut(index)
                    && let Err(message) = tab.save_to(&mut self.fonts, &path, &options, &flatten)
                {
                    self.pending_close = Some(PendingClose::Tabs {
                        indices: vec![index],
                        then_exit,
                    });
                    self.report(message);
                    return;
                }
            }
        }

        let closed = targets.len();
        self.close_tabs(&targets);
        self.report(format!("Closed {closed} tab(s)."));
        if then_exit {
            self.exit = true;
        }
    }

    /// Removes tabs and frees their textures.
    fn close_tabs(&mut self, indices: &[usize]) {
        self.settle_edits();
        // Removing a tab shifts every index after it, and the session is tied
        // to one tab index (F4): it ends before the list changes.
        self.commit_text_session();
        let mut sorted = indices.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        for index in sorted.into_iter().rev() {
            if let Some(tab) = self.tabs.get_mut(index) {
                // Dropping the tab drops its texture handles, which is what
                // frees the GPU memory (SPEC 7.2 keeps no texture registry).
                tab.forget_all_textures();
            }
            if index < self.tabs.len() {
                self.tabs.remove(index);
            }
        }
        self.set_active(self.active.min(self.tabs.len().saturating_sub(1)));
    }

    // ---------------------------------------------------------------------
    // Zoom (SPEC 7.2)
    // ---------------------------------------------------------------------

    /// Multiplies the zoom, keeping the middle of the canvas fixed.
    ///
    /// A manual zoom, so it stands auto-fit down for this tab (NEXT_STEPS
    /// step 13).
    fn zoom(&mut self, factor: f32) {
        let canvas = self.canvas;
        let flatten = self.settings.settings().flatten_options();
        let content = self.viewport_content(flatten.expand);
        let (Some(tab), Some(content)) = (self.active_tab(), content) else {
            return;
        };
        tab.user_zoomed();
        let centre = pos2(canvas.x / 2.0, canvas.y / 2.0);
        tab.viewport.zoom_around(factor, centre, content, canvas);
        tracing::debug!(target: "snp_ligase::ui", factor, zoom = tab.viewport.zoom, "zoomed");
    }

    /// The rectangle the view may pan and zoom over for the tab at `index`:
    /// the export area with NEXT_STEPS step 10's expansion, the image itself
    /// with "Cut".
    ///
    /// J3: the export area comes from the tab's **sprite cache**, so this no
    /// longer lays out every Text annotation per frame.
    fn viewport_content_at(&self, index: usize, expand: bool) -> Option<Rect> {
        let tab = self.tabs.get(index)?;
        Some(tab.export_rect(expand))
    }

    /// [`Self::viewport_content_at`] for the active tab.
    fn viewport_content(&self, expand: bool) -> Option<Rect> {
        let index = self.active;
        self.viewport_content_at(index, expand)
    }

    // ---------------------------------------------------------------------
    // Running actions
    // ---------------------------------------------------------------------

    /// Runs one action from a panel or a shortcut.
    fn run(&mut self, action: Action) {
        // An open text session ends before anything that would move the ground
        // under it: the editor's annotation could otherwise be edited, undone
        // or closed away while the editor is still typing into it (SPEC 7.5).
        if ends_text_session(&action) {
            self.commit_text_session();
        }
        // A hotkey recording ends before the dialog that owns it closes; the
        // guard would re-register the keys on Drop anyway, this makes the
        // moment deliberate and logged (NEXT_STEPS step 7).
        if ends_hotkey_recording(&action) {
            self.end_hotkey_recording();
        }
        match action {
            Action::Open => self.action_open(),
            Action::PasteAsNewTab => self.action_paste(),
            Action::PasteIntoTab => self.action_paste_into_tab(),
            Action::Save => self.action_save(false),
            Action::SaveAs => self.action_save(true),
            Action::SaveAll => self.action_save_all(),
            Action::CopyImage => self.action_copy(),
            Action::CloseTab(index) => {
                // Ctrl+W has no index yet: it means "this tab".
                let index = if index == usize::MAX {
                    self.active
                } else {
                    index
                };
                self.request_close(index)
            }
            Action::CloseAllTabs => self.request_close_all(),
            Action::Settings => self.open_settings(),
            Action::ApplySettings => self.apply_settings(),
            Action::CancelSettings => {
                self.settings_draft = None;
                self.settings_warnings.clear();
                self.report("Settings closed without changes.");
            }
            Action::PickSaveFolder => self.pick_save_folder(),
            Action::RecordHotkey(mode) => self.start_hotkey_recording(mode),
            Action::TestHotkeys => self.test_hotkeys(),
            Action::Exit => self.request_exit(),

            Action::Undo => {
                if let Some(tab) = self.active_tab()
                    && tab.undo()
                {
                    // Undo can bring annotations back or take them away; the
                    // textures of the ones that are gone go with them (R30).
                    tab.forget_deleted();
                    self.gesture = Gesture::None;
                    self.active_point = None;
                    self.report("Undone.");
                }
            }
            Action::Redo => {
                if let Some(tab) = self.active_tab()
                    && tab.redo()
                {
                    tab.forget_deleted();
                    self.gesture = Gesture::None;
                    self.active_point = None;
                    self.report("Redone.");
                }
            }
            Action::Duplicate => {
                self.with_selection("Duplicate", tools::select::duplicate_selection)
            }
            Action::Delete => self.delete(),
            Action::DeletePoint(kind) => {
                self.with_selection("Delete point", |tab| tools::select::delete_point(tab, kind))
            }
            Action::SelectAll => {
                if let Some(tab) = self.active_tab() {
                    tab.select_all();
                    self.report("Selected every annotation.");
                }
            }
            Action::BringToFront => {
                self.with_selection("Bring to Front", |tab| tools::select::reorder(tab, true))
            }
            Action::BringForward => self.with_selection("Bring Forward", |tab| {
                tools::select::reorder_one_place(tab, true)
            }),
            Action::SendBackward => self.with_selection("Send Backward", |tab| {
                tools::select::reorder_one_place(tab, false)
            }),
            Action::SendToBack => {
                self.with_selection("Send to Back", |tab| tools::select::reorder(tab, false))
            }
            Action::Crop => {
                // SPEC 8.3's Edit > Crop: pick the Crop tool, so the drag and
                // the handles behave exactly as SPEC 7.3 describes.
                self.tool = Tool::Crop;
                self.gesture = Gesture::None;
                self.report("Crop tool (C). Drag a rectangle, then Enter applies it.");
            }

            Action::Capture(mode) => {
                // SPEC 5.7: a menu-triggered capture honours the Delay setting;
                // the controller hides the window, waits, captures and brings
                // the window back (SPEC 6.2).
                self.controller.send(AppCommand::Capture {
                    mode,
                    delay_seconds: self.delay_seconds,
                });
                self.report(format!(
                    "Capturing {}{}…",
                    mode.label().to_lowercase(),
                    if self.delay_seconds == 0 {
                        String::new()
                    } else {
                        format!(" in {} s", self.delay_seconds)
                    }
                ));
            }
            Action::Delay(seconds) => {
                self.delay_seconds = seconds;
                self.report(format!(
                    "Delay set to {}.",
                    if seconds == 0 {
                        "none".to_owned()
                    } else {
                        format!("{seconds} s")
                    }
                ));
            }

            Action::Tool(tool) => {
                // SPEC 7.5: the Text tool is disabled with a "Loading fonts…"
                // label until the font system is ready, and the shortcut has to
                // say the same thing.
                if tool == Tool::Text && !self.fonts_ready() {
                    self.report("Loading fonts…");
                } else {
                    self.tool = tool;
                    self.gesture = Gesture::None;
                    self.report(format!("{} tool ({}).", tool.name(), tool.key()));
                }
            }

            Action::ZoomIn => self.zoom(1.25),
            Action::ZoomOut => self.zoom(0.8),
            // SPEC 8.3: Ctrl+0 and Ctrl+= / Ctrl+-.
            Action::ActualSize => {
                let canvas = self.canvas;
                let flatten = self.settings.settings().flatten_options();
                let content = self.viewport_content(flatten.expand);
                if let (Some(tab), Some(content)) = (self.active_tab(), content) {
                    tab.user_zoomed();
                    tab.viewport.actual_size(content, canvas);
                    self.report("100%.");
                }
            }
            // Ctrl+1 both fits and re-arms auto-fit for this tab (NEXT_STEPS
            // step 13), so a manual zoom can be undone from the View menu or
            // the keyboard.
            Action::FitToWindow => {
                let canvas = self.canvas;
                let flatten = self.settings.settings().flatten_options();
                let content = self.viewport_content(flatten.expand);
                if let (Some(tab), Some(content)) = (self.active_tab(), content) {
                    tab.auto_fit = true;
                    tab.viewport.fit(content, canvas, 16.0);
                    // The fit is what auto-fit would have done, so record it:
                    // without this the next frame would see "not fitted to
                    // this layout" and fit again, undoing the upscale a small
                    // image gets from Ctrl+1.
                    tab.viewport.fitted_to =
                        Some(crate::viewport::FittedLayout { canvas, content });
                    self.report("Fitted to the window.");
                }
            }
            // View > Auto-fit to Window (NEXT_STEPS step 13).
            Action::AutoFit(value) => {
                let canvas = self.canvas;
                let flatten = self.settings.settings().flatten_options();
                let content = self.viewport_content(flatten.expand);
                if let (Some(tab), Some(content)) = (self.active_tab(), content) {
                    tab.auto_fit = value;
                    if value {
                        // Turning it on fits now, so the row's effect is
                        // visible at once rather than at the next resize.
                        tab.viewport.fit_for_auto_fit(content, canvas);
                    }
                    self.report(if value {
                        "Auto-fit to the window on."
                    } else {
                        "Auto-fit to the window off; the zoom stays where it is."
                    });
                }
            }
            Action::ShowToolLabels(value) => {
                self.show_tool_labels = value;
                // SPEC section 10 lists this as a setting, so the View menu's
                // toggle is persisted too (debounced by the settings store).
                let mut settings = self.settings.settings().clone();
                settings.show_tool_labels = value;
                self.settings.replace(settings);
                self.report(if value {
                    "Tool labels on."
                } else {
                    "Tool labels off."
                });
            }
            Action::ShowProperties(value) => {
                self.show_properties = value;
                self.report(if value {
                    "Properties panel shown."
                } else {
                    "Properties panel hidden."
                });
            }

            Action::KeyboardShortcuts => {
                self.shortcuts_open = true;
                self.report("Keyboard shortcuts (F1).");
            }
            Action::OpenLogFolder => self.open_log_folder(),
            Action::About => self.report(format!(
                "snp-ligase {} — a Windows 11 screenshot and annotation tool.",
                env!("CARGO_PKG_VERSION")
            )),

            Action::SelectTab(index) => {
                if index < self.tabs.len() {
                    self.set_active(index);
                }
            }
            // SPEC 7.1: Ctrl+Tab / Ctrl+Shift+Tab.
            Action::SwitchTab(direction) => self.switch_tab(direction),
            // SPEC 7.4: the arrow keys nudge the selection; the step is already
            // in the delta, because Shift is part of the key event.
            Action::Nudge(dx, dy) => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tools::select::nudge(tab, dx, dy);
                }
            }
            Action::ShapeRemoveLastPoint => {
                if let Some(message) = tools::shape::remove_last_point(&mut self.gesture) {
                    self.report(message);
                }
            }
            Action::ShapeFinish => {
                let styles = self.styles.clone();
                let message = self
                    .tabs
                    .get_mut(self.active)
                    .and_then(|tab| tools::shape::finish(tab, &styles, &mut self.gesture));
                if let Some(message) = message {
                    self.report(message);
                }
            }
            Action::ApplyCrop => self.apply_crop(),
            Action::CommitText => self.commit_text_session(),
            Action::CancelText => {
                self.cancel_text_session();
                self.report("Text editing cancelled.");
            }
            Action::ConfirmSave => {
                // The Save As dialog for a tab with no file needs the settings
                // (default folder, template, format), and `confirm_close`
                // borrows the whole app.
                let settings = self.settings.settings().clone();
                self.confirm_close(true, &mut |_tab| save_dialog(&settings, None));
            }
            Action::ConfirmDiscard => {
                let settings = self.settings.settings().clone();
                self.confirm_close(false, &mut |_tab| save_dialog(&settings, None));
            }
            Action::ConfirmCancel => {
                self.pending_close = None;
                self.report("Cancelled; nothing was closed.");
            }

            Action::PickColour(field) => {
                self.colour_picker = Some(field);
            }
            Action::PickFont => {
                self.font_picker = true;
            }
            Action::PickNextNumber => {
                self.next_number_editor = true;
            }
            Action::SetNextNumber(value) => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    if tab.document.next_number != value {
                        // Folded into the text session's undo step like any
                        // other property change (M7).
                        if self.text_session.is_none() {
                            tab.begin_edit();
                        }
                        tab.document.next_number = value;
                        self.report(format!("The next number will be {value}."));
                    } else {
                        self.report(format!("The next number is already {value}."));
                    }
                }
            }
            Action::CustomValue(field) => {
                self.custom_editor = Some(field);
            }
            Action::Style(property) => self.apply_style(property),

            // Esc cancels a move or a handle drag, and puts the shape back where
            // it was: the document was already edited live (R21).
            Action::CancelGesture => {
                let cancelled = self.gesture.is_active() || self.edit_in_flight();
                let restored = self.cancel_gesture();
                self.gesture = Gesture::None;
                self.report(if cancelled || restored {
                    "Cancelled."
                } else {
                    "Nothing to cancel."
                });
            }
        }
    }

    /// Applies one style property to the selection, or to the active tool's
    /// defaults with nothing selected (SPEC 7.7).
    ///
    /// This is the one write path the Annotation menu and the properties panel
    /// share.
    fn apply_style(&mut self, property: StyleProperty) {
        let has_selection = self
            .active_ref()
            .is_some_and(|tab| !tab.selection().is_empty());

        if !has_selection {
            // Nothing selected: the property belongs to the active tool's
            // defaults (SPEC 7.7). It is not document state, so there is no undo
            // step to take.
            let Some(style) = self.styles.get_mut(self.tool) else {
                self.report("Select an annotation, or pick a tool to change what it draws.");
                return;
            };
            properties::write_into_style(style, &property);
            let tool = self.tool;
            let report = property_label(&property, tool);
            self.report(report);
            return;
        }

        let ids = match self.active_ref() {
            Some(tab) if !tab.selection().is_empty() => tab.selection().to_vec(),
            _ => Vec::new(),
        };
        if ids.is_empty() {
            self.report("Nothing there takes that property.");
            return;
        }
        let Some(tab) = self.tabs.get_mut(self.active) else {
            self.report("Nothing to apply that to.");
            return;
        };
        // M7: while the text editor is open, a property change folds into the
        // session's undo step instead of taking one of its own. Two steps for
        // one session made the first Ctrl+Z return to the pre-session document
        // and the second resurrect the half-typed text (see `DECISIONS.md`).
        if self.text_session.is_none() {
            tab.stage_edit();
        }
        // The styles are moved out of `self` for the duration, so the two
        // borrows do not overlap. Taking them through the field rather than
        // through a `&mut self` method is what makes the disjoint borrow legal.
        let mut styles = std::mem::take(&mut self.styles);
        let changed =
            properties::apply_properties(tab, &mut styles, &ids, std::slice::from_ref(&property));
        self.styles = styles;
        if changed == 0 {
            self.report("Nothing there takes that property.");
            return;
        }
        let report = format!(
            "{} changed on {changed} annotation{}.",
            property_name(&property),
            if changed == 1 { "" } else { "s" }
        );
        self.report(report);
    }

    /// `Delete`: removes the picked-out point if there is one, otherwise the
    /// whole selection.
    fn delete(&mut self) {
        let picked_out = self.active_point;
        self.active_point = None;
        if picked_out.is_some() {
            self.with_selection("Delete point", |tab| {
                let kind = picked_out?;
                tools::select::delete_point(tab, kind)
            });
            return;
        }
        self.with_selection("Delete", tools::select::delete_selection);
    }

    /// Applies the Crop tool's rectangle (SPEC 7.3): one undo step, and the
    /// base image's size changes, so the viewport starts over and the cached
    /// textures go.
    fn apply_crop(&mut self) {
        self.commit_text_session();
        let Some(rect) = self.gesture.crop_rect() else {
            self.report("Drag a rectangle with the Crop tool first.");
            return;
        };
        let Some(tab) = self.tabs.get_mut(self.active) else {
            self.report("Nothing to crop.");
            return;
        };
        // `edit::crop` rounds out and *then* clips, so the rectangle has to be
        // clipped here first: otherwise a crop that misses the image entirely
        // becomes a 0×0 document (F3).
        let rect = tools::crop::clip_to_image(tab, rect);
        if !tools::crop::crop_big_enough(tab, rect) {
            self.report("That crop is too small; drag a bigger rectangle.");
            return;
        }

        tab.begin_edit();
        tab.document = snp_ligase_core::edit::crop(&tab.document, rect);
        // The image is a different size now: the viewport re-fits itself next
        // frame, and the base texture is rebuilt from the new image.
        tab.viewport.initialised = false;
        tab.forget_all_textures();
        self.gesture = Gesture::None;
        let (width, height) = tab.size();
        self.report(format!("Cropped to {width} × {height} px."));
    }

    /// Runs an editing command on the selection and reports what it said.
    fn with_selection(&mut self, what: &str, command: impl FnOnce(&mut Tab) -> Option<String>) {
        let Some(tab) = self.active_tab() else {
            self.report(format!("{what}: no image open."));
            return;
        };
        match command(tab) {
            Some(message) => self.report(message),
            None => self.report(format!("{what}: nothing selected.")),
        }
    }

    /// Ends an in-flight gesture by putting the document back the way it was
    /// before the gesture started (R21).
    fn cancel_gesture(&mut self) -> bool {
        self.active_tab().is_some_and(|tab| tab.cancel_edit())
    }

    /// Ends an edit that is still in flight, as one undo step.
    ///
    /// The snapshot is taken on the first change and held while the pointer is
    /// down, so a slider drag is one undo step and a click that changed nothing
    /// is none (R20).
    fn settle_edits(&mut self) {
        if let Some(tab) = self.active_tab() {
            tab.commit_edit();
        }
    }

    /// True when the active tab has an edit in flight that Esc could undo.
    fn edit_in_flight(&self) -> bool {
        self.active_ref().is_some_and(Tab::edit_in_flight)
    }

    /// Drops the picked-out point when it no longer exists — after an undo, a
    /// tab change, a tool change, or a selection that is no longer a line or a
    /// shape (R10: a stale index was an out-of-bounds `Vec::remove` and a
    /// crash).
    fn sync_active_point(&mut self) {
        let Some(active) = self.active_point else {
            return;
        };
        let still_there = self.active_ref().is_some_and(|tab| {
            tab.selected().is_some_and(|annotation| {
                let length = match &annotation.kind {
                    snp_ligase_core::model::Kind::Path(path) => path.points.len(),
                    snp_ligase_core::model::Kind::Polygon(polygon) => polygon.points.len(),
                    _ => 0,
                };
                length > 0 && matches!(active, snp_ligase_core::edit::HandleKind::Point(index) if index < length)
            })
        });
        if !still_there {
            self.active_point = None;
        }
    }

    /// Opens the log directory in Explorer, so the human can send it back.
    fn open_log_folder(&mut self) {
        match std::process::Command::new("explorer")
            .arg(self.log_dir.as_os_str())
            .spawn()
        {
            Ok(_child) => {
                let message = format!("Opened {}", self.log_dir.display());
                tracing::info!(target: "snp_ligase::ui", "{message}");
                self.status = message;
            }
            Err(err) => self.report(format!("Could not open the log folder: {err}")),
        }
    }
}

impl App for SnpLigaseApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.frame_ui(ui);
    }

    fn on_exit(&mut self) {
        // H6: a save that is still running is nearly done — wait for it
        // (bounded), because exiting now would throw the file away.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !self.saving.is_empty() && std::time::Instant::now() < deadline {
            self.drain_saves();
            if !self.saving.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        if !self.saving.is_empty() {
            tracing::warn!(
                saves = self.saving.len(),
                "exiting with saves still in flight"
            );
        }
        // H3: a change made inside the last debounce interval is not on disk
        // yet — quitting must not lose it.
        if self.settings.dirty()
            && let Err(message) = self.settings.flush()
        {
            tracing::warn!(error = %message, "the settings could not be written on exit");
        }
        // Anything the human would want in the log if the app misbehaved.
        let open = self.tabs.len();
        let dirty = self.tabs.iter().filter(|tab| tab.dirty()).count();
        tracing::info!(
            target: "snp_ligase::ui",
            tabs = open,
            unsaved = dirty,
            "exiting"
        );
    }
}

impl SnpLigaseApp {
    /// One frame of the whole window.
    fn frame_ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let now = std::time::Instant::now();
        // H4: a move that is under way this frame is what lets the sprite cache
        // reuse translated pixels; whether it ended is checked at the bottom.
        let was_moving = self.gesture.is_moving();
        // SPEC 10's "Start hidden in the tray": the hide goes out on the second
        // frame, after eframe's own first show (see the field's comment).
        match self.start_hidden {
            1 => {
                self.start_hidden = 2;
                // H5: ask for the frame that does the hiding. Nothing else
                // runs one — the window just sits there — so without this
                // request the hide would wait for the first mouse move.
                ctx.request_repaint();
            }
            2 => {
                self.start_hidden = 0;
                self.controller.window().hide();
                self.report(
                    "Started hidden in the tray. Print Screen captures a region; the tray's \
                     Show Window brings this back.",
                );
            }
            _ => {}
        }
        // The controller thread may have finished a capture while no frame was
        // running — that is the whole point of it (SPEC 6.2) — so its queue is
        // drained before anything is drawn.
        self.drain_controller_events();
        self.drain_saves();
        self.poll_fonts();
        self.open_pending();
        self.open_dropped(&ctx);
        // The picked-out point can go stale (R10), and it is cheaper to check
        // once per frame than to try to catch every way it can happen.
        self.sync_active_point();
        self.handle_close_request(ui);

        // NEXT_STEPS step 7: while a hotkey is being recorded the keyboard is
        // polled every frame, and the frame asks for the next one — the keys
        // are read with `GetAsyncKeyState`, so no input event arrives by
        // itself and nothing else would run a frame.
        if self.recording.is_some() {
            if ctx.input(|input| input.viewport().focused.unwrap_or(true)) {
                let snapshot = snp_ligase_platform::windows::keys::keyboard_snapshot();
                self.poll_hotkey_recording(&snapshot);
                ctx.request_repaint();
            } else {
                // The keyboard belongs to another window now; a key pressed
                // there must not become our hotkey.
                self.end_hotkey_recording();
            }
        }

        let mut actions: Vec<Action> = Vec::new();
        let state = self.menu_state();

        panels::menu_bar(ui, &state, &mut actions);
        panels::toolbar(ui, &state, &mut actions);
        panels::tab_strip(ui, &self.tabs, self.active, &mut actions);

        Panel::left("tools").show(ui, |ui| {
            panels::tools_panel(
                ui,
                self.tool,
                self.show_tool_labels,
                self.fonts_ready(),
                &mut actions,
            );
        });

        if self.show_properties {
            let mut styles = std::mem::take(&mut self.styles);
            let mut recent = std::mem::take(&mut self.recent_colours);
            let tool = self.tool;
            // M7: a property change made while the text editor is open folds
            // into the session's undo step.
            let fold_edits = self.text_session.is_some();
            // The font list is read before the tab borrow, so the panel gets a
            // plain slice of family names (SPEC 7.5).
            let families: Vec<String> = self.fonts.families().to_vec();
            let tab = self.tabs.get_mut(self.active);
            Panel::right("properties")
                .default_size(300.0)
                .show(ui, |ui| {
                    properties::show(
                        ui,
                        properties::PanelContext {
                            tab,
                            tool,
                            styles: &mut styles,
                            recent: &mut recent,
                            families: &families,
                            fold_edits,
                        },
                        &mut actions,
                    )
                });
            self.styles = styles;
            self.recent_colours = recent;
        }

        // The export rectangle for this frame, from the sprite cache: the
        // status bar and the canvas both use it, and it is computed once
        // (NEXT_STEPS step 11, J3).
        let flatten = self.settings.settings().flatten_options();
        self.content_rect = self.viewport_content(flatten.expand).unwrap_or(Rect::ZERO);

        // The status bar must be added **before** the central panel: a panel
        // added after it has no room left, so its content was laid out below
        // the bottom of the window (found while verifying step 11's "no tab"
        // status line; it was effectively invisible before).
        let saves_as = self.active_ref().map(|_| {
            (
                self.content_rect.w.max(0.0) as u32,
                self.content_rect.h.max(0.0) as u32,
            )
        });
        panels::status_bar(ui, self.active_ref(), self.cursor, &self.status, saves_as);

        self.canvas(ui, &mut actions);
        self.shortcuts(ui);

        if let Some(PendingClose::Tabs { indices, .. }) = &self.pending_close {
            let (title, plural) = match indices.as_slice() {
                [index] => (
                    self.tabs
                        .get(*index)
                        .map_or_else(|| "This image".to_owned(), Tab::title),
                    false,
                ),
                _ => (format!("{} images", indices.len()), true),
            };
            panels::unsaved_prompt(ui, &title, plural, &mut actions);
        }

        self.colour_window(ui, &mut actions);
        self.font_window(ui, &mut actions);
        self.next_number_window(ui, &mut actions);
        self.custom_window(ui, &mut actions);
        self.text_editor_window(ui, &mut actions);
        self.settings_window(ui, &mut actions);
        self.shortcuts_window(ui);

        // An edit the panel staged is held while the pointer is down, so a whole
        // slider drag or popup session is one undo step; it ends here (R20).
        let pointer_down = ui.input(|input| input.pointer.any_down());
        if !pointer_down {
            self.settle_edits();
        }

        // SPEC section 10: tool defaults and recent colours are saved
        // (debounced) as they change, not only when Settings is confirmed.
        self.settings_tick();

        for action in actions {
            self.run(action);
        }

        // H4: a move that ended this frame — the release, Esc, a tab switch —
        // must be followed by the frame that rasterizes the sprite at its real
        // position; without this the shifted, resampled pixels stay on screen
        // until the next input event.
        if was_moving && !self.gesture.is_moving() {
            ctx.request_repaint();
        }

        // SPEC 12 M7's drag budget: a gesture's frames are accumulated and
        // reported once, when it ends.
        self.frame_timing(now);

        if self.exit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Records this frame's work, and reports a finished gesture's timing
    /// (SPEC 12 M7: "dragging an annotation on a 4K image stays above 30 fps").
    ///
    /// One line per gesture, not one per frame. `work` is what the frame itself
    /// cost: if its worst case is under 33 ms, the drag can hold 30 fps. The
    /// frame *period* is logged too, but it includes the wait for the next input
    /// event, so a synthetic drag's gaps are not rendering costs.
    fn frame_timing(&mut self, started: std::time::Instant) {
        let work = started.elapsed();
        let now = std::time::Instant::now();
        let period = self
            .last_frame
            .map(|last| now.saturating_duration_since(last));
        self.last_frame = Some(now);

        if self.gesture.is_active() {
            self.gesture_frames += 1;
            self.gesture_total += work;
            self.gesture_max = self.gesture_max.max(work);
            // The first period contains the idle wait before the gesture.
            if self.gesture_frames > 1
                && let Some(period) = period
            {
                self.gesture_period_total += period;
                self.gesture_periods += 1;
            }
            if work > std::time::Duration::from_millis(33) {
                self.gesture_slow += 1;
                // The frame number matters: it says *when* a hitch happened
                // (the first frame of a drag, a release, a re-upload).
                tracing::debug!(
                    frame = self.gesture_frames,
                    ms = work.as_secs_f64() * 1000.0,
                    "a gesture frame's work missed 30 fps"
                );
            }
            return;
        }
        if self.gesture_frames == 0 {
            return;
        }
        let frames = f64::from(self.gesture_frames);
        let average_work_ms = self.gesture_total.as_secs_f64() * 1000.0 / frames;
        let average_period_ms = if self.gesture_periods > 0 {
            self.gesture_period_total.as_secs_f64() * 1000.0 / f64::from(self.gesture_periods)
        } else {
            0.0
        };
        tracing::info!(
            frames = self.gesture_frames,
            avg_work_ms = average_work_ms,
            max_work_ms = self.gesture_max.as_secs_f64() * 1000.0,
            avg_period_ms = average_period_ms,
            slow = self.gesture_slow,
            "gesture frame work"
        );
        self.gesture_frames = 0;
        self.gesture_total = std::time::Duration::ZERO;
        self.gesture_max = std::time::Duration::ZERO;
        self.gesture_period_total = std::time::Duration::ZERO;
        self.gesture_periods = 0;
        self.gesture_slow = 0;
    }

    /// The window's close button (SPEC 6.1): hide to the tray, or — when an
    /// exit is already under way — go through the same questions File > Exit
    /// asks (R09).
    fn handle_close_request(&mut self, ui: &egui::Ui) {
        // Read the request on its own: `Ui::input` takes a lock, and calling
        // another `Context` method from inside that closure deadlocks the frame
        // (DECISIONS.md, M3).
        let close_requested = ui.input(|input| input.viewport().close_requested());
        if !close_requested {
            return;
        }
        if self.exit {
            // G1: this request *is* the exit under way — `frame_ui` sent
            // `ViewportCommand::Close` once `self.exit` was set, and the backend
            // reports it back like any other close request. Cancelling it here
            // kept eframe's window alive, and the next frame sent Close again:
            // File > Exit and the tray's Quit never ended the process. Let it
            // through.
            tracing::debug!("the close request belongs to the exit under way");
            return;
        }
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if self.pending_close.is_some() {
            // A save prompt is already up: this request is not a new decision.
            return;
        }
        if self.close_hides {
            // SPEC 6.1: the X hides to the tray by default. File > Exit and the
            // tray's Quit really leave, and both ask about unsaved tabs first.
            self.hide_to_tray();
        } else {
            // G3: without a tray icon there is no way back from a hidden
            // window, so the X exits.
            self.request_exit();
        }
    }

    /// The state the menus need, read before any action runs.
    fn menu_state(&self) -> MenuState {
        let tab = self.active_ref();
        let (style, nodes) = match tab {
            // Nothing selected: the Annotation menu edits the active tool's
            // defaults (SPEC 7.7).
            Some(tab) if tab.selection().is_empty() => (
                self.styles.get(self.tool).cloned(),
                crate::tool::StyleNode::of_tool(self.tool),
            ),
            Some(tab) => {
                let ids = tab.selection().to_vec();
                let kinds: Vec<snp_ligase_core::model::Kind> = ids
                    .iter()
                    .filter_map(|id| tab.document.find(*id))
                    .map(|annotation| annotation.kind.clone())
                    .collect();
                let style = ids
                    .first()
                    .and_then(|id| tab.document.find(*id))
                    .and_then(properties::style_of);
                (
                    style,
                    crate::tool::StyleNode::common_to(&kinds.iter().collect::<Vec<_>>()),
                )
            }
            None => (None, Vec::new()),
        };
        let settings = self.settings.settings();
        // The menus show the hotkeys the user configured, the same text the
        // Settings dialog and the Keyboard Shortcuts window use (9d).
        let capture_hotkeys = CaptureMode::ALL
            .iter()
            .map(|mode| (*mode, settings.hotkey_text(*mode).trim().to_owned()))
            .collect();
        MenuState {
            can_undo: tab.is_some_and(Tab::can_undo),
            can_redo: tab.is_some_and(Tab::can_redo),
            has_tab: tab.is_some(),
            has_selection: tab.is_some_and(|tab| !tab.selection().is_empty()),
            tool: self.tool,
            show_properties: self.show_properties,
            show_tool_labels: self.show_tool_labels,
            // The View menu's Auto-fit check shows the active tab's state
            // (NEXT_STEPS step 13); with no tab open it shows off.
            auto_fit: tab.is_some_and(|tab| tab.auto_fit),
            fonts_ready: self.fonts_ready(),
            delay_seconds: self.delay_seconds,
            active_tab: self.active,
            style,
            nodes,
            capture_hotkeys,
        }
    }

    /// Opens files dropped on the window (SPEC 7.1).
    fn open_dropped(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        });
        for path in dropped {
            self.open_file(&path);
        }
    }

    /// The central canvas, or the empty-window message.
    fn canvas(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        // The canvas always lives in the central panel. Without this it would
        // be laid out over the whole window and paint on top of the tools and
        // properties panels.
        CentralPanel::default().show(ui, |ui| {
            if self.tabs.is_empty() {
                self.empty_state(ui, actions);
                return;
            }
            self.draw_canvas(ui, actions);
        });
    }

    /// What the canvas area says when there is nothing to edit.
    fn empty_state(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        self.cursor = None;
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            ui.heading("snp-ligase");
            ui.label("No image open yet.");
            ui.label("File > Open… opens a PNG, Ctrl+V pastes one, or drop a file here.");
            ui.add_space(8.0);
            if ui.button("Open an image…").clicked() {
                actions.push(Action::Open);
            }
        });
    }

    /// Draws the image and runs the active tool for this frame.
    fn draw_canvas(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        // SPEC 7.2: sampling changes at 200% zoom, and egui decides that when a
        // texture is uploaded, so it is re-applied here rather than at paint
        // time.
        let space_held = ui
            .ctx()
            .input(|input| input.keys_down.contains(&Key::Space));
        let tool = self.tool;
        let drawing_from = self.gesture.drawing_from();
        let active_point = self.active_point;
        let canvas_size = ui.available_size();
        self.canvas = canvas_size;
        // For the paste actions' texture-limit check (NEXT_STEPS step 11).
        self.max_texture_side = ui.ctx().input(|input| input.max_texture_side);

        // The export area and the fill the canvas has to draw behind the image
        // (NEXT_STEPS step 10): `frame_ui` computed the rectangle once this
        // frame from the sprite cache (J3), for the status bar as well as here.
        let flatten = self.settings.settings().flatten_options();
        let content = self.content_rect;

        let Some(tab) = self.tabs.get_mut(self.active) else {
            self.cursor = None;
            return;
        };

        tab.apply_sampling(tab.viewport.zoom);
        let input: CanvasInput = canvas::show(
            ui,
            tab,
            tool,
            canvas::CanvasFrame {
                drawing_from,
                polygon: self.gesture.polygon_points(),
                freehand: self.gesture.freehand_points(),
                crop: self.gesture.crop_rect(),
                active_point,
                moving: self.gesture.is_moving(),
                space_held,
                content,
                flatten,
            },
            &mut self.fonts,
            actions,
        );
        // The tab is not used again: the tool run below borrows the whole app.
        let message = self.canvas_input(&input);

        self.cursor = input.image_pointer;
        if let Some(message) = message {
            self.report(message);
        }
    }

    /// One frame of canvas input through the tools, plus the text session's
    /// rules (SPEC 7.3 and 7.5).
    ///
    /// A press that landed on the canvas commits an open session **before** the
    /// tool sees it (F6). The session's undo step then lands before the
    /// gesture's, so Ctrl+Z unwinds them in the order the user acted: move a
    /// half-typed text, and the first undo undoes the move, not the typing.
    /// Letting the press through afterwards also keeps the Text tool's "place
    /// on release" behaviour safe: the old session is finished and pushed
    /// before the new annotation — and its session — even start.
    ///
    /// `canvas_press` is kept for the editor window, which needs to know where
    /// the click that took its focus went (F5). The session is already gone by
    /// the time the editor runs, so only the "not the canvas" case is left.
    fn canvas_input(&mut self, input: &CanvasInput) -> Option<String> {
        self.canvas_press = input.primary_pressed;
        if self.canvas_press {
            self.commit_text_session();
        }

        // SPEC section 10's "Switch to Select after drawing": a drawing gesture
        // that ends this frame hands the pointer back to the Select tool.
        //
        // The drawing tools keep their gesture until the next frame (the
        // release is what ends them, and the R16 rule clears the gesture
        // later), so for them the release frame *is* the end. The Shape and
        // Number tools clear their own gesture, so theirs is "no longer
        // active". The Text tool is not here: its gesture ends in
        // `Gesture::TextEdit`, which becomes a session, and the switch happens
        // when that session commits a new annotation (9b).
        //
        // The Freehand stroke (NEXT_STEPS step 14b) behaves like the other
        // drag tools, so its release frame is its end too.
        let was_drag_drawing = matches!(
            self.gesture,
            Gesture::Drawing { .. } | Gesture::Freehand { .. }
        );
        let was_click_drawing = matches!(
            self.gesture,
            Gesture::Polygon { .. } | Gesture::NumberDrag { .. }
        );

        let index = self.active;
        let tool = self.tool;
        let styles = self.styles.clone();
        let tab = self.tabs.get_mut(index)?;
        let message = if tab.display_error.is_some() {
            // The image is too big to show: the drawing tools are left alone.
            // A press still committed any open session above.
            None
        } else {
            tools::handle(
                tab,
                tool,
                &styles,
                input,
                &mut self.fonts,
                &mut self.gesture,
                &mut self.active_point,
            )
        };

        if (was_drag_drawing && input.primary_released
            || was_click_drawing && !self.gesture.is_active())
            && self.settings.settings().switch_to_select_after_drawing
        {
            self.tool = Tool::Select;
            if message.is_none() {
                return Some("Select tool (V).".to_owned());
            }
        }

        // The Text tool (or a double-click on a text) asks for the floating
        // editor by leaving a `TextEdit` gesture behind (SPEC 7.5).
        if matches!(self.gesture, Gesture::TextEdit { .. }) {
            let Gesture::TextEdit {
                id,
                created,
                snapshot,
            } = std::mem::replace(&mut self.gesture, Gesture::None)
            else {
                unreachable!("just matched");
            };
            self.begin_text_session(id, created, *snapshot);
        }
        message
    }

    /// Keyboard shortcuts (SPEC 7.3 and 7.4).
    ///
    /// Single-key tool shortcuts only apply when no text field has focus, and
    /// the whole decision is made by [`shortcuts::route`], which is a pure
    /// function of this frame's input.
    fn shortcuts(&mut self, ui: &Ui) {
        // "Does a text field have focus?" has to be asked on its own:
        // `Context::input` takes a lock, so calling another `Context` method
        // from inside its closure deadlocks the frame.
        let text_field = ui.ctx().egui_wants_keyboard_input();

        // Everything the frame needs, in one pass.
        let (modifiers, events, focused, paste_held, shift_held) = ui.input(|input| {
            (
                input.modifiers,
                input.events.clone(),
                input.viewport().focused != Some(false),
                snp_ligase_platform::windows::keys::paste_held(),
                snp_ligase_platform::windows::keys::shift_down(),
            )
        });

        let mut pasted = false;
        let mut actions = Vec::new();
        let shape_drawing = self.gesture.polygon_points().is_some();
        let crop_pending = self.gesture.crop_rect().is_some();
        for event in &events {
            if let Some(action) =
                shortcuts::route(event, &modifiers, text_field, shape_drawing, crop_pending)
            {
                if matches!(action, Action::PasteAsNewTab | Action::PasteIntoTab) {
                    pasted = true;
                }
                actions.push(action);
            }
        }

        // The paste egui never reported: an image-only clipboard produces no
        // event at all, so the Ctrl+V has to be noticed from the key state and
        // only on the frame the key goes down (R04). Shift tells the two
        // shortcuts apart (NEXT_STEPS step 11). The window has to have focus,
        // otherwise Ctrl+V in another program would paste here.
        let paste_now = !text_field && focused && paste_held;
        if paste_now && !self.paste_held && !pasted {
            actions.push(if shift_held {
                Action::PasteIntoTab
            } else {
                Action::PasteAsNewTab
            });
        }
        self.paste_held = paste_now;

        for action in actions {
            self.run(action);
        }
    }

    /// The Annotation menu's "Stroke Colour…" and "Fill Colour…" picker
    /// (SPEC 8.3). A small window holding the same picker the properties panel's
    /// colour button opens.
    fn colour_window(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let Some(field) = self.colour_picker else {
            return;
        };
        let (label, current) = self.current_colour(field);
        let mut chosen = current;
        let mut open = true;
        egui::Window::new(format!("{label} colour"))
            .collapsible(false)
            .resizable(false)
            .default_width(320.0)
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                let before = chosen;
                crate::panels::colour::panel(ui, label, &mut chosen, &mut self.recent_colours);
                if chosen != before {
                    // The same write path the panel uses.
                    actions.push(Action::Style(match field {
                        ColourField::Stroke => StyleProperty::StrokeColour(chosen),
                        ColourField::Fill => StyleProperty::Fill(Some(chosen)),
                        ColourField::Text => StyleProperty::TextColour(chosen),
                        ColourField::TextBackground => StyleProperty::TextBackground(Some(chosen)),
                        ColourField::TextOutline => StyleProperty::TextOutlineColour(chosen),
                        ColourField::Number => StyleProperty::NumberFill(chosen),
                        ColourField::Highlight => StyleProperty::HighlightColour(chosen),
                    }));
                }
            });
        if !open {
            self.colour_picker = None;
            // M4: a picker interaction that ended because the window closed
            // still leaves its one colour in the recent list.
            crate::panels::colour::flush_pending(ui.ctx(), label, &mut self.recent_colours);
        }
    }

    /// The Annotation menu's "Font…" row: a small window with the installed
    /// families, searchable (SPEC 7.5 and 8.3).
    fn font_window(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        if !self.font_picker {
            return;
        }
        let families: Vec<String> = self.fonts.families().to_vec();
        let current = self.current_style().font_family;
        let mut chosen = current.clone();
        let mut open = true;
        egui::Window::new("Font")
            .collapsible(false)
            .resizable(false)
            .default_width(260.0)
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.label(format!("Current: {current}"));
                ui.label("Search");
                let mut filter = ui.ctx().data_mut(|data| {
                    data.get_temp::<String>(egui::Id::new("font_window_filter"))
                        .unwrap_or_default()
                });
                ui.add(
                    egui::TextEdit::singleline(&mut filter)
                        .hint_text("Type to filter")
                        .desired_width(240.0),
                );
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .show(ui, |ui| {
                        let needle = filter.to_lowercase();
                        let mut shown = 0usize;
                        for family in &families {
                            if !needle.is_empty() && !family.to_lowercase().contains(&needle) {
                                continue;
                            }
                            shown += 1;
                            if ui.selectable_label(family == &chosen, family).clicked() {
                                chosen = family.clone();
                            }
                        }
                        if shown == 0 {
                            ui.label("No font matches.");
                        }
                    });
                ui.ctx().data_mut(|data| {
                    data.insert_temp(egui::Id::new("font_window_filter"), filter);
                });
            });
        if chosen != current {
            actions.push(Action::Style(StyleProperty::FontFamily(chosen)));
        }
        if !open {
            self.font_picker = false;
        }
    }

    /// The Annotation menu's "Next Number…" row: a small window with the
    /// document's next number (SPEC 7.6).
    fn next_number_window(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        if !self.next_number_editor {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            self.next_number_editor = false;
            return;
        };
        let current = tab.document.next_number;
        let mut value = current;
        let mut open = true;
        egui::Window::new("Next number")
            .collapsible(false)
            .resizable(false)
            .default_width(240.0)
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.label("The value the next Number badge gets.");
                ui.horizontal(|ui| {
                    ui.label("Next number");
                    ui.add(
                        egui::DragValue::new(&mut value)
                            .range(1..=u32::MAX)
                            .speed(1.0),
                    );
                });
            });
        if value != current {
            actions.push(Action::SetNextNumber(value));
        }
        if !open {
            self.next_number_editor = false;
        }
    }

    /// The Annotation menu's "Custom…" rows: a small window with a numeric
    /// field (SPEC 8.3). A change goes through the same [`Action::Style`] path
    /// the menu and the panel use, so undo and defaults behave identically.
    fn custom_window(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let Some(field) = self.custom_editor else {
            return;
        };
        let style = self.current_style();
        let mut value = field.value_of(&style);
        let original = value;
        let mut open = true;
        egui::Window::new(format!("Custom {}", field.label()))
            .collapsible(false)
            .resizable(false)
            .default_width(240.0)
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.label(format!("{} in image pixels", field.label()));
                ui.add(
                    egui::DragValue::new(&mut value)
                        .range(field.range())
                        .speed(0.25)
                        .suffix(" px"),
                );
            });
        if value != original {
            actions.push(Action::Style(field.property(value)));
        }
        if !open {
            self.custom_editor = None;
        }
    }

    /// The style the Annotation menu's colour and custom windows are editing:
    /// the selection's own, or the active tool's default (SPEC 7.7).
    fn current_style(&self) -> ToolStyle {
        let default = ToolStyle::default();
        let from_selection = self.active_ref().and_then(|tab| {
            let id = *tab.selection().first()?;
            properties::style_of(tab.document.find(id)?)
        });
        from_selection
            .or_else(|| self.styles.get(self.tool).cloned())
            .unwrap_or(default)
    }

    /// The colour the picker is editing: the selection's own, or the active
    /// tool's default (SPEC 7.7).
    fn current_colour(&self, field: ColourField) -> (&'static str, Rgba) {
        let style = self.current_style();
        match field {
            ColourField::Stroke => ("Stroke", style.stroke_color),
            ColourField::Fill => ("Fill", style.fill.unwrap_or([255, 255, 255, 255])),
            ColourField::Text => ("Text", style.text_color),
            ColourField::TextBackground => (
                "Background",
                style.text_background.unwrap_or([0, 0, 255, 255]),
            ),
            ColourField::TextOutline => (
                "Outline",
                style
                    .text_outline
                    .map_or(snp_ligase_core::model::defaults::STROKE_COLOR, |stroke| {
                        stroke.color
                    }),
            ),
            ColourField::Number => ("Colour", style.number_fill),
            ColourField::Highlight => ("Colour", style.highlight_color),
        }
    }

    /// Moves to the next or previous tab (SPEC 7.1: Ctrl+Tab).
    fn switch_tab(&mut self, direction: i32) {
        if self.tabs.len() < 2 {
            return;
        }
        let count = self.tabs.len() as i32;
        let next = (self.active as i32 + direction).rem_euclid(count);
        self.set_active(next as usize);
    }
}

/// The native Save As dialog, with SPEC 9's default name and folder.
///
/// The filters are one per format **this build can write** (SPEC section 9:
/// formats whose feature is disabled do not appear), the default format's
/// filter first. A name with no extension — or one this build cannot write —
/// gets the default format's extension, so the bytes always match the name
/// (R11).
fn save_dialog(settings: &Settings, existing: Option<&Path>) -> Option<PathBuf> {
    let default_format = settings.default_format();
    let mut dialog = rfd::FileDialog::new().set_title("Save image");
    let mut file_name = String::new();
    if let Some(path) = existing {
        if let Some(folder) = path.parent() {
            dialog = dialog.set_directory(folder);
        }
        if let Some(name) = path.file_name() {
            file_name = name.to_string_lossy().into_owned();
        }
    } else {
        let now = chrono::Local::now();
        let (folder, warning) = settings.resolved_save_folder_at(now);
        if let Some(warning) = warning {
            tracing::warn!("{warning}");
        }
        // A `%Y-%m` folder that has not been used yet does not exist, and
        // creating folders just to open a dialog is not wanted: the dialog
        // starts in its nearest existing parent (9c).
        dialog = dialog.set_directory(crate::settings::existing_start_folder(&folder));
        // SPEC 9: the default template is `snap_%Y%m%d-%H%M%S` and PNG is the
        // default format.
        file_name = suggested_save_name(settings, &folder, now);
    }
    file_name = ensure_extension(&file_name, default_format);
    dialog = dialog.set_file_name(file_name);

    // The default format's filter goes first, so it is the one the dialog
    // starts on.
    let mut formats: Vec<codec::Format> = vec![default_format];
    formats.extend(
        codec::Format::ALL
            .iter()
            .copied()
            .filter(|format| *format != default_format),
    );
    for format in formats {
        dialog = dialog.add_filter(format.label(), format.extensions());
    }
    dialog.save_file()
}

/// The name the Save As dialog suggests for a file that does not exist yet
/// (SPEC 9): the template's name in the default format, numbered until it is
/// free (H1).
///
/// The dialog still asks before overwriting anything; the point is that the
/// suggestion is not the file that is already there, which is what let a second
/// auto-save — or a Save As on a full folder — silently replace the first.
#[must_use]
fn suggested_save_name(
    settings: &Settings,
    folder: &Path,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    let (name, warning) = settings.file_name(now);
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    }
    free_file_name(folder, &name, settings.default_format().extension())
}

/// A file name whose extension is one this build can write.
///
/// An extension this build cannot write is **replaced** rather than appended
/// to, so typing `shot.gif` in the dialog gives `shot.png`, not `shot.gif.png`.
#[must_use]
fn ensure_extension(name: &str, default_format: codec::Format) -> String {
    match Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some(extension) if codec::Format::from_extension(extension).is_some() => name.to_owned(),
        Some(_) => {
            let stem = Path::new(name).file_stem().map_or_else(
                || name.to_owned(),
                |stem| stem.to_string_lossy().into_owned(),
            );
            format!("{stem}.{}", default_format.extension())
        }
        None => format!("{name}.{}", default_format.extension()),
    }
}

/// A file name that is not taken in `folder`: `name.ext`, then `name-1.ext`,
/// `name-2.ext`, … (H1).
///
/// Auto-save calls this so it can never replace a file that is already there.
/// The Save As dialog uses it for the name it suggests — the dialog still asks
/// before overwriting, but the default suggestion is not the file that is
/// already on disk.
#[must_use]
fn free_file_name(folder: &Path, name: &str, extension: &str) -> String {
    free_file_name_with(name, extension, |candidate| folder.join(candidate).exists())
}

/// How many numbers [`free_file_name_with`] tries before giving up.
///
/// Reaching this means a thousand files share a name, which no template does
/// in practice; the fallback name is returned rather than looping forever.
const FREE_NAME_ATTEMPTS: u32 = 1000;

/// The numbering half of [`free_file_name`], with "is this name taken?" passed
/// in so the choice itself is a pure function with a table test.
#[must_use]
fn free_file_name_with(name: &str, extension: &str, taken: impl Fn(&str) -> bool) -> String {
    for attempt in 0..FREE_NAME_ATTEMPTS {
        let candidate = if attempt == 0 {
            format!("{name}.{extension}")
        } else {
            format!("{name}-{attempt}.{extension}")
        };
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{name}-{FREE_NAME_ATTEMPTS}.{extension}")
}

/// Removes a text annotation and any selection of it (SPEC 7.5).
fn remove_text(tab: &mut Tab, id: u64) {
    tab.document
        .annotations
        .retain(|annotation| annotation.id != id);
    tab.selection.retain(|selected| *selected != id);
    tab.forget_deleted();
}

/// Whether committing a text session hands the pointer back to the Select tool
/// (SPEC section 10's "Switch to Select after drawing"; NEXT_STEPS step 9b).
///
/// `created` is the session's own flag: `true` when this session is the one
/// that placed the annotation, `false` when it re-opened an editor for a text
/// that was already there. Only a session that **drew** something new switches,
/// and only when the setting is on — a caller (cancel, an empty commit that
/// deletes the annotation) does not ask, so the rule is one line and
/// table-testable.
#[must_use]
fn switches_after_text_session(setting: bool, created: bool) -> bool {
    setting && created
}

/// What the text editor's keys mean this frame (SPEC 7.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorKeys {
    /// Nothing happened: keep editing.
    None,
    /// Ctrl+Enter, or a click outside: keep the text.
    Commit,
    /// Esc: put the annotation back.
    Cancel,
    /// A click elsewhere than the canvas took the focus off the editor: keep
    /// the session and ask for the focus back (F5).
    KeepEditing,
}

/// The text editor's commit/cancel decision, as a pure function.
///
/// `had_focus` guards the `lost_focus` case: on the frame the editor opens it
/// has not been given focus yet, and a `lost_focus` that never had focus would
/// commit an empty annotation the moment it appeared.
///
/// `canvas_press` says where the click that took the focus went (F5): a click
/// on the canvas is SPEC 7.5's "clicking outside commits"; anything else — a
/// properties panel control, a menu — must leave the session open. A canvas
/// press commits the session before the tools see it (F6), so the editor never
/// *reads* that case in the running app; the rule lives here anyway, in one
/// place, with a test.
#[must_use]
fn editor_keys(
    ctrl_enter: bool,
    escape: bool,
    lost_focus: bool,
    had_focus: bool,
    canvas_press: bool,
) -> EditorKeys {
    if escape {
        EditorKeys::Cancel
    } else if ctrl_enter {
        EditorKeys::Commit
    } else if had_focus && lost_focus {
        if canvas_press {
            EditorKeys::Commit
        } else {
            EditorKeys::KeepEditing
        }
    } else {
        EditorKeys::None
    }
}

/// Whether an action ends an open text editing session (SPEC 7.5).
///
/// Everything that would move the ground under the editor — undoing, closing or
/// switching tabs, opening a file, cropping — commits the session first.
/// Changing a style property is not in this list: SPEC 7.5 wants the properties
/// panel to work while the editor is open.
#[must_use]
fn ends_text_session(action: &Action) -> bool {
    matches!(
        action,
        Action::Undo
            | Action::Redo
            | Action::Open
            | Action::PasteAsNewTab
            | Action::PasteIntoTab
            | Action::CloseTab(_)
            | Action::CloseAllTabs
            | Action::Exit
            | Action::SelectTab(_)
            | Action::SwitchTab(_)
            | Action::Capture(_)
            | Action::ApplyCrop
            | Action::Save
            | Action::SaveAs
            | Action::SaveAll
            | Action::Tool(_)
    )
}

/// Whether an action closes the Settings dialog, and so ends a hotkey
/// recording before it runs (NEXT_STEPS step 7). The recording guard would
/// re-register on `Drop` anyway; ending it here makes the moment deliberate
/// and logged.
///
/// "Test" is in the list as well: it ends by re-registering the stored
/// bindings, so without this a recording would go on with the global keys live
/// and the next press would start a capture instead of being recorded
/// (carry-over from the Step 7 review).
#[must_use]
fn ends_hotkey_recording(action: &Action) -> bool {
    matches!(
        action,
        Action::ApplySettings | Action::CancelSettings | Action::TestHotkeys
    )
}

/// What the status bar calls a property, so the panel and the menu say the same
/// thing.
fn property_name(property: &StyleProperty) -> &'static str {
    match property {
        StyleProperty::StrokeColour(_) => "Stroke colour",
        StyleProperty::StrokeWidth(_) => "Stroke width",
        StyleProperty::Fill(_) => "Fill",
        StyleProperty::CornerRadius(_) => "Corner radius",
        StyleProperty::Shadow(_) => "Shadow",
        StyleProperty::ArrowEnds(_) => "Arrow ends",
        StyleProperty::Curved(_) => "Line style",
        StyleProperty::TextColour(_) => "Text colour",
        StyleProperty::TextBackground(_) => "Text background",
        StyleProperty::TextOutline(_) => "Text outline",
        StyleProperty::TextOutlineColour(_) => "Text outline colour",
        StyleProperty::TextOutlineWidth(_) => "Text outline width",
        StyleProperty::FontFamily(_) => "Font",
        StyleProperty::FontSize(_) => "Text size",
        StyleProperty::Bold(_) => "Bold",
        StyleProperty::Italic(_) => "Italic",
        StyleProperty::NumberFill(_) => "Colour",
        StyleProperty::NumberSize(_) => "Size",
        StyleProperty::NumberValue(_) => "Value",
        StyleProperty::HighlightColour(_) => "Colour",
        StyleProperty::BlurRadius(_) => "Blur strength",
    }
}

/// The status-bar line for a property that went into the tool's defaults, so the
/// words match the menu row it came from (SPEC 8.1).
fn property_label(property: &StyleProperty, tool: Tool) -> String {
    let value = match property {
        StyleProperty::StrokeColour(colour) => {
            format!("#{}", crate::panels::colour::to_hex(*colour))
        }
        StyleProperty::StrokeWidth(width) => format!("{width} px"),
        StyleProperty::Fill(fill) => match fill {
            Some(_) => "solid".to_owned(),
            None => "none".to_owned(),
        },
        StyleProperty::CornerRadius(radius) => format!("{radius} px"),
        StyleProperty::Shadow(shadow) => {
            if *shadow {
                "on".to_owned()
            } else {
                "off".to_owned()
            }
        }
        StyleProperty::ArrowEnds(ends) => ends.label().to_owned(),
        StyleProperty::Curved(curved) => {
            if *curved {
                "curved".to_owned()
            } else {
                "straight segments".to_owned()
            }
        }
        StyleProperty::TextColour(colour) => {
            format!("#{}", crate::panels::colour::to_hex(*colour))
        }
        StyleProperty::TextBackground(background) => match background {
            Some(_) => "solid".to_owned(),
            None => "none".to_owned(),
        },
        StyleProperty::TextOutline(outline) => match outline {
            Some(_) => "solid".to_owned(),
            None => "none".to_owned(),
        },
        StyleProperty::TextOutlineColour(colour) => {
            format!("#{}", crate::panels::colour::to_hex(*colour))
        }
        StyleProperty::TextOutlineWidth(width) => format!("{width} px"),
        StyleProperty::FontFamily(family) => family.clone(),
        StyleProperty::FontSize(size) => format!("{size} px"),
        StyleProperty::Bold(bold) => {
            if *bold {
                "on".to_owned()
            } else {
                "off".to_owned()
            }
        }
        StyleProperty::Italic(italic) => {
            if *italic {
                "on".to_owned()
            } else {
                "off".to_owned()
            }
        }
        StyleProperty::NumberFill(colour) => {
            format!("#{}", crate::panels::colour::to_hex(*colour))
        }
        StyleProperty::NumberSize(radius) => format!("{radius} px"),
        StyleProperty::NumberValue(value) => value.to_string(),
        StyleProperty::HighlightColour(colour) => {
            format!("#{}", crate::panels::colour::to_hex(*colour))
        }
        StyleProperty::BlurRadius(radius) => properties::blur_strength_label(*radius),
    };
    format!(
        "{} = {value} for the {} tool.",
        property_name(property),
        tool.name()
    )
}

/// Loads an image from disk (SPEC 7.1).
fn load_image(path: &Path) -> Result<Document, String> {
    let bytes = std::fs::read(path).map_err(|err| err.to_string())?;
    let image = codec::decode(&bytes).map_err(|err| err.to_string())?;
    Ok(Document::new(image))
}

/// `HH:MM:SS` for a capture's tab title (SPEC 7.1).
fn clock_now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// The top-left an image of `size` gets when its centre goes at `centre`, on
/// whole pixels (NEXT_STEPS step 11).
///
/// Whole pixels keep a pasted image 1:1: it is never resampled, so what the
/// user sees is exactly what is saved.
#[must_use]
fn image_position_for_centre(centre: Point, size: (u32, u32)) -> Point {
    Point::new(
        (centre.x - size.0 as f32 / 2.0).round(),
        (centre.y - size.1 as f32 / 2.0).round(),
    )
}

/// Where the pointer is, in image pixels, for the status bar.
impl std::fmt::Debug for SnpLigaseApp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnpLigaseApp")
            .field("tabs", &self.tabs.len())
            .field("active", &self.active)
            .field("tool", &self.tool)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbaImage;
    use snp_ligase_core::geom::Rect;
    use snp_ligase_core::model::defaults;
    use snp_ligase_core::model::{Kind, RectA, Stroke};

    fn app() -> SnpLigaseApp {
        SnpLigaseApp::new(std::env::temp_dir(), Vec::new())
    }

    fn app_with_rects(count: usize) -> SnpLigaseApp {
        let mut app = app();
        for index in 0..count {
            // `from_path` makes the tab clean, which is what a just-opened file
            // is: a capture or a paste has no file and counts as unsaved (R22).
            let path = std::env::temp_dir().join(format!("snp-ligase-rework-{index}.png"));
            std::fs::write(&path, b"not really a png").expect("write should work");
            let tab = Tab::from_path(Document::new(RgbaImage::new(200, 200)), path.clone());
            app.tabs.push(tab);
        }
        app.active = 0;
        app
    }

    /// Tabs with no file yet, the state a fresh capture is in.
    fn app_with_unsaved(count: usize) -> SnpLigaseApp {
        let mut app = app();
        for index in 0..count {
            app.tabs.push(Tab::new(
                Document::new(RgbaImage::new(200, 200)),
                format!("tab {index}"),
            ));
        }
        app.active = 0;
        app
    }

    /// The shell as it starts when the tray icon could not be installed (G3):
    /// there is no tray, so the X must exit instead of hiding the window.
    fn app_without_tray() -> SnpLigaseApp {
        let mut app = app();
        app.tray_installed = false;
        app.close_hides = false;
        app
    }

    /// Runs one egui pass whose input reports a close request — the way the
    /// backend reports the window's X, and, with `exit` set, the app's own
    /// `ViewportCommand::Close` — and returns the commands the frame sent back.
    fn run_close_request(app: &mut SnpLigaseApp) -> Vec<egui::ViewportCommand> {
        let mut input = egui::RawInput::default();
        input.viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                events: vec![egui::ViewportEvent::Close],
                ..Default::default()
            },
        );
        let context = egui::Context::default();
        let mut output = context.run_ui(input, |ui| app.handle_close_request(ui));
        let commands = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        // The textures a pass produces are never uploaded in a test, and
        // epaint insists they are handled before the output is dropped.
        output.textures_delta.clear();
        commands
    }

    /// Whether one of the commands a frame sent is `wanted`.
    fn sent(commands: &[egui::ViewportCommand], wanted: &egui::ViewportCommand) -> bool {
        commands.iter().any(|command| command == wanted)
    }

    fn dirty_rectangle(tab: &mut Tab) {
        let id = tab.document.push(Kind::Rect(RectA {
            rect: Rect::new(10.0, 10.0, 20.0, 20.0),
            stroke: Some(Stroke {
                color: [255, 0, 0, 255],
                width: 2.0,
            }),
            fill: None,
            corner_radius: 0.0,
        }));
        let _ = id;
    }

    /// The text of one annotation, for the session tests.
    fn text_of(tab: &Tab, id: u64) -> String {
        match &tab.document.find(id).expect("the annotation").kind {
            Kind::Text(text) => text.text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// A text annotation at (10, 10), Inter 28: the state the Text tool leaves
    /// behind, so hit testing and layout work with the bundled font.
    fn push_text(tab: &mut Tab, text: &str) -> u64 {
        tab.document.push(Kind::Text(snp_ligase_core::model::TextA {
            pos: Point::new(10.0, 10.0),
            text: text.to_owned(),
            font_family: "Inter".to_owned(),
            size: 28.0,
            bold: false,
            italic: false,
            color: [0, 0, 0, 255],
            background: None,
            padding: 6.0,
            outline: None,
        }))
    }

    /// Overwrites one text annotation's string, as the editor's live write
    /// does every frame.
    fn write_text(tab: &mut Tab, id: u64, text: &str) {
        if let Some(annotation) = tab.document.find_mut(id)
            && let Kind::Text(annotation_text) = &mut annotation.kind
        {
            annotation_text.text = text.to_owned();
        }
    }

    /// Opens a session the way the Text tool does: the snapshot is the document
    /// before the annotation existed, and `created` marks it as new.
    fn start_text_session(app: &mut SnpLigaseApp, index: usize, text: &str) -> u64 {
        app.active = index;
        let tab = &mut app.tabs[index];
        let snapshot = tab.document.clone();
        let id = push_text(tab, "");
        tab.select_only(Some(id));
        app.begin_text_session(id, true, snapshot);
        if let Some(session) = app.text_session.as_mut() {
            session.text = text.to_owned();
        }
        // The live write, as the editor does every frame.
        write_text(&mut app.tabs[index], id, text);
        id
    }

    /// Opens a session on a text that is already there, the way a double-click
    /// with the Select tool does (SPEC 7.5).
    fn start_re_edit_session(app: &mut SnpLigaseApp, index: usize, annotation: u64, text: &str) {
        app.active = index;
        let snapshot = app.tabs[index].document.clone();
        app.tabs[index].select_only(Some(annotation));
        app.begin_text_session(annotation, false, snapshot);
        if let Some(session) = app.text_session.as_mut() {
            session.text = text.to_owned();
        }
        write_text(&mut app.tabs[index], annotation, text);
    }

    /// A press frame on the canvas at `at`, in image pixels at 100% zoom.
    fn press_at(at: Point) -> CanvasInput {
        CanvasInput {
            pointer: Some(egui::pos2(at.x, at.y)),
            image_pointer: Some(at),
            primary_pressed: true,
            primary_down: true,
            ..CanvasInput::default()
        }
    }

    /// A frame where the pointer moved to `at` with the button held.
    fn drag_to(at: Point) -> CanvasInput {
        CanvasInput {
            pointer: Some(egui::pos2(at.x, at.y)),
            image_pointer: Some(at),
            primary_down: true,
            ..CanvasInput::default()
        }
    }

    /// A Freehand move frame: the pointer at `at`, plus every position egui
    /// reported this frame (NEXT_STEPS step 14b: a fast stroke arrives as
    /// several events in one frame).
    fn stroke_to(at: Point, samples: &[Point]) -> CanvasInput {
        CanvasInput {
            pointer_samples: samples.to_vec(),
            ..drag_to(at)
        }
    }

    /// A release frame on the canvas.
    fn release_at(at: Point) -> CanvasInput {
        CanvasInput {
            pointer: Some(egui::pos2(at.x, at.y)),
            image_pointer: Some(at),
            primary_released: true,
            ..CanvasInput::default()
        }
    }

    /// The release frame of the second click of a double-click.
    fn double_click_release_at(at: Point) -> CanvasInput {
        CanvasInput {
            double_clicked: true,
            ..release_at(at)
        }
    }

    /// One whole click on the canvas: press, then release.
    fn click_canvas(app: &mut SnpLigaseApp, at: Point) {
        let _ = app.canvas_input(&press_at(at));
        let _ = app.canvas_input(&release_at(at));
    }

    /// Waits for the save worker to answer (H6).
    ///
    /// The running app polls it once per frame; a test cannot wait for frames,
    /// so it polls the same queue and sleeps.
    fn wait_for_saves(app: &mut SnpLigaseApp) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !app.saving.is_empty() {
            app.drain_saves();
            assert!(
                std::time::Instant::now() < deadline,
                "the save worker never answered"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn a_text_session_is_exactly_one_undo_step() {
        // SPEC 7.5: one editing session is one undo step, and it must not be
        // one step per keystroke — the editor writes into the document on every
        // change, so the snapshot is taken once, by the session.
        let mut app = app_with_rects(1);
        let before = app.tabs[0].document.clone();
        let id = start_text_session(&mut app, 0, "Hello");
        assert_eq!(text_of(&app.tabs[0], id), "Hello");

        app.commit_text_session();
        assert!(app.text_session.is_none());
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            1,
            "the session was not exactly one undo step"
        );
        assert!(app.tabs[0].undo());
        assert_eq!(
            app.tabs[0].document, before,
            "undo did not remove the whole annotation"
        );
    }

    #[test]
    fn an_unchanged_text_session_costs_no_undo_step() {
        let mut app = app_with_rects(1);
        // Open a session for an existing annotation and change nothing.
        let tab = &mut app.tabs[0];
        let id = tab.document.push(Kind::Text(snp_ligase_core::model::TextA {
            pos: Point::new(10.0, 10.0),
            text: "Already there".to_owned(),
            font_family: "Inter".to_owned(),
            size: 28.0,
            bold: false,
            italic: false,
            color: [0, 0, 0, 255],
            background: None,
            padding: 6.0,
            outline: None,
        }));
        let snapshot = tab.document.clone();
        app.begin_text_session(id, false, snapshot);
        app.commit_text_session();

        assert_eq!(text_of(&app.tabs[0], id), "Already there");
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            0,
            "a session that changed nothing took an undo step"
        );
    }

    #[test]
    fn cancelling_a_new_text_session_removes_the_annotation() {
        let mut app = app_with_rects(1);
        let id = start_text_session(&mut app, 0, "Gone");
        app.cancel_text_session();

        assert!(app.text_session.is_none());
        assert!(
            app.tabs[0].document.find(id).is_none(),
            "the annotation survived the cancel"
        );
        assert_eq!(app.tabs[0].undo_depth_for_test(), 0, "cancel took a step");
    }

    #[test]
    fn cancelling_an_existing_text_session_restores_the_text() {
        let mut app = app_with_rects(1);
        let tab = &mut app.tabs[0];
        let id = tab.document.push(Kind::Text(snp_ligase_core::model::TextA {
            pos: Point::new(10.0, 10.0),
            text: "Original".to_owned(),
            font_family: "Inter".to_owned(),
            size: 28.0,
            bold: false,
            italic: false,
            color: [0, 0, 0, 255],
            background: None,
            padding: 6.0,
            outline: None,
        }));
        let snapshot = tab.document.clone();
        app.begin_text_session(id, false, snapshot);
        if let Some(session) = app.text_session.as_mut() {
            session.text = "Changed".to_owned();
        }
        if let Some(annotation) = app.tabs[0].document.find_mut(id)
            && let Kind::Text(text) = &mut annotation.kind
        {
            text.text = "Changed".to_owned();
        }
        app.cancel_text_session();

        assert_eq!(text_of(&app.tabs[0], id), "Original");
        assert_eq!(app.tabs[0].undo_depth_for_test(), 0, "cancel took a step");
    }

    #[test]
    fn committing_empty_text_deletes_the_annotation() {
        // SPEC 7.5: committing with empty text deletes the annotation.
        let mut app = app_with_rects(1);
        let tab = &mut app.tabs[0];
        let id = tab.document.push(Kind::Text(snp_ligase_core::model::TextA {
            pos: Point::new(10.0, 10.0),
            text: "Original".to_owned(),
            font_family: "Inter".to_owned(),
            size: 28.0,
            bold: false,
            italic: false,
            color: [0, 0, 0, 255],
            background: None,
            padding: 6.0,
            outline: None,
        }));
        let snapshot = tab.document.clone();
        app.begin_text_session(id, false, snapshot);
        if let Some(session) = app.text_session.as_mut() {
            session.text = String::new();
        }
        app.commit_text_session();

        assert!(
            app.tabs[0].document.find(id).is_none(),
            "it was not deleted"
        );
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            1,
            "emptying a text is a delete and should be undoable"
        );
        assert!(app.tabs[0].undo());
        assert_eq!(text_of(&app.tabs[0], id), "Original");
    }

    #[test]
    fn the_text_editor_takes_focus_when_it_opens() {
        // Found by driving the real window: the editor opened but never took
        // keyboard focus, so the next keystroke became a tool shortcut instead
        // of text (SPEC 7.5 wants the field focused).
        let mut app = app_with_rects(1);
        let _id = start_text_session(&mut app, 0, "");
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut SnpLigaseApp| app.frame_ui(ui), app);
        harness.run();

        use egui_kittest::kittest::Queryable as _;
        let field = harness
            .query_all_by_role(egui::accesskit::Role::MultilineTextInput)
            .next()
            .expect("the text editor did not open");
        assert!(field.is_focused(), "the text editor did not take focus");
        assert!(
            harness.ctx.text_edit_focused(),
            "the app does not think a text field is focused"
        );
    }

    #[test]
    fn a_fresh_app_has_no_text_field_focus() {
        // Found by driving the real window: single-key tool shortcuts did
        // nothing. They are routed only when no text field has focus (SPEC 7.3),
        // so a widget that is focused but is *not* a text field must not block
        // them. This checks the state at startup and after a canvas click.
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, app: &mut SnpLigaseApp| app.frame_ui(ui),
            app_with_rects(1),
        );
        harness.run();
        assert!(
            !harness.ctx.text_edit_focused(),
            "a text field has focus at startup"
        );

        harness.hover_at(egui::pos2(400.0, 400.0));
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(400.0, 400.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(400.0, 400.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run();
        assert!(
            !harness.ctx.text_edit_focused(),
            "clicking the canvas left a text field focused"
        );
    }

    #[test]
    fn the_editor_keys_decide_commit_cancel_and_keeping_the_session() {
        // SPEC 7.5: Ctrl+Enter or clicking outside commits, Esc cancels.
        assert_eq!(
            editor_keys(false, false, false, true, false),
            EditorKeys::None
        );
        assert_eq!(
            editor_keys(true, false, false, true, false),
            EditorKeys::Commit
        );
        assert_eq!(
            editor_keys(false, false, true, true, true),
            EditorKeys::Commit,
            "a click on the canvas is SPEC 7.5's 'clicking outside'"
        );
        // F5: a click that landed anywhere else — a properties panel control, a
        // menu — keeps the session open instead of committing it.
        assert_eq!(
            editor_keys(false, false, true, true, false),
            EditorKeys::KeepEditing
        );
        assert_eq!(
            editor_keys(false, true, false, true, false),
            EditorKeys::Cancel
        );
        // Esc wins over a commit in the same frame.
        assert_eq!(
            editor_keys(true, true, false, true, true),
            EditorKeys::Cancel
        );
        // A `lost_focus` before the editor ever had focus is not a commit: the
        // window would otherwise commit itself shut the moment it opens.
        assert_eq!(
            editor_keys(false, false, true, false, true),
            EditorKeys::None
        );
    }

    /// F4: a text session belongs to its tab. Annotation ids are per document,
    /// so a session that followed a tab switch could type into — or delete — an
    /// annotation that happens to share its id in the other tab, and push its
    /// snapshot onto that tab's undo stack.
    #[test]
    fn a_text_session_ends_when_another_tab_becomes_active() {
        let mut app = app_with_rects(1);
        let id = start_text_session(&mut app, 0, "Hello");

        // A new tab arrives while the session is open: a dropped file, or the
        // M5 controller's capture.
        app.push_tab(Tab::new(
            Document::new(RgbaImage::new(50, 50)),
            "new".to_owned(),
        ));

        assert!(app.text_session.is_none(), "the session crossed tabs");
        assert_eq!(app.active, 1, "the new tab is not active");
        // The first tab got exactly its one undo step, on its own stack...
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            1,
            "the session was not exactly one undo step"
        );
        assert_eq!(text_of(&app.tabs[0], id), "Hello");
        // ...and the new tab is untouched: no undo step, no annotation.
        assert!(
            !app.tabs[1].can_undo(),
            "the session's snapshot landed on the new tab's undo stack"
        );
        assert!(app.tabs[1].document.annotations.is_empty());
    }

    /// F4: closing a tab shifts every index after it, so a session that is tied
    /// to a tab index must not survive the change.
    #[test]
    fn a_text_session_ends_when_a_tab_closes() {
        let mut app = app_with_rects(2);
        // The session is on the second tab, and the *first* one closes.
        let id = start_text_session(&mut app, 1, "On the second tab");
        app.close_tabs(&[0]);

        assert!(app.text_session.is_none(), "the session survived the close");
        assert_eq!(app.active, 0);
        assert_eq!(text_of(&app.tabs[0], id), "On the second tab");
        assert_eq!(app.tabs[0].undo_depth_for_test(), 1);
    }

    /// F5 (SPEC 7.5): the properties panel is usable while the editor is open.
    /// egui surrenders a focused widget's focus on *any* click that does not
    /// land on it, so a click on "Bold" used to commit the session.
    #[test]
    fn clicking_the_properties_panel_keeps_the_text_editor_open() {
        use egui_kittest::kittest::Queryable as _;

        let mut app = app_with_rects(1);
        let id = start_text_session(&mut app, 0, "Hi");
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut SnpLigaseApp| app.frame_ui(ui), app);
        harness.run();

        // The panel shows the selected text's controls: click "Bold".
        harness.get_by_label("Bold").click();
        harness.run();

        assert!(
            harness.state().text_session.is_some(),
            "clicking Bold closed the text editor"
        );
        let Kind::Text(text) = &harness.state().tabs[0]
            .document
            .find(id)
            .expect("the text is still there")
            .kind
        else {
            panic!("expected text");
        };
        assert!(text.bold, "the Bold click did not reach the annotation");
        assert!(
            harness.ctx.text_edit_focused(),
            "the editor did not get the keyboard back"
        );
    }

    /// F5, the other half: clicking a text field in the properties panel — the
    /// colour popup's hex field — must keep the session open *and* keep the
    /// keyboard in that field. The editor only takes its focus back when the
    /// click left nobody holding it.
    #[test]
    fn a_panel_text_field_keeps_the_keyboard_while_the_editor_is_open() {
        use egui_kittest::kittest::Queryable as _;

        let mut app = app_with_rects(1);
        let _id = start_text_session(&mut app, 0, "Hi");
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut SnpLigaseApp| app.frame_ui(ui), app);
        harness.run();

        // Open the colour popup from the properties panel, then click its hex
        // field.
        harness.get_by_label_contains("Text colour").click();
        harness.run();
        harness
            .query_all_by_role(egui::accesskit::Role::TextInput)
            .next()
            .expect("the colour popup has no hex field")
            .click();
        harness.run();

        assert!(
            harness.state().text_session.is_some(),
            "clicking the hex field closed the text editor"
        );
        let field_is_focused = harness
            .query_all_by_role(egui::accesskit::Role::TextInput)
            .next()
            .expect("the colour popup has no hex field")
            .is_focused();
        assert!(
            field_is_focused,
            "the editor stole the keyboard from the hex field"
        );
    }

    /// F6: a drag does not take the focus away, so a gesture can change the
    /// document while the editor is open. The session has to commit on the
    /// press, **before** the tool changes anything: otherwise the undo steps
    /// are in the opposite order and the second Ctrl+Z brings back the
    /// half-typed text.
    #[test]
    fn a_canvas_press_commits_the_session_before_the_move_gesture() {
        let mut app = app_with_rects(1);
        // Hit testing a text needs real metrics: the bundled font is the family
        // the test text asks for.
        app.fonts = snp_ligase_core::text::Fonts::bundled();
        let id = push_text(&mut app.tabs[0], "Original");
        // Double-clicking the text opens the session, and the user types into
        // it. The Select tool stays active, as it does for a re-edit.
        start_re_edit_session(&mut app, 0, id, "Half-typed");
        app.tool = Tool::Select;

        // The user drags the half-typed text: press, move, release, then the
        // frame-end settle the app does every frame.
        let on_text = Point::new(14.0, 14.0);
        let _ = app.canvas_input(&press_at(on_text));
        let _ = app.canvas_input(&drag_to(Point::new(64.0, 44.0)));
        let _ = app.canvas_input(&release_at(Point::new(64.0, 44.0)));
        app.settle_edits();

        assert!(
            app.text_session.is_none(),
            "the press did not commit the session"
        );
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            2,
            "the session and the move are not two undo steps"
        );

        // Ctrl+Z #1: the move goes, the typed text stays.
        assert!(app.tabs[0].undo());
        assert_eq!(
            text_of(&app.tabs[0], id),
            "Half-typed",
            "the first undo brought the half-typed text back"
        );
        let Kind::Text(text) = &app.tabs[0].document.find(id).expect("the text").kind else {
            panic!("expected text");
        };
        assert_eq!(text.pos, Point::new(10.0, 10.0), "the move was not undone");

        // Ctrl+Z #2: the typing goes too.
        assert!(app.tabs[0].undo());
        assert_eq!(text_of(&app.tabs[0], id), "Original");
    }

    /// F7 (SPEC 7.5): a double-click with the Text tool on an existing text
    /// edits that text. The first click places an empty annotation and opens a
    /// session; the second press commits it (which deletes the empty
    /// annotation), and the double-click then opens the editor for the text
    /// that was really clicked.
    #[test]
    fn a_double_click_with_the_text_tool_edits_the_existing_text() {
        let mut app = app_with_rects(1);
        app.fonts = snp_ligase_core::text::Fonts::bundled();
        let existing = push_text(&mut app.tabs[0], "Existing");
        app.tool = Tool::Text;

        let at = Point::new(14.0, 14.0);
        // The real sequence: press, release, press, release+`double_clicked`.
        click_canvas(&mut app, at);
        let _ = app.canvas_input(&press_at(at));
        let _ = app.canvas_input(&double_click_release_at(at));

        let texts = app.tabs[0]
            .document
            .annotations
            .iter()
            .filter(|annotation| matches!(annotation.kind, Kind::Text(_)))
            .count();
        assert_eq!(texts, 1, "the first click left a stray empty text behind");
        let session = app.text_session.as_ref().expect("no editor opened");
        assert_eq!(session.id, existing, "the editor opened for the wrong text");
        assert!(!session.created, "the existing text is marked as new");
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            0,
            "the double click cost an undo step"
        );
        assert!(!app.tabs[0].can_undo());
    }

    /// F3: a crop dragged wholly on the grey area around the image passed the
    /// screen-size check and then clipped to 0×0 inside `edit::crop`.
    #[test]
    fn a_crop_wholly_outside_the_image_changes_nothing() {
        let mut app = app();
        app.push_tab(Tab::new(
            Document::new(RgbaImage::new(400, 400)),
            "test".to_owned(),
        ));
        app.tool = Tool::Crop;
        let before = app.tabs[0].document.clone();

        let _ = app.canvas_input(&press_at(Point::new(450.0, 450.0)));
        let _ = app.canvas_input(&drag_to(Point::new(600.0, 600.0)));
        let _ = app.canvas_input(&release_at(Point::new(600.0, 600.0)));
        app.run(Action::ApplyCrop);

        assert_eq!(app.tabs[0].document, before, "the document changed");
        assert!(!app.tabs[0].can_undo(), "it cost an undo step");
    }

    /// F3, and the "not verified" item from M4: a crop that starts outside the
    /// image keeps exactly the intersection, as one undo step.
    #[test]
    fn a_crop_partly_outside_the_image_is_clipped_to_the_intersection() {
        let mut app = app();
        app.push_tab(Tab::new(
            Document::new(RgbaImage::new(400, 400)),
            "test".to_owned(),
        ));
        app.tool = Tool::Crop;
        let before = app.tabs[0].document.clone();

        let _ = app.canvas_input(&press_at(Point::new(450.0, 450.0)));
        let _ = app.canvas_input(&drag_to(Point::new(200.0, 200.0)));
        let _ = app.canvas_input(&release_at(Point::new(200.0, 200.0)));
        app.run(Action::ApplyCrop);

        assert_eq!(
            app.tabs[0].document.base.dimensions(),
            (200, 200),
            "the crop was not clipped to the image"
        );
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            1,
            "not exactly one undo step"
        );
        assert!(app.tabs[0].undo());
        assert_eq!(
            app.tabs[0].document, before,
            "undo did not restore the crop"
        );
    }

    #[test]
    fn only_actions_that_move_the_ground_end_a_text_session() {
        // SPEC 7.5 wants the properties panel to work while the editor is open,
        // so a style change must not end the session.
        for action in [
            Action::Undo,
            Action::Redo,
            Action::Open,
            Action::CloseTab(0),
            Action::CloseAllTabs,
            Action::SelectTab(1),
            Action::SwitchTab(1),
            Action::ApplyCrop,
            Action::Save,
            Action::Tool(Tool::Select),
        ] {
            assert!(ends_text_session(&action), "{action:?} should commit");
        }
        for action in [
            Action::Style(StyleProperty::Bold(true)),
            Action::PickColour(crate::panels::ColourField::Text),
            Action::CustomValue(crate::panels::CustomField::FontSize),
            Action::CopyImage,
            Action::CommitText,
            Action::CancelText,
        ] {
            assert!(!ends_text_session(&action), "{action:?} should not commit");
        }
    }

    #[test]
    fn a_close_request_with_clean_tabs_exits_without_a_prompt() {
        // R09: the old code let both the window's X and File > Exit close the
        // window with unsaved work still open.
        let mut app = app_with_rects(1);
        app.request_exit();
        assert!(app.exit, "a clean tab should not stop the exit");
        assert!(app.pending_close.is_none(), "it asked about nothing");
    }

    #[test]
    fn a_close_request_with_dirty_tabs_asks_first() {
        let mut app = app_with_rects(1);
        dirty_rectangle(&mut app.tabs[0]);
        app.request_exit();

        assert!(!app.exit, "it exited without asking");
        let Some(PendingClose::Tabs { indices, then_exit }) = app.pending_close.clone() else {
            panic!("it did not ask");
        };
        assert_eq!(indices, vec![0]);
        assert!(then_exit, "an exit request must still exit afterwards");
    }

    #[test]
    fn file_exit_goes_through_the_same_path() {
        let mut app = app_with_rects(1);
        dirty_rectangle(&mut app.tabs[0]);
        app.run(Action::Exit);
        assert!(app.pending_close.is_some(), "File > Exit did not ask");
        assert!(!app.exit);

        // Answering "close them anyway" then really exits.
        app.confirm_close(false, &mut |_| None);
        assert!(app.tabs.is_empty(), "the tabs were not closed");
        assert!(app.exit, "the exit was lost");
    }

    #[test]
    fn an_exit_under_way_is_not_cancelled() {
        // G1: once `exit` is set, the frame sends `ViewportCommand::Close`; the
        // backend reports that back as a close request on the next frame. The
        // old handler sent CancelClose *before* it looked at `exit`, so eframe
        // kept the window open and File > Exit never ended the process.
        let mut app = app_with_rects(1);
        app.exit = true;

        let commands = run_close_request(&mut app);

        assert!(
            !sent(&commands, &egui::ViewportCommand::CancelClose),
            "the exit was cancelled instead of let through: {commands:?}"
        );
    }

    #[test]
    fn the_x_with_dirty_tabs_hides_to_the_tray_and_cancels_the_close() {
        // The X is not a quit: unsaved work stays alive in the tray, and the
        // close request itself is cancelled (R09).
        let mut app = app_with_rects(1);
        dirty_rectangle(&mut app.tabs[0]);

        let commands = run_close_request(&mut app);

        assert!(
            app.pending_close.is_none(),
            "the X asked about tabs; it only hides"
        );
        assert!(!app.exit, "the X exited with unsaved work");
        assert!(
            !app.controller.window().is_visible(),
            "the window did not hide"
        );
        assert!(
            sent(&commands, &egui::ViewportCommand::CancelClose),
            "the close request was not cancelled: {commands:?}"
        );
    }

    #[test]
    fn with_a_tray_the_x_hides() {
        // SPEC 6.1's default, unchanged: with the tray icon installed the X
        // hides the window and the tray brings it back.
        let mut app = app_with_rects(1);

        let commands = run_close_request(&mut app);

        assert!(!app.exit, "the X exited although the tray is installed");
        assert!(
            !app.controller.window().is_visible(),
            "the window did not hide"
        );
        assert!(sent(&commands, &egui::ViewportCommand::CancelClose));
        assert!(app.status.contains("tray"), "{}", app.status);
    }

    #[test]
    fn without_a_tray_the_x_exits_instead_of_hiding() {
        // G3: `close_hides` used to be true even when `Tray::install` failed,
        // so the X hid the window with no way back — a relaunch exits at the
        // single-instance mutex, which leaves an invisible process only Task
        // Manager can end.
        let mut app = app_without_tray();

        let commands = run_close_request(&mut app);

        assert!(app.exit, "the X did not exit without a tray icon");
        assert!(app.pending_close.is_none(), "a clean exit asks nothing");
        assert!(sent(&commands, &egui::ViewportCommand::CancelClose));
    }

    #[test]
    fn a_tray_quit_with_clean_tabs_exits_without_showing_the_window() {
        // G2: the tray's Quit arrives while the window is hidden. With nothing
        // unsaved there is nothing to look at, so it just leaves; the exit is
        // what G1 lets through.
        let mut app = app_with_rects(1);
        app.controller.window().hide();

        app.controller.inject_event(UiEvent::QuitRequested);
        app.drain_controller_events();

        assert!(app.exit, "the tray's Quit did not exit");
        assert!(
            !app.controller.window().is_visible(),
            "the window was shown for nothing"
        );
    }

    #[test]
    fn a_tray_quit_with_dirty_tabs_shows_the_window_for_the_prompt() {
        // G2: with unsaved tabs the prompt must not be drawn in a hidden
        // window.
        let mut app = app_with_unsaved(1);
        app.controller.window().hide();

        app.controller.inject_event(UiEvent::QuitRequested);
        app.drain_controller_events();

        assert!(app.pending_close.is_some(), "it did not ask about the tab");
        assert!(!app.exit, "it exited without asking");
        assert!(
            app.controller.window().is_visible(),
            "the prompt opened in a hidden window"
        );
    }

    #[test]
    fn a_cancelled_save_as_leaves_the_tab_open() {
        // R08: choosing Save on a tab with no file used to close it anyway.
        let mut app = app_with_unsaved(1);
        app.request_close(0);

        // The user cancels the Save As dialog.
        app.confirm_close(true, &mut |_| None);

        assert_eq!(app.tabs.len(), 1, "the tab was closed without saving");
        assert!(
            app.pending_close.is_some(),
            "the close was not kept pending for retry"
        );
        assert!(
            app.status.contains("cancelled"),
            "the status bar should say so: {}",
            app.status
        );
    }

    #[test]
    fn a_saved_save_as_closes_the_tab_and_sets_its_path() {
        let mut app = app_with_unsaved(1);
        let path = std::env::temp_dir().join("snp-ligase-rework-save.png");
        let _ = std::fs::remove_file(&path);
        app.request_close(0);

        app.confirm_close(true, &mut |_| Some(path.clone()));

        assert!(app.tabs.is_empty(), "the tab was not closed");
        assert!(app.pending_close.is_none(), "a prompt is still pending");
        assert!(path.exists(), "the file was not written");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_close_prompt_reuses_a_save_already_in_flight() {
        // M1: Ctrl+S (async) followed by the close prompt's Save used to start
        // a second, synchronous write of the same file. The prompt must wait
        // for the save it started and reuse its result instead.
        let folder = temp_folder("prompt-reuses-save");
        let path = folder.join("shot.png");
        let mut app = app_with_rects(1);
        dirty_rectangle(&mut app.tabs[0]);
        let id = app.tabs[0].id();
        app.tabs[0].set_path(path.clone());
        app.request_close(0);
        assert!(app.pending_close.is_some(), "the prompt should be up");

        // The save starts after the prompt opened (Ctrl+S is still routed).
        app.saving.insert(id, path.clone());
        app.save_worker.inject_outcome(crate::save::SaveOutcome {
            tab: id,
            path: path.clone(),
            document: app.tabs[0].document.clone(),
            auto: false,
            result: Ok(crate::save::SaveStats {
                bytes: 10,
                encode_ms: 0.5,
                write_ms: 0.5,
            }),
        });

        app.confirm_close(true, &mut |_| None);

        assert!(app.tabs.is_empty(), "the tab did not close");
        assert!(
            !path.exists(),
            "the close prompt wrote the file a second time"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_close_prompt_still_saves_after_a_failed_save_in_flight() {
        // M1: waiting for an in-flight save must not treat a failure as done —
        // the tab is still dirty and the prompt has to write it.
        let folder = temp_folder("prompt-after-failure");
        let path = folder.join("shot.png");
        let mut app = app_with_rects(1);
        dirty_rectangle(&mut app.tabs[0]);
        let id = app.tabs[0].id();
        app.tabs[0].set_path(path.clone());
        app.request_close(0);
        app.saving.insert(id, path.clone());
        app.save_worker.inject_outcome(crate::save::SaveOutcome {
            tab: id,
            path: path.clone(),
            document: app.tabs[0].document.clone(),
            auto: false,
            result: Err("the disk is full".to_owned()),
        });

        app.confirm_close(true, &mut |_| None);

        assert!(app.tabs.is_empty(), "the tab did not close");
        let bytes = std::fs::read(&path).expect("the failed save left nothing on disk");
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "not a PNG"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_close_request_waits_for_a_save_in_flight() {
        // M2: a tab that is being written is not shown the unsaved prompt; the
        // close waits for the save and closes the tab if it lands.
        let mut app = app_with_rects(1);
        dirty_rectangle(&mut app.tabs[0]);
        let id = app.tabs[0].id();
        let path = PathBuf::from("shot.png");
        app.saving.insert(id, path.clone());
        app.save_worker.inject_outcome(crate::save::SaveOutcome {
            tab: id,
            path,
            document: app.tabs[0].document.clone(),
            auto: false,
            result: Ok(crate::save::SaveStats {
                bytes: 10,
                encode_ms: 0.5,
                write_ms: 0.5,
            }),
        });

        app.request_close(0);

        assert!(
            app.tabs.is_empty(),
            "the tab did not close after its save finished"
        );
        assert!(
            app.pending_close.is_none(),
            "the prompt asked about a tab that was being saved"
        );
        assert!(app.saving.is_empty(), "the save was left in flight");
    }

    #[test]
    fn an_exit_request_waits_for_a_save_in_flight() {
        // M2: the same for File > Exit — the tab is clean once the save lands,
        // so no prompt is needed.
        let mut app = app_with_rects(1);
        dirty_rectangle(&mut app.tabs[0]);
        let id = app.tabs[0].id();
        let path = PathBuf::from("shot.png");
        app.saving.insert(id, path.clone());
        app.save_worker.inject_outcome(crate::save::SaveOutcome {
            tab: id,
            path,
            document: app.tabs[0].document.clone(),
            auto: false,
            result: Ok(crate::save::SaveStats {
                bytes: 10,
                encode_ms: 0.5,
                write_ms: 0.5,
            }),
        });

        app.request_exit();

        assert!(app.exit, "the exit did not proceed");
        assert!(
            app.pending_close.is_none(),
            "it asked about a tab that was being saved"
        );
    }

    #[test]
    fn asking_about_a_clean_tab_closes_it_without_a_prompt() {
        let mut app = app_with_rects(2);
        app.request_close(1);
        assert_eq!(app.tabs.len(), 1, "the clean tab was not closed");
        assert!(app.pending_close.is_none());
    }

    #[test]
    fn cancelling_the_prompt_closes_nothing() {
        let mut app = app_with_rects(1);
        dirty_rectangle(&mut app.tabs[0]);
        app.request_close(0);
        app.run(Action::ConfirmCancel);
        assert_eq!(app.tabs.len(), 1, "Cancel closed the tab");
        assert!(app.pending_close.is_none());
    }

    #[test]
    fn a_discarded_close_removes_the_unsaved_work() {
        let mut app = app_with_rects(1);
        dirty_rectangle(&mut app.tabs[0]);
        app.request_close(0);
        app.confirm_close(false, &mut |_| None);
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn a_tool_style_change_does_not_touch_another_tools_defaults() {
        // R07: the old code wrote the *active* tool's defaults.
        let mut app = app();
        let mut tab = Tab::new(Document::new(RgbaImage::new(200, 200)), "test".to_owned());
        let id = tab.document.push(Kind::Path(snp_ligase_core::model::PathA {
            points: vec![
                snp_ligase_core::geom::Point::new(0.0, 0.0),
                snp_ligase_core::geom::Point::new(20.0, 0.0),
            ],
            curved: true,
            stroke: defaults::stroke(),
            head_start: false,
            head_end: false,
        }));
        tab.select_only(Some(id));
        app.tabs.push(tab);

        // The Rectangle tool is active, but the selection is a Line.
        app.tool = Tool::Rectangle;
        app.run(Action::Style(StyleProperty::StrokeWidth(11.0)));

        assert!(
            (app.styles
                .get(Tool::Line)
                .expect("a line style")
                .stroke_width
                - 11.0)
                .abs()
                < 0.001,
            "the Line tool did not learn its own property"
        );
        assert!(
            (app.styles
                .get(Tool::Arrow)
                .expect("an arrow style")
                .stroke_width
                - 4.0)
                .abs()
                < 0.001,
            "the Arrow defaults were rewritten by a Line"
        );
        assert!(
            (app.styles
                .get(Tool::Rectangle)
                .expect("a rectangle style")
                .stroke_width
                - 4.0)
                .abs()
                < 0.001,
            "the Rectangle defaults were rewritten while it was only the active tool"
        );
    }

    #[test]
    fn a_picked_out_point_that_no_longer_exists_is_dropped_not_panicked_on() {
        // R10: `Delete` used to index a stale point and crash.
        let mut app = app();
        let mut tab = Tab::new(Document::new(RgbaImage::new(200, 200)), "test".to_owned());
        let id = tab.document.push(Kind::Path(snp_ligase_core::model::PathA {
            points: vec![
                snp_ligase_core::geom::Point::new(0.0, 0.0),
                snp_ligase_core::geom::Point::new(30.0, 10.0),
                snp_ligase_core::geom::Point::new(60.0, 0.0),
            ],
            curved: true,
            stroke: defaults::stroke(),
            head_start: false,
            head_end: true,
        }));
        tab.select_only(Some(id));
        app.tabs.push(tab);
        app.active_point = Some(snp_ligase_core::edit::HandleKind::Point(9));

        app.sync_active_point();
        assert!(
            app.active_point.is_none(),
            "a point that does not exist cannot stay picked out"
        );
    }

    // ---------------------------------------------------------------------
    // M6: settings (SPEC section 10)
    // ---------------------------------------------------------------------

    /// A folder no other test uses.
    fn temp_folder(name: &str) -> PathBuf {
        let folder =
            std::env::temp_dir().join(format!("snp-ligase-m6-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("the test folder should be creatable");
        folder
    }

    /// H2: when the settings file is left alone, Settings > OK cannot save it,
    /// and the status bar says so instead of claiming success.
    #[cfg(windows)]
    #[test]
    fn a_blocked_settings_file_is_reported_in_the_status_bar() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let folder = temp_folder("blocked-status");
        let path = folder.join("config.toml");
        let original = "filename_template = \"kept\"\n";
        std::fs::write(&path, original).expect("write");
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .expect("the file should be lockable");
        let (store, _) = SettingsStore::load(folder.clone());
        drop(holder);

        let mut app = SnpLigaseApp::from_shell(Shell {
            log_dir: folder.clone(),
            files: Vec::new(),
            controller: Controller::detached(),
            hotkeys: Hotkeys::none(),
            settings: store,
            warnings: Vec::new(),
            tray_installed: true,
            start_hidden: false,
        });
        app.run(Action::Settings);
        if let Some(draft) = app.settings_draft.as_mut() {
            draft.show_tool_labels = false;
        }
        app.run(Action::ApplySettings);

        assert!(
            app.status.contains("not saved"),
            "the status bar claimed success: {}",
            app.status
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            original,
            "the file was overwritten"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// H3: a settings change made inside the last debounce interval is not on
    /// disk when the user quits, so `on_exit` has to write it.
    #[test]
    fn quitting_flushes_a_pending_settings_change() {
        let mut app = app();
        let mut settings = app.settings.settings().clone();
        settings.show_tool_labels = false;
        app.settings.replace(settings);
        let path = app.settings.config_path();
        assert!(
            !path.exists(),
            "the test expected an unwritten change to start with"
        );

        app.on_exit();

        assert!(path.exists(), "on_exit left the change unwritten");
        let folder = path.parent().expect("a config folder").to_path_buf();
        let (reloaded, _) = SettingsStore::load(folder);
        assert!(
            !reloaded.settings().show_tool_labels,
            "the pending change was not the one that got written"
        );
    }

    #[test]
    fn the_tray_settings_row_opens_the_dialog() {
        // G2 opened the window; the row now has to open the dialog too.
        let mut app = app_with_rects(1);
        app.controller.inject_event(UiEvent::ShowSettings);
        app.drain_controller_events();

        assert!(
            app.settings_draft.is_some(),
            "the tray's Settings row did not open the dialog"
        );
    }

    #[test]
    fn applying_settings_stores_the_file_and_tells_the_controller() {
        let mut app = app_with_rects(1);
        app.run(Action::Settings);
        let Some(draft) = app.settings_draft.as_mut() else {
            panic!("the dialog did not open");
        };
        draft.include_cursor = true;
        draft.show_tool_labels = false;

        app.run(Action::ApplySettings);

        assert!(app.settings_draft.is_none(), "the dialog stayed open");
        assert!(
            app.settings.settings().include_cursor,
            "the setting was lost"
        );
        assert!(!app.show_tool_labels, "the View toggle did not follow");
        assert!(
            app.settings.config_path().exists(),
            "OK did not write the config file"
        );

        // The controller thread has to hear about the capture switches.
        let commands = app.controller.take_commands();
        assert!(
            commands.iter().any(|command| matches!(
                command,
                AppCommand::Configure(settings) if settings.include_cursor
            )),
            "the controller was not told: {commands:?}"
        );

        // And the file really holds it: a fresh load sees the same settings.
        let (reloaded, warnings) =
            SettingsStore::load(app.settings.config_path().parent().unwrap().to_path_buf());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(reloaded.settings().include_cursor);
    }

    #[test]
    fn cancelling_settings_changes_nothing() {
        let mut app = app_with_rects(1);
        let before = app.settings.settings().clone();

        app.run(Action::Settings);
        if let Some(draft) = app.settings_draft.as_mut() {
            draft.include_cursor = true;
            draft.filename_template = "changed_%Y".to_owned();
        }
        app.run(Action::CancelSettings);

        assert!(app.settings_draft.is_none());
        assert_eq!(*app.settings.settings(), before);
        assert!(
            !app.settings.config_path().exists(),
            "Cancel wrote the file"
        );
    }

    #[test]
    fn the_close_button_only_hides_when_the_setting_says_so() {
        let mut app = app_with_rects(1);
        let mut settings = app.settings.settings().clone();
        settings.close_hides_to_tray = false;
        app.settings.replace(settings);
        app.run(Action::Settings);
        if let Some(draft) = app.settings_draft.as_mut() {
            draft.close_hides_to_tray = false;
        }
        app.run(Action::ApplySettings);

        let commands = run_close_request(&mut app);
        assert!(app.exit, "the X did not exit with hiding switched off");
        assert!(sent(&commands, &egui::ViewportCommand::CancelClose));
    }

    #[test]
    fn a_tray_icon_is_still_needed_for_the_x_to_hide() {
        // G3 + SPEC 10: the setting cannot hide the window when there is no
        // tray icon to bring it back.
        let mut app = app_without_tray();
        app.run(Action::Settings);
        if let Some(draft) = app.settings_draft.as_mut() {
            draft.close_hides_to_tray = true;
        }
        app.run(Action::ApplySettings);

        assert!(!app.close_hides, "the X would strand the process");
        let commands = run_close_request(&mut app);
        assert!(app.exit, "the X did not exit");
        assert!(sent(&commands, &egui::ViewportCommand::CancelClose));
    }

    #[test]
    fn not_asking_about_unsaved_tabs_closes_them_without_a_prompt() {
        let mut app = app_with_rects(1);
        dirty_rectangle(&mut app.tabs[0]);
        let mut settings = app.settings.settings().clone();
        settings.ask_before_closing = false;
        app.settings.replace(settings);

        app.request_exit();
        assert!(app.exit, "it asked although the setting is off");
        assert!(app.pending_close.is_none(), "a prompt appeared anyway");

        app.request_close(0);
        assert!(app.tabs.is_empty(), "the tab was not closed");
    }

    #[test]
    fn auto_save_writes_a_new_capture_into_the_default_folder() {
        let folder = temp_folder("auto-save");
        let mut app = app();
        let mut settings = app.settings.settings().clone();
        settings.auto_save_new_captures = true;
        settings.save_folder = Some(folder.clone());
        settings.filename_template = "auto_%Y".to_owned();
        app.settings.replace(settings);

        app.add_capture(RgbaImage::new(4, 4));
        wait_for_saves(&mut app);

        let entries: Vec<PathBuf> = std::fs::read_dir(&folder)
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .collect();
        assert_eq!(entries.len(), 1, "not exactly one file: {entries:?}");
        let name = entries[0]
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with("auto_"), "{name}");
        assert!(name.ends_with(".png"), "{name}");
        assert!(
            !app.tabs[0].dirty(),
            "the auto-saved tab still counts as unsaved"
        );
        assert!(app.status.contains("Auto-saved"), "{}", app.status);
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// H1: auto-save must never replace a file that is already there. The
    /// default template only goes down to seconds, and a template with no time
    /// fields names every capture the same, so the second save used to leave
    /// only the second file.
    #[test]
    fn auto_save_never_overwrites_an_earlier_capture() {
        let folder = temp_folder("auto-save-twice");
        let mut app = app();
        let mut settings = app.settings.settings().clone();
        settings.auto_save_new_captures = true;
        settings.save_folder = Some(folder.clone());
        settings.filename_template = "shot".to_owned();
        app.settings.replace(settings);

        app.add_capture(RgbaImage::new(4, 4));
        app.add_capture(RgbaImage::new(4, 4));
        app.add_capture(RgbaImage::new(4, 4));
        wait_for_saves(&mut app);

        let mut names: Vec<String> = std::fs::read_dir(&folder)
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["shot-1.png", "shot-2.png", "shot.png"],
            "an auto-save replaced an earlier file"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// H1: the numbering itself, with the "taken" question passed in.
    #[test]
    fn a_free_name_is_numbered_until_it_is_free() {
        let taken = ["shot.png", "shot-1.png", "shot-3.png"];
        assert_eq!(
            free_file_name_with("shot", "png", |name| taken.contains(&name)),
            "shot-2.png"
        );
        let none: [&str; 0] = [];
        assert_eq!(
            free_file_name_with("shot", "png", |name| none.contains(&name)),
            "shot.png"
        );
    }

    /// H1: Save As suggests a free name, so the suggested file is not one that
    /// is already there (the dialog still asks before overwriting).
    #[test]
    fn save_as_suggests_a_free_name() {
        let folder = temp_folder("suggest-free");
        std::fs::write(folder.join("shot.png"), b"already here").expect("write");
        let settings = Settings {
            filename_template: "shot".to_owned(),
            ..Settings::default()
        };

        let suggested = suggested_save_name(&settings, &folder, chrono::Local::now());

        assert_eq!(suggested, "shot-1.png");
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// NEXT_STEPS step 9c: a default folder with `%Y-%m` in it sorts captures
    /// into a folder per month, which auto-save creates on the way.
    #[test]
    fn auto_save_puts_captures_into_a_dated_folder() {
        let root = temp_folder("auto-save-dated");
        let mut app = app();
        let mut settings = app.settings.settings().clone();
        settings.auto_save_new_captures = true;
        // The folder takes chrono variables, so this month's folder is created
        // when the first capture lands in it.
        settings.save_folder = Some(root.join("%Y-%m"));
        app.settings.replace(settings);

        app.add_capture(RgbaImage::new(4, 4));
        wait_for_saves(&mut app);

        let mut months: Vec<String> = std::fs::read_dir(&root)
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        months.sort();
        assert_eq!(
            months.len(),
            1,
            "the dated folder was not created: {months:?}"
        );
        let month = &months[0];
        assert!(
            month.len() == 7 && month.as_bytes()[4] == b'-' && month.starts_with("20"),
            "not a YYYY-MM folder: {month}"
        );
        let files: Vec<PathBuf> = std::fs::read_dir(root.join(month))
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .collect();
        assert_eq!(
            files.len(),
            1,
            "the capture did not land in the month folder: {files:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// NEXT_STEPS step 10: the "annotations outside the image" setting reaches
    /// Save and Copy — the file on disk and the image the copy path uses both
    /// grow to keep an annotation placed beside the image, filled with the
    /// chosen colour.
    #[test]
    fn the_expansion_setting_reaches_save_and_copy() {
        let folder = temp_folder("expand-export");
        let mut app = app();
        let mut settings = app.settings.settings().clone();
        settings.expand_outside_annotations = true;
        settings.expansion_fill = crate::settings::ExpansionFill::Colour;
        settings.expansion_colour = [0, 255, 0, 255];
        app.settings.replace(settings);

        // A black 40 × 40 image with a red 15 × 30 rectangle 25 px to its left.
        let mut tab = Tab::new(
            Document::new(RgbaImage::from_pixel(40, 40, image::Rgba([0, 0, 0, 255]))),
            "expand".to_owned(),
        );
        tab.document.push(Kind::Rect(RectA {
            rect: Rect::new(-25.0, 5.0, 15.0, 30.0),
            stroke: None,
            fill: Some([255, 0, 0, 255]),
            corner_radius: 0.0,
        }));
        app.push_tab(tab);

        // Save: the worker writes the grown image to disk.
        let path = folder.join("expanded.png");
        app.start_save(0, &path, false).expect("the save starts");
        wait_for_saves(&mut app);
        let saved = codec::decode(&std::fs::read(&path).expect("read")).expect("decode");
        assert_eq!(
            (saved.width(), saved.height()),
            (65, 40),
            "the saved image did not grow to hold the rectangle"
        );
        assert_eq!(
            saved.get_pixel(0, 0).0,
            [0, 255, 0, 255],
            "the added area is not the fill colour"
        );
        assert_eq!(
            saved.get_pixel(35, 20).0,
            [0, 0, 0, 255],
            "the base image is not at its offset"
        );
        assert_eq!(
            saved.get_pixel(10, 20).0,
            [255, 0, 0, 255],
            "the rectangle is missing from the save"
        );

        // Copy: the helper the copy path uses produces exactly that image.
        let copied = app.flattened_for_export(0).expect("an image to copy");
        assert_eq!(copied, saved, "Copy and Save disagree about the export");

        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn auto_save_off_leaves_the_capture_unsaved() {
        let folder = temp_folder("no-auto-save");
        let mut app = app();
        let mut settings = app.settings.settings().clone();
        settings.auto_save_new_captures = false;
        settings.save_folder = Some(folder.clone());
        app.settings.replace(settings);

        app.add_capture(RgbaImage::new(4, 4));

        assert!(
            std::fs::read_dir(&folder)
                .expect("read_dir")
                .next()
                .is_none(),
            "a file was written although auto-save is off"
        );
        assert!(app.tabs[0].dirty());
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// H6: the worker writes the format the extension names — AVIF here, the
    /// one slow enough to need a background thread (a real 4K save is ~3 s; a
    /// 24×24 one is quick, and all this checks is that the right encoder ran).
    #[test]
    fn an_async_save_writes_the_named_format() {
        let Some(format) = codec::Format::from_extension("avif") else {
            eprintln!("the avif feature is off; nothing to check");
            return;
        };
        let folder = temp_folder("async-avif");
        let path = folder.join("shot.avif");
        let mut app = app();
        app.tabs.push(Tab::from_path(
            Document::new(RgbaImage::new(24, 24)),
            path.clone(),
        ));
        dirty_rectangle(&mut app.tabs[0]);

        app.run(Action::Save);
        wait_for_saves(&mut app);

        let bytes = std::fs::read(&path).expect("the worker wrote no file");
        assert_eq!(
            &bytes[4..8],
            b"ftyp",
            "not an ISO-BMFF container, so {} did not run",
            format.label()
        );
        assert!(!app.tabs[0].dirty(), "the tab stayed dirty");
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// H6: a second save of the same tab while one is running is refused, so
    /// the two cannot race for the file.
    #[test]
    fn a_second_save_of_the_same_tab_is_refused() {
        let folder = temp_folder("second-save");
        let path = folder.join("shot.png");
        let mut app = app();
        app.tabs.push(Tab::from_path(
            Document::new(RgbaImage::new(64, 64)),
            path.clone(),
        ));
        dirty_rectangle(&mut app.tabs[0]);

        app.run(Action::Save);
        assert!(
            app.status.contains("Saving"),
            "the first save did not start: {}",
            app.status
        );

        app.run(Action::Save);
        assert!(
            app.status.contains("already being saved"),
            "the second save was not refused: {}",
            app.status
        );

        wait_for_saves(&mut app);
        assert!(path.exists(), "the first save did not write the file");
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// H6: the save runs on the worker and the UI applies the result — the file
    /// appears, the status bar reports it, and the tab stops counting as dirty.
    #[test]
    fn a_save_updates_the_tab_when_the_worker_finishes() {
        let folder = temp_folder("async-save");
        let path = folder.join("shot.png");
        let mut app = app();
        app.tabs.push(Tab::from_path(
            Document::new(RgbaImage::new(64, 64)),
            path.clone(),
        ));
        dirty_rectangle(&mut app.tabs[0]);

        app.run(Action::Save);
        wait_for_saves(&mut app);

        let bytes = std::fs::read(&path).expect("the worker wrote no file");
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "not a PNG"
        );
        assert!(!app.tabs[0].dirty(), "the tab stayed dirty after a save");
        assert!(app.status.starts_with("Saved"), "{}", app.status);
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// H6: quitting waits for a save that is still running, so one started a
    /// moment before the exit is not thrown away.
    #[test]
    fn quitting_waits_for_a_save_in_flight() {
        let folder = temp_folder("quit-during-save");
        let path = folder.join("shot.png");
        let mut app = app();
        app.tabs.push(Tab::from_path(
            Document::new(RgbaImage::new(64, 64)),
            path.clone(),
        ));
        dirty_rectangle(&mut app.tabs[0]);

        app.run(Action::Save);
        assert!(!app.saving.is_empty(), "the save did not start");

        // `on_exit` waits for the worker itself; the test does not drain it.
        app.on_exit();

        assert!(path.exists(), "the quit threw the save away");
        assert!(
            !app.tabs[0].dirty(),
            "the saved tab still counts as unsaved"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// H6: a result whose tab was closed before it arrived is still reported —
    /// the file is on disk, and the user should hear about it.
    #[test]
    fn a_save_result_for_a_closed_tab_is_still_reported() {
        let mut app = app();
        app.saving.insert(999_999, PathBuf::from("gone.png"));
        app.save_worker.inject_outcome(crate::save::SaveOutcome {
            tab: 999_999,
            path: PathBuf::from("gone.png"),
            document: Document::new(RgbaImage::new(1, 1)),
            auto: false,
            result: Ok(crate::save::SaveStats {
                bytes: 42,
                encode_ms: 1.0,
                write_ms: 1.0,
            }),
        });

        app.drain_saves();

        assert!(app.status.contains("Saved"), "{}", app.status);
        assert!(app.saving.is_empty(), "the save stayed in flight");
    }

    /// H6: a failed save is reported and leaves the tab dirty, so the user
    /// knows the file is not there.
    #[test]
    fn a_failed_save_is_reported_and_leaves_the_tab_dirty() {
        let mut app = app_with_rects(1);
        dirty_rectangle(&mut app.tabs[0]);
        let id = app.tabs[0].id();
        app.saving.insert(id, PathBuf::from("shot.png"));
        app.save_worker.inject_outcome(crate::save::SaveOutcome {
            tab: id,
            path: PathBuf::from("shot.png"),
            document: Document::new(RgbaImage::new(1, 1)),
            auto: false,
            result: Err("the disk is full".to_owned()),
        });

        app.drain_saves();

        assert!(app.status.contains("disk is full"), "{}", app.status);
        assert!(app.tabs[0].dirty(), "a failed save must not look saved");
        assert!(app.saving.is_empty());
    }

    /// NEXT_STEPS step 9d: a rebind reaches the Capture menu and the toolbar's
    /// hover text. Both used to be built from `CaptureMode::hotkey()`, which
    /// only knows the SPEC 6.1 defaults, so they kept saying "Print Screen"
    /// after the user changed the key in Settings.
    #[test]
    fn the_capture_menu_and_toolbar_use_the_users_hotkeys() {
        let mut app = app_with_rects(1);
        let mut settings = app.settings.settings().clone();
        settings.hotkey_region = "Ctrl+Alt+P".to_owned();
        app.settings.replace(settings);

        let state = app.menu_state();
        let rows = panels::capture_rows(&state);
        assert_eq!(rows[0].0, "Region    Ctrl+Alt+P", "{rows:?}");
        assert!(
            !rows[0].0.contains("Print Screen"),
            "the SPEC default is still shown for the rebound mode"
        );
        // The other three keep their own settings, which here are still the
        // SPEC defaults — the menu reads the settings, not the SPEC table.
        assert_eq!(rows[1].0, "Current Monitor    Ctrl+Print Screen");
        assert_eq!(
            panels::new_capture_hover(app.settings.settings().hotkey_text(CaptureMode::Region)),
            "Capture a new image (Ctrl+Alt+P for a region)"
        );
    }

    /// NEXT_STEPS step 14b: the whole stroke is one annotation and one undo
    /// step, and every move event of a frame is part of it.
    #[test]
    fn a_freehand_stroke_is_one_annotation_and_one_undo_step() {
        let mut app = app_with_unsaved(1);
        app.tool = Tool::Freehand;

        let _ = app.canvas_input(&press_at(Point::new(10.0, 10.0)));
        let _ = app.canvas_input(&stroke_to(
            Point::new(200.0, 10.0),
            &[
                Point::new(80.0, 10.0),
                Point::new(140.0, 10.0),
                Point::new(200.0, 10.0),
            ],
        ));
        let _ = app.canvas_input(&release_at(Point::new(200.0, 10.0)));

        assert_eq!(app.tabs[0].document.annotations.len(), 1);
        let Kind::Freehand(stroke) = &app.tabs[0].document.annotations[0].kind else {
            panic!("expected a freehand stroke");
        };
        assert_eq!(stroke.points.first(), Some(&Point::new(10.0, 10.0)));
        assert_eq!(stroke.points.last(), Some(&Point::new(200.0, 10.0)));
        assert_eq!(app.tabs[0].undo_depth_for_test(), 1, "not one undo step");
        assert!(!app.gesture.is_active());
    }

    /// NEXT_STEPS step 14b: Esc cancels the stroke under way, and the release
    /// that follows adds nothing.
    #[test]
    fn an_escape_cancels_a_freehand_stroke() {
        let mut app = app_with_unsaved(1);
        app.tool = Tool::Freehand;

        let _ = app.canvas_input(&press_at(Point::new(10.0, 10.0)));
        let _ = app.canvas_input(&stroke_to(
            Point::new(150.0, 60.0),
            &[Point::new(150.0, 60.0)],
        ));
        app.run(Action::CancelGesture);
        assert!(!app.gesture.is_active(), "Esc left the stroke under way");
        let _ = app.canvas_input(&release_at(Point::new(150.0, 60.0)));

        assert!(
            app.tabs[0].document.annotations.is_empty(),
            "Esc left a stroke behind"
        );
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            0,
            "a cancelled stroke cost an undo step"
        );
    }

    #[test]
    fn switching_to_select_after_drawing_is_the_setting() {
        let draw = |app: &mut SnpLigaseApp| {
            app.tool = Tool::Rectangle;
            let _ = app.canvas_input(&press_at(Point::new(10.0, 10.0)));
            let _ = app.canvas_input(&drag_to(Point::new(60.0, 40.0)));
            let _ = app.canvas_input(&release_at(Point::new(60.0, 40.0)));
        };

        let mut first = app();
        first.push_tab(Tab::new(
            Document::new(RgbaImage::new(200, 200)),
            "test".to_owned(),
        ));
        let mut settings = first.settings.settings().clone();
        settings.switch_to_select_after_drawing = true;
        first.settings.replace(settings);
        draw(&mut first);
        assert_eq!(
            first.tool,
            Tool::Select,
            "the setting was on and the tool stayed on Rectangle"
        );

        let mut second = app();
        second.push_tab(Tab::new(
            Document::new(RgbaImage::new(200, 200)),
            "test".to_owned(),
        ));
        let mut settings = second.settings.settings().clone();
        settings.switch_to_select_after_drawing = false;
        second.settings.replace(settings);
        draw(&mut second);
        assert_eq!(second.tool, Tool::Rectangle, "the setting was off");

        // NEXT_STEPS step 14b: the Freehand stroke is a drag gesture too, so
        // its release hands the pointer back like any other drawing tool's.
        let mut third = app();
        third.push_tab(Tab::new(
            Document::new(RgbaImage::new(200, 200)),
            "test".to_owned(),
        ));
        let mut settings = third.settings.settings().clone();
        settings.switch_to_select_after_drawing = true;
        third.settings.replace(settings);
        third.tool = Tool::Freehand;
        let _ = third.canvas_input(&press_at(Point::new(10.0, 10.0)));
        let _ = third.canvas_input(&stroke_to(
            Point::new(80.0, 10.0),
            &[Point::new(80.0, 10.0)],
        ));
        let _ = third.canvas_input(&release_at(Point::new(80.0, 10.0)));
        assert_eq!(
            third.tool,
            Tool::Select,
            "a finished freehand stroke left the pointer on Freehand"
        );
        assert_eq!(
            third.tabs[0].document.annotations.len(),
            1,
            "the stroke itself was lost"
        );
    }

    /// NEXT_STEPS step 9b: the Text tool's gesture ends in `Gesture::TextEdit`,
    /// which `canvas_input` turns into a session, so the "a drawing gesture
    /// ended" condition above never held for it and the setting did nothing.
    /// Committing a **newly created** annotation switches now; cancelling, an
    /// empty commit (which deletes it) and re-editing an existing text do not.
    #[test]
    fn switching_to_select_after_drawing_also_covers_the_text_tool() {
        /// Places a text with the Text tool and types `text` into it, the way
        /// the editor's live write does.
        fn type_a_new_text(app: &mut SnpLigaseApp, at: Point, text: &str) -> u64 {
            app.tool = Tool::Text;
            let _ = app.canvas_input(&press_at(at));
            let _ = app.canvas_input(&release_at(at));
            let id = app
                .text_session
                .as_ref()
                .expect("the release did not open the editor")
                .id;
            if let Some(session) = app.text_session.as_mut() {
                session.text = text.to_owned();
            }
            write_text(&mut app.tabs[app.active], id, text);
            id
        }

        /// The settings object with the switch on.
        fn switching_settings() -> Settings {
            Settings {
                switch_to_select_after_drawing: true,
                ..Settings::default()
            }
        }

        // Commit a newly created text: the tool switches.
        let mut committed = app_with_rects(1);
        committed.settings.replace(switching_settings());
        let id = type_a_new_text(&mut committed, Point::new(20.0, 20.0), "Hi");
        committed.run(Action::CommitText);
        assert_eq!(
            committed.tool,
            Tool::Select,
            "a committed new text did not hand the pointer back"
        );
        assert_eq!(text_of(&committed.tabs[0], id), "Hi");
        assert_eq!(committed.tabs[0].document.annotations.len(), 1);

        // Cancel a new text: it stays on Text.
        let mut cancelled = app_with_rects(1);
        cancelled.settings.replace(switching_settings());
        type_a_new_text(&mut cancelled, Point::new(20.0, 20.0), "Hi");
        cancelled.run(Action::CancelText);
        assert_eq!(
            cancelled.tool,
            Tool::Text,
            "cancelling a text switched the tool"
        );

        // Commit an empty new text: the annotation is deleted, so nothing was
        // drawn and the tool stays.
        let mut emptied = app_with_rects(1);
        emptied.settings.replace(switching_settings());
        type_a_new_text(&mut emptied, Point::new(20.0, 20.0), "");
        emptied.run(Action::CommitText);
        assert!(
            emptied.tabs[0].document.annotations.is_empty(),
            "an empty commit did not delete the annotation"
        );
        assert_eq!(
            emptied.tool,
            Tool::Text,
            "an empty commit switched the tool"
        );

        // Re-editing an existing text: the tool stays on Text.
        let mut re_edit = app_with_rects(1);
        re_edit.settings.replace(switching_settings());
        let existing = push_text(&mut re_edit.tabs[0], "There");
        start_re_edit_session(&mut re_edit, 0, existing, "There");
        re_edit.tool = Tool::Text;
        re_edit.run(Action::CommitText);
        assert_eq!(text_of(&re_edit.tabs[0], existing), "There");
        assert_eq!(
            re_edit.tool,
            Tool::Text,
            "re-editing an existing text switched the tool"
        );

        // The setting off: a new text leaves the tool alone.
        let mut off = app_with_rects(1);
        off.settings.replace(Settings::default());
        type_a_new_text(&mut off, Point::new(20.0, 20.0), "Hi");
        off.run(Action::CommitText);
        assert_eq!(off.tool, Tool::Text, "the setting was ignored");
    }

    /// The rule the app asks, on its own: only the setting plus a session that
    /// created the annotation switches.
    #[test]
    fn the_switch_rule_is_the_setting_and_a_created_session() {
        assert!(!switches_after_text_session(false, false));
        assert!(!switches_after_text_session(false, true));
        assert!(!switches_after_text_session(true, false));
        assert!(switches_after_text_session(true, true));
    }

    /// NEXT_STEPS step 9b also asks that Number and Shape are checked: both
    /// clear their own gesture, so the shared "no longer active" branch covers
    /// them and they switch with the setting on.
    #[test]
    fn switching_to_select_after_drawing_covers_the_number_and_shape_tools() {
        let mut app = app_with_rects(1);
        let settings = Settings {
            switch_to_select_after_drawing: true,
            ..Settings::default()
        };
        app.settings.replace(settings);

        // Number: a click places a badge and increments the counter.
        app.tool = Tool::Number;
        let _ = app.canvas_input(&press_at(Point::new(30.0, 30.0)));
        let _ = app.canvas_input(&release_at(Point::new(30.0, 30.0)));
        assert_eq!(app.tool, Tool::Select, "the Number tool did not switch");
        assert_eq!(app.tabs[0].document.annotations.len(), 1);

        // Shape: click three corners, then click the first one to close
        // (SPEC 7.3) — the close that happens inside the tools, so the frame
        // sees the gesture end.
        app.tool = Tool::Shape;
        for corner in [
            Point::new(10.0, 10.0),
            Point::new(60.0, 10.0),
            Point::new(60.0, 60.0),
        ] {
            let _ = app.canvas_input(&press_at(corner));
            let _ = app.canvas_input(&release_at(corner));
        }
        let _ = app.canvas_input(&press_at(Point::new(10.0, 10.0)));
        let _ = app.canvas_input(&release_at(Point::new(10.0, 10.0)));
        assert_eq!(
            app.tabs[0].document.annotations.len(),
            2,
            "the shape did not close"
        );
        assert_eq!(app.tool, Tool::Select, "the Shape tool did not switch");
    }

    #[test]
    fn the_save_name_always_gets_an_extension_this_build_can_write() {
        use codec::Format;
        assert_eq!(ensure_extension("shot", Format::Png), "shot.png");
        assert_eq!(ensure_extension("shot.jpg", Format::Png), "shot.jpg");
        assert_eq!(ensure_extension("shot.jpeg", Format::Png), "shot.jpeg");
        assert_eq!(ensure_extension("shot", Format::Jpeg), "shot.jpg");
        // A format this build cannot write is replaced, not appended to.
        assert_eq!(ensure_extension("shot.gif", Format::Png), "shot.png");
        assert_eq!(ensure_extension("shot.", Format::Png), "shot.png");
        assert_eq!(ensure_extension("shot.tar.gz", Format::Png), "shot.tar.png");
        #[cfg(feature = "avif")]
        assert_eq!(ensure_extension("shot.avif", Format::Png), "shot.avif");
    }

    #[test]
    fn the_tool_defaults_survive_a_restart() {
        // SPEC section 10: per-tool styles are persisted, so a restart keeps
        // what the user chose.
        let mut app = app_with_rects(1);
        app.tool = Tool::Arrow;
        app.run(Action::Style(StyleProperty::StrokeWidth(11.0)));
        // The debounced write is driven by the frame; do it directly here.
        app.settings
            .sync_editor_state(&app.styles, &app.recent_colours);
        app.settings.flush().expect("write");

        let config_dir = app
            .settings
            .config_path()
            .parent()
            .expect("a config folder")
            .to_path_buf();
        let (reloaded, warnings) = SettingsStore::load(config_dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        let style = reloaded
            .settings()
            .styles
            .get(Tool::Arrow)
            .expect("an arrow style");
        assert!(
            (style.stroke_width - 11.0).abs() < 0.001,
            "the tool default did not survive: {}",
            style.stroke_width
        );
    }

    /// NEXT_STEPS step 8: turning the text outline on is one undo step, and it
    /// becomes the Text tool's default for the next annotation (SPEC 7.7).
    #[test]
    fn a_text_outline_change_is_one_undo_step_and_updates_the_tool_defaults() {
        let mut app = app_with_rects(1);
        let id = push_text(&mut app.tabs[0], "Hi");
        app.tabs[0].select_only(Some(id));
        let before = app.tabs[0].document.clone();

        app.run(Action::Style(StyleProperty::TextOutline(Some(
            defaults::stroke(),
        ))));
        app.settle_edits();

        let Kind::Text(text) = &app.tabs[0].document.find(id).expect("the text").kind else {
            panic!("expected text");
        };
        assert_eq!(
            text.outline,
            Some(defaults::stroke()),
            "the outline did not land"
        );
        assert_eq!(
            app.styles
                .get(Tool::Text)
                .expect("a text style")
                .text_outline,
            Some(defaults::stroke()),
            "the Text tool did not learn the outline"
        );
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            1,
            "not exactly one undo step"
        );
        assert!(app.tabs[0].undo());
        assert_eq!(
            app.tabs[0].document, before,
            "undo did not put the outline back"
        );
    }

    /// The outline colour and width reach the annotation and the tool defaults
    /// through the same property path (NEXT_STEPS step 8).
    #[test]
    fn text_outline_colour_and_width_reach_the_annotation_and_defaults() {
        let mut app = app_with_rects(1);
        let id = push_text(&mut app.tabs[0], "Hi");
        app.tabs[0].select_only(Some(id));

        app.run(Action::Style(StyleProperty::TextOutline(Some(
            defaults::stroke(),
        ))));
        app.settle_edits();
        app.run(Action::Style(StyleProperty::TextOutlineColour([
            1, 2, 3, 255,
        ])));
        app.settle_edits();
        app.run(Action::Style(StyleProperty::TextOutlineWidth(9.0)));
        app.settle_edits();

        let Kind::Text(text) = &app.tabs[0].document.find(id).expect("the text").kind else {
            panic!("expected text");
        };
        assert_eq!(
            text.outline,
            Some(Stroke {
                color: [1, 2, 3, 255],
                width: 9.0
            })
        );
        assert_eq!(
            app.styles
                .get(Tool::Text)
                .expect("a text style")
                .text_outline,
            Some(Stroke {
                color: [1, 2, 3, 255],
                width: 9.0
            }),
            "the Text tool did not learn the outline changes"
        );
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            3,
            "not one step per change"
        );
    }

    // ---------------------------------------------------------------------
    // M7: the text session's undo step, and starting hidden in the tray
    // ---------------------------------------------------------------------

    /// M7: a property change made while the text editor is open is part of the
    /// session's single undo step. It used to push its own step, which made the
    /// first Ctrl+Z return to the pre-session document and the second bring the
    /// half-typed text back.
    #[test]
    fn a_property_change_during_a_text_session_is_the_same_undo_step() {
        let mut app = app_with_rects(1);
        let before = app.tabs[0].document.clone();
        let id = start_text_session(&mut app, 0, "Hello");

        // The properties panel's write path (and the Annotation menu's) goes
        // through `Action::Style`.
        app.run(Action::Style(StyleProperty::Bold(true)));
        app.settle_edits();
        app.commit_text_session();

        let Kind::Text(text) = &app.tabs[0].document.find(id).expect("the text").kind else {
            panic!("expected text");
        };
        assert!(text.bold, "the property change did not land");
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            1,
            "the session and the property change are not one undo step"
        );

        assert!(app.tabs[0].undo());
        assert_eq!(
            app.tabs[0].document, before,
            "one undo did not put everything back"
        );
        assert!(
            !app.tabs[0].can_undo(),
            "there is a second, half-empty undo step left"
        );
    }

    /// M7, the other half: `Esc` puts the whole document back, so a property
    /// change made during the session cannot be left behind with no undo step
    /// to reach it.
    #[test]
    fn cancelling_a_text_session_also_reverts_a_property_change() {
        let mut app = app_with_rects(1);
        let tab = &mut app.tabs[0];
        let id = push_text(tab, "Original");
        let before = tab.document.clone();
        start_re_edit_session(&mut app, 0, id, "Half-typed");

        app.run(Action::Style(StyleProperty::Bold(true)));
        app.settle_edits();
        app.cancel_text_session();

        assert_eq!(
            app.tabs[0].document, before,
            "cancel left the property change behind"
        );
        assert_eq!(text_of(&app.tabs[0], id), "Original");
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            0,
            "cancel took an undo step"
        );
    }

    /// SPEC 10's "Start hidden in the tray" needs a tray to come back from
    /// (G3's rule). The hide itself goes out on the second frame (eframe shows
    /// the window after the first), so this drives two frames and checks the
    /// gate as well as the message.
    #[test]
    fn the_window_starts_hidden_when_the_setting_says_so() {
        let folder = temp_folder("start-hidden");
        let (mut store, _) = SettingsStore::load(folder.clone());
        let mut settings = store.settings().clone();
        settings.start_hidden_in_tray = true;
        store.replace(settings.clone());
        assert!(starts_hidden(true, &settings), "the decision");

        let app = SnpLigaseApp::from_shell(Shell {
            log_dir: folder.clone(),
            files: Vec::new(),
            controller: Controller::detached(),
            hotkeys: Hotkeys::none(),
            settings: store,
            warnings: Vec::new(),
            tray_installed: true,
            start_hidden: true,
        });
        assert_eq!(app.start_hidden, 1, "the setting was not picked up");

        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut SnpLigaseApp| app.frame_ui(ui), app);
        harness.run();
        assert_eq!(
            harness.state().start_hidden,
            0,
            "the flag did not clear after the frames"
        );
        assert!(
            !harness.state().controller.window().is_visible(),
            "the window was not hidden"
        );
        assert!(
            harness.state().status.contains("hidden in the tray"),
            "the status bar does not say so: {}",
            harness.state().status
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// H5: the hide goes out on the second frame, and that second frame has to
    /// be **asked for** — nothing else runs a frame while the window is just
    /// sitting there, so without the request the window would stay visible
    /// until the first mouse move.
    ///
    /// A bare `egui::Context` is used rather than the kittest harness: the
    /// harness runs a frame in its constructor, and egui's own "run a couple of
    /// frames at the start" request has to be consumed first, or the check
    /// below would pass for the wrong reason.
    #[test]
    fn the_second_start_hidden_frame_is_asked_for() {
        let folder = temp_folder("start-hidden-repaint");
        let (mut store, _) = SettingsStore::load(folder.clone());
        let mut settings = store.settings().clone();
        settings.start_hidden_in_tray = true;
        store.replace(settings.clone());

        let mut app = SnpLigaseApp::from_shell(Shell {
            log_dir: folder.clone(),
            files: Vec::new(),
            controller: Controller::detached(),
            hotkeys: Hotkeys::none(),
            settings: store,
            warnings: Vec::new(),
            tray_installed: true,
            start_hidden: true,
        });
        assert_eq!(app.start_hidden, 1, "the setting was not picked up");

        let context = egui::Context::default();
        let mut warmup = context.run_ui(egui::RawInput::default(), |_ui| {});
        warmup.textures_delta.clear();

        let mut first = context.run_ui(egui::RawInput::default(), |ui| app.frame_ui(ui));
        first.textures_delta.clear();
        assert_eq!(
            app.start_hidden, 2,
            "the first frame did not move the flag on"
        );

        // The frame that was asked for is the one that hides the window.
        let mut second = context.run_ui(egui::RawInput::default(), |ui| app.frame_ui(ui));
        second.textures_delta.clear();
        assert_eq!(
            app.start_hidden, 0,
            "the hide did not go out on the second frame"
        );
        assert!(
            app.status.contains("hidden in the tray"),
            "the status bar does not say so: {}",
            app.status
        );

        // `repaint_causes` reports the previous pass's requests, so this says
        // whether the frame that moved the flag on asked for the next one. The
        // cause carries its call site, so it can only be the app's own request
        // (and not whatever egui itself asked for).
        let causes = context.repaint_causes();
        assert!(
            causes.iter().any(|cause| cause.file.ends_with("app.rs")),
            "the hide frame was not asked for; causes: {causes:?}"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_window_does_not_start_hidden_without_a_tray_icon() {
        // Without a tray there is no way back, exactly like the close button.
        let folder = temp_folder("start-hidden-no-tray");
        let (mut store, _) = SettingsStore::load(folder.clone());
        let mut settings = store.settings().clone();
        settings.start_hidden_in_tray = true;
        store.replace(settings.clone());
        assert!(
            !starts_hidden(false, &settings),
            "it would hide with no tray to return from"
        );

        let app = SnpLigaseApp::from_shell(Shell {
            log_dir: folder.clone(),
            files: Vec::new(),
            controller: Controller::detached(),
            hotkeys: Hotkeys::none(),
            settings: store,
            warnings: Vec::new(),
            tray_installed: false,
            start_hidden: false,
        });
        assert_eq!(
            app.start_hidden, 0,
            "it would hide with no tray to return from"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    // -----------------------------------------------------------------
    // Recording a hotkey by pressing it (NEXT_STEPS step 7)
    // -----------------------------------------------------------------

    use snp_ligase_platform::windows::keys::KeyboardSnapshot;

    /// A keyboard snapshot for the recorder tests. The real keyboard is never
    /// read: `start_hotkey_recording` takes one snapshot when it starts, and
    /// `start_recording` below clears it.
    fn snapshot(keys: &[u16], ctrl: bool, alt: bool) -> KeyboardSnapshot {
        KeyboardSnapshot {
            ctrl,
            alt,
            keys: keys.to_vec(),
            ..KeyboardSnapshot::default()
        }
    }

    /// Starts a recording for `mode` with an empty keyboard baseline, so the
    /// machine's real keyboard cannot make the test flaky.
    fn start_recording(app: &mut SnpLigaseApp, mode: CaptureMode) {
        app.run(Action::Settings);
        app.run(Action::RecordHotkey(mode));
        assert!(app.recording.is_some(), "the recording did not start");
        // `Recorder::update` replaces the "already down" baseline with the
        // snapshot it was given, so one empty snapshot forgets whatever the
        // real keyboard had down when the recording started.
        app.poll_hotkey_recording(&KeyboardSnapshot::default());
    }

    fn stored_bindings(app: &SnpLigaseApp) -> Vec<crate::hotkeys::Binding> {
        crate::hotkeys::bindings_from(app.settings.settings()).0
    }

    /// NEXT_STEPS step 7: the recording guard re-registers the stored hotkeys
    /// on every exit path. The rebind log is the witness; the real keyboard is
    /// never touched because the guard is inert in tests.
    #[test]
    fn a_hotkey_recording_puts_the_stored_hotkeys_back_on_every_exit_path() {
        let paths = [
            "recorded",
            "escape",
            "backspace",
            "dialog-ok",
            "dialog-cancel",
            "focus-lost",
        ];
        for path in paths {
            let mut app = app();
            start_recording(&mut app, CaptureMode::Region);
            let stored = stored_bindings(&app);

            match path {
                "recorded" => app.poll_hotkey_recording(&snapshot(&[0x50], true, true)),
                "escape" => app.poll_hotkey_recording(&snapshot(&[0x1B], false, false)),
                "backspace" => app.poll_hotkey_recording(&snapshot(&[0x08], false, false)),
                "dialog-ok" => app.run(Action::ApplySettings),
                "dialog-cancel" => app.run(Action::CancelSettings),
                "focus-lost" => app.end_hotkey_recording(),
                _ => unreachable!(),
            }

            assert!(
                app.recording.is_none(),
                "{path}: the recording is still running"
            );
            assert!(
                app.hotkeys
                    .borrow()
                    .rebind_log()
                    .iter()
                    .any(|bindings| bindings == &stored),
                "{path}: the stored hotkeys were not re-registered; log: {:?}",
                app.hotkeys.borrow().rebind_log()
            );
        }
    }

    /// NEXT_STEPS step 7: the combination that was pressed is written into the
    /// dialog's draft — and only there; OK still has to be pressed.
    #[test]
    fn a_recorded_key_is_written_into_the_settings_draft() {
        let mut app = app();
        start_recording(&mut app, CaptureMode::Region);
        app.poll_hotkey_recording(&snapshot(&[0x50], true, true));

        assert!(app.recording.is_none(), "the recording did not end");
        assert_eq!(
            app.settings_draft
                .as_ref()
                .expect("the dialog is open")
                .hotkey_region,
            "Ctrl+Alt+P"
        );
        assert_eq!(
            app.settings.settings().hotkey_region,
            "Print Screen",
            "the stored settings changed before OK"
        );
        assert_eq!(app.status, "Region hotkey recorded as Ctrl+Alt+P.");
    }

    /// Backspace alone clears the draft field; Esc leaves it alone.
    #[test]
    fn backspace_clears_the_draft_field_and_escape_leaves_it() {
        let mut app = app();
        app.run(Action::Settings);
        if let Some(draft) = app.settings_draft.as_mut() {
            draft.hotkey_region = "Ctrl+Alt+P".to_owned();
        }
        app.run(Action::RecordHotkey(CaptureMode::Region));
        app.poll_hotkey_recording(&KeyboardSnapshot::default());
        app.poll_hotkey_recording(&snapshot(&[0x08], false, false));
        assert_eq!(
            app.settings_draft.as_ref().expect("draft").hotkey_region,
            "",
            "Backspace did not clear the field"
        );

        app.run(Action::RecordHotkey(CaptureMode::Region));
        app.poll_hotkey_recording(&KeyboardSnapshot::default());
        app.poll_hotkey_recording(&snapshot(&[0x1B], false, false));
        assert_eq!(
            app.settings_draft.as_ref().expect("draft").hotkey_region,
            "",
            "Esc changed the field"
        );
    }

    /// Carry-over from the Step 7 review: clicking "Test" while a recording is
    /// active must end the recording first. `test_hotkeys` finishes by
    /// re-registering the stored bindings, so otherwise the global keys would
    /// go live mid-recording and the next press would start a capture instead
    /// of being recorded.
    #[test]
    fn testing_the_hotkeys_ends_a_running_recording() {
        let mut app = app();
        start_recording(&mut app, CaptureMode::Region);
        let stored = stored_bindings(&app);

        app.run(Action::TestHotkeys);

        assert!(
            app.recording.is_none(),
            "the recording survived the Test button"
        );
        assert!(
            app.hotkeys
                .borrow()
                .rebind_log()
                .last()
                .is_some_and(|bindings| bindings == &stored),
            "the stored hotkeys are not the live ones after Test; log: {:?}",
            app.hotkeys.borrow().rebind_log()
        );
    }

    /// NEXT_STEPS step 7 item 3: "Test" tries the draft's keys, keeps the
    /// failures for the dialog to show, and puts the stored bindings back.
    #[test]
    fn testing_the_hotkeys_reports_failures_and_restores_the_stored_ones() {
        let mut app = app();
        app.run(Action::Settings);
        let stored = stored_bindings(&app);

        // A draft with one key the parser refuses: the failure has to land in
        // the dialog's list.
        if let Some(draft) = app.settings_draft.as_mut() {
            draft.hotkey_region = "banana".to_owned();
        }
        app.run(Action::TestHotkeys);

        assert!(
            app.settings_warnings
                .iter()
                .any(|warning| warning.contains("Region")),
            "the draft's failure is not in the dialog: {:?}",
            app.settings_warnings
        );
        let log = app.hotkeys.borrow().rebind_log().to_vec();
        assert_eq!(
            log.len(),
            2,
            "expected the draft's rebind and then the stored one"
        );
        assert_eq!(log[1], stored, "the stored hotkeys were not put back");
    }

    // -----------------------------------------------------------------
    // Pasting an image into the current tab (NEXT_STEPS step 11)
    // -----------------------------------------------------------------

    /// The placement rule: the image's centre at the view centre, on whole
    /// pixels, so the image is never resampled.
    #[test]
    fn an_image_positioned_at_a_centre_sits_on_whole_pixels() {
        assert_eq!(
            image_position_for_centre(Point::new(100.0, 50.0), (40, 20)),
            Point::new(80.0, 40.0)
        );
        // An odd size rounds to the nearest whole pixel rather than leaving a
        // half pixel behind.
        assert_eq!(
            image_position_for_centre(Point::new(10.3, 4.7), (5, 3)),
            Point::new(8.0, 3.0)
        );
    }

    /// NEXT_STEPS step 11: Ctrl+Shift+V adds the clipboard image to the
    /// current tab, centred in the view, selected, with the Select tool active,
    /// as exactly one undo step.
    #[test]
    fn pasting_into_the_current_tab_places_the_image_at_the_view_centre() {
        let mut app = app();
        app.add_capture(RgbaImage::new(200, 100));
        app.canvas = egui::vec2(400.0, 200.0);
        app.tool = Tool::Line;

        app.paste_image_into_tab(RgbaImage::from_pixel(40, 20, image::Rgba([1, 2, 3, 255])));

        let tab = &app.tabs[0];
        assert_eq!(tab.document.annotations.len(), 1);
        let annotation = &tab.document.annotations[0];
        let Kind::Image(image) = &annotation.kind else {
            panic!("expected a pasted image");
        };
        // The view centre is (200, 100) in image pixels; a 40 × 20 image
        // centred there starts at (180, 90).
        assert_eq!(image.pos, Point::new(180.0, 90.0));
        assert_eq!(image.image.dimensions(), (40, 20));
        assert_eq!(tab.selection(), [annotation.id]);
        assert_eq!(app.tool, Tool::Select, "the tool did not switch");
        assert_eq!(tab.undo_depth_for_test(), 1, "not exactly one undo step");

        // One undo takes it away again.
        assert!(app.tabs[0].undo());
        assert!(app.tabs[0].document.annotations.is_empty());
    }

    /// The full frame lays out the status bar with the tab's zoom (SPEC 8.2):
    /// the probe for "is the bottom panel really in the window".
    #[test]
    fn the_full_frame_shows_the_status_bar() {
        use egui_kittest::kittest::Queryable as _;

        let app = app_with_rects(1);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut SnpLigaseApp| app.frame_ui(ui), app);
        harness.run();
        let zoom = harness
            .query_by_label_contains("Zoom")
            .expect("the status bar's zoom label is not in the frame");
        let rect = zoom.rect();
        let screen = harness.ctx.viewport_rect();
        assert!(
            rect.width() > 0.0 && rect.height() > 0.0,
            "the status bar was laid out with no space: {rect:?}"
        );
        // Before the fix, the bottom panel was added after the central panel,
        // so its content was laid out below the window: rect.max.y was 614 on
        // an 800 × 600 screen.
        assert!(
            rect.max.y <= screen.max.y + 0.5,
            "the status bar is below the window: {rect:?} vs {screen:?}"
        );
    }

    /// A pasted image takes a shadow through the same write path as every
    /// other property (its kind has no tool of its own), as one undo step.
    #[test]
    fn a_pasted_images_shadow_is_one_undo_step() {
        let mut app = app();
        app.add_capture(RgbaImage::new(100, 100));
        app.paste_image_into_tab(RgbaImage::new(10, 10));
        let id = app.tabs[0].document.annotations[0].id;
        app.tabs[0].select_only(Some(id));
        app.settle_edits();

        app.run(Action::Style(StyleProperty::Shadow(true)));
        app.settle_edits();

        assert!(
            app.tabs[0].document.find(id).expect("the image").shadow,
            "the shadow did not land on the pasted image"
        );
        assert_eq!(
            app.tabs[0].undo_depth_for_test(),
            2,
            "one step for the paste and one for the shadow"
        );
        assert!(app.tabs[0].undo());
        assert!(!app.tabs[0].document.find(id).expect("the image").shadow);
    }

    /// SPEC 7.2's texture limit, for a pasted image: a clear message instead of
    /// a crash, and nothing added to the document.
    #[test]
    fn pasting_an_image_larger_than_the_texture_limit_is_refused() {
        let mut app = app();
        app.add_capture(RgbaImage::new(100, 100));
        app.max_texture_side = 32;

        app.paste_image_into_tab(RgbaImage::new(64, 16));

        assert!(
            app.tabs[0].document.annotations.is_empty(),
            "the too-large image was added anyway"
        );
        assert!(
            app.status.contains("larger than the 32 px texture limit"),
            "the status bar does not explain: {}",
            app.status
        );
    }

    // -----------------------------------------------------------------------
    // Auto-fit to the window (NEXT_STEPS step 13)
    // -----------------------------------------------------------------------

    /// A tab whose 2000 × 1500 image does not fit the 800 × 600 canvas the app
    /// reports before a real frame has run.
    fn app_with_a_big_tab() -> SnpLigaseApp {
        let mut app = app();
        app.add_capture(RgbaImage::new(2000, 1500));
        app
    }

    /// NEXT_STEPS step 13: a new tab starts with auto-fit equal to the setting.
    #[test]
    fn a_new_tab_takes_the_auto_fit_setting() {
        let mut app = app();
        let mut settings = app.settings.settings().clone();
        settings.auto_fit_to_window = false;
        app.settings.replace(settings);

        app.add_capture(RgbaImage::new(64, 64));

        assert!(
            !app.tabs[0].auto_fit,
            "a tab opened with the setting off must not auto-fit"
        );
    }

    /// NEXT_STEPS step 13: every manual zoom turns auto-fit off, for the tab it
    /// zoomed and no other. The Ctrl+wheel path is covered in `tools`.
    #[test]
    fn every_manual_zoom_turns_auto_fit_off_for_that_tab_only() {
        for action in [Action::ZoomIn, Action::ZoomOut, Action::ActualSize] {
            let mut app = app_with_a_big_tab();
            app.add_capture(RgbaImage::new(2000, 1500));
            app.active = 0;

            app.run(action.clone());

            assert!(
                !app.tabs[0].auto_fit,
                "{action:?} left auto-fit on for the tab it zoomed"
            );
            assert!(
                app.tabs[1].auto_fit,
                "{action:?} turned auto-fit off in the other tab too"
            );
        }
    }

    /// NEXT_STEPS step 13: Ctrl+1 and the View menu's row re-arm auto-fit and
    /// fit immediately; the row's off side just stands it down.
    #[test]
    fn fit_to_window_and_the_view_toggle_re_arm_auto_fit() {
        let mut app = app_with_a_big_tab();
        app.tabs[0].auto_fit = false;
        app.tabs[0].viewport.zoom = 3.0;

        app.run(Action::FitToWindow);

        assert!(app.tabs[0].auto_fit, "Ctrl+1 did not re-arm auto-fit");
        assert!(
            app.tabs[0].viewport.zoom < 1.0,
            "Ctrl+1 did not fit the 2000 px image: {}",
            app.tabs[0].viewport.zoom
        );
        assert!(
            app.tabs[0].viewport.fitted_to.is_some(),
            "Ctrl+1 did not record what it fitted, so the next frame would fit again"
        );

        // View > Auto-fit to Window, off...
        app.run(Action::AutoFit(false));
        assert!(
            !app.tabs[0].auto_fit,
            "the View row did not turn auto-fit off"
        );

        // ...and on again: it fits now.
        app.tabs[0].viewport.zoom = 3.0;
        app.run(Action::AutoFit(true));
        assert!(
            app.tabs[0].auto_fit,
            "the View row did not turn auto-fit on"
        );
        assert!(
            app.tabs[0].viewport.zoom < 1.0,
            "turning auto-fit on did not fit: {}",
            app.tabs[0].viewport.zoom
        );
        assert!(
            app.tabs[0].viewport.fitted_to.is_some(),
            "turning auto-fit on did not record what it fitted"
        );
    }

    /// NEXT_STEPS step 13 through the real canvas in a real frame: a resized
    /// window re-fits while auto-fit is on, a manual zoom stands it down, and
    /// Ctrl+1 arms it again.
    #[test]
    fn a_window_resize_re_fits_only_while_auto_fit_is_on() {
        let app = app_with_a_big_tab();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1400.0, 1000.0))
            .build_ui_state(|ui, app: &mut SnpLigaseApp| app.frame_ui(ui), app);
        harness.run();
        let fitted = harness.state().tabs[0].viewport.zoom;
        assert!(fitted < 1.0, "the 2000 px image was not fitted: {fitted}");

        // A smaller window re-fits.
        harness.set_size(egui::vec2(800.0, 700.0));
        harness.run();
        let smaller = harness.state().tabs[0].viewport.zoom;
        assert!(
            smaller < fitted,
            "the resize did not re-fit: {fitted} -> {smaller}"
        );

        // A manual zoom stands auto-fit down, so the next resize leaves the
        // view alone.
        harness.state_mut().run(Action::ZoomIn);
        harness.run();
        let zoomed = harness.state().tabs[0].viewport.zoom;
        harness.set_size(egui::vec2(600.0, 500.0));
        harness.run();
        assert_eq!(
            harness.state().tabs[0].viewport.zoom,
            zoomed,
            "a resize moved the zoom after a manual zoom"
        );

        // Ctrl+1 re-arms it and fits; the next resize fits again.
        harness.state_mut().run(Action::FitToWindow);
        harness.run();
        let refitted = harness.state().tabs[0].viewport.zoom;
        assert!(
            harness.state().tabs[0].auto_fit,
            "Ctrl+1 did not re-arm auto-fit in the real frame"
        );
        harness.set_size(egui::vec2(1300.0, 950.0));
        harness.run();
        let bigger = harness.state().tabs[0].viewport.zoom;
        assert!(
            bigger > refitted,
            "the resize after Ctrl+1 did not re-fit: {refitted} -> {bigger}"
        );
    }
}
