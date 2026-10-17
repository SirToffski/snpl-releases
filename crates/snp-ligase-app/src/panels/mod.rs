//! The panels: menu bar, toolbar, tab strip and status bar (SPEC section 8.2).
//!
//! Every panel only *collects* [`Action`]s. The app runs them afterwards, so a
//! menu item, a toolbar button and a keyboard shortcut are the same thing and
//! cannot drift apart (SPEC 8.1: every property is reachable from the menu bar
//! as well as from the panel).

pub mod colour;
pub mod properties;
pub mod settings;
pub mod shortcuts;

use egui::{MenuBar, Panel, RichText, Ui};
use snp_ligase_core::model::Rgba;

use crate::tab::Tab;
use crate::tool::{CORNER_RADII, STROKE_WIDTHS, TOOL_MENU, Tool, tool_for_row};

/// The four capture modes of SPEC 5.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    Region,
    CurrentMonitor,
    AllMonitors,
    ActiveWindow,
    /// Repeats the region the user last dragged out.
    RepeatLastRegion,
}

impl CaptureMode {
    /// The rows of the Capture menu and of the toolbar dropdown (SPEC 8.3).
    pub const ALL: [Self; 5] = [
        Self::Region,
        Self::CurrentMonitor,
        Self::AllMonitors,
        Self::ActiveWindow,
        Self::RepeatLastRegion,
    ];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Region => "Region",
            Self::CurrentMonitor => "Current Monitor",
            Self::AllMonitors => "All Monitors",
            Self::ActiveWindow => "Active Window",
            Self::RepeatLastRegion => "Repeat Last Region",
        }
    }

    /// The global hotkey SPEC 6.1 gives each mode.
    #[must_use]
    pub fn hotkey(self) -> &'static str {
        match self {
            Self::Region => "Print Screen",
            Self::CurrentMonitor => "Ctrl+Print Screen",
            Self::AllMonitors => "Ctrl+Shift+Print Screen",
            Self::ActiveWindow => "Shift+Print Screen",
            Self::RepeatLastRegion => "",
        }
    }
}

/// Everything the UI can ask the app to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Open,
    PasteAsNewTab,
    /// Ctrl+Shift+V / File > "Paste Image into This Tab": add the clipboard
    /// image to the current tab (NEXT_STEPS step 11).
    PasteIntoTab,
    Save,
    SaveAs,
    SaveAll,
    CopyImage,
    CloseTab(usize),
    CloseAllTabs,
    Settings,
    Exit,
    Undo,
    Redo,
    Duplicate,
    Delete,
    SelectAll,
    BringToFront,
    BringForward,
    SendBackward,
    SendToBack,
    Crop,
    Capture(CaptureMode),
    Delay(u8),
    Tool(Tool),
    ZoomIn,
    ZoomOut,
    ActualSize,
    FitToWindow,
    /// View > Auto-fit to Window (NEXT_STEPS step 13): the active tab's
    /// auto-fit state. On fits it immediately; off leaves the zoom alone.
    AutoFit(bool),
    ShowToolLabels(bool),
    ShowProperties(bool),
    KeyboardShortcuts,
    OpenLogFolder,
    About,
    /// Switch to another tab.
    SelectTab(usize),
    /// The three answers to "this tab has unsaved changes" (SPEC 7.1).
    ConfirmSave,
    ConfirmDiscard,
    ConfirmCancel,
    /// Right-click a point handle > "Delete point" (SPEC 7.3).
    DeletePoint(snp_ligase_core::edit::HandleKind),
    /// One change to a style property, from the Annotation menu or the
    /// properties panel. Both go through `Action` so they cannot drift apart.
    Style(StyleProperty),
    /// "Stroke Colour…" / "Fill Colour…": open the colour picker window
    /// (SPEC 8.3).
    PickColour(ColourField),
    /// "Font…": open the font list window (SPEC 8.3).
    PickFont,
    /// "Stroke Width > Custom…" / "Corner Radius > Custom…": open the small
    /// numeric window (SPEC 8.3).
    CustomValue(CustomField),
    /// "Next Number…": open the small window that edits `doc.next_number`
    /// (SPEC 7.6).
    PickNextNumber,
    /// "Reset Numbering to 1" (SPEC 7.6).
    SetNextNumber(u32),
    /// Esc: cancel the gesture in progress and put the document back.
    CancelGesture,
    /// Ctrl+Tab / Ctrl+Shift+Tab (SPEC 7.1). Positive moves towards the last
    /// tab, negative towards the first.
    SwitchTab(i32),
    /// Arrow keys: nudge the selection by (dx, dy) pixels (SPEC 7.4).
    Nudge(f32, f32),
    /// `Backspace` while the Shape tool is collecting vertices: remove the last
    /// one (SPEC 7.3).
    ShapeRemoveLastPoint,
    /// `Enter` while the Shape tool is collecting vertices: close the shape
    /// (SPEC 7.3).
    ShapeFinish,
    /// `Enter` or the "Apply crop" button while the Crop tool has a rectangle:
    /// crop the image to it (SPEC 7.3).
    ApplyCrop,
    /// `Ctrl+Enter` or a click outside the text editor: keep the text
    /// (SPEC 7.5).
    CommitText,
    /// `Esc` in the text editor: put the annotation back (SPEC 7.5).
    CancelText,
    /// The Settings dialog's OK: store the draft and apply its side effects
    /// (SPEC section 10).
    ApplySettings,
    /// The Settings dialog's Cancel (or its X).
    CancelSettings,
    /// The Settings dialog's "Browse…" for the default save folder.
    PickSaveFolder,
    /// The Settings dialog's "Record…" next to a hotkey field (NEXT_STEPS
    /// step 7): unregister the global keys and read the next key press.
    RecordHotkey(CaptureMode),
    /// The Settings dialog's "Test": register the draft's hotkeys for real to
    /// see whether another program owns them, then put the stored ones back
    /// (NEXT_STEPS step 7 item 3).
    TestHotkeys,
}

/// Which colour the picker window is choosing (SPEC 8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColourField {
    /// A path, rectangle or shape's outline.
    Stroke,
    /// A rectangle or shape's fill.
    Fill,
    /// Text: the glyphs.
    Text,
    /// A text annotation's box.
    TextBackground,
    /// A text annotation's box outline (NEXT_STEPS step 8).
    TextOutline,
    /// A number badge.
    Number,
    /// A highlight.
    Highlight,
}

/// A numeric value the Annotation menu edits in a small window: the "Custom…"
/// rows of SPEC 8.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomField {
    StrokeWidth,
    CornerRadius,
    FontSize,
    NumberSize,
    BlurStrength,
    /// The text box outline's width (NEXT_STEPS step 8).
    TextOutlineWidth,
}

impl CustomField {
    /// The label the window and the panel show.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::StrokeWidth => "Stroke width",
            Self::CornerRadius => "Corner radius",
            Self::FontSize => "Text size",
            Self::NumberSize => "Number size",
            Self::BlurStrength => "Blur strength",
            Self::TextOutlineWidth => "Text outline width",
        }
    }

    /// The value this field edits in a style.
    #[must_use]
    pub fn value_of(self, style: &crate::tool::ToolStyle) -> f32 {
        match self {
            Self::StrokeWidth => style.stroke_width,
            Self::CornerRadius => style.corner_radius,
            Self::FontSize => style.font_size,
            Self::NumberSize => style.number_radius,
            Self::BlurStrength => style.blur_radius,
            Self::TextOutlineWidth => style
                .text_outline
                .map_or(snp_ligase_core::model::defaults::STROKE_WIDTH, |stroke| {
                    stroke.width
                }),
        }
    }

    /// The property change for a new value.
    #[must_use]
    pub fn property(self, value: f32) -> StyleProperty {
        match self {
            Self::StrokeWidth => StyleProperty::StrokeWidth(value),
            Self::CornerRadius => StyleProperty::CornerRadius(value),
            Self::FontSize => StyleProperty::FontSize(value),
            Self::NumberSize => StyleProperty::NumberSize(value),
            Self::BlurStrength => StyleProperty::BlurRadius(value),
            Self::TextOutlineWidth => StyleProperty::TextOutlineWidth(value),
        }
    }

    /// The sensible range for the numeric field.
    #[must_use]
    pub fn range(self) -> std::ops::RangeInclusive<f32> {
        match self {
            Self::StrokeWidth => 0.1..=200.0,
            Self::CornerRadius => 0.0..=500.0,
            Self::FontSize => 4.0..=400.0,
            Self::NumberSize => 4.0..=200.0,
            Self::BlurStrength => 1.0..=200.0,
            // The same range as the stroke width: the outline is one.
            Self::TextOutlineWidth => 0.1..=200.0,
        }
    }
}

