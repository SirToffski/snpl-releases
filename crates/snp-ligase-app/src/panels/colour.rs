//! The colour control (SPEC section 8.4).
//!
//! "A labelled button showing the current colour swatch and its hex value. It
//! opens a popup with 12 preset swatches, the last 8 used colours, and egui's
//! full colour picker."
//!
//! The picker is egui's own: `egui::widgets::color_picker::color_picker_color32`
//! ships in 0.36.2 (an earlier pass through this file recorded it as missing and
//! built a hex field and four sliders instead; both are still here, and the hex
//! field is worth keeping — `DECISIONS.md` carries the correction).

use egui::{Color32, RichText};
use snp_ligase_core::model::Rgba;

/// The 12 preset swatches (SPEC 8.4).
pub const PRESETS: [(&str, Rgba); 12] = [
    ("Red", [232, 17, 35, 255]),
    ("Orange", [255, 140, 0, 255]),
    ("Amber", [255, 200, 0, 255]),
    ("Yellow", [255, 255, 0, 255]),
    ("Lime", [120, 220, 0, 255]),
    ("Green", [0, 176, 80, 255]),
    ("Teal", [0, 160, 160, 255]),
    ("Cyan", [0, 200, 255, 255]),
    ("Blue", [0, 120, 255, 255]),
    ("Indigo", [90, 90, 255, 255]),
    ("Magenta", [230, 0, 230, 255]),
    ("White", [255, 255, 255, 255]),
];

/// How many recent colours are remembered (SPEC 8.4: the last 8).
pub const RECENT_LIMIT: usize = 8;

/// A labelled button that shows the current colour and opens the colour popup.
///
/// The popup is shown **every frame**. `Popup::menu(&response)` is a toggle
/// popup whose open state lives in egui memory and which toggles itself on the
/// click, so returning early unless the button was clicked leaves it alive for
/// one frame and it disappears before it can be used (R06).
///
/// The close behaviour is `CloseOnClickOutside`, not the `menu` default: with
/// `CloseOnClick` any click inside the popup — the hex field, a picker, a
/// slider — closed it on the next frame, so only the preset swatches (which
/// close it on purpose with `ui.close()`) could be used (C1).
pub fn button(ui: &mut egui::Ui, label: &str, colour: &mut Rgba, recent: &mut Vec<Rgba>) {
    let hex = to_hex(*colour);
    let response = ui
        .button(RichText::new(format!("{label}  #{hex}")).color(to_color32(*colour)))
        .on_hover_text(format!("{label}: #{hex}. Click to choose another."));

    let popup = egui::Popup::menu(&response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(300.0);
            panel(ui, label, colour, recent);
        });
    // M4: an interaction that ended because the popup closed — Escape, a click
    // outside, the control going away — still leaves its one colour behind.
    if popup.is_none() {
        flush_pending(ui.ctx(), label, recent);
    }
}

/// The contents of the colour popup: presets, the last used colours, egui's full
/// picker and a hex field.
///
/// Also used on its own by the Annotation menu's "Stroke Colour…", which opens
/// this in a small window instead of a popup.
pub fn panel(ui: &mut egui::Ui, label: &str, colour: &mut Rgba, recent: &mut Vec<Rgba>) {
    let hex = to_hex(*colour);
    ui.heading(label);
    ui.label(format!("Current: #{hex}"));
    ui.separator();

    ui.label("Presets");
    ui.horizontal_wrapped(|ui| {
        for (name, preset) in PRESETS {
            let chosen = ui
                .add(
                    egui::Button::new(RichText::new(name).color(to_color32(preset)))
                        .fill(to_color32(preset)),
                )
                .on_hover_text(format!("{name}: #{}", to_hex(preset)))
                .clicked();
            if chosen {
                *colour = preset;
                remember(recent, preset);
                ui.close();
            }
        }
    });

    ui.separator();
    ui.label("Recently used");
    if recent.is_empty() {
        ui.label("None yet.");
    } else {
        ui.horizontal_wrapped(|ui| {
            for used in recent.clone() {
                let hex = format!("#{}", to_hex(used));
                let chosen = ui
                    .add(
                        egui::Button::new(RichText::new(&hex).color(to_color32(used)))
                            .fill(to_color32(used)),
                    )
                    .on_hover_text(hex.clone())
                    .clicked();
                if chosen {
                    *colour = used;
                    remember(recent, used);
                    ui.close();
                }
            }
        });
    }

    ui.separator();
    ui.label("Custom");
    custom(ui, label, colour, recent);
}

