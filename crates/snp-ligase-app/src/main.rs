#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! snp-ligase — a screenshot and annotation tool (Windows and Linux).
//!
//! Milestone M1 adds the capture pipeline and the `--debug-capture` command it
//! is diagnosed with (SPEC section 6.1); M3 adds the editor (SPEC section 7).
//! `PROGRESS.md` holds the exact state of the project.

mod app;
mod canvas;
mod clipboard;
mod controller;
mod debug_capture;
// The overlay's self-check: the Win32 overlay on Windows, the X11 one on Linux
// (LINUX_SUPPORT.md section 4, phase L1b). The variant names and the reporting
// are the same file; the run belongs to the platform.
#[cfg(any(windows, target_os = "linux"))]
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
#[cfg(windows)]
mod tray;
mod viewport;

use std::path::PathBuf;
use std::process::ExitCode;

/// The application name. It is the workspace name, the window title, the config
/// folder and the log file prefix. SPEC used `snapmark` as the placeholder
/// name; the project is `snp-ligase`, read "snip".
pub const APP_NAME: &str = "snp-ligase";

/// The program's own command name, for `--help` and the CLI commands Settings
/// offers to copy.
///
/// It is the running executable's file name, not a fixed string: the dev
/// build is `snp-ligase`, the released packages install `snpl`, and a command
/// that names the wrong binary does nothing when bound. Tests run from the
/// test harness, so they see the dev name.
pub fn command_name() -> String {
    if cfg!(test) {
        return APP_NAME.to_owned();
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| APP_NAME.to_owned())
}

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
            debug_capture::run(&directory, &paths).map(|()| ExitCode::SUCCESS)
        }

        // snip-ligase --debug-overlay <variant>: drive the real overlay and
        // check it cleans up after itself (NEXT_STEPS section 4.3).
        #[cfg(any(windows, target_os = "linux"))]
        Command::DebugOverlay {
            variant,
            allow_busy,
        } => Ok(debug_overlay::run(variant, allow_busy)),

        // snip-ligase --overlay <dir>: the region overlay itself, as the child
        // process `select_region` starts (LINUX_SUPPORT.md section 4, phase
        // L1b). Hidden on purpose: it is an implementation detail of the
        // capture path, never something to run by hand. Which windows to open
        // follows the session both children read the same run directory:
        // X11's override-redirect windows there, Wayland's fullscreen ones
        // here (LINUX_SUPPORT.md section 6, L3b).
        #[cfg(target_os = "linux")]
        Command::Overlay { directory } => {
            let session = snp_ligase_platform::linux::SessionEnv::read();
            if session.kind() == snp_ligase_platform::linux::SessionKind::Wayland {
                Ok(snp_ligase_platform::linux::wayland::overlay::run_child(
                    &directory,
                ))
            } else {
                Ok(snp_ligase_platform::linux::x11::overlay::run_child(
                    &directory,
                ))
            }
        }

        // The GUI. Capture modes, hotkeys and the tray.
        Command::Window { files, capture } => {
            // LINUX_SUPPORT.md section 3, item 9: a second launch hands what it
            // was asked to do — show the window, run one capture, open image
            // files — to the instance that is already running, and only a
            // launch that finds nobody listening starts a window of its own.
            #[cfg(unix)]
            match forward(capture, &files) {
                Forwarded::HandedOver => return Ok(ExitCode::SUCCESS),
                Forwarded::NobodyListening => {}
            }

            // SPEC 6.1: one instance per sign-in session. A second launch exits
            // immediately. The diagnostics (`--debug-capture`, `--debug-overlay`)
            // do not take the mutex, so they work while the app is open.
            //
            // Windows takes a mutex here; on Linux the instance channel's
            // socket, bound below, is what makes this launch the one instance.
            #[cfg(windows)]
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

            // The instance channel's listening end (LINUX_SUPPORT.md section 3,
            // item 9): the launch that nobody answered binds it, so the next
            // launch finds this instance. A socket a crashed run left behind is
            // replaced; a live one means another instance raced this one in,
            // and this launch runs without a channel rather than refusing to
            // open.
            #[cfg(unix)]
            let channel = match snp_ligase_platform::instance::Server::bind() {
                Ok(server) => Some(server),
                Err(err) => {
                    tracing::warn!("the instance channel is not available: {err}");
                    None
                }
            };
            // The channel thread owns the server, and a quit ends this process
            // before that thread could drop it. This handle is what removes the
            // socket on the way out, below — and only while it is still ours: a
            // launch that replaced a stale one owns the new socket.
            #[cfg(unix)]
            let socket = channel
                .as_ref()
                .map(snp_ligase_platform::instance::Server::socket_file);

            // The controller thread does the capturing. The hotkeys only send
            // it commands, and they are registered here, on the main thread,
            // because `global-hotkey` needs a message loop on the thread that
            // creates the manager — and this thread is the one that pumps
            // messages once eframe is running (SPEC 6.2).
            let platform = std::sync::Arc::new(snp_ligase_platform::CurrentPlatform::new());
            // What this backend can really do (LINUX_SUPPORT.md section 3,
            // item 2): the menus and Settings disable what is missing, and the
            // controller refuses a capture it cannot run.
            let capabilities = {
                use snp_ligase_platform::Platform as _;
                platform.capabilities()
            };
            // Which backend, session and desktop this is (LINUX_SUPPORT.md
            // sections 4 and 10): the first line a Linux bug report is read
            // for, and it costs nothing to log it next to the capabilities.
            let environment = {
                use snp_ligase_platform::Platform as _;
                platform.environment()
            };
            tracing::info!(
                ?capabilities,
                backend = environment.backend,
                session_type = environment.session_type.as_deref().unwrap_or("-"),
                desktop = environment.desktop.as_deref().unwrap_or("-"),
                "platform capabilities"
            );
            let window = std::sync::Arc::new(controller::WindowGate::default());
            window.set_wayland(environment.backend == "wayland-portal");
            window.set_auto_save(store.settings().clone());
            let capture_settings = controller::CaptureSettings {
                copy_new_captures: store.settings().copy_new_captures,
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
            #[cfg(target_os = "linux")]
            let portal_hotkeys =
                capabilities.global_hotkeys == snp_ligase_platform::HotkeySupport::Portal;
            #[cfg(not(target_os = "linux"))]
            let portal_hotkeys = false;
            let (hotkeys, hotkey_warnings) = if portal_hotkeys {
                #[cfg(target_os = "linux")]
                {
                    hotkeys::Hotkeys::register_portal(
                        capabilities,
                        controller.command_sender(),
                        controller.event_sender(),
                        std::sync::Arc::clone(&window),
                    )
                }
                #[cfg(not(target_os = "linux"))]
                {
                    unreachable!("portal hotkeys are Linux only")
                }
            } else {
                hotkeys::Hotkeys::register(controller.command_sender(), bindings)
            };
            warnings.extend(hotkey_warnings);

            #[cfg(windows)]
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
            #[cfg(windows)]
            let tray_installed = tray.is_some();
            // LINUX_SUPPORT.md section 1: no tray icon on Linux for now, so
            // the X exits instead of hiding (the close dialog is L0d).
            #[cfg(not(windows))]
            let tray_installed = {
                tracing::info!("no tray icon on this platform (LINUX_SUPPORT.md section 1)");
                false
            };

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
                // Matches `packaging/linux/snp-ligase.desktop`: on Wayland the
                // compositor (and KDE's portals) find the menu entry and its
                // icon by this id; on X11 it is the window's WM_CLASS.
                .with_app_id(APP_NAME)
                .with_inner_size([1280.0, 800.0])
                .with_min_inner_size([720.0, 480.0]);
            // The title bar / taskbar icon. On Windows the exe's own icon is
            // embedded by build.rs; the running window needs its pixels here
            // too (X11 shows it in the window list; Wayland takes the icon from
            // the .desktop file instead, LINUX_SUPPORT.md L2).
            match icon::window_icon() {
                Ok(window_icon) => viewport = viewport.with_icon(std::sync::Arc::new(window_icon)),
                Err(message) => tracing::warn!("{message}; the window keeps the default icon"),
            }
            let native_options = eframe::NativeOptions {
                viewport,
                ..Default::default()
            };

            // This launch is the one instance, so a capture it was asked for
            // (`--capture <mode>`, when nobody was running) runs in it, right
            // after the controller is up (LINUX_SUPPORT.md section 3, item 9).
            // It starts immediately, like a hotkey's (SPEC 5.7), rather than
            // after the Capture menu's delay.
            if let Some(mode) = capture {
                tracing::info!(?mode, "running the capture this launch was asked for");
                controller.send(controller::AppCommand::Capture {
                    mode: capture_mode(mode),
                    delay_seconds: 0,
                });
            }

            // The instance channel's thread (LINUX_SUPPORT.md section 3, item
            // 9): every message a later launch forwards is turned into what
            // that launch would have done. Like the Windows show event below,
            // `show` goes straight through the gate — not through the
            // controller queue — so it works while an overlay blocks the
            // controller.
            #[cfg(unix)]
            if let Some(server) = channel {
                let window = std::sync::Arc::clone(&window);
                let commands = controller.command_sender();
                let events = controller.event_sender();
                let spawned = std::thread::Builder::new()
                    .name("instance-channel".to_owned())
                    .spawn(move || serve(&server, &window, &commands, &events));
                if let Err(err) = spawned {
                    tracing::warn!("could not start the instance channel thread: {err}");
                }
            }

            // G8: a named event a second launch signals. This thread waits on
            // it and shows the window directly through the gate — not through
            // the controller queue — so it works even while an overlay is open
            // and blocks the controller. Windows-only with the mutex above.
            #[cfg(windows)]
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
                            capabilities,
                        })))
                    }
                }),
            );

            // The registrations live until the window is gone, then they are
            // released: dropping the tray icon removes it from the notification
            // area, and dropping the mutex lets the next launch in. The hotkeys
            // were moved into the app, which dropped them with itself.
            #[cfg(windows)]
            drop(tray);
            #[cfg(windows)]
            drop(guard);
            // The instance channel's socket goes with the window too
            // (LINUX_SUPPORT.md section 3, item 9).
            #[cfg(unix)]
            if let Some(socket) = socket {
                socket.remove();
            }
            result
                .map(|()| ExitCode::SUCCESS)
                .map_err(anyhow::Error::from)
        }
    }
}