/// One annotation property, as the Annotation menu names it (SPEC 8.3).
#[derive(Debug, Clone, PartialEq)]
pub enum StyleProperty {
    StrokeColour(Rgba),
    StrokeWidth(f32),
    Fill(Option<Rgba>),
    CornerRadius(f32),
    Shadow(bool),
    ArrowEnds(crate::tool::ArrowEnds),
    Curved(bool),
    /// Text: the glyph colour.
    TextColour(Rgba),
    /// Text: the box background, `None` for "None".
    TextBackground(Option<Rgba>),
    /// Text: the whole box outline, `None` for "None" (NEXT_STEPS step 8).
    TextOutline(Option<snp_ligase_core::model::Stroke>),
    /// Text: the outline colour.
    TextOutlineColour(Rgba),
    /// Text: the outline width, SPEC 8.3's "Text Outline Width".
    TextOutlineWidth(f32),
    FontFamily(String),
    FontSize(f32),
    Bold(bool),
    Italic(bool),
    /// Number: the badge fill colour.
    NumberFill(Rgba),
    /// Number: the badge radius, SPEC 8.4's "Size".
    NumberSize(f32),
    /// Number: the badge's own value (SPEC 7.6).
    NumberValue(u32),
    HighlightColour(Rgba),
    /// Blur: the radius in pixels.
    BlurRadius(f32),
}

/// What the menus need to know about the current state.
#[derive(Debug, Clone)]
pub struct MenuState {
    pub can_undo: bool,
    pub can_redo: bool,
    pub has_tab: bool,
    pub has_selection: bool,
    pub tool: Tool,
    pub show_properties: bool,
    pub show_tool_labels: bool,
    /// The active tab's auto-fit state, for the View menu's check mark
    /// (NEXT_STEPS step 13). False with no tab open.
    pub auto_fit: bool,
    /// True once the font system has loaded (SPEC 7.5): the Text tool is
    /// disabled with a "Loading fonts…" label until then.
    pub fonts_ready: bool,
    /// True when the Capture > Delay submenu is open, kept so the check mark
    /// follows the setting.
    pub delay_seconds: u8,
    /// Which tab `Ctrl+W` closes.
    pub active_tab: usize,
    /// The style the Annotation menu's rows apply to: the selection's own style
    /// with something selected, the active tool's style otherwise (SPEC 7.7).
    pub style: Option<crate::tool::ToolStyle>,
    /// Which controls the current selection or tool can take (SPEC 8.4).
    pub nodes: Vec<crate::tool::StyleNode>,
    /// The hotkey text configured for each capture mode, in [`CaptureMode::ALL`]
    /// order (NEXT_STEPS step 9d).
    ///
    /// The Capture menu, the toolbar's "New capture" dropdown and the Keyboard
    /// Shortcuts window all read this, so they cannot disagree with what
    /// Settings holds. It used to be `CaptureMode::hotkey()`, which only knows
    /// the SPEC 6.1 defaults, so a rebind never reached the menu.
    pub capture_hotkeys: Vec<(CaptureMode, String)>,
}

impl MenuState {
    /// The hotkey text configured for one capture mode (NEXT_STEPS step 9d).
    ///
    /// Empty means nothing is bound. The SPEC 6.1 default is the fallback for
    /// a mode that is missing from the table, which only a hand-built state can
    /// be: [`SnpLigaseApp::menu_state`] fills all five.
    fn hotkey(&self, mode: CaptureMode) -> &str {
        self.capture_hotkeys
            .iter()
            .find(|(entry, _)| *entry == mode)
            .map_or_else(|| mode.hotkey(), |(_, text)| text.as_str())
    }

    /// Whether the shadow row is currently on.
    fn shadow(&self) -> bool {
        self.style.as_ref().is_some_and(|s| s.shadow)
    }

    /// Whether the selection or tool is a text annotation.
    fn annotation_is_text(&self) -> bool {
        self.has(&crate::tool::StyleNode::Font)
    }

    /// Whether it is a number badge.
    fn annotation_is_number(&self) -> bool {
        self.has(&crate::tool::StyleNode::NumberFill)
    }

    /// Whether it is a highlight.
    fn annotation_is_highlight(&self) -> bool {
        self.has(&crate::tool::StyleNode::HighlightColour)
    }

    /// Whether it is a blur.
    fn annotation_is_blur(&self) -> bool {
        self.has(&crate::tool::StyleNode::BlurStrength)
    }

    /// Whether the text is currently bold or italic.
    fn bold(&self) -> bool {
        self.style.as_ref().is_some_and(|s| s.bold)
    }

    fn italic(&self) -> bool {
        self.style.as_ref().is_some_and(|s| s.italic)
    }

    /// Whether the selection or tool takes a stroke at all.
    fn annotation_has_stroke(&self) -> bool {
        self.has(&crate::tool::StyleNode::Stroke)
    }

    /// Whether the text outline is currently Solid (NEXT_STEPS step 8).
    fn text_outline(&self) -> bool {
        self.style
            .as_ref()
            .is_some_and(|style| style.text_outline.is_some())
    }

    /// Whether it can be filled.
    fn annotation_has_fill(&self) -> bool {
        self.has(&crate::tool::StyleNode::Fill)
    }

    /// Whether its corners can be rounded.
    fn annotation_has_corner_radius(&self) -> bool {
        self.has(&crate::tool::StyleNode::CornerRadius)
    }

    /// Whether it is a line or an arrow.
    fn annotation_is_path(&self) -> bool {
        self.has(&crate::tool::StyleNode::ArrowEnds)
    }

    fn has(&self, node: &crate::tool::StyleNode) -> bool {
        self.nodes.contains(node)
    }
}

/// The menu bar (SPEC section 8.3).
pub fn menu_bar(ui: &mut Ui, state: &MenuState, actions: &mut Vec<Action>) {
    Panel::top("menu_bar").show(ui, |ui| {
        MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| file(ui, state, actions));
            ui.menu_button("Edit", |ui| edit(ui, state, actions));
            ui.menu_button("Capture", |ui| capture(ui, state, actions));
            ui.menu_button("Tools", |ui| tools(ui, state, actions));
            ui.menu_button("Annotation", |ui| annotation(ui, state, actions));
            ui.menu_button("View", |ui| view(ui, state, actions));
            ui.menu_button("Help", |ui| help(ui, actions));
        });
    });
}

