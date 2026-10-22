# Five-minute Linux check

1. Capture a Region on each output. Check that the frozen image, selection
   edges and size label line up with what you dragged.
2. If outputs use different scaling, drag across their boundary and capture
   a small region on each. Check size and sharpness after pasting.
3. Use a bound capture shortcut (KDE defaults: Ctrl+Print for Region,
   Ctrl+Shift+Print for All). On XFCE, free Print from its screenshot tool first.
4. Capture, leave snp-ligase minimized, and immediately paste into another
   image editor. The image should arrive without reopening snp-ligase.
5. If KDE offers HDR, repeat with HDR on and off and compare colours and
   brightness. Restore your original setting. Send any problem description
   with the newest `snp-ligase.YYYY-MM-DD.log` from
   `${XDG_DATA_HOME:-~/.local/share}/snp-ligase/logs/`.
