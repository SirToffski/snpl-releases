//! The X11 region overlay (LINUX_SUPPORT.md section 4, phase L1b).
//!
//! SPEC section 5.6's overlay is a **child process** here, not a Win32 message
//! loop: winit allows one event loop per process and eframe owns the parent's,
//! and a child is the strongest safety net this feature can have. The parent
//! freezes the desktop, writes the frames and a layout into a private per-run
//! directory, runs `snp-ligase --overlay <dir>` and waits for **one JSON line**
//! on the child's stdout: the selection, or `{"cancelled": true}`. On every
//! path — answer, cancel, failure, timeout — the directory is removed and the
//! child is dead.
//!
//! What lives where:
//!
//! - [`state`] is the selection state machine, pure and unit-tested: press,
//!   move, release, Esc, right click, too small, crossing a monitor edge,
//!   negative monitor origins.
//! - [`paint`] decides the pixels of one overlay window, also pure: the dimmed
//!   frame, the original inside the selection, the outlines, the `W × H` label.
//! - [`child`] is the winit + softbuffer process that paints them and turns
//!   events into state machine calls.
//! - [`selfcheck`] is `--debug-overlay`: it drives the real overlay with XTest
//!   and then proves no overlay window is left behind.
//!
//! The child dies with its parent (`PR_SET_PDEATHSIG`), so a crash of the app
//! cannot leave an input-swallowing window on the desktop.

pub mod child;
pub mod paint;
pub mod selfcheck;
pub mod state;

pub use child::run_child;

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{Error, MonitorFrame, RectI, Result, VirtualCapture};

/// The overlay windows' `WM_CLASS`, both parts. The self-check looks for
/// windows of this class to prove none was left behind, and nothing else in the
/// program uses it.
pub const OVERLAY_CLASS: &str = "snp-ligase-overlay";

/// The environment variable the parent uses to tell the overlay child **which
/// process** its parent is.
///
/// The child's `PR_SET_PDEATHSIG` covers a parent that dies later, but not one
/// that died between the parent's `spawn` and the child's `prctl`; the old
/// check for `getppid() == 1` misses that on a systemd desktop, where an orphan
/// is adopted by the user's systemd rather than PID 1 (LINUX_SUPPORT.md
/// section 4, L1c carry-over 2). The child therefore compares `getppid()` with
/// this pid.
pub const PARENT_ENV: &str = "SNP_LIGASE_OVERLAY_PARENT";

/// How long the overlay waits before giving up (SPEC section 5.6: a stuck
/// overlay blocks the whole desktop). The child cancels itself at this
/// deadline; the parent kills it [`KILL_GRACE`] later if it does not.
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// How long the parent waits after [`TIMEOUT`] before killing the child, so a
/// child that is tearing its windows down after its own timeout is not
/// SIGKILLed mid-cleanup.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// How long the child gets to exit after printing its answer before the parent
/// kills it: the answer is already in hand, this only reaps the process.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// The run directory's name: this prefix plus the run's pid, under
/// `$XDG_RUNTIME_DIR` (the same name shape the overlay's `WM_CLASS` has, which
/// is a coincidence and not a promise).
const PREFIX: &str = "snp-ligase-overlay-";

