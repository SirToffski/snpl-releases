//! Shared parent-death safety for both overlay backends.
use super::x11::overlay::PARENT_ENV;

/// `prctl(PR_SET_PDEATHSIG, SIGTERM)`, then "is the parent still alive?".
///
/// The kernel delivers `SIGTERM` when the parent's thread exits, which is the
/// strongest promise Linux has: a crash of the app cannot leave a window
/// behind, not even one the app never gets the chance to kill. The check right
/// after arming the signal closes the race where the parent died first — the
/// child would then be adopted by somebody else and never receive the signal.
///
/// The adopted parent is not PID 1 on a systemd desktop: it is the user's
/// systemd (LINUX_SUPPORT.md section 4, L1c carry-over 2), so the parent tells
/// the child its pid in [`PARENT_ENV`] and the child compares. A run started by
/// hand has no such variable; the old "not PID 1" check is kept for it.
pub(crate) fn die_with_parent() -> Result<(), String> {
    // SAFETY: `prctl(PR_SET_PDEATHSIG, SIGTERM)` only stores the signal number
    // for this process and `getppid` is a query; neither touches memory.
    let result = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
    if result != 0 {
        // Not fatal: the parent's own `kill` after the timeout still applies,
        // and the X server destroys the windows when this process dies either
        // way. The log line is what says this safety net is not armed.
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "PR_SET_PDEATHSIG failed; only the parent's kill is left"
        );
    }

    let expected = std::env::var(PARENT_ENV).ok().and_then(|value| {
        let pid = parse_parent_pid(&value);
        if pid.is_none() {
            tracing::warn!(
                value = %value,
                "the overlay's parent pid is not a pid; checking for PID 1 instead"
            );
        }
        pid
    });
    // SAFETY: `getppid` takes no arguments and does not touch memory.
    let actual = unsafe { libc::getppid() };
    if !parent_matches(expected, actual) {
        return Err(match expected {
            Some(expected) => format!(
                "the parent process is already gone (was {expected}, the parent is {actual})"
            ),
            None => format!("the parent process is already gone (the parent is {actual})"),
        });
    }
    Ok(())
}

/// A parent pid from [`PARENT_ENV`]: a positive, representable pid, or `None`.
#[must_use]
fn parse_parent_pid(value: &str) -> Option<i32> {
    value.trim().parse::<i32>().ok().filter(|pid| *pid > 0)
}

/// Whether `actual` is the parent the run was started by.
///
/// Without a pid in the environment (a child started by hand, or by something
/// that does not know [`PARENT_ENV`]) the only check left is the old one: a
/// process adopted by PID 1 has lost its parent.
#[must_use]
fn parent_matches(expected: Option<i32>, actual: i32) -> bool {
    match expected {
        Some(expected) => actual == expected,
        None => actual != 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// LINUX_SUPPORT.md section 4, L1c carry-over 2: the parent passes its
    /// pid and the child compares, because an orphan on a systemd desktop is
    /// adopted by the user's systemd rather than PID 1.
    #[test]
    fn the_parent_check_uses_the_pid_the_parent_passed() {
        assert_eq!(parse_parent_pid("4242"), Some(4242));
        assert_eq!(parse_parent_pid(" 4242\n"), Some(4242), "whitespace");
        assert_eq!(parse_parent_pid("0"), None, "PID 0 is not a parent");
        assert_eq!(parse_parent_pid("-7"), None);
        assert_eq!(parse_parent_pid(""), None);
        assert_eq!(parse_parent_pid("4242x"), None);
        assert_eq!(parse_parent_pid("99999999999"), None, "not an i32");

        // The pid matches: the parent is alive.
        assert!(parent_matches(Some(4242), 4242));
        // A different process (the parent died and systemd adopted us): the
        // run has to stop — the error is the only other way to say it.
        assert!(!parent_matches(Some(4242), 1893));
        assert!(!parent_matches(Some(4242), 1));

        // No pid passed (a child started by hand): the old check, which only
        // catches adoption by PID 1.
        assert!(parent_matches(None, 900));
        assert!(!parent_matches(None, 1));
    }
}
