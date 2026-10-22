//! The Settings dialog (SPEC section 10).
//!
//! "A normal window with labelled controls grouped under headings: Capture,
//! Saving, Hotkeys, Editor, Advanced." It edits a **draft** [`Settings`]; only
//! OK replaces the stored one, so Cancel really cancels.
//!
//! Like every other panel, this module only collects [`Action`]s: the app
//! applies them, so the dialog cannot do anything the rest of the app does not
//! know about.

use egui::Ui;
use snp_ligase_core::codec::Format;
use snp_ligase_core::model::Rgba;
use snp_ligase_platform::{Capabilities, HdrHighlights, HotkeySupport};

use crate::hotkeys::{HOTKEY_MODES, default_hotkey_strings, display_hotkey, parse_hotkey};
use crate::panels::colour;
use crate::panels::{Action, CaptureMode, HIDE_WINDOW_UNAVAILABLE, HOTKEYS_UNAVAILABLE};
use crate::settings::{ExpansionFill, Settings};

/// Everything the dialog only reads: where the settings file lives, which
/// hotkey is being recorded, what the platform can do, and the registration
/// warnings from the last "Test".
///
/// A struct rather than four more arguments, like the properties panel's
/// [`crate::panels::properties::PanelContext`].
pub struct SettingsContext<'a> {
    /// The settings file, shown in the Advanced section so the human can find
    /// it.
    pub config_path: &'a std::path::Path,
    /// The mode whose hotkey is being recorded right now, if any.
    pub recording: Option<CaptureMode>,
    /// What the platform backend can really do (LINUX_SUPPORT.md section 3,
    /// item 2): controls it cannot honour are disabled with the reason in their
    /// hover text.
    pub capabilities: &'a Capabilities,
    /// The registration failures from the last "Test" (NEXT_STEPS step 7).
    pub warnings: &'a [String],
}

/// Draws the dialog. `draft` is edited in place; the rest is read from
/// `context`, except `recent`, the app's recent-colour list shared with the
/// properties panel (SPEC 8.4).
pub fn show(
    ui: &mut Ui,
    draft: &mut Settings,
    context: SettingsContext<'_>,
    recent: &mut Vec<Rgba>,
    actions: &mut Vec<Action>,
) {
    let SettingsContext {
        config_path,
        recording,
        capabilities,
        warnings,
    } = context;
    let mut open = true;
    egui::Window::new("Settings")
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .open(&mut open)
        .show(ui.ctx(), |ui| {
            egui::ScrollArea::vertical()
                .max_height(520.0)
                .show(ui, |ui| {
                    capture(ui, draft, capabilities);
                    ui.add_space(8.0);
                    saving(ui, draft, recent, actions);
                    ui.add_space(8.0);
                    hotkeys(ui, draft, recording, capabilities, warnings, actions);
                    ui.add_space(8.0);
                    editor(ui, draft, capabilities);
                    ui.add_space(8.0);
                    advanced(ui, draft, config_path);
                });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    actions.push(Action::ApplySettings);
                }
                if ui.button("Cancel").clicked() {
                    actions.push(Action::CancelSettings);
                }
            });
        });
    if !open {
        // The window's X is a cancel, like the button.
        actions.push(Action::CancelSettings);
    }
}

