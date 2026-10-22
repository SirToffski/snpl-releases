//! The instance channel (LINUX_SUPPORT.md section 3, item 9).
//!
//! One snp-ligase per sign-in session. A second launch hands what it was asked
//! to do — bring the window forward, run one capture, open image files — to the
//! instance that is already running; only a launch that finds nobody listening
//! starts a window of its own. That is also the universal hotkey fallback on
//! Linux: the user binds `snp-ligase --capture region` in their desktop's own
//! keyboard settings.
//!
//! The **messages** are the same everywhere. The transport on Unix is a
//! line-based protocol over a socket in `$XDG_RUNTIME_DIR` (std only, no socket
//! crate); Windows keeps its mutex and show event, which do the `show` half of
//! this, so the message type is here for the day Windows forwards the rest.
//!
//! The protocol is one message per connection, one line of UTF-8:
//!
//! ```text
//! show
//! capture region | monitor | all | window | repeat
//! open <path>
//! ```
//!
//! A launch with several files forwards several `open` messages, so a line
//! never has to carry a list — a file name with spaces in it stays whole
//! because the path runs to the end of the line.

use std::path::PathBuf;

// The socket transport is Unix-only; the message types above compile
// everywhere (Windows keeps its mutex and show event for now).
#[cfg(unix)]
use crate::{Error, Result};
#[cfg(unix)]
use std::io::BufRead as _;

/// The socket's file name inside the runtime directory.
pub const SOCKET_NAME: &str = "snp-ligase.sock";

/// One capture mode the channel can name: the words `--capture` takes
/// (LINUX_SUPPORT.md section 3, item 9).
///
/// The five modes mirror the app's `CaptureMode`, in the app's menu order. The
/// mapping lives in the app; this crate never learns what a `CaptureMode` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureRequest {
    Region,
    Monitor,
    All,
    Window,
    Repeat,
}

impl CaptureRequest {
    /// Every mode, in the order `--help` lists them.
    pub const ALL: [Self; 5] = [
        Self::Region,
        Self::Monitor,
        Self::All,
        Self::Window,
        Self::Repeat,
    ];

    /// The name `--capture` and the wire protocol use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Region => "region",
            Self::Monitor => "monitor",
            Self::All => "all",
            Self::Window => "window",
            Self::Repeat => "repeat",
        }
    }

    /// The mode `text` names. `None` for anything else: the caller writes the
    /// error message, because `--help`'s wording is the app's.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == text)
    }
}

/// One request a launch hands to the instance that is already running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Bring the main window forward (G8's job on Windows).
    Show,
    /// Run one capture, as if the hotkey had been pressed.
    Capture(CaptureRequest),
    /// Activation token accompanying the next capture request on this connection.
    Activated {
        token: String,
        request: Box<Message>,
    },
    /// Open one image file in a tab.
    Open(PathBuf),
}

impl Message {
    /// The one-line form, without its newline.
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Self::Activated { token, request } => format!(
                "activated {} {}",
                token
                    .chars()
                    .filter(|c| !c.is_whitespace() && !c.is_control())
                    .collect::<String>(),
                request.encode()
            ),
            Self::Show => "show".to_owned(),
            Self::Capture(mode) => format!("capture {}", mode.as_str()),
            Self::Open(path) => format!("open {}", path.display()),
        }
    }

    /// The message one line holds. `None` when the line is not one of ours, so
    /// a bad connection is ignored instead of guessed at.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        if let Some(rest) = line.strip_prefix("activated ") {
            let (token, request) = rest.split_once(' ')?;
            if token.is_empty()
                || token.chars().any(|c| c.is_whitespace() || c.is_control())
                || request.starts_with("activated ")
            {
                return None;
            }
            return Some(Self::Activated {
                token: token.into(),
                request: Box::new(Self::parse(request)?),
            });
        }
        if line == "show" {
            return Some(Self::Show);
        }
        if let Some(mode) = line.strip_prefix("capture ") {
            return CaptureRequest::parse(mode).map(Self::Capture);
        }
        if let Some(path) = line.strip_prefix("open ") {
            return (!path.is_empty()).then(|| Self::Open(PathBuf::from(path)));
        }
        None
    }
}