/// File menu (SPEC 8.3).
fn file(ui: &mut Ui, state: &MenuState, actions: &mut Vec<Action>) {
    item(ui, "Open…", "Ctrl+O", Action::Open, true, actions);
    item(
        ui,
        "Paste Image as New Tab",
        "Ctrl+V",
        Action::PasteAsNewTab,
        true,
        actions,
    );
    item(
        ui,
        "Paste Image into This Tab",
        "Ctrl+Shift+V",
        Action::PasteIntoTab,
        state.has_tab,
        actions,
    );
    ui.separator();
    item(ui, "Save", "Ctrl+S", Action::Save, state.has_tab, actions);
    item(
        ui,
        "Save As…",
        "Ctrl+Shift+S",
        Action::SaveAs,
        state.has_tab,
        actions,
    );
    item(ui, "Save All", "", Action::SaveAll, state.has_tab, actions);
    item(
        ui,
        "Copy Image to Clipboard",
        "Ctrl+C",
        Action::CopyImage,
        state.has_tab,
        actions,
    );
    ui.separator();
    item(
        ui,
        "Close Tab",
        "Ctrl+W",
        Action::CloseTab(state.active_tab),
        state.has_tab,
        actions,
    );
    item(
        ui,
        "Close All Tabs",
        "",
        Action::CloseAllTabs,
        state.has_tab,
        actions,
    );
    ui.separator();
    item(ui, "Settings…", "", Action::Settings, true, actions);
    item(ui, "Exit", "", Action::Exit, true, actions);
}

/// Edit menu (SPEC 8.3 and 7.4).
fn edit(ui: &mut Ui, state: &MenuState, actions: &mut Vec<Action>) {
    item(ui, "Undo", "Ctrl+Z", Action::Undo, state.can_undo, actions);
    item(ui, "Redo", "Ctrl+Y", Action::Redo, state.can_redo, actions);
    ui.separator();
    item(
        ui,
        "Duplicate",
        "Ctrl+D",
        Action::Duplicate,
        state.has_selection,
        actions,
    );
    item(
        ui,
        "Delete",
        "Del",
        Action::Delete,
        state.has_selection,
        actions,
    );
    item(
        ui,
        "Select All",
        "Ctrl+A",
        Action::SelectAll,
        state.has_tab,
        actions,
    );
    ui.separator();
    // Items that do not apply are disabled, not hidden (SPEC 8.3).
    item(
        ui,
        "Bring to Front",
        "",
        Action::BringToFront,
        state.has_selection,
        actions,
    );
    item(
        ui,
        "Bring Forward",
        "",
        Action::BringForward,
        state.has_selection,
        actions,
    );
    item(
        ui,
        "Send Backward",
        "",
        Action::SendBackward,
        state.has_selection,
        actions,
    );
    item(
        ui,
        "Send to Back",
        "",
        Action::SendToBack,
        state.has_selection,
        actions,
    );
    ui.separator();
    item(ui, "Crop", "", Action::Crop, state.has_selection, actions);
}

/// The Capture menu's rows, built before they are drawn (SPEC 8.3).
///
/// Each row is the mode, the action, and whether it applies — with the
/// **user's own** hotkey in its text (NEXT_STEPS step 9d): the same text the
/// Settings dialog's field and the Keyboard Shortcuts window show, not the
/// SPEC 6.1 default `CaptureMode::hotkey()` knows. After a rebind the menu used
/// to keep advertising "Print Screen".
#[must_use]
pub fn capture_rows(state: &MenuState) -> Vec<(String, Action, bool)> {
    CaptureMode::ALL
        .iter()
        .map(|mode| {
            let label = capture_row_label(*mode, state.hotkey(*mode));
            (label, Action::Capture(*mode), true)
        })
        .collect()
}

/// The text one Capture menu row shows: the mode, then its hotkey (SPEC 8.3).
///
/// A mode with nothing bound shows just its name, because an empty shortcut
/// next to a label reads like a mistake.
#[must_use]
pub fn capture_row_label(mode: CaptureMode, hotkey: &str) -> String {
    match hotkey.trim() {
        "" => mode.label().to_owned(),
        hotkey => format!("{}    {hotkey}", mode.label()),
    }
}

/// The toolbar's "New capture" hover text, naming the region hotkey the user
/// configured (NEXT_STEPS step 9d).
#[must_use]
pub fn new_capture_hover(region_hotkey: &str) -> String {
    match region_hotkey.trim() {
        "" => "Capture a new image".to_owned(),
        hotkey => format!("Capture a new image ({hotkey} for a region)"),
    }
}

/// Capture menu (SPEC 8.3). Each item shows its global hotkey.
fn capture(ui: &mut Ui, state: &MenuState, actions: &mut Vec<Action>) {
    for (label, action, enabled) in capture_rows(state) {
        // The hotkey is already in the label, so no shortcut is passed on.
        item(ui, &label, "", action, enabled, actions);
    }
    ui.separator();
    ui.label("Delay");
    for seconds in [0u8, 1, 2, 3, 5] {
        let label = if seconds == 0 {
            "None".to_owned()
        } else {
            format!("{seconds} s")
        };
        if ui
            .selectable_label(state.delay_seconds == seconds, label)
            .clicked()
        {
            actions.push(Action::Delay(seconds));
            ui.close();
        }
    }
}

/// Tools menu (SPEC 8.3): one item per tool with its key, the active one
/// checked.
///
/// A tool whose milestone has not built it yet is disabled and says so; the
/// Text tool is disabled with "Loading fonts…" until the font system is ready
/// (SPEC 7.5).
fn tools(ui: &mut Ui, state: &MenuState, actions: &mut Vec<Action>) {
    for (index, (name, key, built)) in TOOL_MENU.iter().enumerate() {
        let Some(chosen) = tool_for_row(index) else {
            continue;
        };
        let blocked = tool_blocked(name, *built, state.fonts_ready);
        let response = ui
            .selectable_label(Some(chosen) == Some(state.tool), format!("{name}  ({key})"))
            .on_hover_text(tool_hover_text(name, key, blocked));
        if !response.clicked() || blocked.is_some() {
            continue;
        }
        actions.push(Action::Tool(chosen));
        ui.close();
    }
}

/// Why a tool cannot be picked right now, if it cannot.
///
/// `None` means it can. The words are SPEC 8.1's: a disabled control says what
/// is wrong, not just that it is disabled.
#[must_use]
pub fn tool_blocked(name: &str, built: bool, fonts_ready: bool) -> Option<&'static str> {
    if name == "Text" && !fonts_ready {
        Some("Loading fonts…")
    } else if !built {
        Some("not built yet (M4)")
    } else {
        None
    }
}

/// The hover text for a tool row: what it is, and why it is disabled when it is.
fn tool_hover_text(name: &str, key: &str, blocked: Option<&str>) -> String {
    match blocked {
        Some(reason) => format!("{name} tool ({key}) — {reason}"),
        None => format!("{name} tool ({key})"),
    }
}

/// One row of the Annotation menu, built before it is drawn.
///
/// Keeping the rows as data is what lets the test below check that every row
/// SPEC 8.3 gives this milestone really does something (R14) without a window.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuRow {
    Item {
        label: &'static str,
        action: Action,
        enabled: bool,
    },
    Submenu {
        label: &'static str,
        rows: Vec<(String, Action)>,
        enabled: bool,
    },
    Separator,
}

/// The "Stroke Width" rows, ending in "Custom…" (SPEC 8.3).
#[must_use]
pub fn stroke_width_rows() -> Vec<(String, Action)> {
    STROKE_WIDTHS
        .iter()
        .map(|width| {
            (
                format!("{width} px"),
                Action::Style(StyleProperty::StrokeWidth(*width)),
            )
        })
        .chain(std::iter::once((
            "Custom…".to_owned(),
            Action::CustomValue(CustomField::StrokeWidth),
        )))
        .collect()
}

/// The "Corner Radius" rows, ending in "Custom…" (SPEC 8.3).
#[must_use]
pub fn corner_radius_rows() -> Vec<(String, Action)> {
    CORNER_RADII
        .iter()
        .map(|radius| {
            (
                format!("{radius} px"),
                Action::Style(StyleProperty::CornerRadius(*radius)),
            )
        })
        .chain(std::iter::once((
            "Custom…".to_owned(),
            Action::CustomValue(CustomField::CornerRadius),
        )))
        .collect()
}

/// The "Fill" rows (SPEC 8.3).
#[must_use]
pub fn fill_rows() -> Vec<(String, Action)> {
    vec![
        ("None".to_owned(), Action::Style(StyleProperty::Fill(None))),
        (
            "Solid".to_owned(),
            Action::Style(StyleProperty::Fill(Some([255, 255, 255, 255]))),
        ),
    ]
}