/// Capture (SPEC 10's first four rows).
///
/// The two options that need the OS are disabled with the reason when the
/// platform cannot honour them (LINUX_SUPPORT.md section 3, item 2): hiding
/// the window, and reading the cursor.
fn capture(ui: &mut Ui, draft: &mut Settings, capabilities: &Capabilities) {
    ui.heading("Capture");
    ui.separator();
    let hide = ui.add_enabled(
        capabilities.hide_main_window,
        egui::Checkbox::new(
            &mut draft.hide_while_capturing,
            "Hide snp-ligase while capturing",
        ),
    );
    if capabilities.hide_main_window {
        hide.on_hover_text(
            "The window is hidden before the screenshot, so it is not in the image.",
        );
    } else {
        hide.on_disabled_hover_text(HIDE_WINDOW_UNAVAILABLE);
    }

    let cursor = ui.add_enabled(
        capabilities.capture_cursor,
        egui::Checkbox::new(
            &mut draft.include_cursor,
            "Include the mouse cursor in captures",
        ),
    );
    if !capabilities.capture_cursor {
        cursor.on_disabled_hover_text("This capture backend cannot include cursor pixels. Windows and KDE native capture support this option.");
    }

    ui.horizontal(|ui| {
        ui.label("HDR highlights");
        egui::ComboBox::from_id_salt("hdr_highlights")
            .selected_text(match draft.hdr_highlights {
                HdrHighlights::Clip => "Clip",
                HdrHighlights::RollOff => "Roll Off",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut draft.hdr_highlights, HdrHighlights::Clip, "Clip");
                ui.selectable_value(
                    &mut draft.hdr_highlights,
                    HdrHighlights::RollOff,
                    "Roll Off",
                );
            });
    });
    ui.label(
        "Clip: everything brighter than SDR white becomes white. Roll Off: highlights are \
         compressed so detail above SDR white survives.",
    );

    ui.horizontal(|ui| {
        ui.label("SDR white level override (nits)");
        let mut text = draft
            .sdr_white_nits
            .map(|nits| format!("{nits:.0}"))
            .unwrap_or_default();
        let response = ui.add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(80.0)
                .hint_text("Automatic"),
        );
        if response.changed() {
            let trimmed = text.trim();
            draft.sdr_white_nits = if trimmed.is_empty() {
                None
            } else {
                // A half-typed number is not worth an error message: it simply
                // does not take effect until it parses.
                trimmed.parse::<f32>().ok().filter(|nits| *nits > 0.0)
            };
        }
    });
    ui.label(
        "Empty means the level Windows reports. A number overrides it for every monitor \
         (SPEC 5.5).",
    );
}