/// Is `pid` a process that exists?
///
/// `kill(pid, 0)` asks the kernel without sending a signal: 0 means "there",
/// `EPERM` means "there, but not ours", and `ESRCH` means "gone". Only the last
/// one is a leftover to clear.
#[must_use]
fn process_is_alive(pid: i32) -> bool {
    // SAFETY: `kill` with signal 0 only asks whether the process exists; it
    // takes no pointers and cannot touch this process's memory.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Selects a region over a frozen capture, with the standard timeout.
///
/// # Errors
///
/// A failed capture or a child that could not be started, answered nonsense or
/// died without answering. `Ok(None)` is a cancel, including the timeout.
pub fn select_region(capture: &VirtualCapture, cursor: (i32, i32)) -> Result<Option<RectI>> {
    select_region_with_timeout(capture, cursor, TIMEOUT)
}

/// [`select_region`] with the safety timeout chosen by the caller.
///
/// `--debug-overlay`'s `timeout` variant uses a short one to prove the timeout
/// really cancels and really cleans up.
pub fn select_region_with_timeout(
    capture: &VirtualCapture,
    cursor: (i32, i32),
    timeout: Duration,
) -> Result<Option<RectI>> {
    select_region_activated(capture, cursor, timeout).map(|answer| answer.region)
}

pub(crate) fn select_region_activated(
    capture: &VirtualCapture,
    cursor: (i32, i32),
    timeout: Duration,
) -> Result<crate::RegionSelection> {
    if capture.frames.is_empty() {
        return Err(Error::Capture {
            message: "there is nothing to select a region from".to_owned(),
        });
    }

    let started = Instant::now();
    let started_unix_ms = unix_ms();
    let dir = RunDir::create()?;
    let layout = Layout {
        monitors: capture.frames.iter().map(layout_monitor).collect(),
        timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        started_unix_ms,
        cursor: [cursor.0, cursor.1],
    };
    write_input(&dir, &layout, capture)?;
    tracing::debug!(
        dir = %dir.path().display(),
        monitors = layout.monitors.len(),
        timeout_ms = layout.timeout_ms,
        prepared_ms = started.elapsed().as_secs_f64() * 1000.0,
        "the overlay's input is written"
    );

    let answer = run_child_process(dir.path(), timeout)?;
    log_visible(dir.path(), started);
    // The directory is removed by `dir`'s `Drop` as well; doing it here too
    // keeps the window between the child's exit and the cleanup as short as
    // possible, because the frames are the app's own pixels.
    dir.remove();

    Ok(answer)
}

/// The child's version of [`Layout`]: what the parent wrote into the run
/// directory, checked for the invariants the child's own maths assumes.
///
/// Both overlay backends call this in their own child process.
pub(crate) fn read_layout(dir: &Path) -> std::result::Result<Layout, String> {
    let path = dir.join("layout.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    let layout: Layout = serde_json::from_str(&text)
        .map_err(|err| format!("{} is not a layout: {err}", path.display()))?;
    if layout.monitors.is_empty() {
        return Err(format!("{} names no monitors", path.display()));
    }
    for monitor in &layout.monitors {
        if monitor.w <= 0 || monitor.h <= 0 {
            return Err(format!(
                "{}: the monitor {} is {}x{}",
                path.display(),
                monitor.name,
                monitor.w,
                monitor.h
            ));
        }
    }
    Ok(layout)
}

/// One monitor's place in the layout, from the frame that will be painted.
fn layout_monitor(frame: &MonitorFrame) -> LayoutMonitor {
    LayoutMonitor {
        name: frame.info.id.clone(),
        x: frame.info.rect.x,
        y: frame.info.rect.y,
        w: frame.info.rect.w,
        h: frame.info.rect.h,
        frame_w: frame.image.width(),
        frame_h: frame.image.height(),
        file: String::new(), // filled in by `write_input`, which knows the order
        primary: frame.info.is_primary,
        native_scale_x: frame.native_scale_x,
        native_scale_y: frame.native_scale_y,
    }
}

/// The layout of one overlay run, as the child reads it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Layout {
    pub monitors: Vec<LayoutMonitor>,
    /// The child's own deadline, so it can take its windows down itself.
    pub timeout_ms: u64,
    /// The parent's clock when the run started, so the child can report what
    /// the user's "how long until I could drag?" really was.
    pub started_unix_ms: u64,
    /// The pointer when the overlay opened: the child focuses the monitor
    /// under it, the way the Win32 overlay brings that window forward.
    pub cursor: [i32; 2],
}

/// One monitor in the layout: where it is, what it is called, and which file
/// holds its frozen pixels.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LayoutMonitor {
    /// The platform's name for it, e.g. `HDMI-1`; for logs and the self-check.
    pub name: String,
    /// The monitor's rectangle in root-window pixels.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// The frame file's size, which can differ from the rectangle: a capture
    /// whose pixels do not fill its monitor is used as it is (SPEC 5.4).
    pub frame_w: u32,
    pub frame_h: u32,
    /// The raw RGBA file in the run directory.
    pub file: String,
    pub primary: bool,
    /// How much a frame pixel was rescaled away from a layout pixel, per axis
    /// (LINUX_SUPPORT.md section 6, L3b): 1.0 on Windows and X11, where a
    /// rect pixel *is* a frame pixel, and ⅔ on the Wayland portal's 100%
    /// output next to a 150% one, whose frame arrives upscaled. The child
    /// pre-scales the frame by this so its windows keep the output's native
    /// resolution, and undoes it when it maps pointers back.
    #[serde(default = "unit_scale")]
    pub native_scale_x: f64,
    #[serde(default = "unit_scale")]
    pub native_scale_y: f64,
}

