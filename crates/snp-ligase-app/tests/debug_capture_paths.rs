//! LINUX_SUPPORT.md section 3, item 4: `--debug-capture` must say where the
//! config file and the logs live, so a Linux bug report carries them.
//!
//! A whole-process test: `directories` reads the environment, and the report is
//! only worth having if the real binary prints it. The test gives the child its
//! own `XDG_CONFIG_HOME`/`XDG_DATA_HOME` (so it can never touch the real
//! folders) and reads what it wrote to stdout. On Linux the capture itself
//! fails with "not supported yet" (phase L1), which is exactly why the folders
//! have to be printed before it is tried.

#![cfg(target_os = "linux")]

use std::process::Command;

#[test]
fn debug_capture_prints_the_config_and_log_folders() {
    let root = std::env::temp_dir().join(format!(
        "snp-ligase-paths-{}-{}",
        std::process::id(),
        line!()
    ));
    let config_home = root.join("config-home");
    let data_home = root.join("data-home");
    let expected_config = config_home.join("snp-ligase");
    let expected_logs = data_home.join("snp-ligase").join("logs");

    let output = Command::new(env!("CARGO_BIN_EXE_snpl"))
        .arg("--debug-capture")
        .arg(root.join("captures"))
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .output()
        .expect("the binary should start");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains(&expected_config.display().to_string()),
        "the config folder is not in the report.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(&expected_logs.display().to_string()),
        "the log folder is not in the report.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
