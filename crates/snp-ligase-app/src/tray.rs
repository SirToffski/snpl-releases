//! The tray icon and its menu (SPEC section 6.1).
//!
//! Like the hotkeys, the tray is created on the main thread — tray-icon needs a
//! message loop on the thread that builds it — and its handlers only send
//! [`AppCommand`]s to the controller (SPEC 6.2). The window itself is shown
//! through [`crate::controller::WindowGate`], so a hidden window comes back
//! even when no frame is running.
//!
//! The menu is built from [`TrayAction`] data, not from hand-written rows, so
//! the menu and the handler cannot drift apart (the same rule the menu bar
//! follows in `panels`).

use std::sync::mpsc::Sender;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::controller::AppCommand;
use crate::panels::CaptureMode;

/// The tray icon's side, in pixels. Windows scales it to the taskbar's DPI.
const ICON_SIDE: u32 = 32;

/// A row of the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayAction {
    Capture(CaptureMode),
    ShowWindow,
    Settings,
    Quit,
}

impl TrayAction {
    /// Every row, in menu order (SPEC 6.1).
    const ALL: [Self; 7] = [
        Self::Capture(CaptureMode::Region),
        Self::Capture(CaptureMode::CurrentMonitor),
        Self::Capture(CaptureMode::AllMonitors),
        Self::Capture(CaptureMode::ActiveWindow),
        Self::ShowWindow,
        Self::Settings,
        Self::Quit,
    ];

    /// The row's text. Every control in this program says what it does
    /// (SPEC 8.1), and the tray is no exception.
    fn label(self) -> String {
        match self {
            Self::Capture(mode) => format!("Capture {}", mode.label()),
            Self::ShowWindow => "Show Window".to_owned(),
            Self::Settings => "Settings".to_owned(),
            Self::Quit => "Quit".to_owned(),
        }
    }

    /// A stable id, so the handler can tell the rows apart.
    fn id(self) -> &'static str {
        match self {
            Self::Capture(CaptureMode::Region) => "capture-region",
            Self::Capture(CaptureMode::CurrentMonitor) => "capture-current-monitor",
            Self::Capture(CaptureMode::AllMonitors) => "capture-all-monitors",
            Self::Capture(CaptureMode::ActiveWindow) => "capture-active-window",
            Self::Capture(CaptureMode::RepeatLastRegion) => "capture-repeat-last",
            Self::ShowWindow => "show-window",
            Self::Settings => "settings",
            Self::Quit => "quit",
        }
    }

    /// The row for a menu event's id, if it is one of ours.
    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }

    /// The command a row sends (SPEC 5.7: the tray starts immediately).
    fn command(self) -> AppCommand {
        match self {
            Self::Capture(mode) => AppCommand::Capture {
                mode,
                delay_seconds: 0,
            },
            Self::ShowWindow => AppCommand::ShowMainWindow,
            Self::Settings => AppCommand::OpenSettings,
            Self::Quit => AppCommand::Quit,
        }
    }
}

/// The installed tray icon. Keep it alive: dropping it removes the icon.
pub struct Tray {
    /// Never read; living here is the whole point.
    _icon: TrayIcon,
}

impl Tray {
    /// Builds the icon, the menu and the event handlers.
    ///
    /// # Errors
    ///
    /// A message for the log and the status bar when the shell refuses the
    /// icon. The program carries on without a tray icon: captures still work.
    pub fn install(commands: Sender<AppCommand>) -> Result<Self, String> {
        let menu = Menu::new();
        for action in TrayAction::ALL {
            // Two groups, because the first four rows capture and the rest do
            // not.
            if action == TrayAction::ShowWindow || action == TrayAction::Quit {
                menu.append(&PredefinedMenuItem::separator())
                    .map_err(|err| err.to_string())?;
            }
            let item = MenuItem::with_id(action.id(), action.label(), true, None);
            menu.append(&item).map_err(|err| err.to_string())?;
        }

        // The handlers run on the main thread's message pump, so they only
        // send (SPEC 6.2).
        let menu_commands = commands.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let Some(action) = TrayAction::from_id(event.id().as_ref()) else {
                return;
            };
            tracing::info!(row = action.label(), "tray menu");
            if menu_commands.send(action.command()).is_err() {
                tracing::warn!("the controller is gone; the tray did nothing");
            }
        }));

        let click_commands = commands;
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            // A left click shows the window (SPEC 6.1). The double-click
            // variant is Windows-only and covered by the same click on up.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                tracing::info!("tray icon clicked");
                let _ = click_commands.send(AppCommand::ShowMainWindow);
            }
        }));

        // The app's artwork; the hand-drawn plate below is only the fallback
        // for an embedded PNG that somehow does not decode.
        let (rgba, width, height) = match crate::icon::tray_pixels() {
            Ok(pixels) => (pixels.rgba, pixels.width, pixels.height),
            Err(message) => {
                tracing::warn!("{message}; the tray uses the drawn fallback icon");
                (icon_rgba(), ICON_SIDE, ICON_SIDE)
            }
        };
        let icon = Icon::from_rgba(rgba, width, height)
            .map_err(|err| format!("the tray icon image was refused: {err}"))?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(crate::APP_NAME)
            .with_icon(icon)
            .build()
            .map_err(|err| format!("the tray icon could not be created: {err}"))?;

        tracing::info!("tray icon installed");
        Ok(Self { _icon: icon })
    }
}