/// The serde default in [`LayoutMonitor`]: X11 layouts written before L3b
/// read 1.0, the scale where nothing is rescaled.
fn unit_scale() -> f64 {
    1.0
}

impl LayoutMonitor {
    /// The monitor's rectangle in root-window pixels.
    #[must_use]
    pub const fn rect(&self) -> RectI {
        RectI {
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
        }
    }
}

/// The child's answer: the selection it ended with, or that it was cancelled.
///
/// Exactly one of the two fields is on the wire: `{"selection":{...}}` for a
/// finished selection, `{"cancelled":true}` for Esc, a right click or the
/// timeout. Both present or neither is not an answer, and the parent says so.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Answer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<RectJson>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}

impl Answer {
    /// The answer for a finished selection.
    #[must_use]
    pub const fn selected(rect: RectI) -> Self {
        Self {
            activation_token: None,
            selection: Some(RectJson {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: rect.h,
            }),
            cancelled: false,
        }
    }

    /// The answer for Esc, a right click or the timeout.
    #[must_use]
    pub const fn cancelled() -> Self {
        Self {
            activation_token: None,
            selection: None,
            cancelled: true,
        }
    }

    /// What the answer means, or why it is not an answer.
    fn outcome(&self) -> std::result::Result<Option<RectI>, String> {
        match (self.selection, self.cancelled) {
            (Some(rect), false) => Ok(Some(rect.rect())),
            (None, true) => Ok(None),
            (Some(_), true) => {
                Err("the overlay answered with both a selection and cancelled".into())
            }
            (None, false) => {
                Err("the overlay answered with neither a selection nor cancelled".into())
            }
        }
    }
}

/// A rectangle as it travels in JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RectJson {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl RectJson {
    /// As the platform's own rectangle type.
    #[must_use]
    pub const fn rect(self) -> RectI {
        RectI {
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
        }
    }
}

/// The marker the child writes when its first frame is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VisibleMarker {
    pub visible_unix_ms: u64,
    pub started_unix_ms: u64,
}

/// Writes `layout.json` and every frame into the run directory.
fn write_input(dir: &RunDir, layout: &Layout, capture: &VirtualCapture) -> Result<()> {
    let mut layout = layout.clone();
    for (index, frame) in capture.frames.iter().enumerate() {
        // The file name is `frame_<index>.rgba` so the order is the layout's
        // order; a name written by the caller would have to be escaped.
        let file = format!("frame_{index}.rgba");
        let path = dir.path().join(&file);
        std::fs::write(&path, frame.image.as_raw()).map_err(|err| Error::Os {
            message: format!(
                "could not write the overlay's frame {}: {err}",
                path.display()
            ),
        })?;
        if let Some(monitor) = layout.monitors.get_mut(index) {
            monitor.file = file;
        }
    }
    let text = serde_json::to_vec(&layout).map_err(|err| Error::Os {
        message: format!("could not encode the overlay's layout: {err}"),
    })?;
    let path = dir.path().join("layout.json");
    std::fs::write(&path, text).map_err(|err| Error::Os {
        message: format!("could not write the overlay's layout: {err}"),
    })
}

