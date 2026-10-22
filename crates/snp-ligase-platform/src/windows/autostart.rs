//! "Start with Windows" (SPEC section 10): one string value under the current
//! user's `Run` key, holding the quoted path of the exe to start at login.
//!
//! The value is written or removed only when the user changes the setting
//! (the app calls [`enable`] / [`disable`] from Settings > OK), never on
//! startup. A dev build and a release build share one config folder, and a
//! startup "sync" would let whichever one ran last take over the login entry.

use std::path::Path;

use ::windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use ::windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use ::windows::core::PCWSTR;

use crate::{Error, Result};

/// The per-user key Windows reads at login.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The name of this app's value under [`RUN_KEY`].
pub const VALUE_NAME: &str = "snp-ligase";

/// The command line stored for `exe`: the path in quotes, so a folder with a
/// space in it (`C:\Program Files\…`) still starts the right program.
#[must_use]
pub fn command_line(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// Starts `exe` at login.
///
/// # Errors
///
/// The registry refused the write.
pub fn enable(exe: &Path) -> Result<()> {
    let command = command_line(exe);
    set_value(RUN_KEY, VALUE_NAME, &command)?;
    tracing::info!(%command, "start with Windows: on");
    Ok(())
}

/// Stops starting at login. A value that is already gone is not an error.
///
/// # Errors
///
/// The registry refused the delete.
pub fn disable() -> Result<()> {
    delete_value(RUN_KEY, VALUE_NAME)?;
    tracing::info!("start with Windows: off");
    Ok(())
}

/// What the login entry holds now, if anything (for logs and diagnostics).
///
/// # Errors
///
/// The registry refused the read.
pub fn current() -> Result<Option<String>> {
    get_value(RUN_KEY, VALUE_NAME)
}

/// A NUL-terminated wide string for the registry calls.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn check(status: WIN32_ERROR, what: &str) -> Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(Error::Os {
            message: format!(
                "could not {what} the login entry: {}",
                std::io::Error::from_raw_os_error(status.0 as i32)
            ),
        })
    }
}

fn set_value(subkey: &str, name: &str, data: &str) -> Result<()> {
    let (subkey, name, data) = (wide(subkey), wide(name), wide(data));
    let bytes = u32::try_from(data.len() * 2).map_err(|_| Error::Os {
        message: "the login entry's command line is too long".to_owned(),
    })?;
    // Safety: the three strings are NUL-terminated and outlive the call;
    // `bytes` is the byte length of `data` including its terminator, as REG_SZ
    // requires. RegSetKeyValueW creates the subkey if it does not exist.
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            REG_SZ.0,
            Some(data.as_ptr().cast()),
            bytes,
        )
    };
    check(status, "write")
}

fn delete_value(subkey: &str, name: &str) -> Result<()> {
    let (subkey, name) = (wide(subkey), wide(name));
    // Safety: both strings are NUL-terminated and outlive the call.
    let status = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(status, "remove")
}

fn get_value(subkey: &str, name: &str) -> Result<Option<String>> {
    let (subkey, name) = (wide(subkey), wide(name));
    let mut size = 0u32;
    // Safety: the strings are NUL-terminated; a null data pointer asks only
    // for the size, written into `size`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&raw mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status, "read")?;

    let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
    // Safety: `buffer` holds `size` bytes, and `size` is passed in and out.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status, "read")?;
    let units = (size as usize / 2).min(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..units]);
    Ok(Some(text.trim_end_matches('\0').to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::windows::Win32::System::Registry::RegDeleteKeyW;

    /// A throwaway key, so the tests never touch the real `Run` key.
    fn test_key() -> String {
        format!(r"Software\snp-ligase-test-autostart-{}", std::process::id())
    }

    fn remove_test_key(subkey: &str) {
        let subkey = wide(subkey);
        // Safety: a NUL-terminated string that outlives the call.
        let _ = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(subkey.as_ptr())) };
    }

    #[test]
    fn the_command_line_quotes_the_path() {
        let exe = Path::new(r"C:\Program Files\snpl\snpl.exe");
        assert_eq!(command_line(exe), r#""C:\Program Files\snpl\snpl.exe""#);
    }

    #[test]
    fn a_value_is_written_read_back_and_removed() {
        let key = test_key();
        let command = command_line(Path::new(r"C:\Tools\snp ligase\snp-ligase.exe"));

        assert_eq!(
            get_value(&key, VALUE_NAME).expect("read"),
            None,
            "the test key was not empty"
        );
        set_value(&key, VALUE_NAME, &command).expect("write");
        assert_eq!(
            get_value(&key, VALUE_NAME).expect("read"),
            Some(command.clone())
        );

        delete_value(&key, VALUE_NAME).expect("remove");
        assert_eq!(
            get_value(&key, VALUE_NAME).expect("read"),
            None,
            "the value survived"
        );
        // Removing a value that is already gone is fine (unticking twice).
        delete_value(&key, VALUE_NAME).expect("a second remove");

        remove_test_key(&key);
    }
}