/// Saving (SPEC 10's save folder, template, format and quality, plus
/// NEXT_STEPS step 10's expansion rows).
fn saving(ui: &mut Ui, draft: &mut Settings, recent: &mut Vec<Rgba>, actions: &mut Vec<Action>) {
    ui.heading("Saving");
    ui.separator();
    ui.checkbox(
        &mut draft.copy_new_captures,
        "Copy new captures to the clipboard",
    );
    ui.checkbox(&mut draft.auto_save_new_captures, "Auto-save new captures");

    // NEXT_STEPS step 10: an annotation placed outside the image (a label above
    // a screenshot) either grows the saved image or is cut off at its edge.
    ui.separator();
    ui.label("Annotations outside the image");
    ui.horizontal(|ui| {
        ui.selectable_value(
            &mut draft.expand_outside_annotations,
            true,
            "Expand the image to include them",
        );
        ui.selectable_value(
            &mut draft.expand_outside_annotations,
            false,
            "Cut them off at the image edge",
        );
    });
    ui.label(
        "Expand keeps a label placed above the screenshot: the saved image grows to hold it. \
         Cut is what the app did before, and leaves the saved size alone.",
    );

    let fill_row = ui.horizontal(|ui| {
        ui.label("Fill for the added area");
        ui.add_enabled_ui(draft.expand_outside_annotations, |ui| {
            ui.selectable_value(&mut draft.expansion_fill, ExpansionFill::Colour, "Colour");
            ui.selectable_value(
                &mut draft.expansion_fill,
                ExpansionFill::Transparent,
                "Transparent",
            );
        });
    });
    fill_row.response.on_hover_text(
        "Transparent leaves the added area clear. JPEG has no transparency and is flattened \
         onto white (SPEC 9), so Transparent + JPEG gives white.",
    );
    ui.horizontal(|ui| {
        ui.add_enabled_ui(
            draft.expand_outside_annotations && draft.expansion_fill == ExpansionFill::Colour,
            |ui| {
                colour::button(ui, "Added area colour", &mut draft.expansion_colour, recent);
            },
        );
    });

    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Default save folder");
        let mut folder_text = draft
            .save_folder
            .clone()
            .unwrap_or_else(|| draft.resolved_save_folder())
            .display()
            .to_string();
        let response = ui.add(egui::TextEdit::singleline(&mut folder_text).desired_width(300.0));
        if response.changed() {
            let trimmed = folder_text.trim();
            draft.save_folder = if trimmed.is_empty() {
                None
            } else {
                Some(std::path::PathBuf::from(trimmed))
            };
        }
        if ui.button("Browse…").clicked() {
            actions.push(Action::PickSaveFolder);
        }
    });
    ui.label(
        "The folder takes the same % variables as the filename template: \
              %Y-%m puts each month in its own folder, %% is a literal percent sign.",
    );

    ui.horizontal(|ui| {
        ui.label("Filename template");
        ui.add(
            egui::TextEdit::singleline(&mut draft.filename_template)
                .desired_width(220.0)
                .hint_text("snap_%Y%m%d-%H%M%S"),
        );
    });
    // The full path a capture saved now would get (NEXT_STEPS step 9c): one
    // clock reading for the folder and for the name, so the line cannot show a
    // folder and a name from two different months. Whatever is wrong is said
    // in words above the line (SPEC 8.1).
    let (path, warnings) = draft.save_path_at(chrono::Local::now());
    for warning in warnings {
        ui.colored_label(egui::Color32::from_rgb(255, 160, 120), warning);
    }
    ui.label(format!("A capture saved now would be {}", path.display()));

    ui.horizontal(|ui| {
        ui.label("Default format");
        let selected = draft.default_format();
        egui::ComboBox::from_id_salt("default_format")
            .selected_text(selected.label())
            .show_ui(ui, |ui| {
                for format in Format::ALL {
                    if ui
                        .selectable_label(*format == selected, format.label())
                        .clicked()
                    {
                        draft.default_format = format.extension().to_owned();
                    }
                }
            });
    });

    ui.horizontal(|ui| {
        ui.label("JPEG quality");
        ui.add(egui::Slider::new(&mut draft.jpeg_quality, 1..=100));
    });
    #[cfg(feature = "avif")]
    {
        ui.horizontal(|ui| {
            ui.label("AVIF quality");
            ui.add(egui::Slider::new(&mut draft.avif_quality, 1..=100));
        });
        ui.horizontal(|ui| {
            ui.label("AVIF speed");
            ui.add(egui::Slider::new(&mut draft.avif_speed, 1..=10));
        });
    }
    if cfg!(feature = "jxl-lossy") {
        ui.horizontal(|ui| {
            ui.label("JPEG XL lossy distance");
            ui.add(egui::Slider::new(&mut draft.jxl_distance, 0.1..=10.0));
        });
    }
}