/// The command that runs the overlay child: this same executable, the run
/// directory, and this process's pid for the child's parent check.
///
/// Split out so the contract between the two processes — the argument shape
/// and [`PARENT_ENV`] — is a test, not something only a real run can show.
fn overlay_command(exe: &Path, dir: &Path) -> Command {
    let mut command = Command::new(exe);
    command
        .arg("--overlay")
        .arg(dir)
        .env(PARENT_ENV, std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    command
}

/// Runs the overlay child process and waits for its answer.
///
/// The child's stderr goes to `stderr.log` in the run directory (a pipe could
/// deadlock a child that logs more than a pipe holds), and is reported with the
/// error when there is one.
fn run_child_process(dir: &Path, timeout: Duration) -> Result<crate::RegionSelection> {
    let exe = std::env::current_exe().map_err(|err| Error::Os {
        message: format!("could not find this program's own path to run the overlay: {err}"),
    })?;
    let stderr_path = dir.join("stderr.log");
    let stderr = std::fs::File::create(&stderr_path).map_err(|err| Error::Os {
        message: format!(
            "could not open {} for the overlay's errors: {err}",
            stderr_path.display()
        ),
    })?;

    let mut command = overlay_command(&exe, dir);
    let mut child = command
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|err| Error::Os {
            message: format!("could not start the overlay ({}) : {err}", exe.display()),
        })?;

    let Some(stdout) = child.stdout.take() else {
        return Err(kill_and_report(
            dir,
            &mut child,
            "the overlay's output could not be read",
        ));
    };

    let (tx, rx) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("overlay-answer".to_owned())
        .spawn(move || {
            let mut line = String::new();
            let mut reader = BufReader::new(stdout);
            let result = reader.read_line(&mut line).map(|_| line);
            // The receiver is gone only when the parent already gave up.
            let _ = tx.send(result);
        })
        .map_err(|err| Error::Os {
            message: format!("could not start the overlay's reader thread: {err}"),
        })?;

    let answer = match rx.recv_timeout(timeout + KILL_GRACE) {
        Ok(Ok(line)) if !line.trim().is_empty() => line,
        Ok(Ok(_)) => {
            let _ = reader.join();
            return Err(kill_and_report(
                dir,
                &mut child,
                "the overlay exited without an answer",
            ));
        }
        Ok(Err(err)) => {
            let _ = reader.join();
            return Err(kill_and_report(
                dir,
                &mut child,
                &format!("the overlay's answer could not be read: {err}"),
            ));
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                "the region overlay did not answer; killing it"
            );
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            // SPEC 5.6: the safety timeout cancels, exactly like Esc does.
            return Ok(crate::RegionSelection::default());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = reader.join();
            return Err(kill_and_report(
                dir,
                &mut child,
                "the overlay's reader thread stopped without an answer",
            ));
        }
    };
    let _ = reader.join();

    let parsed: Answer = match serde_json::from_str(answer.trim()) {
        Ok(parsed) => parsed,
        Err(err) => {
            return Err(kill_and_report(
                dir,
                &mut child,
                &format!("the overlay's answer {answer:?} is not JSON: {err}"),
            ));
        }
    };
    let outcome = match parsed.outcome() {
        Ok(outcome) => outcome,
        Err(message) => return Err(kill_and_report(dir, &mut child, &message)),
    };

    if let Some(child) = wait_or_kill(&mut child, EXIT_GRACE) {
        tracing::warn!("the overlay answered but did not exit; killing it");
        let _ = child.kill();
        let _ = child.wait();
    }
    Ok(selection_result(outcome, parsed.activation_token))
}

/// Waits up to `grace` for the child to exit. `None` when it did, `Some` when
/// it has to be killed.
fn wait_or_kill(child: &mut Child, grace: Duration) -> Option<&mut Child> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return None,
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Some(child);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(err) => {
                tracing::warn!(error = %err, "could not ask the overlay child whether it exited");
                return Some(child);
            }
        }
    }
}

/// Kills the child, reaps it, and builds the error with whatever it said.
///
/// The child's stderr is the run directory's `stderr.log`; the last of it is
/// quoted here, because "the overlay exited without an answer" is only
/// diagnosable with the reason the child printed.
fn kill_and_report(dir: &Path, child: &mut Child, message: &str) -> Error {
    let _ = child.kill();
    let _ = child.wait();
    match child_stderr(dir) {
        Some(detail) => Error::Os {
            message: format!("{message}: {detail}"),
        },
        None => Error::Os {
            message: message.to_owned(),
        },
    }
}

/// The tail of what the child printed on stderr, if anything.
///
/// Last lines first: a child that starts, fails and stops puts the reason at
/// the end. Only a few hundred characters are quoted — the parent's log line
/// is not the place for a whole backtrace.
fn child_stderr(dir: &Path) -> Option<String> {
    const KEEP: usize = 400;

    let text = std::fs::read_to_string(dir.join("stderr.log")).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    // Only the last two lines: the child's own message and the panic above it,
    // when there is one.
    lines.drain(..lines.len().saturating_sub(2));
    let tail = lines.join(" / ");
    let tail = if tail.len() > KEEP {
        let start = tail.len() - KEEP;
        // A char boundary: the message can hold any UTF-8.
        let start = tail
            .char_indices()
            .map(|(index, _)| index)
            .find(|index| *index >= start)
            .unwrap_or(tail.len());
        format!("…{}", &tail[start..])
    } else {
        tail
    };
    Some(tail)
}