/// The "Arrow Ends" rows (SPEC 8.3).
#[must_use]
pub fn arrow_ends_rows() -> Vec<(String, Action)> {
    crate::tool::ArrowEnds::ALL
        .iter()
        .map(|ends| {
            (
                ends.label().to_owned(),
                Action::Style(StyleProperty::ArrowEnds(*ends)),
            )
        })
        .collect()
}

/// The "Line Style" rows (SPEC 8.3).
#[must_use]
pub fn line_style_rows() -> Vec<(String, Action)> {
    vec![
        (
            "Curved".to_owned(),
            Action::Style(StyleProperty::Curved(true)),
        ),
        (
            "Straight Segments".to_owned(),
            Action::Style(StyleProperty::Curved(false)),
        ),
    ]
}

/// The "Text Size" rows (SPEC 8.3), ending in "Custom…".
#[must_use]
pub fn font_size_rows() -> Vec<(String, Action)> {
    let mut rows: Vec<(String, Action)> = crate::tool::FONT_SIZES
        .iter()
        .map(|size| {
            (
                format!("{size} px"),
                Action::Style(StyleProperty::FontSize(*size)),
            )
        })
        .collect();
    rows.push((
        "Custom…".to_owned(),
        Action::CustomValue(CustomField::FontSize),
    ));
    rows
}

/// The "Text Background" rows (SPEC 8.3).
#[must_use]
pub fn text_background_rows() -> Vec<(String, Action)> {
    vec![
        (
            "None".to_owned(),
            Action::Style(StyleProperty::TextBackground(None)),
        ),
        (
            "Solid".to_owned(),
            Action::Style(StyleProperty::TextBackground(Some(
                snp_ligase_core::model::defaults::TEXT_BACKGROUND,
            ))),
        ),
    ]
}

/// The "Text Outline" rows (SPEC 8.3, NEXT_STEPS step 8): Solid starts from
/// SPEC 7.7's stroke, `#E81123` at 4 px.
#[must_use]
pub fn text_outline_rows() -> Vec<(String, Action)> {
    vec![
        (
            "None".to_owned(),
            Action::Style(StyleProperty::TextOutline(None)),
        ),
        (
            "Solid".to_owned(),
            Action::Style(StyleProperty::TextOutline(Some(
                snp_ligase_core::model::defaults::stroke(),
            ))),
        ),
    ]
}

/// The "Text Outline Width" rows (SPEC 8.3), ending in "Custom…". The same
/// width list as "Stroke Width".
#[must_use]
pub fn text_outline_width_rows() -> Vec<(String, Action)> {
    STROKE_WIDTHS
        .iter()
        .map(|width| {
            (
                format!("{width} px"),
                Action::Style(StyleProperty::TextOutlineWidth(*width)),
            )
        })
        .chain(std::iter::once((
            "Custom…".to_owned(),
            Action::CustomValue(CustomField::TextOutlineWidth),
        )))
        .collect()
}

/// The "Blur Strength" rows (SPEC 8.3), ending in "Custom…".
#[must_use]
pub fn blur_strength_rows() -> Vec<(String, Action)> {
    let mut rows: Vec<(String, Action)> = crate::tool::BLUR_STRENGTHS
        .iter()
        .map(|(name, value)| {
            (
                format!("{name} ({value})"),
                Action::Style(StyleProperty::BlurRadius(*value)),
            )
        })
        .collect();
    rows.push((
        "Custom…".to_owned(),
        Action::CustomValue(CustomField::BlurStrength),
    ));
    rows
}

/// The Annotation menu's rows for the current selection or tool (SPEC 8.3).
///
/// Every row is either a real action, a disabled row that does not apply, or a
/// disabled row for a later milestone. None of them is a clickable row that
/// only prints a status message.
#[must_use]
pub fn annotation_rows(state: &MenuState) -> Vec<MenuRow> {
    let applies = state.has_selection || state.has_tab;
    let stroke = applies && state.annotation_has_stroke();
    let fill = applies && state.annotation_has_fill();
    let radius = applies && state.annotation_has_corner_radius();
    let path = applies && state.annotation_is_path();
    let text = applies && state.annotation_is_text();
    let number = applies && state.annotation_is_number();
    let highlight = applies && state.annotation_is_highlight();
    let blur = applies && state.annotation_is_blur();

    vec![
        MenuRow::Item {
            label: "Stroke Colour…",
            action: Action::PickColour(ColourField::Stroke),
            enabled: stroke,
        },
        MenuRow::Submenu {
            label: "Stroke Width",
            rows: stroke_width_rows(),
            enabled: stroke,
        },
        MenuRow::Submenu {
            label: "Fill",
            rows: fill_rows(),
            enabled: fill,
        },
        MenuRow::Item {
            label: "Fill Colour…",
            action: Action::PickColour(ColourField::Fill),
            enabled: fill,
        },
        MenuRow::Item {
            label: "Shadow",
            action: Action::Style(StyleProperty::Shadow(!state.shadow())),
            // SPEC 4.2: a shadow under an opaque effect would never be seen, so
            // Highlight and Blur do not take one.
            enabled: applies && state.has(&crate::tool::StyleNode::Shadow),
        },
        MenuRow::Submenu {
            label: "Arrow Ends",
            rows: arrow_ends_rows(),
            enabled: path,
        },
        MenuRow::Submenu {
            label: "Line Style",
            rows: line_style_rows(),
            enabled: path,
        },
        MenuRow::Submenu {
            label: "Corner Radius",
            rows: corner_radius_rows(),
            enabled: radius,
        },
        MenuRow::Separator,
        MenuRow::Item {
            label: "Font…",
            action: Action::PickFont,
            enabled: text,
        },
        MenuRow::Submenu {
            label: "Text Size",
            rows: font_size_rows(),
            enabled: text,
        },
        MenuRow::Item {
            label: "Bold",
            action: Action::Style(StyleProperty::Bold(!state.bold())),
            enabled: text,
        },
        MenuRow::Item {
            label: "Italic",
            action: Action::Style(StyleProperty::Italic(!state.italic())),
            enabled: text,
        },
        MenuRow::Item {
            label: "Text Colour…",
            action: Action::PickColour(ColourField::Text),
            enabled: text,
        },
        MenuRow::Submenu {
            label: "Text Background",
            rows: text_background_rows(),
            enabled: text,
        },
        MenuRow::Item {
            label: "Text Background Colour…",
            action: Action::PickColour(ColourField::TextBackground),
            enabled: text,
        },
        MenuRow::Submenu {
            label: "Text Outline",
            rows: text_outline_rows(),
            enabled: text,
        },
        MenuRow::Item {
            label: "Text Outline Colour…",
            action: Action::PickColour(ColourField::TextOutline),
            // The colour and width rows only make sense once the outline is
            // Solid (NEXT_STEPS step 8).
            enabled: text && state.text_outline(),
        },
        MenuRow::Submenu {
            label: "Text Outline Width",
            rows: text_outline_width_rows(),
            enabled: text && state.text_outline(),
        },
        MenuRow::Separator,
        MenuRow::Item {
            label: "Next Number…",
            action: Action::PickNextNumber,
            enabled: number,
        },
        MenuRow::Item {
            label: "Reset Numbering to 1",
            action: Action::SetNextNumber(1),
            enabled: number,
        },
        MenuRow::Item {
            // SPEC 8.4 lists "Colour" for a number badge; SPEC 8.1 rule 2 wants
            // every property reachable from the menu, so it gets a row here.
            label: "Number Colour…",
            action: Action::PickColour(ColourField::Number),
            enabled: number,
        },
        MenuRow::Separator,
        MenuRow::Item {
            label: "Highlight Colour…",
            action: Action::PickColour(ColourField::Highlight),
            enabled: highlight,
        },
        MenuRow::Submenu {
            label: "Blur Strength",
            rows: blur_strength_rows(),
            enabled: blur,
        },
    ]
}