/// The channel's socket path: `$XDG_RUNTIME_DIR/snp-ligase.sock`, or a
/// per-user directory under the temp directory when the session has no runtime
/// directory (a plain SSH shell, a service started by hand).
///
/// `None` means the fallback directory is not safe to use (LINUX_SUPPORT.md
/// section 4, L1a carry-over 1) and the process runs without a channel.
#[cfg(unix)]
#[must_use]
pub fn socket_path() -> Option<PathBuf> {
    socket_path_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// [`socket_path`] with the session's runtime directory passed in, so which
/// directory is used is a test of its own and no test has to touch the
/// environment.
#[cfg(unix)]
#[must_use]
fn socket_path_from(runtime: Option<std::ffi::OsString>) -> Option<PathBuf> {
    socket_dir_from(runtime).map(|directory| directory.join(SOCKET_NAME))
}

/// The directory the channel's socket lives in.
///
/// The session's runtime directory is used as it comes: it is the session
/// manager's private directory for this user. The temp-directory fallback is
/// the L1a carry-over from the L0d review, and it is where another user could
/// plant a socket, so it is checked before use ([`private_temp_dir`]).
#[cfg(unix)]
#[must_use]
fn socket_dir_from(runtime: Option<std::ffi::OsString>) -> Option<PathBuf> {
    // A variable that does not point at a directory is not one to bind into:
    // the socket would fail to be created, or land in a place the session
    // cannot reach.
    runtime
        .map(PathBuf::from)
        .filter(|directory| directory.is_dir())
        .or_else(|| private_temp_dir(&std::env::temp_dir()))
}

/// The prefix of the fallback directory's name: `snp-ligase-<uid>`.
#[cfg(unix)]
const FALLBACK_DIR_PREFIX: &str = "snp-ligase-";

/// `temp_dir()/snp-ligase-<uid>/`, created 0700, for this user.
///
/// Shared temp directories are a trap on a multi-user machine: whoever creates
/// `temp_dir()/snp-ligase.sock` first receives every later launch's
/// `capture`/`open <path>` messages and makes those launches believe an
/// instance is running (L0d review). A per-user directory that is a real
/// directory, owned by this user and closed to everyone else is the fix.
#[cfg(unix)]
#[must_use]
pub(crate) fn private_temp_dir(base: &std::path::Path) -> Option<PathBuf> {
    private_temp_dir_owned_by(base, current_uid())
}

/// [`private_temp_dir`] with the owner it must have passed in, so a test can
/// check the ownership rule without being root.
#[cfg(unix)]
#[must_use]
fn private_temp_dir_owned_by(base: &std::path::Path, uid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;

    let directory = base.join(format!("{FALLBACK_DIR_PREFIX}{uid}"));

    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // `create_dir`, not `create_dir_all`: only the last component is
            // ours to create, and a symlink planted at this name fails the
            // create instead of being followed.
            match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {}
                // A launch racing this one is fine; the checks below decide.
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    tracing::warn!(
                        path = %directory.display(),
                        error = %err,
                        "could not create the instance channel's fallback directory; \
                         running without a channel"
                    );
                    return None;
                }
            }
            match std::fs::symlink_metadata(&directory) {
                Ok(metadata) => metadata,
                Err(err) => {
                    tracing::warn!(
                        path = %directory.display(),
                        error = %err,
                        "the instance channel's fallback directory cannot be looked at; \
                         running without a channel"
                    );
                    return None;
                }
            }
        }
        Err(err) => {
            tracing::warn!(
                path = %directory.display(),
                error = %err,
                "the instance channel's fallback directory cannot be looked at; \
                 running without a channel"
            );
            return None;
        }
    };

    if let Some(problem) = private_dir_problem(&metadata, uid) {
        tracing::warn!(
            path = %directory.display(),
            reason = %problem,
            "refusing to use the instance channel's fallback directory; running without a channel"
        );
        return None;
    }
    Some(directory)
}