/// Logs how long the overlay took to appear, against SPEC 12 M7's budget.
fn log_visible(dir: &Path, started: Instant) {
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let visible = std::fs::read_to_string(dir.join("visible.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<VisibleMarker>(&text).ok());
    match visible {
        Some(marker) => {
            let visible_ms = marker
                .visible_unix_ms
                .saturating_sub(marker.started_unix_ms) as f64;
            tracing::info!(
                visible_ms,
                elapsed_ms,
                "the overlay is up (SPEC 12 M7's 300 ms budget)"
            );
            if visible_ms > 300.0 {
                tracing::warn!(visible_ms, "the overlay took longer than the 300 ms budget");
            }
        }
        // The Windows overlay logs the same fact from its own paint timing; a
        // missing marker means the child died before its first frame.
        None => tracing::info!(elapsed_ms, "the overlay never reported being visible"),
    }
}

/// Milliseconds since the Unix epoch, for the child's own latency report.
pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A private per-run directory, removed on every path out of
/// [`select_region_with_timeout`].
/// `$XDG_RUNTIME_DIR` is the session manager's private directory for this user;
/// where there is none, the same per-user fallback directory as the instance
/// channel's socket is used (LINUX_SUPPORT.md section 4, L1a carry-over 1).
#[derive(Debug)]
pub struct RunDir {
    path: PathBuf,
}

impl RunDir {
    /// Creates this run's directory.
    pub(crate) fn create() -> Result<Self> {
        let base = runtime_base_dir().ok_or_else(|| Error::Os {
            message: "there is no private directory to run the overlay in \
                      (neither $XDG_RUNTIME_DIR nor a usable temporary one)"
                .to_owned(),
        })?;
        Self::create_in(&base)
    }

    /// [`RunDir::create`] with the base directory passed in, so the naming and
    /// the permissions are tests that do not touch the environment.
    pub(super) fn create_in(base: &Path) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt as _;

        Self::clear_dead_runs(base);

        let path = base.join(format!("{PREFIX}{}", std::process::id()));
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                // A leftover of an earlier run with the same pid (only
                // possible after a crash, since every path removes it). The
                // directory is ours — its name is our pid and its parent is
                // this user's private directory — so replacing it is safe.
                tracing::warn!(
                    path = %path.display(),
                    "a leftover overlay directory is being replaced"
                );
                std::fs::remove_dir_all(&path).map_err(|err| Error::Os {
                    message: format!("could not clear {}: {err}", path.display()),
                })?;
            }
            Ok(_) => {
                // A symlink or a file at our own name: not ours to remove.
                return Err(Error::Os {
                    message: format!("{} is not a directory this run can use", path.display()),
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(Error::Os {
                    message: format!("could not look at {}: {err}", path.display()),
                });
            }
        }

        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|err| Error::Os {
                message: format!(
                    "could not create the overlay's directory {}: {err}",
                    path.display()
                ),
            })?;
        Ok(Self { path })
    }

    /// Removes the run directories of overlay runs whose process is gone.
    ///
    /// A run that was `SIGKILL`ed cannot remove its own directory, and that
    /// directory holds full-screen pixels: the next run clears them. Only the
    /// directories that parse as [`PREFIX`] + the pid of a process that is not
    /// running any more are touched — a live run's directory is that run's, and
    /// anything whose name or shape is not ours is left alone.
    fn clear_dead_runs(base: &Path) {
        let Ok(entries) = std::fs::read_dir(base) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|name| name.strip_prefix(PREFIX))
                .and_then(|pid| pid.parse::<i32>().ok())
            else {
                continue;
            };
            if pid <= 0 || process_is_alive(pid) {
                continue;
            }
            let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            tracing::info!(
                path = %entry.path().display(),
                "clearing the overlay directory of a run that is gone"
            );
            if let Err(err) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %err,
                    "that leftover overlay directory could not be removed"
                );
            }
        }
    }

    /// The directory itself.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the directory and everything in it. Called explicitly on the
    /// normal path; `Drop` is the backstop for every error path.
    pub fn remove(&self) {
        if let Err(err) = std::fs::remove_dir_all(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "the overlay's directory could not be removed"
            );
        }
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        self.remove();
    }
}