/// The custom part: egui's picker, a hex field and four labelled sliders.
fn custom(ui: &mut egui::Ui, label: &str, colour: &mut Rgba, recent: &mut Vec<Rgba>) {
    let pending_id = pending_id(label);
    // A colour this control's unfinished interaction has already produced
    // (M4): held in egui's own data, because the frame that remembers it may
    // be a later one than the frame that changed it.
    let mut pending: Option<Rgba> = ui.ctx().data_mut(|data| data.get_temp::<Rgba>(pending_id));

    // SPEC 8.4's "egui's full colour picker".
    //
    // The picker runs in straight alpha (NEXT_STEPS step 9a). Our model's
    // colour is straight alpha, while `Color32` is premultiplied, so handing
    // it a `Color32` and reading the result back with `.r()/.g()/.b()` — which
    // return the *premultiplied* channels — loses the RGB: at alpha 0 all three
    // are 0, so raising the alpha again gives black, and at any alpha below
    // 255 the stored colour gets darker on every change.
    let mut hsva = to_hsva(*colour);
    if egui::widgets::color_picker::color_picker_hsva_2d(
        ui,
        &mut hsva,
        egui::widgets::color_picker::Alpha::OnlyBlend,
    ) {
        let parsed = from_hsva(hsva, *colour);
        *colour = parsed;
        // Held, not remembered: a drag through the picker changes the colour
        // on every frame (M4).
        pending = Some(parsed);
    }

    let mut hex = to_hex(*colour);
    ui.horizontal(|ui| {
        ui.label("Hex");
        if ui
            .add(egui::TextEdit::singleline(&mut hex).desired_width(90.0))
            .on_hover_text("Six hex digits, for example E81123")
            .changed()
        {
            match from_hex(&hex) {
                Some(parsed) => {
                    *colour = parsed;
                    // A typed hex is one event, not a drag: it is remembered
                    // straight away.
                    remember(recent, parsed);
                }
                // A half-typed hex is not an error worth a message; the field
                // just goes back to what the colour really is.
                None => hex = to_hex(*colour),
            }
        }
    });

    let mut rgba = *colour;
    ui.horizontal(|ui| {
        slider(ui, "Red", &mut rgba[0]);
        slider(ui, "Green", &mut rgba[1]);
    });
    ui.horizontal(|ui| {
        slider(ui, "Blue", &mut rgba[2]);
        slider(ui, "Alpha", &mut rgba[3]);
    });
    if rgba != *colour {
        *colour = rgba;
        pending = Some(rgba);
    }

    // M4: the interaction ends when the pointer comes up, and that is the one
    // moment the drag's final colour is remembered. Without this a single drag
    // filled all 8 recent slots with shades a pixel apart.
    if ui.input(|input| input.pointer.any_released())
        && let Some(colour) = pending.take()
    {
        remember(recent, colour);
    }
    match pending {
        Some(colour) => {
            ui.ctx()
                .data_mut(|data| data.insert_temp(pending_id, colour));
        }
        None => {
            ui.ctx()
                .data_mut(|data| data.remove_temp::<Rgba>(pending_id));
        }
    }
}

