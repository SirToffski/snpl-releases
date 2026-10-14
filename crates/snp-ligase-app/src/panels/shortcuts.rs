//! The Keyboard Shortcuts window (SPEC 8.3's Help > Keyboard Shortcuts).
//!
//! The rows are built by [`crate::shortcuts::help_rows`], which takes the tool
//! keys from the tool list and the capture keys from the user's settings, so
//! this window cannot list a key the app does not have. Like every other panel
//! it only draws: the flag it is given is what closes it.

use egui::Ui;

use crate::shortcuts::ShortcutRow;

/// Draws the window. `open` is cleared by the X or the Close button.
pub fn window(ui: &mut Ui, rows: &[ShortcutRow], open: &mut bool) {
    // The X writes into `is_open`; the Close button cannot touch it (the
    // builder holds that borrow), so it records the click and the two are
    // combined below.
    let mut is_open = *open;
    let mut close_clicked = false;
    egui::Window::new("Keyboard shortcuts")
        .collapsible(false)
        .resizable(true)
        .default_width(540.0)
        .open(&mut is_open)
        .show(ui.ctx(), |ui| {
            egui::ScrollArea::vertical()
                .max_height(560.0)
                .show(ui, |ui| {
                    let mut group = "";
                    for row in rows {
                        if row.group != group {
                            if !group.is_empty() {
                                ui.add_space(8.0);
                            }
                            group = row.group;
                            ui.heading(group);
                            ui.separator();
                        }
                        ui.horizontal(|ui| {
                            // A fixed-width key column keeps the list scannable
                            // (SPEC 8.1: words, not icons).
                            ui.add_sized(
                                [220.0, 18.0],
                                egui::Label::new(egui::RichText::new(&row.keys).strong()),
                            );
                            ui.label(&row.what);
                        });
                    }
                });
            ui.separator();
            if ui.button("Close").clicked() {
                close_clicked = true;
            }
        });
    if close_clicked {
        is_open = false;
    }
    *open = is_open;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use crate::shortcuts::help_rows;

    /// The window renders every group and row, and its Close button closes it.
    #[test]
    fn the_window_lists_the_rows_and_closes() {
        use egui_kittest::kittest::Queryable as _;

        struct State {
            rows: Vec<ShortcutRow>,
            open: bool,
        }

        let rows = help_rows(&Settings::default());
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut State| {
                let mut open = state.open;
                window(ui, &state.rows, &mut open);
                state.open = open;
            },
            State { rows, open: true },
        );
        harness.run();

        assert!(harness.query_by_label("Keyboard shortcuts").is_some());
        for group in ["File", "Edit", "Tools", "Help"] {
            assert!(
                harness.query_by_label(group).is_some(),
                "the {group} heading is missing"
            );
        }
        assert!(harness.query_by_label("Save As…").is_some());

        // `click_accesskit` works even when the button is below the harness
        // viewport: the window is taller than the test screen.
        harness.get_by_label("Close").click_accesskit();
        harness.run();
        assert!(!harness.state().open, "Close did not close the window");
    }
}
