//! The settings file (SPEC section 10).
//!
//! `%APPDATA%\snp-ligase\config.toml` holds everything SPEC section 10's table
//! lists: the four capture hotkeys, the capture switches, the save folder and
//! file-name template, the per-format quality, the editor switches, the
//! per-tool style defaults and the recent colours.
//!
//! The folder is **injected** ([`SettingsStore::load`] takes it) rather than
//! discovered here, so tests write to a temporary directory and never touch the
//! real one.
//!
//! The rules SPEC section 10 sets, and where they are implemented:
//!
//! - a missing file is the defaults (no warning: a first run is not a problem);
//! - a file that cannot be read or parsed falls back to the defaults, and the
//!   file itself is **renamed aside** to `config.toml.bad` — never overwritten
//!   (and a second broken file becomes `config.toml.bad.1`, so nothing is ever
//!   lost);
//! - a file that could be neither read nor renamed aside (a permission
//!   problem, a lock) leaves the store **write-blocked**: every write is
//!   skipped and logged once, so the one thing that must never happen — a
//!   write replacing the file nobody could read — cannot (H2);
//! - unknown keys are ignored, so a newer version's file still loads;
//! - writes are atomic (temporary file + rename) and debounced.
//!
//! The **save folder takes `%` variables** like the file-name template does
//! (NEXT_STEPS step 9c): `C:\Shots\%Y-%m` puts each month's captures in their
//! own folder, `%%` is a literal percent sign, and the folder and the name are
//! resolved from one clock reading so the two can never disagree.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::format::StrftimeItems;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use snp_ligase_core::codec::{EncodeOptions, Format};
use snp_ligase_core::model::Rgba;
use snp_ligase_core::render::FlattenOptions;
use snp_ligase_platform::HdrHighlights;

use crate::APP_NAME;
use crate::panels::CaptureMode;
use crate::tool::ToolStyles;

/// The config file's name inside the config folder (SPEC section 10).
pub const CONFIG_FILE: &str = "config.toml";

/// What an unreadable config file is renamed to (SPEC section 10).
pub const BAD_FILE: &str = "config.toml.bad";

/// SPEC section 9's default file-name template.
pub const DEFAULT_TEMPLATE: &str = "snap_%Y%m%d-%H%M%S";

/// How often a change is written at most, while it keeps changing
/// (SPEC section 10: "save ... when tool defaults change (debounced)").
pub const SAVE_INTERVAL: Duration = Duration::from_secs(1);

/// What fills the area a saved image grows by to keep annotations that stick
/// out past the base image (NEXT_STEPS step 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ExpansionFill {
    /// Fill with `expansion_colour`. The default: ksnip fills with white.
    #[default]
    Colour,
    /// Leave the added area transparent. A JPEG has no transparency, so it is
    /// flattened onto white (SPEC section 9).
    Transparent,
}

/// Everything SPEC section 10's table lists.
///
/// Every field has a default, and `#[serde(default)]` fills a missing field
/// from it, so a config file written by an older version still loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // -- Hotkeys (SPEC 6.1's four capture modes).
    /// The key that captures a region.
    pub hotkey_region: String,
    /// The key that captures the monitor under the cursor.
    pub hotkey_current_monitor: String,
    /// The key that captures the active window.
    pub hotkey_active_window: String,
    /// The key that captures every monitor.
    pub hotkey_all_monitors: String,

    // -- Capture.
    /// Hide the window while a capture is being taken (default on).
    pub hide_while_capturing: bool,
    /// Include the mouse cursor in captures (default off).
    pub include_cursor: bool,
    /// What to do with values above SDR white on an HDR monitor.
    pub hdr_highlights: HdrHighlights,
    /// SDR white level override in nits; `None` is "automatic".
    pub sdr_white_nits: Option<f32>,

    // -- Saving.
    /// Copy a new capture to the clipboard (default on).
    pub copy_new_captures: bool,
    /// Save a new capture to the default folder without asking (default off).
    pub auto_save_new_captures: bool,
    /// Where new files are saved; `None` is `Pictures\snp-ligase`.
    ///
    /// The folder takes the same `%` variables as the file-name template, so
    /// `C:\Shots\%Y-%m` sorts captures into a folder per month (NEXT_STEPS
    /// step 9c); `%%` is a literal percent sign.
    pub save_folder: Option<PathBuf>,
    /// The chrono format for a new file name.
    pub filename_template: String,
    /// Expand the saved image to include annotations outside it (NEXT_STEPS
    /// step 10; default). Off cuts them off at the image edge, which is what
    /// the app did before.
    pub expand_outside_annotations: bool,
    /// What the added area is filled with when expanding (NEXT_STEPS step 10).
    pub expansion_fill: ExpansionFill,
    /// The colour of the added area when `expansion_fill` is `Colour`
    /// (default white, as ksnip does).
    pub expansion_colour: Rgba,
    /// The default save format, by extension (`png`, `jpg`, ...).
    pub default_format: String,
    /// JPEG quality, 1–100 (SPEC section 9: default 90).
    pub jpeg_quality: u8,
    /// AVIF quality, 1–100 (SPEC section 9: default 80).
    pub avif_quality: u8,
    /// AVIF speed, 1–10 (SPEC section 9: default 6).
    pub avif_speed: u8,
    /// JPEG XL lossy distance (SPEC section 9: default 1.0).
    pub jxl_distance: f32,

    // -- Editor.
    /// Ask before closing tabs with unsaved changes (default on).
    pub ask_before_closing: bool,
    /// Switch to the Select tool after drawing an annotation (default off).
    pub switch_to_select_after_drawing: bool,
    /// Show each tool's name next to its key (default on).
    pub show_tool_labels: bool,
    /// The window's X hides to the tray (default on).
    pub close_hides_to_tray: bool,
    /// Start with the window hidden in the tray (M7; default off).
    pub start_hidden_in_tray: bool,
    /// Start with Windows (P2; default off).
    pub start_with_windows: bool,

    // -- State that is remembered rather than chosen.
    /// The last 8 colours used (SPEC 8.4).
    pub recent_colours: Vec<Rgba>,
    /// Each tool's style defaults (SPEC 7.7).
    pub styles: ToolStyles,
}