/// Where one colour control's unfinished interaction is held (M4).
///
/// Keyed by the control's label rather than by a `Ui` id, because the frame
/// that has to flush it — the popup or window closing — may not have the same
/// `Ui` any more. Two colour controls with the same label are never open at
/// once (the panel's popup closes when a menu opens), and the worst a collision
/// could do is remember a colour the user really did pick.
fn pending_id(label: &str) -> egui::Id {
    egui::Id::new(("colour_pending", label))
}

/// Remembers the colour of an unfinished interaction, if there is one, and
/// clears it. Called when the control goes away — the popup or the window that
/// held it closed — so a drag that ended that way is not lost (M4).
pub fn flush_pending(ctx: &egui::Context, label: &str, recent: &mut Vec<Rgba>) {
    let pending = ctx.data_mut(|data| data.remove_temp::<Rgba>(pending_id(label)));
    if let Some(colour) = pending {
        remember(recent, colour);
    }
}

/// One labelled slider, so no control is a bare number (SPEC 8.1).
fn slider(ui: &mut egui::Ui, label: &str, value: &mut u8) {
    ui.vertical(|ui| {
        ui.label(label);
        let mut number = f32::from(*value);
        if ui
            .add(
                egui::Slider::new(&mut number, 0.0..=255.0)
                    .clamping(egui::SliderClamping::Always)
                    .show_value(true),
            )
            .changed()
        {
            *value = number.round().clamp(0.0, 255.0) as u8;
        }
    });
}

/// Adds a colour to the recent list, most recent first, without duplicates.
pub fn remember(recent: &mut Vec<Rgba>, colour: Rgba) {
    recent.retain(|used| *used != colour);
    recent.insert(0, colour);
    recent.truncate(RECENT_LIMIT);
}

/// `#RRGGBB` for a colour.
#[must_use]
pub fn to_hex(colour: Rgba) -> String {
    format!("{:02X}{:02X}{:02X}", colour[0], colour[1], colour[2])
}

/// Parses `#RRGGBB` or `RRGGBB`.
#[must_use]
pub fn from_hex(text: &str) -> Option<Rgba> {
    let trimmed = text.trim().trim_start_matches('#');
    if trimmed.len() != 6 || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some([
        u8::from_str_radix(&trimmed[0..2], 16).ok()?,
        u8::from_str_radix(&trimmed[2..4], 16).ok()?,
        u8::from_str_radix(&trimmed[4..6], 16).ok()?,
        255,
    ])
}

/// The model's colour as egui's, which is straight alpha like the model's.
#[must_use]
pub fn to_color32(colour: Rgba) -> Color32 {
    Color32::from_rgba_unmultiplied(colour[0], colour[1], colour[2], colour[3])
}

/// Our straight-alpha `Rgba` as the picker's colour.
///
/// `Hsva::from_srgba_unmultiplied` cannot be used for this: it goes through the
/// premultiplied `Color32`, which stores an alpha of 0 as all zeros, so the
/// hue, saturation and value would be lost exactly when the colour needs
/// remembering. Here the hue, saturation and value come from the RGB alone and
/// the alpha travels beside them, which is what our model holds (NEXT_STEPS
/// step 9a).
#[must_use]
pub fn to_hsva(colour: Rgba) -> egui::ecolor::Hsva {
    let mut hsva = egui::ecolor::Hsva::from_srgb([colour[0], colour[1], colour[2]]);
    hsva.a = f32::from(colour[3]) / 255.0;
    hsva
}