/// Why `metadata` is not usable as the channel's private directory: a real
/// directory (not a symlink), owned by `uid`, mode exactly 0700. `None` when it
/// is.
#[cfg(unix)]
fn private_dir_problem(metadata: &std::fs::Metadata, uid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    if metadata.file_type().is_symlink() {
        return Some("it is a symlink".to_owned());
    }
    if !metadata.is_dir() {
        return Some("it is not a directory".to_owned());
    }
    if metadata.uid() != uid {
        return Some(format!("it is owned by uid {}, not {uid}", metadata.uid()));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Some(format!("its mode is {mode:03o}, not 700"));
    }
    None
}

/// This process's effective user id: the fallback directory is private to it.
#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// The listening end of the channel: the first process to bind it serves every
/// later launch.
///
/// Dropping it removes the socket file, and so does [`Server::socket_file`]'s
/// handle, which is what a clean exit uses: the channel thread owns the server
/// and a quit ends the process before that thread's drop can run. A process
/// that dies without either leaves the file behind, and the next launch
/// replaces it (a socket nobody listens on refuses the connection, which is how
/// a stale one is told apart from a live instance).
#[cfg(unix)]
#[derive(Debug)]
pub struct Server {
    listener: std::os::unix::net::UnixListener,
    socket: SocketFile,
}

/// The socket file a [`Server`] bound, for the instance to remove on the way
/// out.
///
/// [`Server`]'s own drop is not enough for a clean exit: the channel thread
/// owns the server, and a quit ends the process, so that thread never runs its
/// drop. The app keeps this handle and removes the socket once the window is
/// gone (LINUX_SUPPORT.md section 3, item 9).
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct SocketFile {
    path: PathBuf,
    /// `(device, inode)` of the socket this handle stands for, `None` when the
    /// file could not be looked at. A socket a later instance bound in the
    /// meantime has a different inode and is left alone.
    identity: Option<(u64, u64)>,
}

#[cfg(unix)]
impl SocketFile {
    /// Removes the socket, unless it is no longer the one this handle bound.
    pub fn remove(&self) {
        use std::os::unix::fs::MetadataExt as _;
        match (std::fs::metadata(&self.path), self.identity) {
            (Ok(metadata), Some((device, inode)))
                if metadata.dev() == device && metadata.ino() == inode => {}
            (Ok(_), Some(_)) => {
                tracing::debug!(
                    path = %self.path.display(),
                    "the instance channel's socket is not this instance's to remove"
                );
                return;
            }
            // Gone already, or nothing was recorded to match it on: there is
            // nothing to remove either way.
            _ => return,
        }
        if let Err(err) = std::fs::remove_file(&self.path) {
            tracing::debug!(
                path = %self.path.display(),
                error = %err,
                "could not remove the instance channel's socket"
            );
        }
    }
}

#[cfg(unix)]
impl Server {
    /// Binds the channel at [`socket_path`].
    ///
    /// # Errors
    ///
    /// There is no private directory to put the socket in (the L1a carry-over
    /// from the L0d review), the socket could not be created, or another
    /// instance already owns it. The caller tries [`send`] first, so "already
    /// owned" means an instance that was not there a moment ago.
    pub fn bind() -> Result<Self> {
        let Some(path) = socket_path() else {
            return Err(Error::Instance {
                message: "there is no private directory for the instance channel".to_owned(),
            });
        };
        Self::bind_at(&path)
    }

