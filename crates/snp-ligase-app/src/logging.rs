//! Logging: one rotating log file per day, plus stderr while debugging.
//!
//! Everything needed to diagnose a bad capture is logged with `tracing`, and
//! the log file is the artifact the human will paste back (SPEC section 0,
//! rule 11). `RUST_LOG` overrides the level if the human needs more detail.
//!
//! The `env-filter` and `tracing-log` features of `tracing-subscriber` are
//! enabled so that `log` records from eframe, winit and the Windows crates
//! also land in our file instead of being dropped.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Daily log files kept on disk (SPEC section 10: keep 7).
const MAX_LOG_FILES: usize = 7;

/// Installs the global tracing subscriber.
///
/// Returns the worker guard of the background log writer. The guard must stay
/// alive for as long as the program runs: dropping it discards log lines that
/// are still buffered. `None` means there is no log file (no writable data
/// directory) and the subscriber is writing to stderr instead.
pub fn init(log_dir: &Path, app_name: &str) -> Option<WorkerGuard> {
    // Debug builds log everything; release builds stay at info. RUST_LOG wins.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive()));

    // The rolling appender creates the directory itself; doing it here means a
    // permissions problem is reported once, at startup, instead of silently
    // turning into a missing file.
    let _ = std::fs::create_dir_all(log_dir);

    let appender = match tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(app_name)
        // `.log` so the files are recognisable and double-clickable (R30).
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(log_dir)
    {
        Ok(appender) => appender,
        Err(err) => {
            eprintln!(
                "{app_name}: cannot write logs to {}: {err}",
                log_dir.display()
            );
            log_to_stderr(filter);
            return None;
        }
    };

    #[cfg(debug_assertions)]
    eprintln!("{app_name}: logging to {}", log_dir.display());

    // Writes happen on a background thread so a slow disk never stalls a frame.
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    if let Err(err) = tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false) // no colour escapes in a plain text file
        .with_env_filter(filter)
        .try_init()
    {
        eprintln!("{app_name}: tracing is already initialised: {err}");
    }

    Some(guard)
}

/// Filter directive used when `RUST_LOG` is not set.
fn default_directive() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "info"
    }
}

/// Last-resort subscriber when no log file can be opened.
fn log_to_stderr(filter: EnvFilter) {
    if let Err(err) = tracing_subscriber::fmt().with_env_filter(filter).try_init() {
        eprintln!("{err}");
    }
}
