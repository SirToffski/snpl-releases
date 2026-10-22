"""Copy the source of a snp-ligase-dev checkout into this repo for a release.

    python scripts/sync_from_dev.py <path to the dev checkout> <version>

It takes the dev repo's committed HEAD (git archive, so nothing untracked
comes along), replaces this repo's `crates/`, `Cargo.lock` and the Linux
`.desktop` template with it, and then applies the release-only edits:

- the binary is `snpl` (`[[bin]]` name, the integration test that runs it,
  the Windows version strings and manifest identity);
- the workspace version is <version>, the license is MIT, and the release
  profile (thin LTO, one codegen unit, strip) is appended.

Every edit must match exactly once. If the dev source changed under one of
them, the script stops and names it, rather than shipping a half-renamed
build. The app id, the window title, the config folder and the `.desktop`
name stay `snp-ligase`: portal permissions and KDE shortcuts are tied to it.
"""

import io
import pathlib
import re
import shutil
import subprocess
import sys
import tarfile

ROOT = pathlib.Path(__file__).resolve().parent.parent

RELEASE_PROFILE = """
[profile.release]
lto = "thin"
codegen-units = 1
strip = true
"""


def edit(path: str, old: str, new: str) -> None:
    file = ROOT / path
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        sys.exit(f"sync: {path}: expected exactly one {old!r}, found {count}")
    file.write_text(text.replace(old, new), encoding="utf-8", newline="")


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    dev = pathlib.Path(sys.argv[1]).resolve()
    version = sys.argv[2]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        sys.exit(f"sync: {version!r} is not a version like 0.3.0")

    head = subprocess.run(
        ["git", "-C", str(dev), "rev-parse", "--short", "HEAD"],
        check=True, capture_output=True, text=True,
    ).stdout.strip()
    archive = subprocess.run(
        ["git", "-C", str(dev), "archive", "--format=tar", "HEAD",
         "crates", "Cargo.toml", "Cargo.lock", "packaging/linux/snp-ligase.desktop",
         "packaging/linux/CHECKLIST.md"],
        check=True, capture_output=True,
    ).stdout

    for path in ["crates", "packaging/linux"]:
        shutil.rmtree(ROOT / path, ignore_errors=True)
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        tar.extractall(ROOT, filter="data")

    # The binary is snpl.
    edit("crates/snp-ligase-app/Cargo.toml",
         '[[bin]]\nname = "snp-ligase"\n', '[[bin]]\nname = "snpl"\n')
    edit("crates/snp-ligase-app/tests/debug_capture_paths.rs",
         'env!("CARGO_BIN_EXE_snp-ligase")', 'env!("CARGO_BIN_EXE_snpl")')
    for key, value in [("ProductName", "snpl"), ("FileDescription", "snpl"),
                       ("OriginalFilename", "snpl.exe"), ("InternalName", "snpl")]:
        dev_value = "snp-ligase.exe" if key == "OriginalFilename" else "snp-ligase"
        edit("crates/snp-ligase-app/build.rs",
             f'.set("{key}", "{dev_value}")', f'.set("{key}", "{value}")')
    edit("crates/snp-ligase-app/snp-ligase.manifest",
         'name="ntoff.snp-ligase"', 'name="SirToffski.snpl"')

    # The workspace: version, license, release profile.
    edit("Cargo.toml", '\nversion = "0.1.0"\n', f'\nversion = "{version}"\n')
    edit("Cargo.toml", '\nlicense = "MIT OR Apache-2.0"\n', '\nlicense = "MIT"\n')
    cargo = ROOT / "Cargo.toml"
    if "[profile.release]" in cargo.read_text(encoding="utf-8"):
        sys.exit("sync: the dev Cargo.toml has its own [profile.release]; merge it by hand")
    with cargo.open("a", encoding="utf-8", newline="") as file:
        file.write(RELEASE_PROFILE)

    # The packages carry the same version.
    edit_version("packaging/arch/PKGBUILD", r"^pkgver=.*$", f"pkgver={version}")
    edit_version("packaging/fedora/snpl.spec", r"^Version:.*$", f"Version:        {version}")

    print(f"synced dev {head} as snpl {version}; now run: cargo update -w && cargo test --workspace")


def edit_version(path: str, pattern: str, line: str) -> None:
    file = ROOT / path
    text, count = re.subn(pattern, line, file.read_text(encoding="utf-8"), flags=re.M)
    if count != 1:
        sys.exit(f"sync: {path}: expected one version line, found {count}")
    file.write_text(text, encoding="utf-8", newline="")


if __name__ == "__main__":
    main()
