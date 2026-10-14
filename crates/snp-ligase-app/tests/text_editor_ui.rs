//! UI behaviour tests with `egui_kittest` (SPEC section 3, NEXT_STEPS 1.3).
//!
//! These cover widget behaviour that a pure function cannot reach: what egui's
//! `TextEdit` does with the keys the text editor in SPEC 7.5 relies on. The app
//! commits on Ctrl+Enter and cancels on Esc, which only works if the `TextEdit`
//! does not swallow or misinterpret either key.

use egui_kittest::kittest::Queryable as _;

/// Ctrl+Enter must not insert a newline, or the app could not use it to commit.
#[test]
fn ctrl_enter_does_not_insert_a_newline() {
    let mut harness = egui_kittest::Harness::new_ui_state(
        |ui, text: &mut String| {
            ui.add(egui::TextEdit::multiline(text).desired_rows(2));
        },
        String::from("hello"),
    );
    harness.run();

    let field = harness
        .query_all_by_role(egui::accesskit::Role::MultilineTextInput)
        .next()
        .expect("the text field");
    field.focus();
    harness.run();

    harness.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::Enter);
    harness.run();

    assert_eq!(
        harness.state(),
        "hello",
        "Ctrl+Enter changed the text instead of leaving it for the app"
    );
}

/// Plain Enter must insert a newline (SPEC 7.5: "Enter inserts a newline").
#[test]
fn enter_inserts_a_newline() {
    let mut harness = egui_kittest::Harness::new_ui_state(
        |ui, text: &mut String| {
            ui.add(egui::TextEdit::multiline(text).desired_rows(2));
        },
        String::from("one"),
    );
    harness.run();

    let field = harness
        .query_all_by_role(egui::accesskit::Role::MultilineTextInput)
        .next()
        .expect("the text field");
    field.focus();
    harness.run();

    harness.key_press(egui::Key::Enter);
    harness.run();

    assert_eq!(harness.state(), "one\n", "Enter did not insert a newline");
}

/// Esc must still be visible to the app while the field has focus, because the
/// app cancels the session on it (SPEC 7.5).
#[test]
fn escape_reaches_the_app_while_typing() {
    let mut harness = egui_kittest::Harness::new_ui_state(
        |ui, saw_escape: &mut bool| {
            ui.add(egui::TextEdit::multiline(&mut String::new()).desired_rows(2));
            if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                *saw_escape = true;
            }
        },
        false,
    );
    harness.run();

    let field = harness
        .query_all_by_role(egui::accesskit::Role::MultilineTextInput)
        .next()
        .expect("the text field");
    field.focus();
    harness.run();

    harness.key_press(egui::Key::Escape);
    harness.run();

    assert!(*harness.state(), "the TextEdit swallowed the Escape key");
}