impl Default for Settings {
    fn default() -> Self {
        let hotkeys = crate::hotkeys::default_hotkey_strings();
        Self {
            hotkey_region: hotkeys[0].clone(),
            hotkey_current_monitor: hotkeys[1].clone(),
            hotkey_active_window: hotkeys[2].clone(),
            hotkey_all_monitors: hotkeys[3].clone(),
            hide_while_capturing: true,
            include_cursor: false,
            hdr_highlights: HdrHighlights::Clip,
            sdr_white_nits: None,
            copy_new_captures: true,
            auto_save_new_captures: false,
            save_folder: None,
            filename_template: DEFAULT_TEMPLATE.to_owned(),
            expand_outside_annotations: true,
            expansion_fill: ExpansionFill::Colour,
            expansion_colour: [255, 255, 255, 255],
            default_format: "png".to_owned(),
            jpeg_quality: 90,
            avif_quality: 80,
            avif_speed: 6,
            jxl_distance: 1.0,
            ask_before_closing: true,
            switch_to_select_after_drawing: false,
            show_tool_labels: true,
            close_hides_to_tray: true,
            start_hidden_in_tray: false,
            start_with_windows: false,
            recent_colours: Vec::new(),
            styles: ToolStyles::default(),
        }
    }
}

impl Settings {
    /// The hotkey text for one of the four capture modes.
    #[must_use]
    pub fn hotkey_text(&self, mode: CaptureMode) -> &str {
        match mode {
            CaptureMode::Region => &self.hotkey_region,
            CaptureMode::CurrentMonitor => &self.hotkey_current_monitor,
            CaptureMode::ActiveWindow => &self.hotkey_active_window,
            CaptureMode::AllMonitors => &self.hotkey_all_monitors,
            // SPEC 6.1 gives "repeat last region" no global hotkey.
            CaptureMode::RepeatLastRegion => "",
        }
    }

    /// Records new hotkey text for one of the four capture modes.
    pub fn set_hotkey_text(&mut self, mode: CaptureMode, text: String) {
        match mode {
            CaptureMode::Region => self.hotkey_region = text,
            CaptureMode::CurrentMonitor => self.hotkey_current_monitor = text,
            CaptureMode::ActiveWindow => self.hotkey_active_window = text,
            CaptureMode::AllMonitors => self.hotkey_all_monitors = text,
            CaptureMode::RepeatLastRegion => {}
        }
    }

    /// The default save format, falling back to PNG when the stored name is not
    /// one this build can write (the AVIF feature may be off, for instance).
    #[must_use]
    pub fn default_format(&self) -> Format {
        Format::from_extension(&self.default_format).unwrap_or_else(|| {
            tracing::warn!(
                format = %self.default_format,
                "the configured default format is not one this build writes; using PNG"
            );
            Format::Png
        })
    }

    /// The quality settings the encoders take (SPEC section 9's Options).
    #[must_use]
    pub fn encode_options(&self) -> EncodeOptions {
        EncodeOptions {
            jpeg_quality: self.jpeg_quality.clamp(1, 100),
            avif_quality: self.avif_quality.clamp(1, 100),
            avif_speed: self.avif_speed.clamp(1, 10),
            jxl_distance: self.jxl_distance,
        }
    }

    /// What `flatten` should do with annotations outside the image
    /// (NEXT_STEPS step 10). Every caller that saves or copies uses this, so
    /// the setting cannot reach one path and not another.
    ///
    /// With "Cut them off" the fill setting is meaningless, so the result is
    /// exactly [`FlattenOptions::CUT`]: a test can pin the two modes apart
    /// without knowing the rest of the config.
    #[must_use]
    pub fn flatten_options(&self) -> FlattenOptions {
        if !self.expand_outside_annotations {
            return FlattenOptions::CUT;
        }
        FlattenOptions {
            expand: true,
            fill: match self.expansion_fill {
                ExpansionFill::Colour => Some(self.expansion_colour),
                ExpansionFill::Transparent => None,
            },
        }
    }

    /// Where new files go: the configured folder, or `Pictures\snp-ligase`
    /// (SPEC section 10).
    #[must_use]
    pub fn resolved_save_folder(&self) -> PathBuf {
        let (folder, _) = self.resolved_save_folder_at(Local::now());
        folder
    }

    /// Where new files go **at a given instant**, with the folder's `%`
    /// variables expanded (NEXT_STEPS step 9c), plus a warning when the
    /// configured folder could not be expanded.
    ///
    /// The folder takes the same clock reading as the file name, so a capture
    /// at 23:59:59.999 on the last day of a month cannot land in one month's
    /// folder under the next month's name.
    #[must_use]
    pub fn resolved_save_folder_at(&self, now: DateTime<Local>) -> (PathBuf, Option<String>) {
        let Some(configured) = self.save_folder.as_ref() else {
            // The OS's Pictures path is literal, even if a user name has `%`.
            return (default_save_folder(), None);
        };
        match resolve_save_folder(configured, now) {
            Ok(folder) => (folder, None),
            Err(message) => {
                let warning = format!(
                    "The save folder \"{}\" is not usable ({message}); using the default folder.",
                    configured.display()
                );
                (default_save_folder(), Some(warning))
            }
        }
    }

    /// The full path a new file saved at `now` would get: the folder with its
    /// `%` variables expanded, under the template's name in the default format
    /// (NEXT_STEPS step 9c's live preview).
    ///
    /// Returns the path and every warning along the way, so the Settings dialog
    /// can show the line and auto-save can log them.
    #[must_use]
    pub fn save_path_at(&self, now: DateTime<Local>) -> (PathBuf, Vec<String>) {
        let (folder, folder_warning) = self.resolved_save_folder_at(now);
        let (name, name_warning) = self.file_name(now);
        let extension = self.default_format().extension();
        let mut warnings = Vec::new();
        warnings.extend(folder_warning);
        warnings.extend(name_warning);
        (folder.join(format!("{name}.{extension}")), warnings)
    }

    /// A new file name from the template, with the default template as the
    /// fallback so a broken template can never stop a save.
    ///
    /// Returns the name and, when the configured template was unusable, a
    /// warning for the status bar.
    #[must_use]
    pub fn file_name(&self, now: DateTime<Local>) -> (String, Option<String>) {
        match file_name_from_template(&self.filename_template, now) {
            Ok(name) => (name, None),
            Err(message) => {
                let warning = format!(
                    "The filename template \"{}\" is not usable ({message}); using {DEFAULT_TEMPLATE}.",
                    self.filename_template
                );
                let fallback = file_name_from_template(DEFAULT_TEMPLATE, now)
                    .unwrap_or_else(|_| format!("snap_{}", now.format("%Y%m%d-%H%M%S")));
                (fallback, Some(warning))
            }
        }
    }

