//! One running instance (SPEC section 6.1).
//!
//! A named mutex: the first process creates it, and every later one sees
//! `ERROR_ALREADY_EXISTS` and exits immediately. The name lives in the `Local\`
//! namespace, so it is one instance per sign-in session, not per machine —
//! which is what a desktop screenshot tool wants (a second user on the same PC
//! gets their own instance).
//!
//! Only the windowed run takes the mutex. `--debug-capture` and
//! `--debug-overlay` are diagnostics and have to work while the app is open.

use std::time::Duration;

use ::windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0,
};
use ::windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent,
    WaitForSingleObject,
};

use crate::{Error, Result};

/// The one instance's name. `Local\` is the per-session namespace.
pub const MUTEX_NAME: &str = "Local\\snp-ligase-single-instance";

/// The event a second launch signals to bring the running window forward (G8).
pub const SHOW_EVENT_NAME: &str = "Local\\snp-ligase-show-window";

/// A NUL-terminated wide version of `name`, for the Win32 calls that take one.
fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The held mutex. Dropping it releases the name.
#[derive(Debug)]
pub struct InstanceGuard {
    handle: HANDLE,
}

impl InstanceGuard {
    /// Takes the mutex called `name`.
    ///
    /// `Ok(None)` means another instance already holds it: SPEC 6.1 says the
    /// second launch exits immediately, and the show-window event (G8) is how
    /// it asks the running instance to come forward.
    ///
    /// # Errors
    ///
    /// A Win32 failure that is not "already exists".
    pub fn acquire(name: &str) -> Result<Option<Self>> {
        let wide = wide(name);
        // Safety: creating a named mutex with a null security descriptor; the
        // wide string lives for the whole call.
        let handle = unsafe { CreateMutexW(None, false, ::windows::core::PCWSTR(wide.as_ptr())) }
            .map_err(Error::from)?;
        // Safety: reads this thread's last-error code, which `CreateMutexW` set
        // just above when the name was already taken.
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if already_exists {
            // Safety: the handle this call just opened is not needed.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Ok(None);
        }
        Ok(Some(Self { handle }))
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // Safety: closing the handle this guard owns, exactly once.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// The named event the second launch signals (G8).
///
/// The first instance creates it and waits on it; a second launch — which sees
/// the mutex taken and would otherwise just exit — opens it, signals it and
/// exits. The first instance then brings its window forward, so relaunching
/// from the taskbar is visible even while the window is hidden in the tray.
#[derive(Debug)]
pub struct ShowEvent {
    handle: HANDLE,
}

// Safety: a kernel event handle is process-wide and any thread may wait on or
// signal it; the type only closes the handle in `Drop`, and moving it to the
// waiting thread is exactly what it is for.
unsafe impl Send for ShowEvent {}

impl ShowEvent {
    /// Creates the event. Only the first instance does this: it owns the name.
    ///
    /// # Errors
    ///
    /// A Win32 failure (the name being taken is not an error here: an
    /// auto-reset event with an existing name would be opened instead, which is
    /// the second launch's job).
    pub fn create() -> Result<Self> {
        Self::create_named(SHOW_EVENT_NAME)
    }

    /// Creates (or opens) an event with the given name; the tests use their
    /// own, so a running snp-ligase cannot make them lie.
    fn create_named(name: &str) -> Result<Self> {
        let wide = wide(name);
        // Safety: creating a named auto-reset event, initially unsignalled,
        // with a null security descriptor; the wide string lives for the call.
        let handle =
            unsafe { CreateEventW(None, false, false, ::windows::core::PCWSTR(wide.as_ptr())) }
                .map_err(Error::from)?;
        Ok(Self { handle })
    }

    /// Opens the event the running instance created.
    ///
    /// # Errors
    ///
    /// A Win32 failure. "File not found" simply means no instance has created
    /// the event: the caller logs a warning and exits as before (NEXT_STEPS
    /// 4b G8).
    pub fn open() -> Result<Self> {
        Self::open_named(SHOW_EVENT_NAME)
    }

    /// Opens an event with the given name; the tests use their own.
    fn open_named(name: &str) -> Result<Self> {
        let wide = wide(name);
        // Safety: opening an existing named event. `EVENT_MODIFY_STATE` is what
        // `SetEvent` needs; the string lives for the call.
        let handle = unsafe {
            OpenEventW(
                EVENT_MODIFY_STATE,
                false,
                ::windows::core::PCWSTR(wide.as_ptr()),
            )
        }
        .map_err(Error::from)?;
        Ok(Self { handle })
    }

    /// Signals the event: the instance waiting on it shows its window.
    ///
    /// # Errors
    ///
    /// A Win32 failure from `SetEvent`.
    pub fn signal(&self) -> Result<()> {
        // Safety: signalling the handle this type owns.
        unsafe { SetEvent(self.handle) }.map_err(Error::from)
    }

    /// Waits until the event is signalled. `false` means the wait failed, which
    /// the waiting thread treats as "stop waiting".
    #[must_use]
    pub fn wait(&self) -> bool {
        // Safety: waiting on the handle this type owns, for as long as it takes.
        unsafe { WaitForSingleObject(self.handle, INFINITE) == WAIT_OBJECT_0 }
    }

    /// Waits up to `timeout` for the event. The tests use a short timeout so a
    /// failure cannot hang them.
    #[must_use]
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let millis = timeout
            .as_millis()
            .min(u128::from(INFINITE - 1))
            .try_into()
            .unwrap_or(INFINITE - 1);
        // Safety: waiting on the handle this type owns. An abandoned or failed
        // wait is reported as `false`.
        unsafe { WaitForSingleObject(self.handle, millis) == WAIT_OBJECT_0 }
    }
}

impl Drop for ShowEvent {
    fn drop(&mut self) {
        // Safety: closing the handle this type owns, exactly once.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_acquire_succeeds_and_a_second_one_says_already_running() {
        // A name of this process's own, so a running snp-ligase (or a parallel
        // test run) cannot make this test lie.
        let name = format!("{MUTEX_NAME}-test-{}", std::process::id());

        let first = InstanceGuard::acquire(&name).expect("the mutex should be creatable");
        let second = InstanceGuard::acquire(&name).expect("the second call should not fail");
        assert!(first.is_some(), "the first acquire did not get the mutex");
        assert!(
            second.is_none(),
            "a second acquire got the mutex while the first was held"
        );

        // Releasing it lets a new instance in.
        drop(first);
        let third = InstanceGuard::acquire(&name).expect("the third call should not fail");
        assert!(third.is_some(), "the mutex was not released by the drop");
    }

    #[test]
    fn the_show_event_carries_one_signal_to_the_waiting_handle() {
        // G8: the first instance creates and waits; what the second launch gets
        // from `open` is what signals it.
        let name = format!("{SHOW_EVENT_NAME}-test-{}", std::process::id());
        let waiting = ShowEvent::create_named(&name).expect("the event should be creatable");
        assert!(
            !waiting.wait_timeout(Duration::from_millis(10)),
            "a fresh event was already signalled"
        );

        let signalling = ShowEvent::open_named(&name).expect("the event should open");
        signalling.signal().expect("the signal should be delivered");
        assert!(
            waiting.wait_timeout(Duration::from_millis(1000)),
            "the signal never arrived"
        );

        // An auto-reset event wakes one wait and resets itself.
        assert!(
            !waiting.wait_timeout(Duration::from_millis(10)),
            "the event did not reset itself after the wait"
        );
    }

    #[test]
    fn opening_a_show_event_that_nobody_created_fails() {
        // The second launch logs a warning and exits when this happens; it must
        // be an error and not, say, a fresh event that nobody is waiting on.
        let name = format!("{SHOW_EVENT_NAME}-missing-{}", std::process::id());
        assert!(ShowEvent::open_named(&name).is_err());
    }
}