    /// [`Server::bind`] at a path of the caller's own; the tests use their own,
    /// so a running snp-ligase cannot make them lie.
    fn bind_at(path: &std::path::Path) -> Result<Self> {
        if path.exists() {
            // A crashed run cannot remove its own socket; connecting to what it
            // left fails (nothing is listening), while a *running* instance
            // answers. Only the failed connection is a reason to replace it.
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(Error::Instance {
                    message: format!("another instance is listening on {}", path.display()),
                });
            }
            tracing::info!(
                path = %path.display(),
                "replacing the socket a crashed run left behind"
            );
            std::fs::remove_file(path).map_err(|err| Error::Instance {
                message: format!(
                    "could not remove the stale socket {}: {err}",
                    path.display()
                ),
            })?;
        }
        let listener =
            std::os::unix::net::UnixListener::bind(path).map_err(|err| Error::Instance {
                message: format!("could not bind {}: {err}", path.display()),
            })?;
        // Private to this account even when the socket ended up in a shared
        // temp directory: only this user may ask the instance to do things.
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "could not tighten the instance channel's permissions"
            );
        }
        let identity = match std::fs::metadata(path) {
            Ok(metadata) => {
                use std::os::unix::fs::MetadataExt as _;
                Some((metadata.dev(), metadata.ino()))
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "could not look at the instance channel's socket"
                );
                None
            }
        };
        Ok(Self {
            listener,
            socket: SocketFile {
                path: path.to_path_buf(),
                identity,
            },
        })
    }

    /// The socket file this server bound, so the instance can remove it on the
    /// way out even though the channel thread owns the server.
    #[must_use]
    pub fn socket_file(&self) -> SocketFile {
        self.socket.clone()
    }

    /// Waits for the next message.
    ///
    /// Blocking, on purpose: the app calls it on its own thread. A connection
    /// that sends nothing — the liveness check [`Server::bind`] makes, or a
    /// launch that died mid-way — and a line that is not a message are skipped
    /// with a log line, so a bad client cannot take the channel down.
    ///
    /// # Errors
    ///
    /// `accept` failed, which the caller retries (after a pause, so a full
    /// descriptor table cannot turn into a busy loop).
    pub fn recv(&self) -> Result<Message> {
        loop {
            let (stream, _address) = self.listener.accept().map_err(|err| Error::Instance {
                message: format!("accept on {}: {err}", self.socket.path.display()),
            })?;
            let mut line = String::new();
            match std::io::BufReader::new(stream).read_line(&mut line) {
                // Nothing was sent: a probe, or a client that left.
                Ok(0) => continue,
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "the instance channel read failed");
                    continue;
                }
            }
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                continue;
            }
            match Message::parse(line) {
                Some(message) => return Ok(message),
                None => {
                    tracing::warn!(line, "ignoring an unknown instance-channel message");
                    continue;
                }
            }
        }
    }
}

#[cfg(unix)]
impl Drop for Server {
    fn drop(&mut self) {
        self.socket.remove();
    }
}

/// Hands every message to the running instance.
///
/// `Ok(false)` means nobody is running one — no socket, nothing listening on
/// it, or no private directory to put one in — and the caller starts the app
/// instead. Each message opens its own connection, so the receiving side never
/// has to reassemble anything.
///
/// # Errors
///
/// A connection that was made and then failed to write, or a socket that
/// existed but could not be connected to for a reason other than "nobody is
/// listening".
#[cfg(unix)]
pub fn send(messages: &[Message]) -> Result<bool> {
    match socket_path() {
        Some(path) => send_to(&path, messages),
        // No channel: handing the messages over is not possible, so this
        // launch starts the app (see `socket_path`).
        None => Ok(false),
    }
}

/// [`send`] to a path of the caller's own; the tests use their own.
#[cfg(unix)]
fn send_to(path: &std::path::Path, messages: &[Message]) -> Result<bool> {
    let mut delivered = false;
    for message in messages {
        delivered |= deliver(path, &message.encode())?;
    }
    Ok(delivered)
}