/// Hotkeys (SPEC 10's "Hotkeys for the four capture modes", with conflicts).
///
/// Each row has a "Record…" button (NEXT_STEPS step 7): while `recording` is
/// that mode the field is replaced by a label that says to press the keys, so
/// nothing can type into it and the recorder owns the keyboard.
///
/// Without a global-hotkey backend the whole section is disabled, with the
/// reason as a visible line above it and in its hover text
/// (LINUX_SUPPORT.md section 3, item 2).
fn hotkeys(
    ui: &mut Ui,
    draft: &mut Settings,
    recording: Option<CaptureMode>,
    capabilities: &Capabilities,
    warnings: &[String],
    actions: &mut Vec<Action>,
) {
    ui.heading("Hotkeys");
    ui.separator();
    if capabilities.global_hotkeys == HotkeySupport::Portal {
        ui.label(
            "Change shortcuts in System Settings > Shortcuts. The desktop owns these bindings.",
        );
        for (mode, trigger) in
            crate::shortcuts::capture_bindings(draft, capabilities.global_hotkeys)
        {
            let trigger = if trigger.is_empty() {
                "Not bound"
            } else {
                &trigger
            };
            ui.label(format!("{}: {trigger}", mode.label()));
        }
        ui.label("You can also assign these commands in your desktop's shortcut settings:");
        if !capabilities.compositor_targets {
            ui.label("Current Monitor and Active Window are unavailable: this Wayland desktop has no native capture targets.");
        } else {
            ui.label("Current Monitor follows the pointer. Bind it and Active Window in System Settings > Shortcuts.");
        }
        for mode in CaptureMode::ALL
            .into_iter()
            .filter(|mode| mode.blocked(capabilities).is_none())
        {
            let mode = crate::hotkeys::portal_id(mode);
            let command = format!("{} --capture {mode}", crate::command_name());
            ui.horizontal(|ui| {
                ui.monospace(&command);
                if ui.button("Copy command").clicked() {
                    ui.ctx().copy_text(command);
                }
            });
        }
        for warning in warnings {
            ui.label(warning);
        }
        return;
    }

    ui.label("Type a combination like Ctrl+Shift+P, or leave a field empty for no hotkey.");
    let can_bind = capabilities.global_hotkeys != HotkeySupport::None;
    if !can_bind {
        ui.label(HOTKEYS_UNAVAILABLE);
    }
    let section = ui.add_enabled_ui(can_bind, |ui| {
        hotkey_rows(ui, draft, recording, warnings, actions);
    });
    if !can_bind {
        section.response.on_disabled_hover_text(HOTKEYS_UNAVAILABLE);
    }
}

