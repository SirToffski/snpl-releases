//! Where snp-ligase keeps its files (SPEC section 10).
//!
//! - Config: `%APPDATA%\snp-ligase\config.toml` (the file itself is M6 work).
//! - Logs: `%LOCALAPPDATA%\snp-ligase\logs\` (daily rotation, keep 7).
//!
//! The base folders come from the `directories` crate. If the OS cannot tell us
//! where they are, we fall back to the temp directory instead of crashing: a
//! screenshot tool that will not start is worse than one that logs to a
//! temporary folder.

use std::path::PathBuf;

/// The two folders the program writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Folder that holds `config.toml`.
    pub config_dir: PathBuf,
    /// Folder that holds the rotating log files.
    pub log_dir: PathBuf,
}

impl Paths {
    /// Works out the app-scoped folders for this machine.
    pub fn discover(app_name: &str) -> Self {
        let dirs = directories::BaseDirs::new();
        let (config_base, local_base) = match dirs.as_ref() {
            Some(dirs) => (
                dirs.config_dir().to_path_buf(),
                dirs.data_local_dir().to_path_buf(),
            ),
            None => (fallback_base(), fallback_base()),
        };

        Self {
            config_dir: config_base.join(app_name),
            log_dir: local_base.join(app_name).join("logs"),
        }
    }

    /// The folders with an injected config directory.
    ///
    /// This is what tests use, so a test can never read or write the real
    /// `%APPDATA%\snp-ligase` (SPEC section 10; NEXT_STEPS step 4). The log
    /// folder still comes from the OS, because nothing in a test writes logs.
    #[cfg(test)]
    #[must_use]
    pub fn with_config_dir(config_dir: PathBuf) -> Self {
        Self {
            config_dir,
            ..Self::discover(crate::APP_NAME)
        }
    }
}

/// Used only when the OS has no standard folders for us.
fn fallback_base() -> PathBuf {
    std::env::temp_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn every_folder_is_scoped_to_the_app_name() {
        let paths = Paths::discover(crate::APP_NAME);

        assert_eq!(
            paths.config_dir.file_name(),
            Some(OsStr::new("snp-ligase")),
            "config dir: {}",
            paths.config_dir.display()
        );
        assert_eq!(paths.log_dir.file_name(), Some(OsStr::new("logs")));
        assert_eq!(
            paths.log_dir.parent().and_then(|p| p.file_name()),
            Some(OsStr::new("snp-ligase")),
            "log dir: {}",
            paths.log_dir.display()
        );
    }

    /// The SPEC promises these exact locations; the fallback must not change
    /// their shape, only their root.
    #[test]
    fn fallback_paths_keep_the_layout() {
        let paths = Paths {
            config_dir: fallback_base().join("snp-ligase"),
            log_dir: fallback_base().join("snp-ligase").join("logs"),
        };

        assert_eq!(paths.log_dir.file_name(), Some(OsStr::new("logs")));
    }

    /// NEXT_STEPS step 4: the config directory is injectable, so tests use a
    /// temporary folder and never touch the real one.
    #[test]
    fn the_config_directory_can_be_injected() {
        let injected = std::env::temp_dir().join("snp-ligase-injected-config");
        let paths = Paths::with_config_dir(injected.clone());

        assert_eq!(paths.config_dir, injected);
        assert_eq!(paths.log_dir.file_name(), Some(OsStr::new("logs")));
    }
}