    /// Everything that is wrong with the values, as one line each.
    ///
    /// Loading a file never fails because of these: each one falls back to its
    /// default where it is used, and this is what the status bar and the
    /// Settings dialog say about it.
    #[must_use]
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for (mode, text) in [
            (CaptureMode::Region, &self.hotkey_region),
            (CaptureMode::CurrentMonitor, &self.hotkey_current_monitor),
            (CaptureMode::ActiveWindow, &self.hotkey_active_window),
            (CaptureMode::AllMonitors, &self.hotkey_all_monitors),
        ] {
            if let Err(message) = crate::hotkeys::parse_hotkey(text) {
                warnings.push(format!("The {} hotkey is invalid: {message}", mode.label()));
            }
        }
        if let Err(message) = file_name_from_template(&self.filename_template, Local::now()) {
            warnings.push(format!("The filename template is not usable: {message}"));
        }
        if let Some(folder) = &self.save_folder
            && let Err(message) = resolve_save_folder(folder, Local::now())
        {
            warnings.push(format!("The save folder is not usable: {message}"));
        }
        if Format::from_extension(&self.default_format).is_none() {
            warnings.push(format!(
                "The default format \"{}\" is not one this build writes; PNG is used instead.",
                self.default_format
            ));
        }
        warnings
    }
}

/// `Pictures\snp-ligase`, the folder SPEC section 10's table names.
fn default_save_folder() -> PathBuf {
    let pictures = directories::UserDirs::new()
        .and_then(|dirs| dirs.picture_dir().map(Path::to_path_buf))
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().join("Pictures")))
        .unwrap_or_else(std::env::temp_dir);
    pictures.join(APP_NAME)
}

/// Expands a save folder's `%` variables, or says why it cannot be used
/// (NEXT_STEPS step 9c).
///
/// A folder takes the same chrono format variables as the file-name template,
/// so `C:\Shots\%Y-%m` sorts captures into a folder per month. A literal
/// percent sign is written `%%`, exactly as the template does. The checks, in
/// words, because they are shown in the Settings dialog:
///
/// - the format string itself is valid (a bare `%`, an unknown `%Q`);
/// - every folder name in the result is a legal Windows one — no `< > : " / \
///   | ? *`, no control characters, no trailing dot or space, and not empty;
/// - a drive or absolute prefix is kept where it was written, which is where a
///   literal one belongs (an expanded segment may not introduce one).
pub fn resolve_save_folder(folder: &Path, now: DateTime<Local>) -> Result<PathBuf, String> {
    let text = folder.to_string_lossy();
    if text.trim().is_empty() {
        return Err("the folder is empty".to_owned());
    }
    // Parse the literal prefix first, then expand each name independently.
    // Expanding first lets `%D` introduce slashes and `components()` silently
    // drops empty names; neither is a valid folder-template segment.
    let prefix_len = match folder.components().next() {
        Some(Component::Prefix(prefix)) => prefix.as_os_str().to_string_lossy().len(),
        _ => 0,
    };
    let (prefix, tail) = text.split_at(prefix_len);
    let prefix_items = StrftimeItems::new(prefix)
        .parse()
        .map_err(|err| format!("the format string is invalid ({err})"))?;
    if prefix_items.iter().any(|item| {
        !matches!(
            item,
            chrono::format::Item::Literal(_) | chrono::format::Item::Space(_)
        )
    }) {
        return Err("a drive or absolute prefix must be literal (no date variables)".to_owned());
    }
    let mut resolved = PathBuf::from(now.format_with_items(prefix_items.iter()).to_string());
    let rooted = tail.starts_with(['\\', '/']);
    let tail = if rooted { &tail[1..] } else { tail };
    if rooted {
        resolved.push("\\");
    }
    // A single trailing separator is conventional for a folder and is allowed,
    // as are drive/UNC roots. Interior empty names are still rejected.
    let tail = tail.strip_suffix(['\\', '/']).unwrap_or(tail);
    if tail.is_empty() && rooted {
        // A second separator after the root is an empty segment, not a root.
        if text.len() > prefix_len + 1 {
            return Err("a folder name is empty".to_owned());
        }
        return Ok(resolved);
    }
    for segment in tail.split(['\\', '/']) {
        if segment == ".." {
            return Err("the folder would be a parent directory".to_owned());
        }
        let items = StrftimeItems::new(segment)
            .parse()
            .map_err(|err| format!("the format string is invalid ({err})"))?;
        let expanded = now.format_with_items(items.iter()).to_string();
        check_path_segment(&expanded)?;
        resolved.push(expanded);
    }
    Ok(resolved)
}

/// Whether one folder name is legal on Windows (the same rule
/// [`file_name_from_template`] applies to a file name).
fn check_path_segment(segment: &str) -> Result<(), String> {
    // The characters Windows forbids in a path, plus the control range.
    const FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    if let Some(bad) = segment
        .chars()
        .find(|c| FORBIDDEN.contains(c) || (*c as u32) < 0x20)
    {
        return Err(format!("a folder name would contain {bad:?}"));
    }
    if segment.is_empty() {
        return Err("a folder name is empty".to_owned());
    }
    if segment.ends_with('.') || segment.ends_with(' ') {
        return Err("a folder name would end with a dot or a space".to_owned());
    }
    Ok(())
}

/// Doubles every percent sign, so a folder chosen in a dialog keeps its
/// meaning when it is written into the save-folder field (NEXT_STEPS step 9c:
/// "Browse… still works: it writes the chosen folder (with any `%` doubled to
/// `%%`)").
#[must_use]
pub fn escape_percent(text: &str) -> String {
    text.replace('%', "%%")
}

