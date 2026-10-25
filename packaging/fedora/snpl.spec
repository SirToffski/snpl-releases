# Builds the snpl RPM for Fedora from this repository's checkout, in place:
#
#   rpmbuild -bb --build-in-place --define "_topdir $PWD/rpmbuild" packaging/fedora/snpl.spec
#
# (run from the repository root). The release workflow runs exactly this in a
# fedora container. Install the result with:  sudo dnf install ./snpl-*.rpm
#
# The program is /usr/bin/snpl. The menu entry, its icon and the app id stay
# `snp-ligase`: KDE ties the screenshot permission and the global shortcuts to
# that id, and grants KWin's native capture to the `Exec` path in the entry.

# Cargo's release profile already strips the binary.
%global debug_package %{nil}

Name:           snpl
Version:        0.3.5
Release:        1%{?dist}
Summary:        Screenshot and annotation tool (snp-ligase)
License:        MIT
URL:            https://github.com/SirToffski/snpl-releases

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  pkgconf-pkg-config
BuildRequires:  libX11-devel
BuildRequires:  libxkbcommon-devel
BuildRequires:  wayland-devel
# %check runs the tests; one of them needs at least one installed font.
BuildRequires:  dejavu-sans-fonts

# Linked: libX11, libX11-xcb (found automatically). Loaded at run time, so
# listed by hand: xkbcommon (and its X11 part), Wayland, EGL or Vulkan for
# drawing, libxcb, Xcursor, Xi, D-Bus.
Requires:       libX11-xcb
Requires:       libxcb
Requires:       libXcursor
Requires:       libXi
Requires:       libxkbcommon
Requires:       libxkbcommon-x11
Requires:       libwayland-client
Requires:       libwayland-egl
Requires:       libglvnd-egl
Requires:       vulkan-loader
Requires:       dbus-libs
Requires:       hicolor-icon-theme
Recommends:     xdg-desktop-portal

%description
snp-ligase is a screenshot and annotation tool: capture a region, a monitor,
a window or every monitor, then annotate the capture with arrows, shapes,
text, highlights, blur and numbered steps. It runs on X11 and on Wayland
(KDE Plasma natively; other desktops through the screenshot portal).

%build
# Fedora's rpmbuild exports its own RUSTFLAGS (opt-level, full debuginfo,
# strip=none, frame pointers). They would override Cargo's release profile
# (thin LTO, stripped) and ship a binary several times larger, so Cargo's
# profile decides here, as it does for the Windows and Arch builds.
unset RUSTFLAGS
cargo build --release --locked -p snp-ligase-app

%check
unset RUSTFLAGS
cargo test --workspace --locked

%install
install -Dm755 target/release/snpl %{buildroot}%{_bindir}/snpl
install -dm755 %{buildroot}%{_datadir}/applications
sed 's|@BINDIR@/snp-ligase|%{_bindir}/snpl|g' packaging/linux/snp-ligase.desktop \
    > %{buildroot}%{_datadir}/applications/snp-ligase.desktop
if grep -q '@' %{buildroot}%{_datadir}/applications/snp-ligase.desktop; then
    echo "the menu entry still has a placeholder" >&2
    exit 1
fi
install -Dm644 crates/snp-ligase-app/assets/snpl-256.png \
    %{buildroot}%{_datadir}/icons/hicolor/256x256/apps/snp-ligase.png
install -Dm644 crates/snp-ligase-app/assets/snpl-32.png \
    %{buildroot}%{_datadir}/icons/hicolor/32x32/apps/snp-ligase.png

%files
%license LICENSE
%doc packaging/linux/CHECKLIST.md
%{_bindir}/snpl
%{_datadir}/applications/snp-ligase.desktop
%{_datadir}/icons/hicolor/256x256/apps/snp-ligase.png
%{_datadir}/icons/hicolor/32x32/apps/snp-ligase.png

%changelog
* Sun Oct 25 2026 SirToffski - 0.3.5-1
- Windows pass: the editor comes back after a capture (Show editor after
  capture, on by default), captures finish while it is minimized, every drag
  direction selects, Show in Explorer, sturdier GDI and WGC capture fallbacks.
  On Linux the overlay now shares one selection state with Windows.

* Thu Oct 22 2026 SirToffski - 0.3.1-1
- First published Linux release (0.3.0 was tagged but never released): the
  instance-channel ownership warning no longer names uids; tests made robust
  on slow and font-less build machines.

* Thu Oct 22 2026 SirToffski - 0.3.0-1
- First Linux release: X11, KDE Plasma Wayland (native capture), and the
  screenshot portal on other Wayland desktops.
