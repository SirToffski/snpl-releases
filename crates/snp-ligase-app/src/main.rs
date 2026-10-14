#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! snp-ligase — a Windows 11 screenshot and annotation tool.
//!
//! Milestone M1 adds the capture pipeline and the `--debug-capture` command it
//! is diagnosed with (SPEC section 6.1); M3 adds the editor (SPEC section 7).
//! `PROGRESS.md` holds the exact state of the project.

mod app;
mod canvas;
mod controller;
mod debug_capture;
mod debug_overlay;
mod hotkeys;
mod icon;
mod logging;
mod panels;
mod paths;
mod save;
mod settings;
mod shortcuts;
mod tab;
mod tool;
mod tools;
mod tray;
mod viewport;

use std::path::PathBuf;
use std::process::ExitCode;

/// The application name. It is the workspace name, the window title, the config
/// folder and the log file prefix. SPEC used `snapmark` as the placeholder
/// name; the project is `snp-ligase`, read "snip".
pub const APP_NAME: &str = "snp-ligase";

fn main() -> ExitCode {
    // Logging starts first, so even a bad command line is recorded: a capture
    // problem is diagnosed from the log file, never from memory.
    let paths = paths::Paths::discover(APP_NAME);
    let _log_guard = logging::init(&paths.log_dir, APP_NAME);

    match run(APP_NAME, paths) {
        Ok(code) => code,
        Err(err) => {
            tracing::error!(error = %err, "exiting with a failure");
            eprintln!("{APP_NAME}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The body of `main`, after logging is up.
///
/// Returns the exit code: the self-checks report through it, so "passed",
/// "failed" and "skipped because the machine is busy" are three different
/// outcomes (NEXT_STEPS section 4.3).
fn run(app_name: &str, paths: paths::Paths) -> anyhow::Result<ExitCode> {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config_dir = %paths.config_dir.display(),
        log_dir = %paths.log_dir.display(),
        "starting {app_name}"
    );

    // A release build is a GUI subsystem app and has no console of its own
    // (SPEC 6.1). Anything started from a terminal should be able to answer
    // back, so borrow that terminal's console before looking at the arguments.
    if std::env::args_os().len() > 1 {
        attach_to_parent_console();
    }

    match arguments()? {
        Command::Help => {
            print!("{}", help_text());
            Ok(ExitCode::SUCCESS)
        }

        // snip-ligase --debug-capture <dir>: capture with no window and write
        // the report (SPEC section 6.1).
        Command::DebugCapture { directory } => {
            debug_capture::run(&directory).map(|()| ExitCode::SUCCESS)
        }

        // snip-ligase --debug-overlay <variant>: drive the real overlay and
        // check it cleans up after itself (NEXT_STEPS section 4.3).
        Command::DebugOverlay {
            variant,
            allow_busy,
        } => Ok(debug_overlay::run(variant, allow_busy)),

        // The GUI. Capture modes, hotkeys and the tray.
        Command::Window { files } => {
            // SPEC 6.1: one instance per sign-in session. A second launch exits
            // immediately. The diagnostics (`--debug-capture`, `--debug-overlay`)
            // do not take the mutex, so they work while the app is open.
            let guard = match snp_ligase_platform::windows::single_instance::InstanceGuard::acquire(
                snp_ligase_platform::windows::single_instance::MUTEX_NAME,
            ) {
                Ok(Some(guard)) => Some(guard),
                Ok(None) => {
                    // G8: the other instance gets asked to bring its window
                    // forward, so double-clicking the icon again does not look
                    // like nothing happened. File arguments are still not
                    // forwarded (SPEC P2).
                    match snp_ligase_platform::windows::single_instance::ShowEvent::open() {
                        Ok(event) => match event.signal() {
                            Ok(()) => tracing::info!(
                                "another snp-ligase is running; asked it to show its window"
                            ),
                            Err(err) => tracing::warn!(
                                "another snp-ligase is running; the show request failed: {err}"
                            ),
                        },
                        Err(err) => tracing::warn!(
                            "another snp-ligase is running; it could not be asked to show \
                             its window: {err}"
                        ),
                    }
                    return Ok(ExitCode::SUCCESS);
                }
                Err(err) => {
                    // A mutex that cannot be created is not a reason to refuse
                    // to run: the worst case is two instances.
                    tracing::warn!("could not take the single-instance mutex: {err}");
                    None
                }
            };

            // SPEC section 10: the config file is loaded before anything that
            // depends on it — the hotkeys, the capture options and the editor
            // state all come from here.
            let (store, mut warnings) = settings::SettingsStore::load(paths.config_dir.clone());

            // The controller thread does the capturing. The hotkeys only send
            // it commands, and they are registered here, on the main thread,
            // because `global-hotkey` needs a message loop on the thread that
            // creates the manager — and this thread is the one that pumps
            // messages once eframe is running (SPEC 6.2).
            let platform = std::sync::Arc::new(snp_ligase_platform::WindowsPlatform::new());
            let window = std::sync::Arc::new(controller::WindowGate::default());
            let capture_settings = controller::CaptureSettings {
                hide_while_capturing: store.settings().hide_while_capturing,
                include_cursor: store.settings().include_cursor,
                hdr_highlights: store.settings().hdr_highlights,
                sdr_white_nits: store.settings().sdr_white_nits,
            };
            let controller = controller::Controller::spawn(
                platform,
                std::sync::Arc::clone(&window),
                capture_settings,
            );

            // The four hotkeys come from the settings file, so a changed key
            // works after a restart and a conflict is reported (SPEC 10).
            let (bindings, hotkey_warnings) = hotkeys::bindings_from(store.settings());
            warnings.extend(hotkey_warnings);
            let (hotkeys, hotkey_warnings) =
                hotkeys::Hotkeys::register(controller.command_sender(), bindings);
            warnings.extend(hotkey_warnings);

            let tray = match tray::Tray::install(controller.command_sender()) {
                Ok(tray) => Some(tray),
                Err(message) => {
                    // No tray icon is annoying, not fatal: every capture mode
                    // is still on the menu and the hotkeys.
                    tracing::warn!("{message}");
                    warnings.push(message);
                    None
                }
            };
            // G3: without a tray icon the X must not hide the window; there
            // would be no way back (a relaunch exits at the mutex).
            let tray_installed = tray.is_some();

            // SPEC section 10's "Start hidden in the tray": the app hides the
            // window on its second frame. It cannot be done on the first:
            // eframe itself calls `window.set_visible(true)` after the first
            // frame is painted (`eframe-0.36.2/src/native/epi_integration.rs:380-386`,
            // the "keep hidden until we've painted" fix), so a hide sent in the
            // first frame is overridden. `starts_hidden` is the one place that
            // decides, so this and the app cannot disagree.
            let start_hidden = app::starts_hidden(tray_installed, store.settings());

            let mut viewport = egui::ViewportBuilder::default()
                .with_title(app_name)
                .with_inner_size([1280.0, 800.0])
                .with_min_inner_size([720.0, 480.0]);
            // The title bar / taskbar icon. The exe's own icon is embedded by
            // build.rs; the running window needs its pixels here too.
            match icon::window_icon() {
                Ok(window_icon) => viewport = viewport.with_icon(std::sync::Arc::new(window_icon)),
                Err(message) => tracing::warn!("{message}; the window keeps the default icon"),
            }
            let native_options = eframe::NativeOptions {
                viewport,
                ..Default::default()
            };

            // G8: a named event a second launch signals. This thread waits on
            // it and shows the window directly through the gate — not through
            // the controller queue — so it works even while an overlay is open
            // and blocks the controller.
            match snp_ligase_platform::windows::single_instance::ShowEvent::create() {
                Ok(event) => {
                    let window = std::sync::Arc::clone(&window);
                    let spawned = std::thread::Builder::new()
                        .name("show-on-launch".to_owned())
                        .spawn(move || {
                            while event.wait() {
                                tracing::info!("another launch asked this instance for its window");
                                window.show();
                            }
                        });
                    if let Err(err) = spawned {
                        tracing::warn!("could not start the show-on-launch thread: {err}");
                    }
                }
                Err(err) => {
                    tracing::warn!("could not create the show-window event: {err}");
                }
            }

            let result = eframe::run_native(
                app_name,
                native_options,
                Box::new({
                    let log_dir = paths.log_dir.clone();
                    let window = std::sync::Arc::clone(&window);
                    move |cc| {
                        // R03: egui reads Ctrl+0 / Ctrl++ / Ctrl+- as commands to
                        // zoom the whole *interface* and consumes them, so the
                        // View menu's "Actual size" never reached the image. Turn
                        // that off and the keys become this app's again.
                        cc.egui_ctx
                            .options_mut(|options| options.zoom_with_keyboard = false);
                        // The controller shows and hides this window through
                        // egui's viewport commands, so it needs the context
                        // before the first capture can be asked for.
                        window.attach(cc.egui_ctx.clone());
                        Ok(Box::new(app::SnpLigaseApp::from_shell(app::Shell {
                            log_dir,
                            files,
                            controller,
                            hotkeys,
                            settings: store,
                            warnings,
                            tray_installed,
                            start_hidden,
                        })))
                    }
                }),
            );

            // The registrations live until the window is gone, then they are
            // released: dropping the tray icon removes it from the notification
            // area, and dropping the mutex lets the next launch in. The hotkeys
            // were moved into the app, which dropped them with itself.
            drop(tray);
            drop(guard);
            result
                .map(|()| ExitCode::SUCCESS)
                .map_err(anyhow::Error::from)
        }
    }
}

/// Borrows the console the program was started from, so `println!` reaches a
/// terminal even in a release build (which is built as a GUI subsystem app per
/// SPEC section 6.1).
///
/// This is one of the few Windows-specific spots SPEC allows in the app; it
/// exists only so the diagnostic command can talk to the human running it.
#[cfg(windows)]
fn attach_to_parent_console() {
    use std::os::windows::io::AsRawHandle;

    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;

    let console = || -> Option<()> {
        unsafe { ::windows::Win32::System::Console::AttachConsole(ATTACH_PARENT_PROCESS) }.ok()?;

        // The console is attached, but this process still has no stdout handle
        // for it: point the standard handles at CONOUT$ before anything prints.
        let conout = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONOUT$")
            .ok()?;
        let handle = ::windows::Win32::Foundation::HANDLE(conout.as_raw_handle());
        unsafe {
            ::windows::Win32::System::Console::SetStdHandle(
                ::windows::Win32::System::Console::STD_OUTPUT_HANDLE,
                handle,
            )
        }
        .ok()?;
        unsafe {
            ::windows::Win32::System::Console::SetStdHandle(
                ::windows::Win32::System::Console::STD_ERROR_HANDLE,
                handle,
            )
        }
        .ok()?;

        Some(())
    };

    if console().is_none() {
        tracing::debug!("no parent console to attach to; output goes to the log file only");
    }
}

/// No console handling is needed off Windows.
#[cfg(not(windows))]
fn attach_to_parent_console() {}

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    /// `--help`, `-h` or `/?`.
    Help,
    /// `--debug-capture <directory>`.
    DebugCapture { directory: PathBuf },
    /// `--debug-overlay <variant> [--allow-busy]`: the region overlay's
    /// self-check.
    DebugOverlay {
        variant: debug_overlay::Variant,
        allow_busy: bool,
    },
    /// Open the window, with one tab per file named on the command line
    /// (SPEC 7.1: CLI arguments open images).
    Window { files: Vec<PathBuf> },
}

/// The `--help` text.
fn help_text() -> String {
    format!(
        "{APP_NAME} {}\n\n  \
         {APP_NAME}                       open the editor\n  \
         {APP_NAME} <image>...            open each image in a tab\n  \
         {APP_NAME} --debug-capture <dir> capture every monitor into <dir>\n  \
         {APP_NAME} --debug-overlay <v>   drive the region overlay and check it cleans up\n                                   \
         (v = drag | esc | right-click | timeout)\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// Reads this process's command line.
fn arguments() -> anyhow::Result<Command> {
    parse(std::env::args_os().skip(1))
}

/// Parses a command line.
///
/// `snip-ligase` on its own opens the window; `snip-ligase a.png b.png` opens
/// both in tabs (SPEC 7.1); `snip-ligase --debug-capture <dir>` runs the
/// headless capture of SPEC 6.1. An unknown `-`-prefixed option is refused
/// rather than treated as a file name.
fn parse<I: IntoIterator<Item = std::ffi::OsString>>(args: I) -> anyhow::Result<Command> {
    let mut args = args.into_iter();

    match args.next().as_deref() {
        None => Ok(Command::Window { files: Vec::new() }),

        Some(flag) if flag == "--debug-capture" => {
            let directory = args.next().map(PathBuf::from).ok_or_else(|| {
                anyhow::anyhow!(
                    "--debug-capture needs a directory, e.g. --debug-capture C:\\Temp\\snp"
                )
            })?;
            if args.next().is_some() {
                anyhow::bail!("--debug-capture takes exactly one directory");
            }
            Ok(Command::DebugCapture { directory })
        }

        // --debug-overlay [variant] [--allow-busy]: the variant defaults to the
        // drag, which is the one that exercises the whole path. --allow-busy is
        // for the human: without it the check refuses to take the screen while
        // the keyboard or mouse was used in the last few seconds.
        Some(flag) if flag == "--debug-overlay" => {
            let mut variant = debug_overlay::Variant::Drag;
            let mut allow_busy = false;
            for argument in args.by_ref() {
                match argument.to_str() {
                    Some("--allow-busy") => allow_busy = true,
                    Some(name) => variant = debug_overlay::Variant::parse(name)?,
                    None => anyhow::bail!("--debug-overlay takes text arguments only"),
                }
            }
            Ok(Command::DebugOverlay {
                variant,
                allow_busy,
            })
        }

        Some(flag) if flag == "--help" || flag == "-h" || flag == "/?" => Ok(Command::Help),

        Some(first) if first.to_string_lossy().starts_with('-') => {
            anyhow::bail!("unknown option {}; try --help", first.to_string_lossy())
        }

        Some(first) => {
            // Everything else is a file to open. Whether it is an image the app
            // can read is decided when it is opened, so a bad path gets the same
            // message as a bad image rather than stopping the whole command.
            let mut files = vec![PathBuf::from(first)];
            files.extend(args.map(PathBuf::from));
            Ok(Command::Window { files })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> anyhow::Result<Command> {
        parse(args.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn no_arguments_opens_an_empty_window() {
        assert_eq!(
            parse_args(&[]).expect("should parse"),
            Command::Window { files: Vec::new() }
        );
    }

    #[test]
    fn files_become_tabs_in_order() {
        assert_eq!(
            parse_args(&["a.png", "b.png"]).expect("should parse"),
            Command::Window {
                files: vec![PathBuf::from("a.png"), PathBuf::from("b.png")]
            }
        );
    }

    #[test]
    fn debug_capture_takes_exactly_one_directory() {
        assert_eq!(
            parse_args(&["--debug-capture", "out"]).expect("should parse"),
            Command::DebugCapture {
                directory: PathBuf::from("out")
            }
        );
        assert!(
            parse_args(&["--debug-capture"]).is_err(),
            "a missing directory was accepted"
        );
        assert!(
            parse_args(&["--debug-capture", "out", "extra"]).is_err(),
            "a second directory was accepted"
        );
    }

    #[test]
    fn help_is_asked_for_rather_than_guessed_at() {
        for flag in ["--help", "-h", "/?"] {
            assert_eq!(
                parse_args(&[flag]).expect("should parse"),
                Command::Help,
                "{flag}"
            );
        }
    }

    #[test]
    fn an_unknown_option_is_refused_rather_than_opened_as_a_file() {
        // This is the bug the M3 CLI check found: an unrecognised flag used to
        // fall through to "open this file", which started the editor.
        let error = parse_args(&["--nonsense"]).expect_err("should be refused");
        assert!(error.to_string().contains("--nonsense"), "{error}");

        let error = parse_args(&["-x", "a.png"]).expect_err("should be refused");
        assert!(error.to_string().contains("-x"), "{error}");
    }

    #[test]
    fn the_help_text_names_every_command() {
        let text = help_text();
        assert!(text.contains(APP_NAME), "{text}");
        assert!(text.contains("--debug-capture"), "{text}");
        assert!(
            text.contains("--help") || text.contains("open the editor"),
            "{text}"
        );
    }
}