/// The folder a native dialog should start in: `folder` when it exists, else
/// its nearest existing parent (NEXT_STEPS step 9c).
///
/// Creating folders just to open a dialog is not wanted, so a `%Y-%m` folder
/// that has not been used yet makes the dialog open one level up.
#[must_use]
pub fn existing_start_folder(folder: &Path) -> PathBuf {
    let mut candidate = folder;
    loop {
        if candidate.as_os_str().is_empty() {
            // The parent of a missing relative path is the working directory,
            // not Pictures (which may itself not exist yet).
            return std::env::current_dir().unwrap_or_else(|_| default_save_folder());
        }
        if candidate.is_dir() {
            return candidate.to_path_buf();
        }
        match candidate.parent() {
            Some(parent) if parent != candidate => candidate = parent,
            _ => return folder.to_path_buf(),
        }
    }
}

/// Formats a file-name template, or says why it cannot be used.
///
/// A template is a chrono `strftime` string (SPEC section 9's
/// `snap_%Y%m%d-%H%M%S`). It is rejected when the format string is invalid, when
/// it produces nothing, or when the result could not be a Windows file name.
pub fn file_name_from_template(template: &str, now: DateTime<Local>) -> Result<String, String> {
    let trimmed = template.trim();
    if trimmed.is_empty() {
        return Err("the template is empty".to_owned());
    }
    let items = StrftimeItems::new(trimmed)
        .parse()
        .map_err(|err| format!("the format string is invalid ({err})"))?;
    let name = now.format_with_items(items.iter()).to_string();
    if name.trim().is_empty() {
        return Err("the template produces an empty name".to_owned());
    }
    // The characters Windows forbids in a file name, plus the control range.
    const FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    if let Some(bad) = name
        .chars()
        .find(|c| FORBIDDEN.contains(c) || (*c as u32) < 0x20)
    {
        return Err(format!("the name would contain {bad:?}"));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("the name would end with a dot or a space".to_owned());
    }
    Ok(name)
}

/// The config file and the state needed to write it back debounced.
///
/// The folder is a constructor argument on purpose: tests inject a temporary
/// folder, so no test can read or write the real `%APPDATA%\snp-ligase`.
#[derive(Debug)]
pub struct SettingsStore {
    /// The folder holding `config.toml`.
    config_dir: PathBuf,
    /// The settings as last loaded, written or replaced.
    current: Settings,
    /// True when `current` has changes that are not on disk yet.
    dirty: bool,
    /// When the file was last written, for [`SAVE_INTERVAL`].
    last_write: Option<Instant>,
    /// Why every write is skipped, when the `config.toml` on disk could not be
    /// read or renamed aside (H2). SPEC section 10's rule is that such a file
    /// is never overwritten, so the only safe move is to leave it alone.
    write_blocked: Option<String>,
    /// True once the block has been logged, so a per-frame tick cannot fill
    /// the log with the same line.
    blocked_logged: bool,
}

impl SettingsStore {
    /// Loads the config file from `config_dir`.
    ///
    /// Never fails: a missing or broken file becomes the defaults, and the
    /// returned warnings are what the status bar shows. A file that could not
    /// be parsed is renamed to `config.toml.bad` first, so it is never
    /// overwritten (SPEC section 10).
    #[must_use]
    pub fn load(config_dir: PathBuf) -> (Self, Vec<String>) {
        let path = config_dir.join(CONFIG_FILE);
        let mut warnings = Vec::new();
        // H2: set when the file on disk was left alone, so writing to it is
        // refused from then on.
        let mut write_blocked: Option<String> = None;

        let settings = match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Settings>(&text) {
                Ok(settings) => {
                    tracing::info!(
                        path = %path.display(),
                        "settings loaded"
                    );
                    settings
                }
                Err(err) => {
                    let message = format!(
                        "{} could not be read ({err}); the defaults are in use.",
                        path.display()
                    );
                    tracing::warn!("{message}");
                    warnings.push(message);
                    match move_bad_file(&path) {
                        Ok(moved) => {
                            let note = format!("The old file was kept as {}.", moved.display());
                            tracing::info!("{note}");
                            warnings.push(note);
                        }
                        Err(err) => {
                            let note = format!(
                                "The old file could not be moved aside ({err}); it was left alone."
                            );
                            tracing::warn!("{note}");
                            warnings.push(note);
                            // H2: the file could not be read *and* could not be
                            // moved aside, so a write would replace it. Refuse
                            // to write at all.
                            write_blocked = Some(format!(
                                "{} could not be read or moved aside",
                                path.display()
                            ));
                        }
                    }
                    Settings::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("no config file yet; using the defaults");
                Settings::default()
            }
            Err(err) => {
                // Unreadable rather than unparseable (a permission problem, a
                // directory in the way): the same rule applies — defaults, and
                // the file is not overwritten. H2: it cannot even be moved
                // aside here, so every write is refused from now on.
                let message = format!(
                    "{} could not be read ({err}); the defaults are in use.",
                    path.display()
                );
                tracing::warn!("{message}");
                warnings.push(message);
                write_blocked = Some(format!("{} could not be read", path.display()));
                Settings::default()
            }
        };

        let store = Self {
            config_dir,
            current: settings,
            dirty: false,
            last_write: None,
            write_blocked,
            blocked_logged: false,
        };
        warnings.extend(store.current.validate());
        (store, warnings)
    }