/// What a second launch did with what it was asked to do (LINUX_SUPPORT.md
/// section 3, item 9).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Forwarded {
    /// The running instance took it; this launch is done.
    HandedOver,
    /// Nobody answered: this launch becomes the instance.
    NobodyListening,
}

/// Hands the request to the running instance, if there is one.
///
/// What the command line means to an instance that is already up: a capture it
/// should run, the files it should open, or — with nothing else to say — a
/// plain launch, which is "bring your window forward" (G8's job on Windows).
#[cfg(unix)]
fn forward(
    capture: Option<snp_ligase_platform::instance::CaptureRequest>,
    files: &[PathBuf],
) -> Forwarded {
    use snp_ligase_platform::instance::{self, Message};

    let mut messages: Vec<Message> = match (capture, files) {
        (Some(mode), _) => vec![Message::Capture(mode)],
        (None, []) => vec![Message::Show],
        (None, files) => files.iter().cloned().map(Message::Open).collect(),
    };
    if let Ok(token) = std::env::var("XDG_ACTIVATION_TOKEN")
        && !token.is_empty()
    {
        messages = messages
            .into_iter()
            .enumerate()
            .map(|(index, request)| {
                if index == 0 {
                    Message::Activated {
                        token: token.clone(),
                        request: Box::new(request),
                    }
                } else {
                    request
                }
            })
            .collect();
    }
    match instance::send(&messages) {
        Ok(true) => {
            tracing::info!(
                messages = messages.len(),
                "the running instance took the request"
            );
            Forwarded::HandedOver
        }
        Ok(false) => {
            tracing::info!("no snp-ligase is running; this launch becomes it");
            Forwarded::NobodyListening
        }
        Err(err) => {
            // A broken channel is not a reason to refuse to start: the worst
            // case is a second window, which is what Linux did before L0d.
            tracing::warn!("the instance channel failed; starting anyway: {err}");
            Forwarded::NobodyListening
        }
    }
}