/// The fields, buttons and messages of the Hotkeys section.
fn hotkey_rows(
    ui: &mut Ui,
    draft: &mut Settings,
    recording: Option<CaptureMode>,
    warnings: &[String],
    actions: &mut Vec<Action>,
) {
    for mode in HOTKEY_MODES {
        ui.horizontal(|ui| {
            ui.label(mode.label());
            if recording == Some(mode) {
                // SPEC 8.1: a visible text label, never an icon.
                ui.label(egui::RichText::new("Press the keys… (Esc cancels)").strong());
            } else {
                let mut text = draft.hotkey_text(mode).to_owned();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut text)
                            .desired_width(200.0)
                            .hint_text("None"),
                    )
                    .changed()
                {
                    draft.set_hotkey_text(mode, text);
                }
                if ui
                    .add_enabled(recording.is_none(), egui::Button::new("Record…"))
                    .clicked()
                {
                    actions.push(Action::RecordHotkey(mode));
                }
            }
        });
        // Errors in words, under the field (NEXT_STEPS step 7 item 2).
        if recording != Some(mode)
            && let Err(message) = parse_hotkey(draft.hotkey_text(mode))
        {
            ui.colored_label(egui::Color32::from_rgb(255, 160, 120), message);
        }
    }

    ui.horizontal(|ui| {
        if ui.button("Restore defaults").clicked() {
            for (mode, text) in HOTKEY_MODES.iter().zip(default_hotkey_strings()) {
                draft.set_hotkey_text(*mode, text);
            }
        }
        if ui
            .add_enabled(
                recording.is_none(),
                egui::Button::new("Test the hotkeys above"),
            )
            .clicked()
        {
            actions.push(Action::TestHotkeys);
        }
    });
    ui.label(
        "Test registers the keys above for a moment to see whether another program owns them; \
         nothing is saved.",
    );

    for conflict in crate::hotkeys::conflicts_in_settings(draft) {
        ui.colored_label(egui::Color32::from_rgb(255, 160, 120), conflict);
    }
    for warning in warnings {
        ui.colored_label(egui::Color32::from_rgb(255, 160, 120), warning);
    }

    let (bindings, _) = crate::hotkeys::bindings_from(draft);
    if bindings.is_empty() {
        ui.label("Every hotkey is off; captures are still on the menu and the tray.");
    } else {
        let list = bindings
            .iter()
            .map(|binding| {
                format!(
                    "{}: {}",
                    binding.mode.label(),
                    display_hotkey(&binding.hotkey)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        ui.label(format!("In use: {list}"));
    }
}

/// Editor (SPEC 10's editor switches).
fn editor(ui: &mut Ui, draft: &mut Settings, capabilities: &Capabilities) {
    ui.heading("Editor");
    ui.separator();
    ui.checkbox(
        &mut draft.ask_before_closing,
        "Ask before closing tabs with unsaved changes",
    );
    // LINUX_SUPPORT.md section 3, item 8: where there is no tray icon at all,
    // the window's X is the only way out and quitting asks first. A platform
    // with a tray leaves through File > Exit or the tray's Quit instead, so the
    // switch would have nothing to do there.
    let quit = ui.add_enabled(
        !capabilities.tray,
        egui::Checkbox::new(&mut draft.ask_before_quitting, "Ask before quitting"),
    );
    if capabilities.tray {
        quit.on_disabled_hover_text(
            "Not used here: the close button hides to the tray, and File > Exit and the \
             tray's Quit leave without asking.",
        );
    }
    ui.checkbox(
        &mut draft.switch_to_select_after_drawing,
        "Switch to the Select tool after drawing",
    );
    ui.checkbox(&mut draft.show_tool_labels, "Show tool labels");
    ui.checkbox(
        &mut draft.auto_fit_to_window,
        "Auto-fit images to the window",
    )
    .on_hover_text(
        "New images fit the window and re-fit when it is resized. A manual zoom turns it off \
         for that image; View > Auto-fit to Window turns it back on.",
    );
    ui.checkbox(
        &mut draft.close_hides_to_tray,
        "The close button hides to the tray",
    );
}

/// Advanced (the P2 switch and the config file).
fn advanced(ui: &mut Ui, draft: &mut Settings, config_path: &std::path::Path) {
    ui.heading("Advanced");
    ui.separator();

    // SPEC 12 M7: implemented. Only honoured when the tray icon exists (there
    // is no way back otherwise), which the app decides at startup.
    ui.checkbox(&mut draft.start_hidden_in_tray, "Start hidden in the tray")
        .on_hover_text(
            "The window is hidden when the app starts. The tray icon (or a second launch) \
             brings it back; without a tray icon this is ignored.",
        );

    // SPEC 10's "Start with Windows": a login entry for this exe, written when
    // the user presses OK (`app::apply_autostart`). Windows only for now.
    let windows = ui
        .add_enabled(
            cfg!(windows),
            egui::Checkbox::new(&mut draft.start_with_windows, "Start with Windows"),
        )
        .on_hover_text(
            "Start this program when you sign in to Windows. The entry points at this              exe, so tick it again after moving the program.",
        );
    windows.on_disabled_hover_text("Starting at login is Windows-only for now.");

    ui.label(format!("Config file: {}", config_path.display()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panels::Action;

    struct State {
        draft: Settings,
        recording: Option<CaptureMode>,
        /// What the platform backend can do; `FULL` unless a test asks for
        /// less (LINUX_SUPPORT.md section 3, item 2).
        capabilities: Capabilities,
        warnings: Vec<String>,
        recent: Vec<Rgba>,
        actions: Vec<Action>,
    }

    /// The dialog with its own state, so a test can set `recording`/`warnings`
    /// between frames.
    fn harness(draft: Settings) -> egui_kittest::Harness<'static, State> {
        egui_kittest::Harness::new_ui_state(
            |ui, state: &mut State| {
                let path = std::path::PathBuf::from("C:\\test\\config.toml");
                let State {
                    draft,
                    recording,
                    capabilities,
                    warnings,
                    recent,
                    actions,
                } = state;
                show(
                    ui,
                    draft,
                    SettingsContext {
                        config_path: &path,
                        recording: *recording,
                        capabilities,
                        warnings,
                    },
                    recent,
                    actions,
                );
            },
            State {
                draft,
                recording: None,
                capabilities: Capabilities::FULL,
                warnings: Vec::new(),
                recent: Vec::new(),
                actions: Vec::new(),
            },
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portal_hotkeys_are_read_only_and_offer_cli_commands() {
        use egui_kittest::kittest::Queryable as _;
        let mut portal = harness(Settings::default());
        portal.state_mut().capabilities = snp_ligase_platform::linux::wayland::CAPABILITIES;
        portal.run();
        assert!(portal.query_by_label("Record…").is_none());
        assert!(portal.query_by_label("Restore defaults").is_none());
        assert!(portal.query_by_label("Test the hotkeys above").is_none());
        assert_eq!(portal.get_all_by_label("Copy command").count(), 3);
        for mode in ["region", "all", "repeat"] {
            assert!(
                portal
                    .query_by_label(&format!("snp-ligase --capture {mode}"))
                    .is_some()
            );
        }
    }

    /// The dialog renders, collects actions, and edits the draft — through
    /// `egui_kittest`, so the real widgets are exercised.
    #[test]
    fn the_dialog_edits_the_draft_and_cancels_without_touching_the_original() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = harness(Settings::default());
        harness.run();

        // The five headings SPEC 10 asks for.
        for heading in ["Capture", "Saving", "Hotkeys", "Editor", "Advanced"] {
            assert!(
                harness.query_by_label(heading).is_some(),
                "the {heading} heading is missing"
            );
        }

        // Ticking a box changes the draft, not the stored settings.
        harness
            .get_by_label("Include the mouse cursor in captures")
            .click();
        harness.run();
        assert!(harness.state().draft.include_cursor);

        // OK asks the app to apply it; the panel itself does nothing.
        harness.get_by_label("OK").click();
        harness.run();
        assert!(
            harness.state().actions.contains(&Action::ApplySettings),
            "OK did not ask to apply: {:?}",
            harness.state().actions
        );
    }

    /// LINUX_SUPPORT.md section 3, item 2: Settings reads the platform's
    /// capabilities. A control the platform cannot honour is disabled — with
    /// the reason in words for the Hotkeys section — instead of pretending to
    /// work.
    #[test]
    fn settings_disables_what_the_platform_cannot_do() {
        use egui_kittest::kittest::{NodeT as _, Queryable as _};

        let mut stub = harness(Settings::default());
        stub.state_mut().capabilities = Capabilities::NONE;
        stub.run();

        assert!(
            stub.get_by_label("Hide snp-ligase while capturing")
                .accesskit_node()
                .is_disabled(),
            "the hide checkbox works without the hide capability"
        );
        assert!(
            stub.get_by_label("Include the mouse cursor in captures")
                .accesskit_node()
                .is_disabled(),
            "the cursor checkbox works without the cursor capability"
        );
        assert!(
            stub.query_by_label(HOTKEYS_UNAVAILABLE).is_some(),
            "the Hotkeys section does not say why it is off"
        );
        assert!(
            !stub
                .get_by_label("Ask before quitting")
                .accesskit_node()
                .is_disabled(),
            "the quit confirmation is disabled where it is the only way out"
        );
        for node in stub.get_all_by_label("Record…") {
            assert!(
                node.accesskit_node().is_disabled(),
                "a Record button is live without global hotkeys"
            );
        }
        assert!(
            stub.get_by_label("Restore defaults")
                .accesskit_node()
                .is_disabled(),
            "Restore defaults is live without global hotkeys"
        );

        // A full platform leaves every one of them alone.
        let mut full = harness(Settings::default());
        full.run();
        assert!(
            !full
                .get_by_label("Hide snp-ligase while capturing")
                .accesskit_node()
                .is_disabled(),
            "the hide checkbox is disabled on a full platform"
        );
        assert!(
            full.query_by_label(HOTKEYS_UNAVAILABLE).is_none(),
            "a full platform still claims global hotkeys are missing"
        );
        // Where the window can hide to a tray, the X never quits, so the switch
        // would have nothing to do (LINUX_SUPPORT.md section 3, item 8).
        assert!(
            full.get_by_label("Ask before quitting")
                .accesskit_node()
                .is_disabled(),
            "the quit confirmation is live on a platform with a tray"
        );

        // LINUX_SUPPORT.md section 4, phase L1a: the X11 backend's own list.
        // Hotkeys are back (the L0b review carry-over), the cursor and the
        // window-hiding are real, and there is no tray — so the X11 sections
        // that were disabled for the stub are live here.
        #[cfg(target_os = "linux")]
        {
            let mut x11 = harness(Settings::default());
            x11.state_mut().capabilities = snp_ligase_platform::linux::x11::CAPABILITIES;
            x11.run();

            assert!(
                x11.query_by_label(HOTKEYS_UNAVAILABLE).is_none(),
                "X11 registers hotkeys itself, but Settings says they are missing"
            );
            for node in x11.get_all_by_label("Record…") {
                assert!(
                    !node.accesskit_node().is_disabled(),
                    "a Record button is disabled where hotkeys work"
                );
            }
            assert!(
                x11.get_by_label("Include the mouse cursor in captures")
                    .accesskit_node()
                    .is_disabled(),
                "the cursor checkbox is enabled where cursor pixels are unavailable"
            );
            assert!(
                !x11.get_by_label("Hide snp-ligase while capturing")
                    .accesskit_node()
                    .is_disabled(),
                "the hide checkbox is disabled where the window can be hidden"
            );
            assert!(
                !x11.get_by_label("Ask before quitting")
                    .accesskit_node()
                    .is_disabled(),
                "the quit confirmation is disabled where there is no tray"
            );
        }
    }

    /// NEXT_STEPS step 9c: Settings > Saving shows the full path a capture
    /// saved *now* would get, so the dated folder is obvious without a
    /// separate "dynamic" switch.
    #[test]
    fn the_saving_section_previews_the_full_path_of_a_capture_saved_now() {
        use egui_kittest::kittest::Queryable as _;

        // The platform's own folder shape, so the expected line is native:
        // `C:\Shots\<month>\` on Windows, `/tmp/Shots/<month>/` on Linux.
        let root = if cfg!(windows) {
            std::path::PathBuf::from("C:\\Shots")
        } else {
            std::path::PathBuf::from("/tmp/Shots")
        };
        let draft = Settings {
            save_folder: Some(root.join("%Y-%m")),
            ..Settings::default()
        };
        let mut harness = harness(draft);
        harness.run();

        let month = chrono::Local::now().format("%Y-%m").to_string();
        let expected = format!(
            "A capture saved now would be {}{sep}{month}{sep}",
            root.display(),
            sep = std::path::MAIN_SEPARATOR
        );
        assert!(
            harness.query_by_label_contains(&expected).is_some(),
            "the folder was not expanded in the preview line: {expected}"
        );
    }

    /// The hint next to the field says the folder takes `%` variables (SPEC
    /// 8.1: the option is explained in words).
    #[test]
    fn the_save_folder_field_says_that_it_takes_percent_variables() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = harness(Settings::default());
        harness.run();

        assert!(
            harness
                .query_by_label_contains("same % variables as the filename template")
                .is_some(),
            "the folder field does not explain its % variables"
        );
    }

    /// NEXT_STEPS step 10: Settings > Saving has the two expansion rows, all
    /// labelled in words (SPEC 8.1), with the colour button the fill needs.
    #[test]
    fn the_saving_section_has_the_expansion_rows() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = harness(Settings::default());
        harness.run();

        for label in [
            "Annotations outside the image",
            "Expand the image to include them",
            "Cut them off at the image edge",
            "Fill for the added area",
            "Colour",
            "Transparent",
        ] {
            assert!(
                harness.query_by_label(label).is_some(),
                "the Saving section has no row called {label}"
            );
        }
        assert!(
            harness
                .query_by_label_contains("Added area colour  #FFFFFF")
                .is_some(),
            "the fill colour button is missing"
        );
    }

    #[test]
    fn a_hotkey_conflict_is_shown_in_the_dialog() {
        use egui_kittest::kittest::Queryable as _;

        let mut draft = Settings::default();
        draft.hotkey_active_window = draft.hotkey_current_monitor.clone();

        let mut harness = harness(draft);
        harness.run();

        assert!(
            harness
                .query_by_label_contains("only one of them can work")
                .is_some(),
            "the conflict warning is not shown"
        );
    }

    /// NEXT_STEPS step 7 item 2: a parse error is a visible message, not the
    /// bare "?" marker SPEC 8.1 rule 1 forbids.
    #[test]
    fn a_bad_hotkey_shows_the_parsers_message_in_words() {
        use egui_kittest::kittest::Queryable as _;

        let draft = Settings {
            hotkey_region: "banana".to_owned(),
            ..Settings::default()
        };
        let mut harness = harness(draft);
        harness.run();

        assert!(
            harness
                .query_by_label_contains("is not a key combination")
                .is_some(),
            "the parse error is not shown in words"
        );
        assert!(
            harness.query_by_label("?").is_none(),
            "the old ? marker is still there"
        );
    }

    #[test]
    fn the_record_button_asks_the_app_to_record_that_hotkey() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = harness(Settings::default());
        harness.run();

        // One "Record…" per row, in HOTKEY_MODES order; the first is Region.
        // `click_accesskit` is used because the dialog is taller than the
        // harness's screen (the usual `click` needs the button on screen).
        harness
            .get_all_by_label("Record…")
            .next()
            .expect("a Record button")
            .click_accesskit();
        harness.run();

        assert!(
            harness
                .state()
                .actions
                .contains(&Action::RecordHotkey(CaptureMode::Region)),
            "the Record button did not ask for a recording: {:?}",
            harness.state().actions
        );
    }

    /// NEXT_STEPS step 7 item 1: while recording, the field says what to do,
    /// as visible text.
    #[test]
    fn the_field_says_to_press_the_keys_while_recording() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = harness(Settings::default());
        harness.state_mut().recording = Some(CaptureMode::Region);
        harness.run();

        assert!(
            harness
                .query_by_label_contains("Press the keys… (Esc cancels)")
                .is_some(),
            "the recording label is not shown"
        );
        // The other three rows keep their fields and buttons.
        assert_eq!(
            harness.query_all_by_label("Record…").count(),
            3,
            "the other rows should still offer a Record button"
        );
    }

    #[test]
    fn restore_defaults_resets_the_four_fields() {
        use egui_kittest::kittest::Queryable as _;

        let draft = Settings {
            hotkey_region: "banana".to_owned(),
            hotkey_current_monitor: String::new(),
            hotkey_all_monitors: "Ctrl+Alt+Z".to_owned(),
            ..Settings::default()
        };
        let mut harness = harness(draft);
        harness.run();

        // The button may be scrolled out of the dialog's visible area; the
        // accesskit click reaches it anyway.
        harness.get_by_label("Restore defaults").click_accesskit();
        harness.run();

        let defaults = default_hotkey_strings();
        let state = harness.state();
        assert_eq!(state.draft.hotkey_region, defaults[0]);
        assert_eq!(state.draft.hotkey_current_monitor, defaults[1]);
        assert_eq!(state.draft.hotkey_active_window, defaults[2]);
        assert_eq!(state.draft.hotkey_all_monitors, defaults[3]);
    }

    /// NEXT_STEPS step 7 item 3: the registration failures are shown in the
    /// dialog, not only in the status bar, and the Test button asks the app.
    #[test]
    fn registration_warnings_are_shown_in_the_dialog_and_test_asks_the_app() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = harness(Settings::default());
        harness.state_mut().warnings =
            vec!["Region (Ctrl+Alt+P) could not be registered: already registered.".to_owned()];
        harness.run();

        assert!(
            harness
                .query_by_label_contains("could not be registered")
                .is_some(),
            "the registration warning is not shown in the dialog"
        );

        harness
            .get_by_label("Test the hotkeys above")
            .click_accesskit();
        harness.run();
        assert!(
            harness.state().actions.contains(&Action::TestHotkeys),
            "Test did not ask the app: {:?}",
            harness.state().actions
        );
    }
}