/// The picker's colour back to our straight-alpha `Rgba`.
///
/// A picker colour with no alpha has nothing to show, so it keeps the RGB it
/// came from: raising the alpha again then brings the original colour back
/// instead of black (NEXT_STEPS step 9a).
#[must_use]
pub fn from_hsva(hsva: egui::ecolor::Hsva, previous: Rgba) -> Rgba {
    let [r, g, b, a] = hsva.to_srgba_unmultiplied();
    if a == 0 {
        [previous[0], previous[1], previous[2], 0]
    } else {
        [r, g, b, a]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let colour = [0xE8, 0x11, 0x23, 0xFF];
        assert_eq!(to_hex(colour), "E81123");
        assert_eq!(from_hex("E81123"), Some([0xE8, 0x11, 0x23, 0xFF]));
        assert_eq!(from_hex("#E81123"), Some([0xE8, 0x11, 0x23, 0xFF]));
        assert_eq!(from_hex(" e81123 "), Some([0xE8, 0x11, 0x23, 0xFF]));
    }

    #[test]
    fn rubbish_hex_is_rejected_rather_than_guessed_at() {
        assert_eq!(from_hex(""), None);
        assert_eq!(from_hex("E8112"), None);
        assert_eq!(from_hex("GGGGGG"), None);
        assert_eq!(from_hex("E81123FF"), None, "8 digits is not this control");
    }

    #[test]
    fn recent_colours_are_most_recent_first_without_duplicates() {
        let mut recent = Vec::new();
        remember(&mut recent, [1, 1, 1, 255]);
        remember(&mut recent, [2, 2, 2, 255]);
        remember(&mut recent, [1, 1, 1, 255]);
        assert_eq!(recent.len(), 2);
        assert_eq!(
            recent[0],
            [1, 1, 1, 255],
            "it did not move back to the front"
        );

        for index in 0..20 {
            remember(&mut recent, [index as u8, 0, 0, 255]);
        }
        assert!(recent.len() <= RECENT_LIMIT, "{} entries", recent.len());
    }

    #[test]
    fn there_are_twelve_presets_and_the_first_is_the_spec_red() {
        assert_eq!(PRESETS.len(), 12);
        assert_eq!(PRESETS[0], ("Red", [232, 17, 35, 255]));
    }

    #[test]
    fn colour_conversion_keeps_the_channels() {
        // An opaque colour survives unchanged.
        let opaque = [10, 20, 30, 255];
        assert_eq!(to_color32(opaque).to_array(), opaque);

        // egui stores premultiplied bytes, so a translucent colour comes back
        // premultiplied. That is what the GPU wants, and it is why the app
        // uploads with `from_rgba_unmultiplied` rather than assigning channels.
        let translucent = [200, 100, 50, 128];
        let stored = to_color32(translucent);
        assert_eq!(stored.a(), translucent[3], "alpha is not premultiplied");
        assert!(
            stored.r() < translucent[0],
            "red {} was not premultiplied",
            stored.r()
        );
    }

    /// The bug NEXT_STEPS step 9a reports: a colour whose alpha is set to 0
    /// came back black for good, and every picker change at alpha < 255 made
    /// it darker.
    ///
    /// The cause was running the picker in egui's premultiplied `Color32` and
    /// reading the result back with `.r()/.g()/.b()`, which are the
    /// *premultiplied* channels. Setting the alpha to 0 through the picker and
    /// raising it again must bring the original RGB back.
    #[test]
    fn an_alpha_of_zero_keeps_the_rgb_and_raising_it_brings_the_colour_back() {
        let original = [232, 17, 35, 255];

        let mut hsva = to_hsva(original);
        hsva.a = 0.0;
        let transparent = from_hsva(hsva, original);
        assert_eq!(
            transparent,
            [232, 17, 35, 0],
            "the RGB was lost at alpha 0: {transparent:?}"
        );

        // The next frame reconstructs the picker from the stored colour.
        let mut hsva = to_hsva(transparent);
        hsva.a = 1.0;
        let back = from_hsva(hsva, transparent);
        assert_eq!(
            back, original,
            "raising the alpha gave black instead of the original colour"
        );
    }

    /// The second half of the same bug: a no-op round trip through the picker
    /// must not darken a translucent colour.
    #[test]
    fn a_translucent_colour_is_not_darkened_by_the_round_trip() {
        let translucent = [200, 100, 50, 128];
        let round_tripped = from_hsva(to_hsva(translucent), translucent);
        for (index, channel) in round_tripped.iter().enumerate() {
            assert!(
                channel.abs_diff(translucent[index]) <= 1,
                "channel {index} went from {} to {channel}",
                translucent[index]
            );
        }
        assert_eq!(round_tripped[3], 128, "the alpha changed");
    }

    /// The three states a colour can be in through the picker: opaque,
    /// translucent and fully transparent (9a).
    #[test]
    fn every_alpha_round_trips_through_the_picker_conversion() {
        for alpha in [0u8, 1, 50, 128, 200, 254, 255] {
            let colour = [90, 160, 200, alpha];
            let back = from_hsva(to_hsva(colour), colour);
            assert_eq!(back, colour, "alpha {alpha} did not round trip");
        }
    }

    /// The state the popup test drives: the colour the control edits.
    struct PopupState {
        colour: Rgba,
        recent: Vec<Rgba>,
    }

    /// C1: a click **inside** the popup must not close it.
    ///
    /// `Popup::menu` defaults to `PopupCloseBehavior::CloseOnClick`, which
    /// closes on any click once the popup was open last frame — so the hex
    /// field, the picker and the sliders were all unusable: the first click
    /// dismissed the popup. Only a preset click closed it, which hid the bug.
    #[test]
    fn the_colour_popup_survives_a_click_inside_it() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut PopupState| {
                button(ui, "Stroke colour", &mut state.colour, &mut state.recent);
            },
            PopupState {
                colour: [232, 17, 35, 255],
                recent: Vec::new(),
            },
        );

        // Open the popup.
        harness.get_by_label_contains("Stroke colour").click();
        harness.run();
        assert!(
            harness.query_by_label("Presets").is_some(),
            "the colour popup did not open"
        );

        // Click a control inside it: the hex field.
        let field = harness
            .query_all_by_role(egui::accesskit::Role::TextInput)
            .next()
            .expect("the popup has no hex field");
        field.click();
        harness.run();

        // The popup must still be there.
        assert!(
            harness.query_by_label("Presets").is_some(),
            "clicking inside the popup closed it"
        );
    }

    /// M4: a slider drag changes the colour on every frame, and the old code
    /// remembered each change — one drag filled all 8 recent slots with shades
    /// a pixel apart. Only the drag's final colour may be remembered.
    #[test]
    fn a_twenty_frame_drag_adds_exactly_one_recent_colour() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut PopupState| {
                button(ui, "Stroke colour", &mut state.colour, &mut state.recent);
            },
            PopupState {
                colour: [232, 17, 35, 255],
                recent: Vec::new(),
            },
        );
        harness.get_by_label_contains("Stroke colour").click();
        harness.run();
        assert!(
            harness.query_by_label("Presets").is_some(),
            "the colour popup did not open"
        );

        // The first slider is the Red one (the sliders are built in the order
        // Red/Green then Blue/Alpha).
        let slider = harness
            .query_all_by_role(egui::accesskit::Role::Slider)
            .next()
            .expect("the red slider");
        let rect = slider.rect();
        let y = rect.center().y;
        let start = egui::pos2(rect.left() + 4.0, y);

        // Press, then 20 frames of dragging, then release: 22 frames, with the
        // colour changing on most of them.
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(start));
        harness.step();
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        });
        harness.step();
        for step in 1..=20 {
            let at = egui::pos2(start.x + step as f32 * 3.0, y);
            harness
                .input_mut()
                .events
                .push(egui::Event::PointerMoved(at));
            harness.step();
        }
        let end = egui::pos2(start.x + 60.0, y);
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: end,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        });
        harness.step();

        let state = harness.state();
        assert_eq!(
            state.recent.len(),
            1,
            "a 20-frame drag left {} recent colours: {:?}",
            state.recent.len(),
            state.recent
        );
        assert_eq!(
            state.recent[0], state.colour,
            "the remembered colour is not the drag's last one"
        );
    }
}