/// Annotation menu (SPEC 8.3): applies to the selection, or to the active
/// tool's defaults.
///
/// Every row produces an [`Action`], the same one the properties panel
/// produces, so undo, defaults and multi-selection behave identically whichever
/// way the property is reached. Rows that do not apply to the current selection
/// or tool are disabled, not hidden (SPEC 8.3).
fn annotation(ui: &mut Ui, state: &MenuState, actions: &mut Vec<Action>) {
    for row in annotation_rows(state) {
        match row {
            MenuRow::Item {
                label,
                action,
                enabled,
            } => {
                let mut response = ui.add_enabled(enabled, egui::Button::new(label));
                if !enabled {
                    response =
                        response.on_disabled_hover_text("Nothing to apply this to right now");
                }
                if response.clicked() {
                    actions.push(action);
                    ui.close();
                }
            }
            MenuRow::Submenu {
                label,
                rows,
                enabled,
            } => submenu(ui, label, &rows, enabled, actions),
            MenuRow::Separator => {
                ui.separator();
            }
        }
    }
}

/// View menu (SPEC 8.3).
fn view(ui: &mut Ui, state: &MenuState, actions: &mut Vec<Action>) {
    item(
        ui,
        "Zoom In",
        "Ctrl++",
        Action::ZoomIn,
        state.has_tab,
        actions,
    );
    item(
        ui,
        "Zoom Out",
        "Ctrl+-",
        Action::ZoomOut,
        state.has_tab,
        actions,
    );
    item(
        ui,
        "Actual Size",
        "Ctrl+0",
        Action::ActualSize,
        state.has_tab,
        actions,
    );
    item(
        ui,
        "Fit to Window",
        "Ctrl+1",
        Action::FitToWindow,
        state.has_tab,
        actions,
    );
    ui.add_enabled_ui(state.has_tab, |ui| {
        check(
            ui,
            "Auto-fit to Window",
            state.auto_fit,
            Action::AutoFit(!state.auto_fit),
            actions,
        );
    });
    ui.separator();
    check(
        ui,
        "Show Tool Labels",
        state.show_tool_labels,
        Action::ShowToolLabels(!state.show_tool_labels),
        actions,
    );
    check(
        ui,
        "Show Properties Panel",
        state.show_properties,
        Action::ShowProperties(!state.show_properties),
        actions,
    );
}

/// Help menu (SPEC 8.3).
fn help(ui: &mut Ui, actions: &mut Vec<Action>) {
    item(
        ui,
        "Keyboard Shortcuts",
        "F1",
        Action::KeyboardShortcuts,
        true,
        actions,
    );
    item(
        ui,
        "Open Log Folder",
        "",
        Action::OpenLogFolder,
        true,
        actions,
    );
    item(ui, "About", "", Action::About, true, actions);
}

/// One menu row: label, its shortcut, and whether it applies.
///
/// The shortcut is in the row's own text, because a shortcut has to be
/// discoverable (SPEC 8.1 rule 5).
fn item(
    ui: &mut Ui,
    label: &str,
    shortcut: &str,
    action: Action,
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    // SPEC 8.1 rule 5: a checkable row shows its state in words, and the
    // tooltip carries the same words for anyone reading it out of context.
    let text = if shortcut.is_empty() {
        label.to_owned()
    } else {
        format!("{label}    {shortcut}")
    };
    let mut response = ui.add_enabled(enabled, egui::Button::new(text));
    if !enabled {
        response = response.on_disabled_hover_text("Nothing to apply this to right now");
    }
    if response.clicked() {
        actions.push(action);
        ui.close();
    }
}

/// One checkable menu row.
fn check(ui: &mut Ui, label: &str, checked: bool, action: Action, actions: &mut Vec<Action>) {
    if ui
        .selectable_label(checked, label)
        .on_hover_text(label)
        .clicked()
    {
        actions.push(action);
        ui.close();
    }
}

/// One menu row that opens a sub-menu of labelled rows (SPEC 8.3).
///
/// The rows are words, not icons (SPEC 8.1 rule 3), and when the row does not
/// apply it is **disabled rather than hidden**, so the menu does not change
/// shape under the user.
fn submenu(
    ui: &mut Ui,
    label: &str,
    rows: &[(String, Action)],
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    if !enabled {
        ui.add_enabled(false, egui::Button::new(label))
            .on_disabled_hover_text("Nothing to apply this to right now");
        return;
    }

    ui.menu_button(label, |ui| {
        for (name, action) in rows {
            if ui.button(name).clicked() {
                actions.push(action.clone());
                ui.close();
            }
        }
    });
}

/// The toolbar (SPEC 8.2): common actions as text buttons, with "New capture" as
/// a dropdown listing the capture modes.
pub fn toolbar(ui: &mut Ui, state: &MenuState, actions: &mut Vec<Action>) {
    Panel::top("toolbar").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.menu_button("New capture ▾", |ui| {
                for mode in [
                    CaptureMode::Region,
                    CaptureMode::CurrentMonitor,
                    CaptureMode::AllMonitors,
                    CaptureMode::ActiveWindow,
                ] {
                    let response = ui.selectable_label(false, mode.label()).on_hover_text(
                        match state.hotkey(mode) {
                            "" => mode.label().to_owned(),
                            hotkey => format!("{} ({hotkey})", mode.label()),
                        },
                    );
                    if response.clicked() {
                        actions.push(Action::Capture(mode));
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text(new_capture_hover(state.hotkey(CaptureMode::Region)));

            if ui
                .button("Open")
                .on_hover_text("Open an image (Ctrl+O)")
                .clicked()
            {
                actions.push(Action::Open);
            }
            if ui.button("Save").on_hover_text("Save (Ctrl+S)").clicked() {
                actions.push(Action::Save);
            }
            if ui
                .button("Copy")
                .on_hover_text("Copy the flattened image to the clipboard (Ctrl+C)")
                .clicked()
            {
                actions.push(Action::CopyImage);
            }
            ui.add_space(12.0);
            if ui
                .add_enabled(state.can_undo, egui::Button::new("Undo"))
                .on_hover_text("Undo (Ctrl+Z)")
                .clicked()
            {
                actions.push(Action::Undo);
            }
            if ui
                .add_enabled(state.can_redo, egui::Button::new("Redo"))
                .on_hover_text("Redo (Ctrl+Y)")
                .clicked()
            {
                actions.push(Action::Redo);
            }
        });
    });
}

/// The strip of open tabs (SPEC 7.1).
///
/// Close is the tab's own `×` button, a middle click, or `Ctrl+W`; when tabs
/// overflow the strip scrolls horizontally because it is built from plain egui
/// widgets inside a [`egui::ScrollArea`].
pub fn tab_strip(ui: &mut Ui, tabs: &[Tab], active: usize, actions: &mut Vec<Action>) {
    if tabs.is_empty() {
        return;
    }
    Panel::top("tab_strip").show(ui, |ui| {
        egui::ScrollArea::horizontal()
            .id_salt("tab_strip_scroll")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (index, tab) in tabs.iter().enumerate() {
                        let title = tab.title();
                        let selected = index == active;
                        let text = RichText::new(&title).strong();
                        let response = ui.selectable_label(selected, text).on_hover_text(format!(
                            "{title}\nClick to switch, middle-click to close"
                        ));
                        if response.clicked() {
                            actions.push(Action::SelectTab(index));
                        }
                        if response.middle_clicked() {
                            actions.push(Action::CloseTab(index));
                        }

                        let close = ui
                            .add(egui::Button::new("×").min_size(egui::vec2(18.0, 18.0)))
                            .on_hover_text(format!("Close {title}"));
                        if close.clicked() {
                            actions.push(Action::CloseTab(index));
                        }
                    }
                });
            });
    });
}