/// The instance channel's thread: one message at a time, for the life of the
/// process (LINUX_SUPPORT.md section 3, item 9).
#[cfg(unix)]
fn serve(
    server: &snp_ligase_platform::instance::Server,
    window: &controller::WindowGate,
    commands: &std::sync::mpsc::Sender<controller::AppCommand>,
    events: &std::sync::mpsc::Sender<controller::UiEvent>,
) {
    loop {
        match server.recv() {
            Ok(message) => apply_channel_message(message, window, commands, events),
            Err(err) => {
                tracing::warn!("the instance channel: {err}");
                // `recv` only fails when accepting the connection did, and that
                // is usually a full descriptor table: a pause keeps the retry
                // from becoming a busy loop.
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// One message from another launch, turned into what that launch would have
/// done itself.
///
/// Takes the three handles instead of the whole app, so a test can drive it
/// without a window (NEXT_STEPS section 1).
#[cfg(unix)]
fn apply_channel_message(
    message: snp_ligase_platform::instance::Message,
    window: &controller::WindowGate,
    commands: &std::sync::mpsc::Sender<controller::AppCommand>,
    events: &std::sync::mpsc::Sender<controller::UiEvent>,
) {
    use snp_ligase_platform::instance::Message;

    match message {
        Message::Activated { token, request } => {
            if let Message::Capture(mode) = *request {
                if commands
                    .send(controller::AppCommand::ActivatedCapture {
                        mode: capture_mode(mode),
                        delay_seconds: 0,
                        token: Some(token),
                    })
                    .is_err()
                {
                    tracing::warn!("the controller is gone; activated capture dropped");
                }
            } else {
                window.show_activated(Some(token));
                apply_channel_message(*request, window, commands, events);
            }
        }
        Message::Show => {
            tracing::info!(target: "snp_ligase::channel", "another launch asked for the window");
            window.show();
        }
        Message::Capture(mode) => {
            let mode = capture_mode(mode);
            tracing::info!(
                target: "snp_ligase::channel",
                ?mode,
                "another launch asked for a capture"
            );
            // Zero delay, like a hotkey's: SPEC 5.7 says those start
            // immediately, and the delay is the Capture menu's.
            let request = controller::AppCommand::Capture {
                mode,
                delay_seconds: 0,
            };
            if commands.send(request).is_err() {
                tracing::warn!("the controller is gone; the capture was dropped");
            }
        }
        Message::Open(path) => {
            tracing::info!(
                target: "snp_ligase::channel",
                path = %path.display(),
                "another launch asked for a file"
            );
            if events
                .send(controller::UiEvent::OpenFiles(vec![path]))
                .is_err()
            {
                tracing::warn!("the app is gone; the file was dropped");
            }
            // The UI drains its events on the next frame and the window may be
            // hidden, so ask for that frame (G2's rule).
            window.show();
        }
    }
}

/// The app's capture mode for a channel request (LINUX_SUPPORT.md section 3,
/// item 9).
#[must_use]
fn capture_mode(request: snp_ligase_platform::instance::CaptureRequest) -> panels::CaptureMode {
    use snp_ligase_platform::instance::CaptureRequest;

    match request {
        CaptureRequest::Region => panels::CaptureMode::Region,
        CaptureRequest::Monitor => panels::CaptureMode::CurrentMonitor,
        CaptureRequest::All => panels::CaptureMode::AllMonitors,
        CaptureRequest::Window => panels::CaptureMode::ActiveWindow,
        CaptureRequest::Repeat => panels::CaptureMode::RepeatLastRegion,
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
    /// self-check, which drives the real overlay of this platform.
    #[cfg(any(windows, target_os = "linux"))]
    DebugOverlay {
        variant: debug_overlay::Variant,
        allow_busy: bool,
    },
    /// `--overlay <directory>`: the region overlay itself, the child process
    /// of the parent that froze the desktop (LINUX_SUPPORT.md section 4,
    /// phase L1b; the Wayland windows are section 6, L3b). Not in `--help`:
    /// only `select_region` starts it.
    #[cfg(target_os = "linux")]
    Overlay { directory: PathBuf },
    /// Open the window, with one tab per file named on the command line
    /// (SPEC 7.1: CLI arguments open images) and, with `--capture <mode>`, one
    /// capture to run at once (LINUX_SUPPORT.md section 3, item 9).
    Window {
        files: Vec<PathBuf>,
        /// The capture this launch was asked for, for the instance it hands
        /// the request to or starts itself. Always `None` where `--capture` is
        /// refused at parse time (the instance channel is Unix-only for now).
        capture: Option<snp_ligase_platform::instance::CaptureRequest>,
    },
}

/// `--capture`'s modes, in `--help`'s wording.
#[cfg(unix)]
fn capture_modes() -> String {
    use snp_ligase_platform::instance::CaptureRequest;
    CaptureRequest::ALL
        .iter()
        .map(|mode| mode.as_str())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The `--help` text.
fn help_text() -> String {
    // The usage lines name the binary as it was installed (`snp-ligase` in
    // development, `snpl` in the released packages); the title is the app.
    let command = command_name();
    let row = |usage: &str, what: &str| format!("  {:<33}{what}", format!("{command}{usage}"));
    let more = |what: &str| format!("  {:<33}{what}", "");
    let mut lines = vec![
        format!("{APP_NAME} {}\n", env!("CARGO_PKG_VERSION")),
        row("", "open the editor"),
        row(" <image>...", "open each image in a tab"),
    ];
    // `--capture` travels on the instance channel, which only the Unix builds
    // have wired (LINUX_SUPPORT.md section 3, item 9).
    #[cfg(unix)]
    {
        lines.push(row(
            " --capture <mode>",
            "run one capture now, in the running instance",
        ));
        lines.push(more(&format!("(mode: {})", capture_modes())));
    }
    lines.push(row(
        " --debug-capture <dir>",
        "capture every monitor into <dir>",
    ));
    lines.push(row(
        " --debug-overlay <v>",
        "drive the region overlay and check it cleans up",
    ));
    lines.push(more("(v = drag | esc | right-click | timeout)"));
    format!("{}\n", lines.join("\n"))
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
        None => Ok(Command::Window {
            files: Vec::new(),
            capture: None,
        }),

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

        // --capture <mode>: run one capture in the running instance, or in the
        // one this launch starts (LINUX_SUPPORT.md section 3, item 9). This is
        // the universal hotkey fallback on Linux: the user binds the command
        // in their desktop's own keyboard settings.
        #[cfg(unix)]
        Some(flag) if flag == "--capture" => {
            let mode = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("--capture needs a mode: {}", capture_modes()))?;
            let mode = mode
                .to_str()
                .and_then(snp_ligase_platform::instance::CaptureRequest::parse)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "--capture takes one of {}; got {}",
                        capture_modes(),
                        mode.to_string_lossy()
                    )
                })?;
            if args.next().is_some() {
                anyhow::bail!("--capture takes exactly one mode and no files");
            }
            Ok(Command::Window {
                files: Vec::new(),
                capture: Some(mode),
            })
        }

        // The same flag where the channel is not wired: say so instead of
        // starting a window that cannot forward anything.
        #[cfg(not(unix))]
        Some(flag) if flag == "--capture" => {
            anyhow::bail!(
                "--capture forwards to the running instance through the instance channel, \
                 which is Linux-only for now (LINUX_SUPPORT.md L0d)"
            )
        }

        // --overlay <dir>: the region overlay as a child process
        // (LINUX_SUPPORT.md section 4, phase L1b). Deliberately absent from
        // `--help`: the parent writes the run directory and starts this, the
        // human never does.
        #[cfg(target_os = "linux")]
        Some(flag) if flag == "--overlay" => {
            let directory = args.next().map(PathBuf::from).ok_or_else(|| {
                anyhow::anyhow!("--overlay needs the run directory the parent prepared")
            })?;
            if args.next().is_some() {
                anyhow::bail!("--overlay takes exactly one directory");
            }
            Ok(Command::Overlay { directory })
        }

        // --debug-overlay [variant] [--allow-busy]: the variant defaults to the
        // drag, which is the one that exercises the whole path. --allow-busy is
        // for the human: without it the check refuses to take the screen while
        // the keyboard or mouse was used in the last few seconds.
        #[cfg(any(windows, target_os = "linux"))]
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

        // The same flag on a platform with no overlay at all: say so instead of
        // pretending the command exists (LINUX_SUPPORT.md 3.1).
        #[cfg(not(any(windows, target_os = "linux")))]
        Some(flag) if flag == "--debug-overlay" => {
            anyhow::bail!(
                "--debug-overlay is not available on this platform; it drives the Win32 \
                 overlay on Windows and the X11 one on Linux"
            )
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
            Ok(Command::Window {
                files,
                capture: None,
            })
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
            Command::Window {
                files: Vec::new(),
                capture: None
            }
        );
    }

    #[test]
    fn files_become_tabs_in_order() {
        assert_eq!(
            parse_args(&["a.png", "b.png"]).expect("should parse"),
            Command::Window {
                files: vec![PathBuf::from("a.png"), PathBuf::from("b.png")],
                capture: None
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

    /// `--debug-overlay` drives a platform's own overlay, so where there is
    /// none it is refused with a reason instead of looking like an unknown flag
    /// (LINUX_SUPPORT.md section 3, item 1).
    #[cfg(not(any(windows, target_os = "linux")))]
    #[test]
    fn debug_overlay_is_refused_on_this_platform() {
        let error = parse_args(&["--debug-overlay"]).expect_err("should be refused");
        let text = error.to_string();
        assert!(text.contains("not available"), "{text}");
    }

    /// The overlay child process is started by `select_region` with the run
    /// directory it prepared, and by nothing else (LINUX_SUPPORT.md section 4,
    /// phase L1b).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_overlay_command_takes_exactly_one_directory() {
        assert_eq!(
            parse_args(&["--overlay", "/run/user/1000/snp-ligase-overlay-1"])
                .expect("should parse"),
            Command::Overlay {
                directory: PathBuf::from("/run/user/1000/snp-ligase-overlay-1")
            }
        );
        assert!(
            parse_args(&["--overlay"]).is_err(),
            "a missing directory was accepted"
        );
        assert!(
            parse_args(&["--overlay", "a", "b"]).is_err(),
            "a second directory was accepted"
        );
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
        // The hotkey fallback is worth naming where it works
        // (LINUX_SUPPORT.md section 3, item 9).
        #[cfg(unix)]
        assert!(text.contains("--capture"), "{text}");
        // The overlay child process stays out of it: `select_region` starts it,
        // the human never does (LINUX_SUPPORT.md section 4, phase L1b).
        #[cfg(target_os = "linux")]
        assert!(!text.contains("--overlay"), "{text}");
    }

    /// `--capture region|monitor|all|window|repeat` is the instance channel's
    /// CLI half (LINUX_SUPPORT.md section 3, item 9).
    #[cfg(unix)]
    #[test]
    fn the_capture_flag_takes_one_mode() {
        use snp_ligase_platform::instance::CaptureRequest;

        for mode in CaptureRequest::ALL {
            assert_eq!(
                parse_args(&["--capture", mode.as_str()]).expect("should parse"),
                Command::Window {
                    files: Vec::new(),
                    capture: Some(mode)
                },
                "{}",
                mode.as_str()
            );
        }

        let error = parse_args(&["--capture"]).expect_err("a missing mode was accepted");
        let text = error.to_string();
        assert!(text.contains("region"), "{text} does not name the modes");

        let error = parse_args(&["--capture", "everything"]).expect_err("a bad mode was accepted");
        assert!(error.to_string().contains("everything"), "{error}");

        // A capture launch has nothing to open: the two would fight over what
        // the instance should do first.
        assert!(
            parse_args(&["--capture", "region", "a.png"]).is_err(),
            "a file after --capture was accepted"
        );
    }

    /// `--capture` needs the instance channel, so where there is none the flag
    /// is refused with a reason instead of starting a window that cannot
    /// forward it (LINUX_SUPPORT.md section 3, item 9).
    #[cfg(not(unix))]
    #[test]
    fn the_capture_flag_is_refused_on_this_platform() {
        let error = parse_args(&["--capture", "region"]).expect_err("should be refused");
        let text = error.to_string();
        assert!(text.contains("Linux-only"), "{text}");
    }

    /// Every channel mode is one of the app's capture modes, and the mapping is
    /// the whole point of the type (LINUX_SUPPORT.md section 3, item 9).
    #[test]
    fn every_channel_mode_maps_onto_a_capture_mode() {
        use snp_ligase_platform::instance::CaptureRequest;

        let expected = [
            (CaptureRequest::Region, panels::CaptureMode::Region),
            (CaptureRequest::Monitor, panels::CaptureMode::CurrentMonitor),
            (CaptureRequest::All, panels::CaptureMode::AllMonitors),
            (CaptureRequest::Window, panels::CaptureMode::ActiveWindow),
            (
                CaptureRequest::Repeat,
                panels::CaptureMode::RepeatLastRegion,
            ),
        ];
        assert_eq!(expected.len(), CaptureRequest::ALL.len());
        for (request, mode) in expected {
            assert_eq!(capture_mode(request), mode, "{request:?}");
        }
    }

    /// One message from another launch does what that launch would have done:
    /// show the window, run the capture, open the file
    /// (LINUX_SUPPORT.md section 3, item 9).
    #[cfg(unix)]
    #[test]
    fn a_channel_message_does_what_the_other_launch_asked() {
        use controller::{AppCommand, UiEvent, WindowGate};
        use snp_ligase_platform::instance::{CaptureRequest, Message};

        let window = WindowGate::default();
        window.hide();
        let (commands, command_rx) = std::sync::mpsc::channel();
        let (events, event_rx) = std::sync::mpsc::channel();

        apply_channel_message(Message::Show, &window, &commands, &events);
        assert!(window.is_visible(), "the window was not asked to show");
        assert!(
            command_rx.try_iter().next().is_none(),
            "showing the window went through the controller instead of the gate"
        );

        apply_channel_message(
            Message::Capture(CaptureRequest::All),
            &window,
            &commands,
            &events,
        );
        let sent: Vec<AppCommand> = command_rx.try_iter().collect();
        assert_eq!(
            sent,
            vec![AppCommand::Capture {
                mode: panels::CaptureMode::AllMonitors,
                delay_seconds: 0
            }],
            "the capture did not reach the controller"
        );

        apply_channel_message(
            Message::Open(PathBuf::from("/tmp/a shot.png")),
            &window,
            &commands,
            &events,
        );
        let posted: Vec<UiEvent> = event_rx.try_iter().collect();
        match posted.as_slice() {
            [UiEvent::OpenFiles(files)] => {
                assert_eq!(files, &vec![PathBuf::from("/tmp/a shot.png")]);
            }
            other => panic!("the file was not handed to the UI: {other:?}"),
        }
    }
}