/// The fallback tray icon: a rounded red square with two white corner marks.
///
/// Used only if the embedded artwork (`crate::icon`) fails to decode, so the
/// tray never ends up with a blank square.
#[must_use]
fn icon_rgba() -> Vec<u8> {
    const RED: [u8; 4] = [232, 17, 35, 255]; // the SPEC 7.7 default stroke colour
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    let side = ICON_SIDE as i32;
    let mut pixels = vec![0u8; (ICON_SIDE * ICON_SIDE * 4) as usize];
    let mut put = |x: i32, y: i32, colour: [u8; 4]| {
        if (0..side).contains(&x) && (0..side).contains(&y) {
            let at = ((y * side + x) * 4) as usize;
            pixels[at..at + 4].copy_from_slice(&colour);
        }
    };

    // A red plate with the four single corner pixels cut, so it reads as a
    // rounded square even at 16 px.
    for y in 0..side {
        for x in 0..side {
            let corner_distance = x.min(side - 1 - x) + y.min(side - 1 - y);
            if corner_distance >= 2 {
                put(x, y, RED);
            }
        }
    }

    // Two white corner marks, like the corners of a selection rectangle.
    for step in 0..10 {
        put(5 + step, 5, WHITE);
        put(5, 5 + step, WHITE);
        put(side - 6 - step, side - 6, WHITE);
        put(side - 6, side - 6 - step, WHITE);
    }

    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_rows_are_the_spec_list() {
        let labels: Vec<String> = TrayAction::ALL.iter().map(|a| a.label()).collect();
        assert_eq!(
            labels,
            vec![
                "Capture Region",
                "Capture Current Monitor",
                "Capture All Monitors",
                "Capture Active Window",
                "Show Window",
                "Settings",
                "Quit"
            ]
        );
    }

    #[test]
    fn every_row_has_a_distinct_id_that_maps_back_to_it() {
        let mut ids: Vec<&str> = TrayAction::ALL.iter().map(|a| a.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), TrayAction::ALL.len(), "two rows share an id");

        for action in TrayAction::ALL {
            assert_eq!(TrayAction::from_id(action.id()), Some(action));
        }
        assert_eq!(TrayAction::from_id("something-else"), None);
    }

    #[test]
    fn the_capture_rows_send_the_mode_they_name() {
        for mode in [
            CaptureMode::Region,
            CaptureMode::CurrentMonitor,
            CaptureMode::AllMonitors,
            CaptureMode::ActiveWindow,
        ] {
            assert_eq!(
                TrayAction::Capture(mode).command(),
                AppCommand::Capture {
                    mode,
                    delay_seconds: 0
                }
            );
        }
        assert_eq!(TrayAction::ShowWindow.command(), AppCommand::ShowMainWindow);
        assert_eq!(TrayAction::Settings.command(), AppCommand::OpenSettings);
        assert_eq!(TrayAction::Quit.command(), AppCommand::Quit);
    }

    #[test]
    fn the_icon_is_a_well_formed_image() {
        let pixels = icon_rgba();
        assert_eq!(pixels.len(), (ICON_SIDE * ICON_SIDE * 4) as usize);

        let opaque_red = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| **pixel == [232, 17, 35, 255])
            .count();
        let white = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| **pixel == [255, 255, 255, 255])
            .count();
        assert!(opaque_red > 100, "the plate is mostly missing");
        assert!(white > 20, "the corner marks are missing");
    }
}