/// The status bar (SPEC 8.2): size, zoom, cursor position, and any message.
///
/// `saves_as` is the export size when it differs from the image's: with
/// NEXT_STEPS step 10's expansion the saved file is bigger than the tab's
/// image, and the status bar is where the user finds that out before saving.
pub fn status_bar(
    ui: &mut Ui,
    tab: Option<&Tab>,
    cursor: Option<snp_ligase_core::geom::Point>,
    message: &str,
    saves_as: Option<(u32, u32)>,
) {
    Panel::bottom("status_bar").show(ui, |ui| {
        ui.horizontal(|ui| {
            match tab {
                Some(tab) => {
                    let (width, height) = tab.size();
                    match saves_as.filter(|size| *size != (width, height)) {
                        Some((saved_width, saved_height)) => ui.label(format!(
                            "{width} × {height} px (saves as {saved_width} × {saved_height})"
                        )),
                        None => ui.label(format!("{width} × {height} px")),
                    };
                    ui.separator();
                    ui.label(format!("Zoom {:.0}%", tab.viewport.zoom * 100.0));
                    // NEXT_STEPS step 13: the user can see why the zoom
                    // changes by itself.
                    if tab.auto_fit {
                        ui.label("Auto-fit");
                    }
                    ui.separator();
                    match cursor {
                        Some(point) => {
                            ui.label(format!("Cursor {}, {}", point.x.round(), point.y.round()));
                        }
                        None => {
                            ui.label("Cursor —");
                        }
                    }
                    if !tab.selection().is_empty() {
                        ui.separator();
                        ui.label(format!("{} selected", tab.selection().len()));
                    }
                }
                None => {
                    ui.label("No image open.");
                }
            }
            if !message.is_empty() {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(130, 200, 255), message);
            }
        });
    });
}

/// The left-hand tool list (SPEC 8.2): one row per tool, name always visible
/// when "Show tool labels" is on.
///
/// The Text row is disabled with a "Loading fonts…" label until the font system
/// is ready (SPEC 7.5).
pub fn tools_panel(
    ui: &mut Ui,
    tool: Tool,
    show_labels: bool,
    fonts_ready: bool,
    actions: &mut Vec<Action>,
) {
    ui.vertical(|ui| {
        ui.heading("Tools");
        ui.separator();
        for (index, (name, key, built)) in TOOL_MENU.iter().enumerate() {
            let Some(chosen) = tool_for_row(index) else {
                continue;
            };
            let blocked = tool_blocked(name, *built, fonts_ready);
            let label = if show_labels {
                format!("{name}  ({key})")
            } else {
                (*key).to_owned()
            };
            let response = ui
                .add_enabled(
                    blocked.is_none(),
                    egui::Button::new(label).selected(chosen == tool),
                )
                .on_hover_text(tool_hover_text(name, key, blocked))
                // A disabled response is consumed by `on_disabled_hover_text`,
                // so it is re-bound before the click is read.
                .on_disabled_hover_text(tool_hover_text(name, key, blocked));
            if !response.clicked() {
                continue;
            }
            actions.push(Action::Tool(chosen));
        }
        ui.separator();
        ui.label("Scroll: pan");
        ui.label("Ctrl+scroll: zoom");
        ui.label("Space or middle drag: pan");
        ui.label("Shift: snap to 15°");
    });
}