/// One message, one connection. `Ok(false)` is "nobody is listening".
#[cfg(unix)]
fn deliver(path: &std::path::Path, line: &str) -> Result<bool> {
    use std::io::Write as _;

    if line.contains('\n') {
        // A file name with a newline in it cannot travel on a line protocol.
        tracing::warn!(
            line,
            "an instance-channel message with a newline was dropped"
        );
        return Ok(false);
    }
    let mut stream = match std::os::unix::net::UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return Ok(false);
        }
        Err(err) => {
            return Err(Error::Instance {
                message: format!("could not connect to {}: {err}", path.display()),
            });
        }
    };
    stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.write_all(b"\n"))
        .and_then(|()| stream.flush())
        .map_err(|err| Error::Instance {
            message: format!("could not write to {}: {err}", path.display()),
        })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_token_round_trips_without_line_injection() {
        let message = Message::Activated {
            token: "token-/abc_123".into(),
            request: Box::new(Message::Show),
        };
        assert_eq!(Message::parse(&message.encode()), Some(message));
        assert_eq!(Message::parse("activation "), None);
        assert_eq!(Message::parse("activation token\nshow"), None);
        assert!(
            !Message::Activated {
                token: "bad\nshow".into(),
                request: Box::new(Message::Show)
            }
            .encode()
            .contains('\n')
        );
    }

    #[test]
    fn every_message_round_trips() {
        for mode in CaptureRequest::ALL {
            assert_eq!(
                CaptureRequest::parse(mode.as_str()),
                Some(mode),
                "{}",
                mode.as_str()
            );
        }

        let cases = [
            (Message::Show, "show"),
            (Message::Capture(CaptureRequest::Region), "capture region"),
            (Message::Capture(CaptureRequest::Monitor), "capture monitor"),
            (Message::Capture(CaptureRequest::All), "capture all"),
            (Message::Capture(CaptureRequest::Window), "capture window"),
            (Message::Capture(CaptureRequest::Repeat), "capture repeat"),
            (
                Message::Open(PathBuf::from("/tmp/a shot with spaces.png")),
                "open /tmp/a shot with spaces.png",
            ),
        ];
        for (message, line) in cases {
            assert_eq!(message.encode(), line);
            assert_eq!(Message::parse(line), Some(message), "{line}");
        }
    }

    #[test]
    fn unknown_lines_are_refused() {
        for line in [
            "",
            " ",
            "sho",
            " show",
            "show ",
            "show me",
            "capture",
            "capture ",
            "capture nonsense",
            "capture Region",
            "open",
            "open ",
            "opened /tmp/a.png",
            "quit",
        ] {
            assert_eq!(Message::parse(line), None, "{line:?} was accepted");
        }
    }

    /// The channel is what makes the CLI's `--capture` work on a running
    /// instance, and `Ok(false)` is how a launch knows to start one.
    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::time::Duration;

        /// A socket no other test uses: its own directory, this process's id
        /// and the test's name in it.
        fn socket_for(what: &str) -> PathBuf {
            let directory = std::env::temp_dir()
                .join(format!("snp-ligase-instance-{}-{what}", std::process::id()));
            std::fs::create_dir_all(&directory).expect("the test directory should be creatable");
            directory.join(SOCKET_NAME)
        }

        /// A base directory for the temp-directory fallback, one per test.
        fn fallback_base(what: &str) -> PathBuf {
            let base = std::env::temp_dir()
                .join(format!("snp-ligase-fallback-{}-{what}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).expect("the test base should be creatable");
            base
        }

        /// The directory name a fallback for `uid` has.
        fn fallback_name(uid: u32) -> String {
            format!("{FALLBACK_DIR_PREFIX}{uid}")
        }

        fn mode_of(path: &std::path::Path) -> u32 {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::symlink_metadata(path)
                .expect("the directory should be there")
                .permissions()
                .mode()
                & 0o777
        }

        fn set_mode(path: &std::path::Path, mode: u32) {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .expect("the mode should be settable");
        }

        #[test]
        fn the_socket_lives_in_the_runtime_directory_or_a_private_temp_one() {
            let runtime =
                std::env::temp_dir().join(format!("snp-ligase-runtime-{}", std::process::id()));
            std::fs::create_dir_all(&runtime).expect("the test directory should be creatable");
            assert_eq!(
                socket_path_from(Some(runtime.clone().into_os_string())),
                Some(runtime.join(SOCKET_NAME)),
                "the session's runtime directory was not used"
            );

            // A session without one (or with a variable that is not a
            // directory) falls back to a per-user directory in the temp
            // directory, never to the temp directory itself (L1a carry-over 1
            // from the L0d review: another user could plant the socket there).
            let fallback =
                socket_path_from(None).expect("the temp-directory fallback should be usable");
            assert_eq!(
                fallback.file_name(),
                Some(std::ffi::OsStr::new(SOCKET_NAME))
            );
            assert_eq!(
                fallback.parent().map(std::path::Path::to_path_buf),
                Some(std::env::temp_dir().join(fallback_name(current_uid()))),
                "the fallback socket is not in a per-user directory"
            );
            assert_eq!(
                socket_path_from(Some(
                    std::env::temp_dir()
                        .join("snp-ligase-not-a-directory")
                        .into_os_string()
                )),
                Some(fallback),
                "a variable pointing nowhere was not replaced by the fallback"
            );
        }

        #[test]
        fn the_fallback_directory_is_created_private_to_this_user() {
            use std::os::unix::fs::MetadataExt as _;

            let base = fallback_base("create");
            let directory =
                private_temp_dir(&base).expect("a fresh fallback directory should be usable");

            assert_eq!(
                directory
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
                Some(fallback_name(current_uid())),
                "the fallback directory is not named after this user"
            );
            let metadata = std::fs::symlink_metadata(&directory)
                .expect("the fallback directory should exist after it was created");
            assert!(metadata.is_dir(), "the fallback is not a directory");
            assert_eq!(metadata.uid(), current_uid(), "the fallback is not ours");
            assert_eq!(
                mode_of(&directory),
                0o700,
                "the fallback directory is not private"
            );

            // The next launch reuses it instead of creating another.
            assert_eq!(private_temp_dir(&base), Some(directory));
        }

        #[test]
        fn a_fallback_directory_others_can_reach_is_refused() {
            let base = fallback_base("mode");
            let directory = base.join(fallback_name(current_uid()));
            std::fs::create_dir(&directory).expect("the test directory should be creatable");

            for mode in [0o755, 0o770, 0o750] {
                set_mode(&directory, mode);
                assert_eq!(
                    private_temp_dir(&base),
                    None,
                    "a fallback directory with mode {mode:03o} was used anyway"
                );
            }

            set_mode(&directory, 0o700);
            assert_eq!(
                private_temp_dir(&base),
                Some(directory),
                "a private fallback directory was refused"
            );
        }

        #[test]
        fn a_fallback_directory_owned_by_someone_else_is_refused() {
            // The owner cannot be changed without root, so the rule is checked
            // against the real directory with the wrong owner id passed in.
            let base = fallback_base("owner");
            let directory =
                private_temp_dir(&base).expect("a fresh fallback directory should be usable");

            assert!(
                private_temp_dir_owned_by(&base, current_uid() + 1).is_none(),
                "a fallback directory owned by another user was used"
            );
            assert_eq!(
                private_temp_dir_owned_by(&base, current_uid()),
                Some(directory.clone()),
                "our own fallback directory was refused"
            );

            let metadata =
                std::fs::symlink_metadata(&directory).expect("the directory should exist");
            let problem = private_dir_problem(&metadata, current_uid() + 1)
                .expect("another user's directory has to be refused");
            assert!(problem.contains("owned by uid"), "{problem}");
        }

        #[test]
        fn a_symlinked_fallback_directory_is_refused() {
            let base = fallback_base("symlink");
            let real = base.join("somewhere-else");
            std::fs::create_dir(&real).expect("the target directory should be creatable");
            set_mode(&real, 0o700);

            let link = base.join(fallback_name(current_uid()));
            std::os::unix::fs::symlink(&real, &link).expect("the symlink should be creatable");

            assert_eq!(
                private_temp_dir(&base),
                None,
                "a symlinked fallback directory was followed"
            );
        }

        #[test]
        fn a_message_reaches_the_instance_that_is_listening() {
            let path = socket_for("delivered");
            let _ = std::fs::remove_file(&path);
            let server = Server::bind_at(&path).expect("the test socket should bind");

            let (sender, receiver) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                let message = server.recv().expect("a message should arrive");
                sender
                    .send(message)
                    .expect("the test channel should be open");
                server
            });

            let delivered = send_to(&path, &[Message::Capture(CaptureRequest::Region)])
                .expect("sending should work");
            assert!(delivered, "the message was not delivered");
            let received = receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("the message should arrive");
            assert_eq!(received, Message::Capture(CaptureRequest::Region));
            let _server = thread.join().expect("the reader thread should finish");
        }

        #[test]
        fn every_message_of_a_batch_arrives() {
            let path = socket_for("batch");
            let _ = std::fs::remove_file(&path);
            let server = Server::bind_at(&path).expect("the test socket should bind");
            let files = [
                Message::Open(PathBuf::from("/tmp/a.png")),
                Message::Open(PathBuf::from("/tmp/b b.png")),
                Message::Show,
            ];
            let (sender, receiver) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                for _ in 0..3 {
                    sender
                        .send(server.recv().expect("a message should arrive"))
                        .expect("the test channel should be open");
                }
                server
            });

            assert!(send_to(&path, &files).expect("sending should work"));
            let received: Vec<Message> = (0..3)
                .map(|_| {
                    receiver
                        .recv_timeout(Duration::from_secs(5))
                        .expect("the message should arrive")
                })
                .collect();
            assert_eq!(received, files);
            let _server = thread.join().expect("the reader thread should finish");
        }

        #[test]
        fn nobody_listening_is_not_an_error() {
            let path = socket_for("nobody");
            let _ = std::fs::remove_file(&path);
            assert!(
                !send_to(&path, &[Message::Show]).expect("an absent socket is not a failure"),
                "a message was delivered to a channel that is not there"
            );
            assert!(
                !send_to(&path, &[]).expect("an empty batch is not a failure"),
                "an empty batch reported a delivery"
            );
        }

        #[test]
        fn a_socket_a_crashed_run_left_behind_is_replaced() {
            let path = socket_for("stale");
            let _ = std::fs::remove_file(&path);
            {
                // A crashed process leaves exactly this: the socket file, with
                // nobody behind it.
                let crashed = std::os::unix::net::UnixListener::bind(&path)
                    .expect("the stale socket should bind");
                drop(crashed);
            }
            assert!(path.exists(), "the test did not leave a socket behind");
            assert!(
                !send_to(&path, &[Message::Show])
                    .expect("connecting to a dead socket is not fatal"),
                "the dead socket took a message"
            );

            let server = Server::bind_at(&path).expect("the stale socket should be replaced");
            assert!(path.exists(), "the channel is not there after the bind");
            drop(server);
            assert!(!path.exists(), "the socket was not removed on drop");
        }

        #[test]
        fn a_socket_a_later_instance_bound_survives_an_old_handle() {
            let path = socket_for("handle");
            let _ = std::fs::remove_file(&path);
            let first = Server::bind_at(&path).expect("the test socket should bind");
            let old_handle = first.socket_file();
            // A clean exit ends the process before the channel thread can drop
            // its server, so the handle the app kept is what removes the socket.
            drop(first);
            assert!(!path.exists(), "the socket was not removed on drop");

            // A later launch binds the channel again, at the same path.
            let second = Server::bind_at(&path).expect("the channel should be free again");
            let live_handle = second.socket_file();

            // The first instance's handle must not take that channel away.
            old_handle.remove();
            assert!(
                path.exists(),
                "an old handle removed a socket it no longer owned"
            );

            // The live instance's own handle is what its clean exit uses.
            live_handle.remove();
            assert!(
                !path.exists(),
                "the socket was not removed by its own handle"
            );
            drop(second);
        }

        #[test]
        fn two_instances_cannot_both_own_the_channel() {
            let path = socket_for("two");
            let _ = std::fs::remove_file(&path);
            let first = Server::bind_at(&path).expect("the first bind should work");

            let error = Server::bind_at(&path).expect_err("a second bind must be refused");
            assert!(error.to_string().contains("another instance"), "{error}");

            // The first instance leaving frees the channel for the next launch.
            drop(first);
            let _second = Server::bind_at(&path).expect("the channel should be free again");
        }

        #[test]
        fn the_socket_is_private_to_this_user() {
            use std::os::unix::fs::PermissionsExt as _;

            let path = socket_for("mode");
            let _ = std::fs::remove_file(&path);
            let server = Server::bind_at(&path).expect("the test socket should bind");
            let mode = std::fs::metadata(&path)
                .expect("the socket should be there")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the socket is not private");
            drop(server);
        }
    }
}

#[cfg(test)]
#[test]
fn w2_activation_and_request_are_one_message() {
    for request in [
        Message::Show,
        Message::Open(PathBuf::from("/tmp/image with spaces.png")),
        Message::Capture(CaptureRequest::Region),
    ] {
        let message = Message::Activated {
            token: "fresh-token".into(),
            request: Box::new(request),
        };
        assert_eq!(Message::parse(&message.encode()), Some(message));
    }
    assert_eq!(Message::parse("activated token activated token show"), None);
    assert_eq!(Message::parse("activated  show"), None);
    assert_eq!(Message::parse("activated token unknown"), None);
}
