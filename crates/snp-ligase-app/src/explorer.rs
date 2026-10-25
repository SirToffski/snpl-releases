//! Reveal a tab's saved file in Windows Explorer (WIN3).

use std::path::Path;

/// Why Show in Explorer cannot run for this tab, without touching the disk:
/// the File menu asks every frame, and a stat on an offline network share can
/// stall the UI. [`blocked`] checks the file itself when the item is clicked.
pub fn unavailable(path: Option<&Path>) -> Option<&'static str> {
    if !cfg!(windows) {
        Some("Show in Explorer is available on Windows only.")
    } else if path.is_none() {
        Some("This tab has not been saved yet.")
    } else {
        None
    }
}

/// [`unavailable`], plus the file must still be on disk.
pub fn blocked(path: Option<&Path>) -> Option<&'static str> {
    unavailable(path)
        .or_else(|| (!path.is_some_and(Path::is_file)).then_some("This tab has no file on disk."))
}

// Explorer parses /select itself. Command::arg's ordinary Windows escaping
// would escape these embedded quotes; pass this one fully quoted raw argument.
#[cfg(any(windows, test))]
fn select_argument(path: &Path) -> std::ffi::OsString {
    let mut argument = std::ffi::OsString::from("/select,\"");
    argument.push(path);
    argument.push("\"");
    argument
}

#[cfg(windows)]
pub fn show(path: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt as _;
    let path = std::path::absolute(path)?;
    std::process::Command::new("explorer.exe")
        .raw_arg(select_argument(&path))
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
pub fn show(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Show in Explorer is available on Windows only.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win3_explorer_quotes_spaces_commas_and_unicode_as_one_path() {
        let path = Path::new(r"C:\My shots\résumé, final.png");
        assert_eq!(
            select_argument(path),
            r#"/select,"C:\My shots\résumé, final.png""#
        );
    }

    #[test]
    fn win3_explorer_requires_an_existing_file_and_windows() {
        assert!(blocked(None).is_some());
        assert!(unavailable(None).is_some());
        assert_eq!(
            unavailable(Some(Path::new("no-such-win3-file.png"))).is_none(),
            cfg!(windows)
        );
        assert!(blocked(Some(Path::new("no-such-win3-file.png"))).is_some());
        assert!(blocked(Some(Path::new("."))).is_some());
        assert_eq!(
            blocked(Some(Path::new("Cargo.toml"))).is_none(),
            cfg!(windows)
        );
    }
}