/// The "this tab has unsaved changes" prompt (SPEC 7.1).
///
/// `plural` picks the verb, so the sentence reads "2 images **have** unsaved
/// changes" rather than "2 images has".
pub fn unsaved_prompt(ui: &mut Ui, title: &str, plural: bool, actions: &mut Vec<Action>) {
    let mut open = true;
    egui::Window::new("Unsaved changes")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .open(&mut open)
        .show(ui.ctx(), |ui| {
            ui.label(format!(
                "{title} {} unsaved changes.",
                if plural { "have" } else { "has" }
            ));
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    actions.push(Action::ConfirmSave);
                }
                if ui.button("Discard").clicked() {
                    actions.push(Action::ConfirmDiscard);
                }
                if ui.button("Cancel").clicked() {
                    actions.push(Action::ConfirmCancel);
                }
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{StyleNode, ToolStyle};
    use snp_ligase_core::geom::{Point, Rect};
    use snp_ligase_core::model::{Kind, PathA, RectA, defaults};

    /// NEXT_STEPS step 10: the status bar shows the size the file will have
    /// when it differs from the image's.
    #[test]
    fn the_status_bar_says_what_the_file_will_save_as() {
        use egui_kittest::kittest::Queryable as _;

        let tab = Tab::new(
            snp_ligase_core::model::Document::new(image::RgbaImage::new(1920, 1080)),
            "test".to_owned(),
        );
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, ()| {
                status_bar(ui, Some(&tab), None, "", Some((1920, 1160)));
            },
            (),
        );
        harness.run();
        assert!(
            harness
                .query_by_label_contains("1920 × 1080 px (saves as 1920 × 1160)")
                .is_some(),
            "the status bar does not say the file grows"
        );

        // And with nothing outside the image there is no parenthetical.
        let tab = Tab::new(
            snp_ligase_core::model::Document::new(image::RgbaImage::new(1920, 1080)),
            "test".to_owned(),
        );
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, ()| {
                status_bar(ui, Some(&tab), None, "", Some((1920, 1080)));
            },
            (),
        );
        harness.run();
        assert!(
            harness.query_by_label_contains("saves as").is_none(),
            "the status bar says 'saves as' when nothing changes"
        );
    }

    /// NEXT_STEPS step 13: the status bar says "Auto-fit" next to the zoom
    /// while auto-fit is on, so the user can see why the zoom changes by
    /// itself (SPEC 8.2's status bar).
    #[test]
    fn the_status_bar_shows_auto_fit_beside_the_zoom() {
        use egui_kittest::kittest::Queryable as _;

        let tab = Tab::new(
            snp_ligase_core::model::Document::new(image::RgbaImage::new(100, 100)),
            "test".to_owned(),
        );
        assert!(tab.auto_fit, "a new tab starts with auto-fit on");
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, ()| {
                status_bar(ui, Some(&tab), None, "", None);
            },
            (),
        );
        harness.run();
        assert!(
            harness.query_by_label_contains("Auto-fit").is_some(),
            "the status bar does not say auto-fit is on"
        );

        let mut tab = Tab::new(
            snp_ligase_core::model::Document::new(image::RgbaImage::new(100, 100)),
            "test".to_owned(),
        );
        tab.auto_fit = false;
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, ()| {
                status_bar(ui, Some(&tab), None, "", None);
            },
            (),
        );
        harness.run();
        assert!(
            harness.query_by_label_contains("Auto-fit").is_none(),
            "the status bar claims auto-fit is on after a manual zoom"
        );
    }

    /// NEXT_STEPS step 11: the File menu's "Paste Image into This Tab" row is a
    /// real action (SPEC 8.1 rule 2), and it is disabled with no tab open.
    #[test]
    fn the_file_menu_pastes_into_the_current_tab() {
        use egui_kittest::kittest::Queryable as _;

        let menu_state = state(Vec::new(), false);
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, actions: &mut Vec<Action>| {
                menu_bar(ui, &menu_state, actions);
            },
            Vec::<Action>::new(),
        );
        harness.run();
        harness.get_by_label("File").click();
        harness.run();
        harness
            .get_by_label_contains("Paste Image into This Tab")
            .click();
        harness.run();
        assert!(
            harness.state().contains(&Action::PasteIntoTab),
            "the row did not produce the action: {:?}",
            harness.state()
        );

        // With no tab open the row is disabled, so a click changes nothing.
        let mut menu_state = state(Vec::new(), false);
        menu_state.has_tab = false;
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, actions: &mut Vec<Action>| {
                menu_bar(ui, &menu_state, actions);
            },
            Vec::<Action>::new(),
        );
        harness.run();
        harness.get_by_label("File").click();
        harness.run();
        harness
            .get_by_label_contains("Paste Image into This Tab")
            .click();
        harness.run();
        assert!(
            harness.state().is_empty(),
            "a disabled row produced an action: {:?}",
            harness.state()
        );
    }

    /// NEXT_STEPS step 13: the View menu carries a checkable "Auto-fit to
    /// Window" row that shows the active tab's state and offers to flip it
    /// (SPEC 8.1 rule 2: every option is reachable from the menu bar).
    #[test]
    fn the_view_menu_has_an_auto_fit_row() {
        use egui_kittest::kittest::Queryable as _;

        let menu_state = state(Vec::new(), false);
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, actions: &mut Vec<Action>| {
                menu_bar(ui, &menu_state, actions);
            },
            Vec::<Action>::new(),
        );
        harness.run();
        harness.get_by_label("View").click();
        harness.run();
        harness.get_by_label_contains("Auto-fit to Window").click();
        harness.run();
        assert!(
            harness.state().contains(&Action::AutoFit(false)),
            "the checked row did not offer to turn auto-fit off: {:?}",
            harness.state()
        );

        // With auto-fit already off the row offers to turn it back on.
        let mut menu_state = state(Vec::new(), false);
        menu_state.auto_fit = false;
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, actions: &mut Vec<Action>| {
                menu_bar(ui, &menu_state, actions);
            },
            Vec::<Action>::new(),
        );
        harness.run();
        harness.get_by_label("View").click();
        harness.run();
        harness.get_by_label_contains("Auto-fit to Window").click();
        harness.run();
        assert!(
            harness.state().contains(&Action::AutoFit(true)),
            "the unchecked row did not offer to turn auto-fit on: {:?}",
            harness.state()
        );
    }

    fn state(nodes: Vec<StyleNode>, has_selection: bool) -> MenuState {
        MenuState {
            can_undo: false,
            can_redo: false,
            has_tab: true,
            has_selection,
            tool: Tool::Select,
            show_properties: true,
            show_tool_labels: true,
            auto_fit: true,
            fonts_ready: true,
            delay_seconds: 0,
            active_tab: 0,
            style: Some(ToolStyle::default()),
            nodes,
            capture_hotkeys: Vec::new(),
        }
    }

    fn path_kind(heads: bool) -> Kind {
        Kind::Path(PathA {
            points: vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)],
            curved: true,
            stroke: defaults::stroke(),
            head_start: heads,
            head_end: heads,
        })
    }

    fn rect_kind() -> Kind {
        Kind::Rect(RectA {
            rect: Rect::new(0.0, 0.0, 5.0, 5.0),
            stroke: None,
            fill: None,
            corner_radius: 0.0,
        })
    }

    /// The actions one menu row stands for: an item's own, or every row of a
    /// sub-menu.
    fn actions_of<'a>(rows: &'a [MenuRow], label: &str) -> Vec<&'a Action> {
        rows.iter()
            .find_map(|row| match row {
                MenuRow::Item {
                    label: found,
                    action,
                    ..
                } if *found == label => Some(vec![action]),
                MenuRow::Submenu {
                    label: found, rows, ..
                } if *found == label => Some(rows.iter().map(|(_, action)| action).collect()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no row called {label}"))
    }

    /// Whether one menu row is enabled, by label.
    fn enabled_of(rows: &[MenuRow], label: &str) -> bool {
        rows.iter()
            .find_map(|row| match row {
                MenuRow::Item {
                    label: found,
                    enabled,
                    ..
                }
                | MenuRow::Submenu {
                    label: found,
                    enabled,
                    ..
                } if *found == label => Some(*enabled),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no row called {label}"))
    }

    /// The label of any menu row.
    fn row_label(row: &MenuRow) -> &str {
        match row {
            MenuRow::Item { label, .. } | MenuRow::Submenu { label, .. } => label,
            MenuRow::Separator => "",
        }
    }

    /// A `MenuState` whose capture hotkeys are `texts`, given in
    /// [`crate::hotkeys::HOTKEY_MODES`] order — the order the settings file
    /// stores, which is Region, Current Monitor, Active Window, All Monitors
    /// (NEXT_STEPS step 9d).
    fn state_with_hotkeys(texts: [&str; 4]) -> MenuState {
        use crate::hotkeys::HOTKEY_MODES;
        let mut state = state(Vec::new(), false);
        state.capture_hotkeys = HOTKEY_MODES
            .iter()
            .zip(texts)
            .map(|(mode, text)| (*mode, text.to_owned()))
            .collect();
        state
    }

    /// NEXT_STEPS step 9d: the Capture menu names the hotkey the user
    /// configured, not the SPEC 6.1 default. It used to be built from
    /// `CaptureMode::hotkey()`, which only knows the defaults, so after a rebind
    /// the row still said "Print Screen".
    #[test]
    fn the_capture_menu_names_the_configured_hotkeys() {
        let state = state_with_hotkeys(["Ctrl+Alt+P", "F9", "", "Ctrl+Shift+F7"]);
        let rows = capture_rows(&state);

        assert_eq!(rows.len(), CaptureMode::ALL.len());
        let labels: Vec<&str> = rows.iter().map(|(label, ..)| label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Region    Ctrl+Alt+P",
                "Current Monitor    F9",
                "All Monitors    Ctrl+Shift+F7",
                // Nothing bound: the row is just its name.
                "Active Window",
                // SPEC 6.1 gives "repeat last region" no global hotkey.
                "Repeat Last Region",
            ],
            "{labels:?}"
        );
        for label in &labels {
            assert!(
                !label.contains("Print Screen"),
                "the SPEC default is still shown: {label}"
            );
        }
    }

    /// The rows carry the action for their own mode, in menu order.
    #[test]
    fn the_capture_rows_ask_for_their_own_mode() {
        let state = state_with_hotkeys(["Ctrl+Alt+P", "F9", "F10", "F11"]);
        let actions: Vec<Action> = capture_rows(&state)
            .into_iter()
            .map(|(_label, action, _enabled)| action)
            .collect();
        let wanted: Vec<Action> = CaptureMode::ALL
            .iter()
            .copied()
            .map(Action::Capture)
            .collect();
        assert_eq!(actions, wanted);
    }

    /// The row text itself: the hotkey is part of the label, and a mode with
    /// nothing bound shows no dangling separator.
    #[test]
    fn a_capture_row_is_its_mode_and_its_hotkey() {
        assert_eq!(
            capture_row_label(CaptureMode::Region, "Ctrl+Alt+P"),
            "Region    Ctrl+Alt+P"
        );
        assert_eq!(
            capture_row_label(CaptureMode::AllMonitors, ""),
            "All Monitors"
        );
        assert_eq!(
            capture_row_label(CaptureMode::AllMonitors, "  "),
            "All Monitors"
        );
    }

    /// The toolbar's hover text names the region hotkey the user bound.
    #[test]
    fn the_toolbar_hover_text_names_the_configured_region_hotkey() {
        assert_eq!(
            new_capture_hover("Ctrl+Alt+P"),
            "Capture a new image (Ctrl+Alt+P for a region)"
        );
        assert_eq!(new_capture_hover(""), "Capture a new image");
        assert_eq!(new_capture_hover("  "), "Capture a new image");
    }

    #[test]
    fn every_m3_annotation_row_for_a_path_is_a_real_action() {
        // R14: for a line or arrow, every SPEC 8.3 row this milestone owns must
        // do something rather than report a later milestone.
        let state = state(StyleNode::of_kind(&path_kind(true)), true);
        let rows = annotation_rows(&state);

        for label in [
            "Stroke Colour…",
            "Stroke Width",
            "Shadow",
            "Arrow Ends",
            "Line Style",
        ] {
            assert!(enabled_of(&rows, label), "{label} is disabled for a path");
            assert!(
                !actions_of(&rows, label).is_empty(),
                "{label} has no action"
            );
        }
        // Rows that do not apply are disabled, not hidden (SPEC 8.3).
        for label in ["Fill", "Fill Colour…", "Corner Radius"] {
            assert!(!enabled_of(&rows, label), "{label} applies to a path?");
        }
    }

    #[test]
    fn every_m3_annotation_row_for_a_rectangle_is_a_real_action() {
        let state = state(StyleNode::of_kind(&rect_kind()), true);
        let rows = annotation_rows(&state);

        for label in [
            "Stroke Colour…",
            "Stroke Width",
            "Fill",
            "Fill Colour…",
            "Corner Radius",
            "Shadow",
        ] {
            assert!(enabled_of(&rows, label), "{label} is disabled for a rect");
            assert!(
                !actions_of(&rows, label).is_empty(),
                "{label} has no action"
            );
        }
        for label in ["Arrow Ends", "Line Style"] {
            assert!(!enabled_of(&rows, label), "{label} applies to a rectangle?");
        }
    }

    #[test]
    fn the_custom_rows_are_real_and_end_the_lists() {
        for rows in [
            stroke_width_rows(),
            corner_radius_rows(),
            font_size_rows(),
            blur_strength_rows(),
            text_outline_width_rows(),
        ] {
            assert_eq!(
                rows.last().map(|(label, _)| label.as_str()),
                Some("Custom…"),
                "the list does not end in Custom…"
            );
            assert!(
                rows.iter()
                    .any(|(_, action)| matches!(action, Action::CustomValue(_))),
                "there is no Custom… action"
            );
        }
    }

    #[test]
    fn the_spec_widths_and_radii_are_the_lists_the_menu_offers() {
        let widths: Vec<String> = stroke_width_rows()
            .iter()
            .map(|(label, _)| label.clone())
            .collect();
        assert_eq!(
            widths,
            [
                "1 px",
                "2 px",
                "3 px",
                "4 px",
                "6 px",
                "8 px",
                "12 px",
                "Custom…"
            ]
        );
        let radii: Vec<String> = corner_radius_rows()
            .iter()
            .map(|(label, _)| label.clone())
            .collect();
        assert_eq!(radii, ["0 px", "4 px", "8 px", "16 px", "Custom…"]);
    }

    #[test]
    fn every_m4_annotation_row_for_a_text_is_a_real_action() {
        // SPEC 8.3: the text rows must be real actions for a text annotation,
        // and disabled for anything else.
        let text = Kind::Text(snp_ligase_core::model::TextA {
            pos: Point::new(0.0, 0.0),
            text: "x".to_owned(),
            font_family: "Inter".to_owned(),
            size: 28.0,
            bold: true,
            italic: false,
            color: [255, 255, 255, 255],
            background: None,
            padding: 6.0,
            outline: None,
        });
        let mut state = state(StyleNode::of_kind(&text), true);
        // With the outline Solid its colour and width rows are enabled too
        // (NEXT_STEPS step 8).
        if let Some(style) = state.style.as_mut() {
            style.text_outline = Some(defaults::stroke());
        }
        let rows = annotation_rows(&state);

        for label in [
            "Font…",
            "Text Size",
            "Bold",
            "Italic",
            "Text Colour…",
            "Text Background",
            "Text Background Colour…",
            "Text Outline",
            "Text Outline Colour…",
            "Text Outline Width",
            "Shadow",
        ] {
            assert!(enabled_of(&rows, label), "{label} is disabled for text");
            assert!(
                !actions_of(&rows, label).is_empty(),
                "{label} has no action"
            );
        }
        for label in [
            "Stroke Colour…",
            "Fill",
            "Arrow Ends",
            "Line Style",
            "Corner Radius",
            "Next Number…",
            "Highlight Colour…",
            "Blur Strength",
        ] {
            assert!(!enabled_of(&rows, label), "{label} applies to text?");
        }
    }

    /// NEXT_STEPS step 8: while the outline is None its colour and width rows
    /// are disabled (not hidden), and the choice row stays enabled.
    #[test]
    fn the_text_outline_colour_and_width_rows_follow_the_outline_choice() {
        let text = Kind::Text(snp_ligase_core::model::TextA {
            pos: Point::new(0.0, 0.0),
            text: "x".to_owned(),
            font_family: "Inter".to_owned(),
            size: 28.0,
            bold: false,
            italic: false,
            color: [0, 0, 0, 255],
            background: None,
            padding: 6.0,
            outline: None,
        });
        let rows = annotation_rows(&state(StyleNode::of_kind(&text), true));
        assert!(
            enabled_of(&rows, "Text Outline"),
            "the choice row is disabled"
        );
        assert!(
            !enabled_of(&rows, "Text Outline Colour…"),
            "the colour row is enabled with no outline"
        );
        assert!(
            !enabled_of(&rows, "Text Outline Width"),
            "the width row is enabled with no outline"
        );
        // Disabled, not hidden (SPEC 8.3).
        for label in ["Text Outline Colour…", "Text Outline Width"] {
            assert!(
                rows.iter().any(|row| row_label(row) == label),
                "{label} is missing from the menu"
            );
        }
    }

    #[test]
    fn every_m4_annotation_row_for_a_number_highlight_and_blur_is_real() {
        let number = Kind::Number(snp_ligase_core::model::NumberA {
            center: Point::new(0.0, 0.0),
            value: 1,
            radius: 16.0,
            fill: [232, 17, 35, 255],
            pointer_tip: None,
        });
        let rows = annotation_rows(&state(StyleNode::of_kind(&number), true));
        for label in ["Next Number…", "Reset Numbering to 1", "Shadow"] {
            assert!(enabled_of(&rows, label), "{label} is disabled for a number");
        }

        let highlight = Kind::Highlight(snp_ligase_core::model::HighlightA {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            color: [255, 255, 0, 255],
        });
        let rows = annotation_rows(&state(StyleNode::of_kind(&highlight), true));
        assert!(
            enabled_of(&rows, "Highlight Colour…"),
            "the highlight colour row is disabled for a highlight"
        );
        assert!(
            !enabled_of(&rows, "Shadow"),
            "a shadow under a highlight would never be seen"
        );

        let blur = Kind::Blur(snp_ligase_core::model::BlurA {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            radius: 14.0,
        });
        let rows = annotation_rows(&state(StyleNode::of_kind(&blur), true));
        assert!(
            enabled_of(&rows, "Blur Strength"),
            "the blur strength row is disabled for a blur"
        );

        // NEXT_STEPS step 11: a pasted image takes only a shadow.
        let image = Kind::Image(snp_ligase_core::model::ImageA {
            pos: Point::new(0.0, 0.0),
            image: std::sync::Arc::new(image::RgbaImage::new(4, 4)),
        });
        let rows = annotation_rows(&state(StyleNode::of_kind(&image), true));
        assert!(
            enabled_of(&rows, "Shadow"),
            "the shadow row is disabled for a pasted image"
        );
        for label in [
            "Stroke Colour…",
            "Fill",
            "Font…",
            "Highlight Colour…",
            "Blur Strength",
        ] {
            assert!(
                !enabled_of(&rows, label),
                "{label} is enabled for a pasted image"
            );
        }
    }

    #[test]
    fn the_m4_rows_are_disabled_for_the_kinds_they_do_not_apply_to() {
        let state = state(StyleNode::of_kind(&path_kind(false)), true);
        let rows = annotation_rows(&state);
        for label in [
            "Font…",
            "Text Size",
            "Bold",
            "Italic",
            "Text Colour…",
            "Text Background",
            "Text Background Colour…",
            "Text Outline",
            "Text Outline Colour…",
            "Text Outline Width",
            "Next Number…",
            "Reset Numbering to 1",
            "Highlight Colour…",
            "Blur Strength",
        ] {
            assert!(
                !enabled_of(&rows, label),
                "{label} is enabled for a plain line"
            );
            // Disabled, not hidden (SPEC 8.3).
            assert!(
                rows.iter().any(|row| row_label(row) == label),
                "{label} is missing from the menu"
            );
        }
    }
}