    /// The full path of the config file.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE)
    }

    /// The settings as they are now.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.current
    }

    /// Replaces the settings (the Settings dialog's OK), marking the file
    /// dirty. The caller flushes it.
    pub fn replace(&mut self, settings: Settings) {
        if self.current != settings {
            self.current = settings;
            self.dirty = true;
        }
    }

    /// Keeps the persisted editor state — per-tool styles and recent colours
    /// (SPEC section 10) — in step with the app.
    ///
    /// Returns true when something changed. Called once per frame with the
    /// app's live copies; the comparison is a few hundred bytes, which is
    /// cheaper than trying to catch every write site.
    pub fn sync_editor_state(&mut self, styles: &ToolStyles, recent: &[Rgba]) -> bool {
        if self.current.styles == *styles && self.current.recent_colours == recent {
            return false;
        }
        self.current.styles = styles.clone();
        self.current.recent_colours = recent.to_vec();
        self.dirty = true;
        true
    }

    /// True when there is something on disk that is out of date.
    ///
    /// The app reads it on exit: a change made inside the last debounce
    /// interval is still only in memory (H3).
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Writes the file now, atomically.
    ///
    /// # Errors
    ///
    /// A message for the status bar when the file could not be written.
    pub fn flush(&mut self) -> Result<(), String> {
        self.flush_at(Instant::now())
    }

    /// [`SettingsStore::flush`] with the clock passed in, so [`SettingsStore::tick`]
    /// can drive the debounce deterministically in a test.
    fn flush_at(&mut self, now: Instant) -> Result<(), String> {
        // H2: SPEC section 10's rule — a file that could not be read must never
        // be replaced. Every write is refused while the block is set.
        if let Some(reason) = self.blocked_reason() {
            return Err(format!("The settings were not saved: {reason}."));
        }
        let path = self.config_path();
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            return Err(format!("Could not create {}: {err}", parent.display()));
        }
        let text = toml::to_string_pretty(&self.current)
            .map_err(|err| format!("The settings could not be written as TOML: {err}"))?;
        crate::tab::write_atomically(&path, text.as_bytes())?;
        self.dirty = false;
        self.last_write = Some(now);
        tracing::debug!(path = %path.display(), "settings saved");
        Ok(())
    }

    /// Writes the file when a change has waited out [`SAVE_INTERVAL`].
    ///
    /// Returns true when it wrote. `now` is passed in so tests can drive the
    /// debounce without sleeping.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.write_blocked.is_some() {
            // H2: the file on disk is not ours to replace. The block is logged
            // once, not on every frame, and the store stays dirty.
            self.blocked_reason();
            return false;
        }
        if !self.dirty {
            return false;
        }
        let due = self
            .last_write
            .is_none_or(|last| now.duration_since(last) >= SAVE_INTERVAL);
        if !due {
            return false;
        }
        match self.flush_at(now) {
            Ok(()) => true,
            Err(message) => {
                tracing::warn!("{message}");
                // Leave it dirty: the next tick tries again. The debounce is
                // not restarted, so a failing write does not turn into a busy
                // loop, only into one attempt per interval.
                self.last_write = Some(now);
                false
            }
        }
    }

    /// The reason writes are refused, logged once (H2).
    ///
    /// [`SettingsStore::tick`] runs once per frame; one line in the log
    /// explains the stuck state, thousands would bury it.
    fn blocked_reason(&mut self) -> Option<String> {
        let reason = self.write_blocked.clone()?;
        if !self.blocked_logged {
            self.blocked_logged = true;
            tracing::warn!(
                reason = %reason,
                "the settings file is left alone; writes are skipped"
            );
        }
        Some(reason)
    }
}