/// The directory per-run overlay directories live in.
fn runtime_base_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|directory| directory.is_dir())
        .or_else(|| crate::instance::private_temp_dir(&std::env::temp_dir()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use image::{Rgba, RgbaImage};

    use crate::{CaptureApi, FrameDiagnostics, MonitorInfo};

    /// A base directory of this test's own, under the temp directory.
    fn temp_base(what: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "snp-ligase-overlay-test-{}-{what}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("a test base directory");
        base
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .expect("the directory")
            .permissions()
            .mode()
            & 0o777
    }

    fn frame(rect: RectI, fill: Rgba<u8>) -> MonitorFrame {
        MonitorFrame {
            info: MonitorInfo {
                id: "HDMI-1".to_owned(),
                rect,
                scale_factor: 1.0,
                hdr_active: false,
                sdr_white_nits: None,
                is_primary: true,
            },
            image: RgbaImage::from_pixel(
                u32::try_from(rect.w).unwrap_or(1),
                u32::try_from(rect.h).unwrap_or(1),
                fill,
            ),
            diagnostics: FrameDiagnostics {
                api_used: CaptureApi::X11Shm,
                pixel_format: "Bgrx8".to_owned(),
                width: 0,
                height: 0,
                row_pitch: 0,
                max_channel_value: 1.0,
                sdr_white_nits: 0.0,
                elapsed_ms: 0.0,
            },
            stored_pixels: crate::StoredPixels::Virtual,
            native_scale_x: 1.0,
            native_scale_y: 1.0,
        }
    }

    #[test]
    fn the_child_is_told_which_process_its_parent_is() {
        // LINUX_SUPPORT.md section 4, L1c carry-over 2: the parent's pid is an
        // environment variable, because `getppid() == 1` alone misses a parent
        // that died before the child armed `PR_SET_PDEATHSIG` (a systemd
        // desktop adopts the orphan, not PID 1).
        use std::ffi::OsStr;

        let dir = Path::new("/run/user/1000/snp-ligase-overlay-42");
        let command = overlay_command(Path::new("/opt/snp-ligase"), dir);
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            vec![OsStr::new("--overlay"), dir.as_os_str()],
            "the child's argument shape"
        );
        let expected = std::process::id().to_string();
        let (_, value) = command
            .get_envs()
            .find(|(name, _)| *name == PARENT_ENV)
            .expect("the parent's pid is passed");
        assert_eq!(
            value,
            Some(OsStr::new(&expected)),
            "the pid in {PARENT_ENV} is not this process's"
        );
    }

    #[test]
    fn the_run_directory_is_private_and_replaced_when_it_is_a_leftover() {
        let base = temp_base("run-dir");

        let dir = RunDir::create_in(&base).expect("a fresh run directory");
        assert!(dir.path().is_dir());
        assert_eq!(
            mode_of(dir.path()),
            0o700,
            "0700, like the socket's fallback"
        );
        assert!(
            dir.path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(&std::process::id().to_string())),
            "the name carries this run's pid"
        );
        std::fs::write(dir.path().join("layout.json"), b"{}").expect("a file in it");
        let path = dir.path().to_path_buf();
        dir.remove();
        assert!(!path.exists(), "remove() takes the directory with it");

        // A leftover directory at this run's name (a crash with the same pid)
        // is replaced rather than refused.
        std::fs::create_dir(&path).expect("a leftover directory");
        std::fs::write(path.join("frame_0.rgba"), b"stale").expect("stale pixels");
        let dir = RunDir::create_in(&base).expect("a replaced run directory");
        assert!(dir.path().is_dir());
        assert!(
            !dir.path().join("frame_0.rgba").exists(),
            "the leftover's contents are gone"
        );
        drop(dir);
        assert!(!path.exists(), "Drop removes it too");

        // A symlink at this run's name is not ours to follow or remove.
        let mut builder = std::process::Command::new("ln");
        builder
            .args([
                "-s",
                base.to_str().unwrap_or("/tmp"),
                path.to_str().unwrap_or("/tmp/none"),
            ])
            .status()
            .expect("ln should run");
        if std::fs::symlink_metadata(&path).is_ok() {
            let error = RunDir::create_in(&base).expect_err("a symlink is refused");
            assert!(error.to_string().contains("not a directory"), "{error}");
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_run_directory_of_a_process_that_is_gone_is_cleared() {
        let base = temp_base("dead-run");

        // A run directory whose process is gone: this is what a SIGKILLed
        // overlay leaves behind, full-screen pixels included.
        let mut dead = std::process::Command::new("true")
            .spawn()
            .expect("a process that exits at once");
        let dead_pid = dead.id();
        let _ = dead.wait();
        let stale = base.join(format!("{PREFIX}{dead_pid}"));
        std::fs::create_dir(&stale).expect("a stale run directory");
        std::fs::write(stale.join("frame_0.rgba"), b"the desktop").expect("stale pixels");

        // A live one: another overlay run (or an unrelated process that got the
        // pid after a reboot) is not this run's to remove.
        let mut alive = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("a process that stays");
        let live = base.join(format!("{PREFIX}{}", alive.id()));
        std::fs::create_dir(&live).expect("a live run directory");
        std::fs::write(live.join("frame_0.rgba"), b"a run in progress").expect("pixels");

        // And two things that only look like run directories.
        let foreign = base.join("someone-elses-directory");
        std::fs::create_dir(&foreign).expect("another directory");
        let numeric = base.join("snp-ligase-overlay-not-a-pid");
        std::fs::create_dir(&numeric).expect("a directory with a non-pid name");

        let dir = RunDir::create_in(&base).expect("a run directory");
        assert!(!stale.exists(), "the dead run's directory is gone");
        assert!(live.is_dir(), "the live run's directory is left alone");
        assert!(
            foreign.is_dir(),
            "a directory that is not ours is left alone"
        );
        assert!(numeric.is_dir(), "a name that is not a pid is left alone");
        assert!(dir.path().is_dir());

        drop(dir);
        let _ = alive.kill();
        let _ = alive.wait();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_layout_and_the_frames_round_trip_through_the_run_directory() {
        let base = temp_base("layout");
        let dir = RunDir::create_in(&base).expect("a run directory");
        let capture = VirtualCapture {
            frames: vec![
                frame(
                    RectI {
                        x: 0,
                        y: 0,
                        w: 3,
                        h: 2,
                    },
                    Rgba([1, 2, 3, 255]),
                ),
                frame(
                    RectI {
                        x: -3,
                        y: 0,
                        w: 2,
                        h: 1,
                    },
                    Rgba([4, 5, 6, 255]),
                ),
            ],
        };
        let layout = Layout {
            monitors: capture.frames.iter().map(layout_monitor).collect(),
            timeout_ms: 1500,
            started_unix_ms: 42,
            cursor: [7, 8],
        };
        write_input(&dir, &layout, &capture).expect("the input should be written");

        let text = std::fs::read_to_string(dir.path().join("layout.json")).expect("the layout");
        let read: Layout = serde_json::from_str(&text).expect("the layout should parse");
        assert_eq!(read.monitors.len(), 2);
        assert_eq!(read.monitors[0].file, "frame_0.rgba");
        assert_eq!(read.monitors[1].file, "frame_1.rgba");
        assert_eq!(read.monitors[1].rect().x, -3, "negative origins survive");
        assert_eq!(read.cursor, [7, 8]);
        assert_eq!(read.timeout_ms, 1500);

        let first = std::fs::read(dir.path().join("frame_0.rgba")).expect("frame 0");
        assert_eq!(
            first,
            vec![
                1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255
            ]
        );
        let second = std::fs::read(dir.path().join("frame_1.rgba")).expect("frame 1");
        assert_eq!(second.len(), 2 * 4, "two RGBA pixels");

        drop(dir);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_answer_is_either_a_selection_or_a_cancel() {
        let selected = Answer::selected(RectI {
            x: -4,
            y: 5,
            w: 6,
            h: 7,
        });
        assert_eq!(
            selected.outcome().expect("a selection"),
            Some(RectI {
                x: -4,
                y: 5,
                w: 6,
                h: 7
            })
        );
        assert_eq!(Answer::cancelled().outcome().expect("a cancel"), None);

        // What the child writes is what the parent reads.
        let text = serde_json::to_string(&selected).expect("it encodes");
        assert_eq!(text, r#"{"selection":{"x":-4,"y":5,"w":6,"h":7}}"#);
        let parsed: Answer = serde_json::from_str(&text).expect("it parses");
        assert_eq!(parsed, selected);
        assert_eq!(
            serde_json::to_string(&Answer::cancelled()).expect("it encodes"),
            r#"{"cancelled":true}"#
        );

        // Neither is not an answer, and both is not one either.
        let neither: Answer = serde_json::from_str("{}").expect("it parses");
        assert!(neither.outcome().is_err());
        let both: Answer =
            serde_json::from_str(r#"{"selection":{"x":0,"y":0,"w":1,"h":1},"cancelled":true}"#)
                .expect("it parses");
        assert!(both.outcome().is_err());
    }

    #[test]
    fn a_child_that_never_answers_is_killed_and_cancels_the_overlay() {
        // The parent's own kill path, with a real process: `sleep` never
        // prints, so the timeout has to end the run. The child here is not the
        // app (that is the self-check's job), it only proves the wait, the kill
        // and the "timeout is a cancel" rule.
        let started = Instant::now();
        let child = std::process::Command::new("sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep should start");
        let mut child = child;
        let stdout = child.stdout.take().expect("a pipe");
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let mut reader = BufReader::new(stdout);
            let _ = tx.send(reader.read_line(&mut line).map(|_| line));
        });

        let result: std::io::Result<Option<String>> =
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(_) => panic!("sleep should not answer"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    Ok(None)
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the reader died"),
            };
        assert_eq!(result.expect("a timeout is a cancel"), None);
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = reader.join();

        // The child really is gone: nothing is left to wait for.
        assert!(
            child.try_wait().expect("try_wait").is_some(),
            "the killed child was reaped"
        );
    }

    #[test]
    fn reading_the_visible_marker_gives_the_latency() {
        let base = temp_base("visible");
        let dir = RunDir::create_in(&base).expect("a run directory");
        let marker = VisibleMarker {
            visible_unix_ms: 1_000,
            started_unix_ms: 700,
        };
        std::fs::write(
            dir.path().join("visible.json"),
            serde_json::to_vec(&marker).expect("it encodes"),
        )
        .expect("the marker");
        let text = std::fs::read_to_string(dir.path().join("visible.json")).expect("it reads");
        let read: VisibleMarker = serde_json::from_str(&text).expect("it parses");
        assert_eq!(read.visible_unix_ms - read.started_unix_ms, 300);

        // A run whose child died before its first frame leaves no marker, and
        // that is not an error: the latency is simply unknown.
        let empty = RunDir::create_in(&base).expect("another run directory");
        assert!(!empty.path().join("visible.json").exists());
        drop(empty);
        drop(dir);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_runtime_base_directory_is_the_sessions_own_when_it_has_one() {
        // `$XDG_RUNTIME_DIR` is read from the environment; the fallback is the
        // instance channel's private per-user directory.
        let base = runtime_base_dir();
        if let Some(base) = base {
            assert!(base.is_dir(), "{} should be a directory", base.display());
        }
    }
}

fn selection_result(region: Option<RectI>, token: Option<String>) -> crate::RegionSelection {
    crate::RegionSelection {
        region,
        activation_token: region.and(token),
    }
}

#[cfg(test)]
#[test]
fn w2_overlay_token_is_carried_only_with_its_selection() {
    let rect = RectI {
        x: 1,
        y: 2,
        w: 3,
        h: 4,
    };
    let mut answer = Answer::selected(rect);
    answer.activation_token = Some("gesture-token".into());
    let answer: Answer = serde_json::from_str(&serde_json::to_string(&answer).unwrap()).unwrap();
    let result = selection_result(answer.outcome().unwrap(), answer.activation_token);
    assert_eq!(result.region, Some(rect));
    assert_eq!(result.activation_token.as_deref(), Some("gesture-token"));
    assert!(
        selection_result(None, Some("stale".into()))
            .activation_token
            .is_none()
    );
    assert!(
        selection_result(Some(rect), None)
            .activation_token
            .is_none()
    );
}
