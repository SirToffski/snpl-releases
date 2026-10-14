//! Build script for `snp-ligase`: embeds the Windows application manifest.
//!
//! The manifest is what makes the process **Per-Monitor DPI Aware V2**
//! (SPEC section 5.2), which is a MUST: it is not something we can turn on at
//! runtime and trust. It declares `<dpiAwareness>PerMonitorV2</dpiAwareness>`
//! plus the legacy `<dpiAware>true/pm</dpiAware>`.
//!
//! `embed-manifest` hands the manifest file to the MSVC linker with
//! `/MANIFESTINPUT`, so no Windows SDK manifest tool is needed and the build
//! stays a plain `cargo build`.

use std::path::Path;

/// The manifest, next to this build script.
const MANIFEST: &str = "snp-ligase.manifest";

/// The application icon (16-256 px), shown by Explorer, shortcuts and the
/// taskbar. Generated from `assets/icon.png` at the repository root.
const ICON: &str = "assets/snpl.ico";

fn main() {
    println!("cargo:rerun-if-changed={MANIFEST}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON}");

    // Only Windows targets carry a manifest embed; skip everything else so the
    // workspace still configures cleanly on other hosts.
    if std::env::var_os("CARGO_CFG_TARGET_OS").as_deref() == Some(std::ffi::OsStr::new("windows")) {
        let manifest = Path::new(MANIFEST);
        match embed_manifest::embed_manifest_file(manifest) {
            Ok(()) => {}
            Err(err) => panic!(
                "could not embed the Windows manifest {}: {err}",
                manifest.display()
            ),
        }

        // The icon and the version strings go in through a resource script,
        // compiled with the Windows SDK's `rc.exe`. `winresource` adds no
        // manifest of its own, so it does not clash with the one above.
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon(ICON)
            .set("ProductName", "snpl")
            .set("FileDescription", "snpl")
            .set("OriginalFilename", "snpl.exe")
            .set("InternalName", "snpl");
        if let Err(err) = resource.compile() {
            panic!("could not embed the application icon {ICON}: {err}");
        }
    }
}