/// Moves a config file that could not be read out of the way (SPEC section 10).
///
/// The name is `config.toml.bad`; if that already exists, `.bad.1`, `.bad.2`
/// and so on are tried, so a second broken file never destroys the first one.
fn move_bad_file(path: &Path) -> Result<PathBuf, String> {
    let folder = path.parent().unwrap_or_else(|| Path::new("."));
    let candidates = std::iter::once(folder.join(BAD_FILE))
        .chain((1..=99).map(|index| folder.join(format!("{BAD_FILE}.{index}"))));
    for candidate in candidates {
        if candidate.exists() {
            continue;
        }
        return std::fs::rename(path, &candidate)
            .map(|()| candidate)
            .map_err(|err| err.to_string());
    }
    Err("every .bad name is taken".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    /// A folder no other test uses, under the temp directory.
    fn temp_dir(name: &str) -> PathBuf {
        let folder =
            std::env::temp_dir().join(format!("snp-ligase-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("the test folder should be creatable");
        folder
    }

    fn fixed_time() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 12, 15, 30, 45)
            .single()
            .expect("a valid local time")
    }

    #[test]
    fn a_missing_file_is_the_defaults_without_complaint() {
        let folder = temp_dir("missing");
        let (store, warnings) = SettingsStore::load(folder.clone());

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(*store.settings(), Settings::default());
        assert!(!store.config_path().exists(), "load created a file");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let folder = temp_dir("round-trip");
        let (mut store, warnings) = SettingsStore::load(folder.clone());
        assert!(warnings.is_empty());

        let mut settings = store.settings().clone();
        settings.include_cursor = true;
        settings.jpeg_quality = 42;
        settings.filename_template = "shot_%Y-%m-%d".to_owned();
        settings.default_format = "jpg".to_owned();
        settings.expand_outside_annotations = false;
        settings.expansion_fill = ExpansionFill::Transparent;
        settings.expansion_colour = [9, 8, 7, 255];
        settings.recent_colours = vec![[1, 2, 3, 255], [4, 5, 6, 255]];
        if let Some(text) = settings.styles.get_mut(crate::tool::Tool::Text) {
            text.font_size = 33.0;
            text.bold = false;
            text.text_outline = Some(snp_ligase_core::model::Stroke {
                color: [4, 5, 6, 255],
                width: 7.0,
            });
        }
        store.replace(settings.clone());
        store.flush().expect("the file should be writable");

        let (reloaded, warnings) = SettingsStore::load(folder.clone());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(*reloaded.settings(), settings);
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// NEXT_STEPS step 8: `ToolStyle` is serde-defaulted, so a config file
    /// written before the text outline existed still loads, with the outline
    /// off.
    #[test]
    fn a_config_without_the_text_outline_still_loads() {
        let folder = temp_dir("old-text-style");
        std::fs::write(
            folder.join(CONFIG_FILE),
            "[styles.text]\n\
             text_background = [0, 0, 255, 255]\n\
             font_size = 28.0\n",
        )
        .expect("the test file should be writable");

        let (store, warnings) = SettingsStore::load(folder.clone());
        assert!(warnings.is_empty(), "{warnings:?}");
        let text = store
            .settings()
            .styles
            .get(crate::tool::Tool::Text)
            .expect("a text style");
        assert_eq!(text.text_outline, None, "the outline default is not None");
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// NEXT_STEPS step 10: the expansion rows are serde-defaulted, so a config
    /// file written before they existed still loads, with Expand and white.
    #[test]
    fn a_config_without_the_expansion_rows_still_loads() {
        let folder = temp_dir("old-expansion");
        std::fs::write(
            folder.join(CONFIG_FILE),
            "include_cursor = true\njpeg_quality = 77\n",
        )
        .expect("the test file should be writable");

        let (store, warnings) = SettingsStore::load(folder.clone());
        assert!(warnings.is_empty(), "{warnings:?}");
        let settings = store.settings();
        assert!(
            settings.expand_outside_annotations,
            "Expand is not the default"
        );
        assert_eq!(settings.expansion_fill, ExpansionFill::Colour);
        assert_eq!(settings.expansion_colour, [255, 255, 255, 255]);
        assert_eq!(
            settings.flatten_options(),
            FlattenOptions {
                expand: true,
                fill: Some([255, 255, 255, 255]),
            }
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The two settings rows map onto the core's options: Colour vs Transparent
    /// and Expand vs Cut (NEXT_STEPS step 10).
    #[test]
    fn the_expansion_rows_map_onto_flatten_options() {
        let settings = Settings {
            expand_outside_annotations: false,
            ..Settings::default()
        };
        assert_eq!(settings.flatten_options(), FlattenOptions::CUT);

        let settings = Settings {
            expand_outside_annotations: true,
            expansion_fill: ExpansionFill::Transparent,
            ..Settings::default()
        };
        assert_eq!(
            settings.flatten_options(),
            FlattenOptions {
                expand: true,
                fill: None,
            }
        );

        let settings = Settings {
            expand_outside_annotations: true,
            expansion_fill: ExpansionFill::Colour,
            expansion_colour: [1, 2, 3, 128],
            ..Settings::default()
        };
        assert_eq!(
            settings.flatten_options(),
            FlattenOptions {
                expand: true,
                fill: Some([1, 2, 3, 128]),
            }
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // A file from a newer version (or a hand-edited one) still loads.
        let folder = temp_dir("unknown-keys");
        std::fs::write(
            folder.join(CONFIG_FILE),
            "include_cursor = true\n\
             something_new = \"hello\"\n\
             \n\
             [a_section_from_the_future]\n\
             answer = 42\n",
        )
        .expect("the test file should be writable");

        let (store, warnings) = SettingsStore::load(folder.clone());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(store.settings().include_cursor);
        assert_eq!(store.settings().jpeg_quality, 90, "the defaults were lost");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn an_invalid_file_becomes_the_defaults_and_is_renamed_not_overwritten() {
        let folder = temp_dir("invalid");
        let path = folder.join(CONFIG_FILE);
        let broken = "this is not = = valid toml";
        std::fs::write(&path, broken).expect("the test file should be writable");

        let (store, warnings) = SettingsStore::load(folder.clone());

        assert_eq!(*store.settings(), Settings::default());
        assert!(!warnings.is_empty(), "the problem was not reported");
        assert!(!path.exists(), "the broken file is still at config.toml");
        let kept = folder.join(BAD_FILE);
        assert_eq!(
            std::fs::read_to_string(&kept).expect("the bad file should be kept"),
            broken,
            "the bad file's bytes were changed"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_second_bad_file_does_not_destroy_the_first() {
        let folder = temp_dir("second-bad");
        let path = folder.join(CONFIG_FILE);
        std::fs::write(&path, "first broken file = =").expect("write");
        let (_, _) = SettingsStore::load(folder.clone());
        std::fs::write(&path, "second broken file = =").expect("write");
        let (_, _) = SettingsStore::load(folder.clone());

        assert_eq!(
            std::fs::read_to_string(folder.join(BAD_FILE)).expect("the first .bad file"),
            "first broken file = ="
        );
        assert_eq!(
            std::fs::read_to_string(folder.join(format!("{BAD_FILE}.1")))
                .expect("the second .bad file"),
            "second broken file = ="
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// H2: a config file that could not be read must never be replaced by a
    /// later write (SPEC section 10: never overwrite an unreadable file).
    ///
    /// The read is made to fail the way a permission problem does: the file is
    /// held with no sharing at all while the store loads.
    #[cfg(windows)]
    #[test]
    fn a_file_that_cannot_be_read_is_never_overwritten() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let folder = temp_dir("blocked-read");
        let path = folder.join(CONFIG_FILE);
        let original = "filename_template = \"kept\"\n";
        std::fs::write(&path, original).expect("the test file should be writable");
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .expect("the file should be lockable");
        let (mut store, warnings) = SettingsStore::load(folder.clone());
        drop(holder);

        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("could not be read")),
            "{warnings:?}"
        );

        let mut settings = store.settings().clone();
        settings.show_tool_labels = false;
        store.replace(settings);
        let error = store.flush().expect_err("the write must be blocked");
        assert!(!error.is_empty(), "the failure must say something");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the file should still be readable"),
            original,
            "the unreadable file was overwritten"
        );

        // The debounced path must not write it either.
        assert!(
            !store.tick(Instant::now()),
            "the debounced write ignored the block"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            original,
            "the debounced write replaced the file"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// H2: when the broken file cannot even be renamed aside, it must stay
    /// exactly as it is — the first debounced write used to replace it.
    #[test]
    fn a_file_that_cannot_be_moved_aside_is_never_overwritten() {
        let folder = temp_dir("blocked-move");
        let path = folder.join(CONFIG_FILE);
        let broken = "this is not = = valid toml";
        std::fs::write(&path, broken).expect("write");
        // Every `.bad` name a rename could use is taken, so the file cannot be
        // moved out of the way.
        std::fs::create_dir(folder.join(BAD_FILE)).expect("create_dir");
        for index in 1..=99 {
            std::fs::create_dir(folder.join(format!("{BAD_FILE}.{index}"))).expect("create_dir");
        }

        let (mut store, warnings) = SettingsStore::load(folder.clone());

        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("could not be moved aside")),
            "{warnings:?}"
        );

        let mut settings = store.settings().clone();
        settings.show_tool_labels = false;
        store.replace(settings);
        let error = store.flush().expect_err("the write must be blocked");
        assert!(!error.is_empty(), "the failure must say something");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            broken,
            "the file that could not be moved aside was overwritten"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_default_template_makes_the_spec_name() {
        let name = file_name_from_template(DEFAULT_TEMPLATE, fixed_time()).expect("valid");
        assert_eq!(name, "snap_20261012-153045");
    }

    #[test]
    fn invalid_templates_are_refused_with_a_reason() {
        // Empty.
        assert!(file_name_from_template("", fixed_time()).is_err());
        assert!(file_name_from_template("   ", fixed_time()).is_err());
        // An unknown conversion.
        let error = file_name_from_template("shot_%Q", fixed_time()).expect_err("invalid");
        assert!(error.contains("invalid"), "{error}");
        // A bare % at the end.
        assert!(file_name_from_template("shot_%", fixed_time()).is_err());
        // Characters a file name cannot hold.
        let error = file_name_from_template("shot/%Y", fixed_time()).expect_err("separator");
        assert!(error.contains('/'), "{error}");
        assert!(file_name_from_template("shot:%Y", fixed_time()).is_err());
        // A trailing dot is silently dropped by Windows, so it is refused.
        assert!(file_name_from_template("shot.", fixed_time()).is_err());
    }

    #[test]
    fn a_broken_template_falls_back_to_the_default_name() {
        let settings = Settings {
            filename_template: "shot_%Q".to_owned(),
            ..Settings::default()
        };
        let (name, warning) = settings.file_name(fixed_time());

        assert_eq!(name, "snap_20261012-153045");
        let warning = warning.expect("the problem should be reported");
        assert!(warning.contains("shot_%Q"), "{warning}");
    }

    #[test]
    fn validation_reports_bad_values_without_failing_the_load() {
        let settings = Settings {
            hotkey_region: "not a key".to_owned(),
            default_format: "tiff".to_owned(),
            filename_template: "%Q".to_owned(),
            ..Settings::default()
        };

        let warnings = settings.validate();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("Region"), "{warnings:?}");
        assert!(warnings[1].contains("template"), "{warnings:?}");
        assert!(warnings[2].contains("tiff"), "{warnings:?}");
    }

    // -- NEXT_STEPS step 9c: a save folder with `%` variables ---------------

    /// `%Y-%m` in the default folder resolves to the month it names.
    #[test]
    fn a_dated_folder_resolves_to_that_month() {
        let folder = Path::new("C:\\Shots\\%Y-%m");
        let resolved = resolve_save_folder(folder, fixed_time()).expect("valid");
        assert_eq!(resolved, PathBuf::from("C:\\Shots\\2026-10"));
    }

    #[test]
    fn folder_validation_checks_segments_before_path_normalization() {
        for template in [
            r"C:\Shots\\%Y",
            r"C:\Shots\%D",
            r"C:\Shots\%x",
            r"\\server\%Y\Shots",
        ] {
            assert!(
                resolve_save_folder(Path::new(template), fixed_time()).is_err(),
                "accepted an empty segment, generated separator, or dynamic prefix: {template}"
            );
        }
    }

    #[test]
    fn literal_roots_and_unc_prefixes_are_valid_save_folders() {
        for template in [r"C:\", r"\\server\share\", r"C:\Shots\%Y\"] {
            assert!(
                resolve_save_folder(Path::new(template), fixed_time()).is_ok(),
                "{template}"
            );
        }
    }

    /// A couple of the other date variables, in a nested folder.
    #[test]
    fn dated_folders_can_nest() {
        let folder = Path::new("%Y\\%m-%d");
        let resolved = resolve_save_folder(folder, fixed_time()).expect("valid");
        assert_eq!(resolved, PathBuf::from("2026\\10-12"));
    }

    /// `%%` stays a literal percent sign, so a folder that really has one in
    /// its name can be written.
    #[test]
    fn a_doubled_percent_is_a_literal_one() {
        let folder = Path::new("C:\\100%% done\\%Y");
        let resolved = resolve_save_folder(folder, fixed_time()).expect("valid");
        assert_eq!(resolved, PathBuf::from("C:\\100% done\\2026"));
    }

    /// A folder with no `%` in it is unchanged.
    #[test]
    fn a_plain_folder_is_left_alone() {
        let folder = Path::new("C:\\Shots");
        let resolved = resolve_save_folder(folder, fixed_time()).expect("valid");
        assert_eq!(resolved, folder);
    }

    /// What is refused, and the words used for it.
    #[test]
    fn a_broken_folder_is_refused_with_a_reason() {
        // An unknown conversion.
        let error = resolve_save_folder(Path::new("C:\\%Q"), fixed_time()).expect_err("bad");
        assert!(error.contains("invalid"), "{error}");
        // A bare percent sign, at the end or in the middle.
        assert!(resolve_save_folder(Path::new("C:\\Shots\\%"), fixed_time()).is_err());
        assert!(resolve_save_folder(Path::new("C:\\%"), fixed_time()).is_err());
        // Empty.
        assert!(resolve_save_folder(Path::new(""), fixed_time()).is_err());
        assert!(resolve_save_folder(Path::new("   "), fixed_time()).is_err());
        // Characters Windows forbids, whether written or expanded.
        for bad in [
            "C:\\Shots<>",
            "C:\\Shots:",
            "C:\\Shots|",
            "C:\\Shots?",
            "C:\\Shots*",
        ] {
            let error = resolve_save_folder(Path::new(bad), fixed_time()).expect_err("bad");
            assert!(
                error.contains("folder name"),
                "{bad} gave the wrong reason: {error}"
            );
        }
        // An empty segment.
        let error =
            resolve_save_folder(Path::new("C:\\\\"), fixed_time()).expect_err("empty segment");
        assert!(
            error.contains("empty") || error.contains("no folder name"),
            "{error}"
        );
        // A trailing dot or space, which Windows drops silently.
        assert!(resolve_save_folder(Path::new("C:\\Shots."), fixed_time()).is_err());
        assert!(resolve_save_folder(Path::new("C:\\Shots "), fixed_time()).is_err());
        // A parent directory is not a save folder.
        assert!(resolve_save_folder(Path::new("..\\%Y"), fixed_time()).is_err());
    }

    /// A drive prefix is only accepted where it was written: an expanded
    /// segment may not introduce one (it would hold a colon).
    #[test]
    fn a_drive_prefix_stays_where_it_was_written() {
        let resolved = resolve_save_folder(Path::new("%Y"), fixed_time()).expect("valid");
        assert_eq!(resolved, PathBuf::from("2026"));
        // `%d` for the day is a number, so it can never spell a drive.
        let error = resolve_save_folder(Path::new("C:\\%H:%M"), fixed_time()).expect_err("bad");
        assert!(error.contains("folder name"), "{error}");
    }

    /// The warning the app logs when the configured folder cannot be
    /// expanded, and the fallback it uses instead.
    #[test]
    fn an_unusable_folder_is_reported_and_uses_the_default() {
        let settings = Settings {
            save_folder: Some(PathBuf::from("C:\\Shots\\%Q")),
            ..Settings::default()
        };

        let (folder, warning) = settings.resolved_save_folder_at(fixed_time());
        assert_eq!(folder, default_save_folder(), "the fallback");
        assert!(
            warning
                .as_deref()
                .is_some_and(|warning| warning.contains("%Q")),
            "{warning:?}"
        );
    }

    /// One clock reading feeds both the folder and the name, so a capture at
    /// 23:59:59.999 on the last day of a month cannot land in one month's
    /// folder with the next month's name.
    #[test]
    fn the_folder_and_the_name_use_the_same_instant() {
        let settings = Settings {
            save_folder: Some(PathBuf::from("C:\\Shots\\%Y-%m")),
            ..Settings::default()
        };
        let moment = Local
            .with_ymd_and_hms(2026, 10, 31, 23, 59, 59)
            .single()
            .expect("a valid local time")
            + chrono::Duration::milliseconds(999);

        let (path, warnings) = settings.save_path_at(moment);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            path,
            PathBuf::from("C:\\Shots\\2026-10\\snap_20261031-235959.png")
        );
    }

    /// The live preview line the Settings dialog shows.
    #[test]
    fn the_preview_names_the_full_path_of_a_capture_saved_now() {
        let settings = Settings {
            save_folder: Some(PathBuf::from("C:\\Shots\\%Y-%m")),
            default_format: "jpg".to_owned(),
            ..Settings::default()
        };

        let (path, warnings) = settings.save_path_at(fixed_time());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            path,
            PathBuf::from("C:\\Shots\\2026-10\\snap_20261012-153045.jpg")
        );
    }

    /// `Browse…` writes the folder it picked with every `%` doubled, so a
    /// folder that really contains one keeps its name.
    #[test]
    fn a_browsed_folder_has_its_percent_signs_doubled() {
        assert_eq!(escape_percent("C:\\Shots"), "C:\\Shots");
        assert_eq!(escape_percent("C:\\100% done"), "C:\\100%% done");
        assert_eq!(escape_percent("%Y-%m"), "%%Y-%%m");
    }

    /// A dialog starts in the nearest existing parent of a folder that does not
    /// exist yet, rather than creating folders to open itself.
    #[test]
    fn a_dialog_starts_in_the_nearest_existing_folder() {
        let folder = temp_dir("start");
        let dated = folder.join("2026-10");
        let not_there = dated.join("never");
        assert_eq!(existing_start_folder(&dated), folder);
        assert_eq!(existing_start_folder(&not_there), folder);
        std::fs::create_dir(&dated).expect("create_dir");
        assert_eq!(existing_start_folder(&dated), dated);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_missing_relative_dialog_folder_starts_in_the_current_directory() {
        let missing = Path::new("snp-ligase-dialog-missing-relative-parent").join("2026-10");
        assert!(!missing.exists());
        assert_eq!(
            existing_start_folder(&missing),
            std::env::current_dir().expect("current directory")
        );
        assert!(!missing.exists(), "opening a dialog created the folder");
    }

    /// The save-folder validation reaches `validate`, in words.
    #[test]
    fn validation_reports_a_broken_save_folder() {
        let settings = Settings {
            save_folder: Some(PathBuf::from("C:\\Shots\\%Q")),
            ..Settings::default()
        };
        let warnings = settings.validate();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("save folder")),
            "{warnings:?}"
        );
    }

    #[test]
    fn the_default_format_falls_back_to_png_for_an_unknown_name() {
        let mut settings = Settings::default();
        assert_eq!(settings.default_format(), Format::Png);

        settings.default_format = "jpg".to_owned();
        assert_eq!(settings.default_format(), Format::Jpeg);

        settings.default_format = "gif".to_owned();
        assert_eq!(
            settings.default_format(),
            Format::Png,
            "a format this build cannot write must not be chosen"
        );
    }

    #[test]
    fn the_debounce_writes_once_per_interval() {
        let folder = temp_dir("debounce");
        let (mut store, _) = SettingsStore::load(folder.clone());
        let start = Instant::now();

        let mut settings = store.settings().clone();
        settings.show_tool_labels = false;
        store.replace(settings);
        assert!(store.dirty(), "replace did not mark the file dirty");

        // The first change writes at once: there is nothing on disk yet.
        assert!(store.tick(start), "the first change was not written");
        assert!(!store.dirty());

        // A second change inside the interval waits.
        let mut settings = store.settings().clone();
        settings.show_tool_labels = true;
        store.replace(settings);
        assert!(
            !store.tick(start + Duration::from_millis(100)),
            "the debounce did not hold the write back"
        );
        assert!(store.dirty(), "the change was forgotten");
        assert!(
            store.tick(start + SAVE_INTERVAL),
            "the change was never written"
        );
        assert!(!store.dirty());

        let (reloaded, _) = SettingsStore::load(folder.clone());
        assert!(reloaded.settings().show_tool_labels);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_write_that_fails_leaves_the_store_dirty() {
        let folder = temp_dir("unwritable");
        let (mut store, _) = SettingsStore::load(folder.clone());
        // A directory where the config file wants to be makes the write fail.
        std::fs::create_dir(folder.join(CONFIG_FILE)).expect("create_dir");

        let mut settings = store.settings().clone();
        settings.show_tool_labels = false;
        store.replace(settings);

        assert!(!store.tick(Instant::now()), "the write should have failed");
        assert!(store.dirty(), "a failed write must not look saved");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_editor_state_is_synced_and_saved() {
        let folder = temp_dir("editor-state");
        let (mut store, _) = SettingsStore::load(folder.clone());

        let mut styles = store.settings().styles.clone();
        if let Some(line) = styles.get_mut(crate::tool::Tool::Line) {
            line.stroke_width = 12.0;
        }
        assert!(
            store.sync_editor_state(&styles, &[[9, 9, 9, 255]]),
            "a change was not noticed"
        );
        assert!(store.dirty());
        assert!(
            !store.sync_editor_state(&styles, &[[9, 9, 9, 255]]),
            "an unchanged state was marked dirty again"
        );

        store.flush().expect("write");
        let (reloaded, _) = SettingsStore::load(folder.clone());
        assert_eq!(
            reloaded.settings().recent_colours,
            vec![[9, 9, 9, 255]],
            "the recent colours did not survive"
        );
        let line = reloaded
            .settings()
            .styles
            .get(crate::tool::Tool::Line)
            .expect("a line style");
        assert!((line.stroke_width - 12.0).abs() < 0.001);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
