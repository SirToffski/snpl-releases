//! The app's one clipboard (LINUX_SUPPORT.md section 3, item 6).
//!
//! On Linux the process that copied is the process that serves the paste:
//! `arboard` stops handing the data out the moment its `Clipboard` is dropped,
//! so a fresh handle per operation — what the app used to open — made a copied
//! image vanish as soon as it was copied. [`ClipboardHandle`] opens the OS
//! clipboard on the first use and keeps it for the app's whole life; copy and
//! paste both go through it.
//!
//! On Linux `arboard`'s `wayland-data-control` feature is on (see the app's
//! `Cargo.toml`), so a Wayland session is served through the compositor's
//! data-control protocol and an X11 session through the X11 selection.

use image::RgbaImage;

/// What the app needs from a clipboard.
///
/// A trait only so a test can stand in for the OS one: which handle an
/// operation used cannot be seen from a pure function, and a test has no
/// display to open a real clipboard on.
pub(crate) trait ClipboardOps: Send {
    /// Stores `bytes` (RGBA8, `width × height`) as the clipboard image.
    fn set_image(&mut self, width: usize, height: usize, bytes: Vec<u8>) -> Result<(), String>;
    /// The clipboard's image as `(width, height, RGBA8 bytes)`.
    fn get_image(&mut self) -> Result<(usize, usize, Vec<u8>), String>;
}

/// The OS clipboard: `arboard`, and the only thing in this module that needs
/// one.
struct SystemClipboard(arboard::Clipboard);

impl ClipboardOps for SystemClipboard {
    fn set_image(&mut self, width: usize, height: usize, bytes: Vec<u8>) -> Result<(), String> {
        self.0
            .set_image(arboard::ImageData {
                width,
                height,
                bytes: bytes.into(),
            })
            .map_err(|err| err.to_string())
    }

    fn get_image(&mut self) -> Result<(usize, usize, Vec<u8>), String> {
        let image = self
            .0
            .get_image()
            .map_err(|err| format!("The clipboard holds no image ({err})."))?;
        Ok((image.width, image.height, image.bytes.into_owned()))
    }
}

/// The app's clipboard: one OS handle, opened on first use and kept.
#[derive(Clone, Default)]
pub struct ClipboardHandle {
    shared: std::sync::Arc<std::sync::Mutex<ClipboardState>>,
}

#[derive(Default)]
struct ClipboardState {
    ops: Option<Box<dyn ClipboardOps>>,
}

impl std::fmt::Debug for ClipboardHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClipboardHandle(shared)")
    }
}

impl ClipboardState {
    /// The open OS clipboard, opening it on first use.
    fn ops(&mut self) -> Result<&mut Box<dyn ClipboardOps>, String> {
        if self.ops.is_none() {
            match arboard::Clipboard::new() {
                Ok(clipboard) => self.ops = Some(Box::new(SystemClipboard(clipboard))),
                Err(err) => return Err(format!("Could not reach the clipboard: {err}")),
            }
        }
        self.ops
            .as_mut()
            .ok_or_else(|| "the clipboard is not open".to_owned())
    }
}

impl ClipboardHandle {
    /// Puts `image` on the clipboard (SPEC 6.3 and 7.4: captures and Copy).
    ///
    /// On Linux the handle has to stay alive for as long as the copy is to be
    /// pasteable, which is why the caller never opens one of its own.
    pub fn copy_image(&mut self, image: &RgbaImage) -> Result<(), String> {
        let (width, height) = (image.width(), image.height());
        self.shared
            .lock()
            .map_err(|_| "clipboard lock poisoned".to_owned())?
            .ops()?
            .set_image(width as usize, height as usize, image.as_raw().clone())
    }

    /// Wayland must finish this on the controller even when frames stop.
    pub fn copy_capture_blocking(&mut self, image: &RgbaImage) -> Result<(), String> {
        let started = std::time::Instant::now();
        let mut attempts = 0;
        loop {
            attempts += 1;
            match self.copy_image(image) {
                Ok(()) => {
                    tracing::info!(
                        attempts,
                        width = image.width(),
                        height = image.height(),
                        "controller copied capture to clipboard"
                    );
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(attempts, %error, "controller capture copy refused");
                    let remaining =
                        crate::app::CAPTURE_COPY_WINDOW.saturating_sub(started.elapsed());
                    std::thread::sleep(remaining.min(crate::app::CAPTURE_COPY_RETRY));
                    if started.elapsed() >= crate::app::CAPTURE_COPY_WINDOW {
                        tracing::warn!(attempts, %error, "controller capture copy gave up after retry deadline");
                        return Err(error);
                    }
                }
            }
        }
    }

    /// The clipboard's image (SPEC 7.1: Ctrl+V and "Paste Image into This
    /// Tab").
    pub fn image(&mut self) -> Result<RgbaImage, String> {
        let (width, height, mut bytes) = self
            .shared
            .lock()
            .map_err(|_| "clipboard lock poisoned".to_owned())?
            .ops()?
            .get_image()?;
        // 32-bit DIBs from other applications often carry an all-zero alpha
        // channel, which would make the whole picture transparent.
        if !bytes.is_empty() && bytes.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0) {
            for pixel in bytes.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
        }
        RgbaImage::from_raw(width as u32, height as u32, bytes)
            .ok_or_else(|| "The clipboard image could not be read.".to_owned())
    }

    /// A handle already standing on `ops`, for tests: the app's paste and copy
    /// tests pass a [`FakeClipboard`] so they need neither a display nor a
    /// clipboard of their own.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_test(ops: Box<dyn ClipboardOps>) -> Self {
        Self {
            shared: std::sync::Arc::new(std::sync::Mutex::new(ClipboardState { ops: Some(ops) })),
        }
    }
}

/// A stand-in for the OS clipboard (tests only). It records what the app did
/// with it, which is how the "one handle for copy and paste" rule is checked.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct FakeClipboard {
    state: std::sync::Arc<std::sync::Mutex<FakeClipboardState>>,
}

#[cfg(test)]
#[derive(Default)]
struct FakeClipboardState {
    /// Every image the app copied, in order.
    copied: Vec<(usize, usize, Vec<u8>)>,
    /// The image the next paste returns.
    image: Option<(usize, usize, Vec<u8>)>,
    /// How many copies are refused first, like a clipboard another program
    /// holds open.
    refusals: usize,
}

#[cfg(test)]
impl FakeClipboard {
    /// Every image the app copied, in order.
    pub fn copied(&self) -> Vec<(usize, usize, Vec<u8>)> {
        self.state.lock().unwrap().copied.clone()
    }

    /// What the next paste hands back.
    pub fn set_paste_image(&self, width: usize, height: usize, bytes: Vec<u8>) {
        self.state.lock().unwrap().image = Some((width, height, bytes));
    }

    /// Refuses the next `count` copies, the way Windows does while another
    /// program holds the clipboard open.
    pub fn refuse_copies(&self, count: usize) {
        self.state.lock().unwrap().refusals = count;
    }
}

#[cfg(test)]
impl ClipboardOps for FakeClipboard {
    fn set_image(&mut self, width: usize, height: usize, bytes: Vec<u8>) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if state.refusals > 0 {
            state.refusals -= 1;
            return Err("the clipboard is held by another program (fake)".to_owned());
        }
        state.copied.push((width, height, bytes));
        Ok(())
    }

    fn get_image(&mut self) -> Result<(usize, usize, Vec<u8>), String> {
        self.state
            .lock()
            .unwrap()
            .image
            .clone()
            .ok_or_else(|| "The clipboard holds no image (fake).".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clipboard that fails, to check the error the user sees.
    #[derive(Default)]
    struct EmptyClipboard;

    impl ClipboardOps for EmptyClipboard {
        fn set_image(&mut self, _: usize, _: usize, _: Vec<u8>) -> Result<(), String> {
            Err("the fake refuses to copy".to_owned())
        }

        fn get_image(&mut self) -> Result<(usize, usize, Vec<u8>), String> {
            Err("The clipboard holds no image (empty).".to_owned())
        }
    }

    fn pixel() -> RgbaImage {
        RgbaImage::from_raw(2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]).expect("a 2×1 image")
    }

    /// LINUX_SUPPORT.md L0c item 6: the copy and the paste after it go through
    /// the one handle the app keeps — the fake would not see the second call if
    /// either operation opened a clipboard of its own.
    #[test]
    fn copy_and_paste_use_the_one_handle() {
        let fake = FakeClipboard::default();
        let mut clipboard = ClipboardHandle::for_test(Box::new(fake.clone()));
        let image = pixel();

        clipboard.copy_image(&image).expect("copy");
        fake.set_paste_image(2, 1, image.as_raw().clone());

        assert_eq!(fake.copied(), vec![(2, 1, image.as_raw().clone())]);
        assert_eq!(clipboard.image().expect("paste"), image);
    }

    /// The all-zero-alpha DIB fix still applies to what a paste returns.
    #[test]
    fn a_zero_alpha_dib_is_made_opaque() {
        let fake = FakeClipboard::default();
        let mut clipboard = ClipboardHandle::for_test(Box::new(fake.clone()));
        fake.set_paste_image(2, 1, vec![1, 2, 3, 0, 4, 5, 6, 0]);

        assert_eq!(
            clipboard.image().expect("paste").into_raw(),
            vec![1, 2, 3, 255, 4, 5, 6, 255]
        );
    }

    /// A clipboard that cannot be reached is an error message, not a panic
    /// (SPEC section 0 rule 10).
    #[test]
    fn failures_are_messages() {
        let mut clipboard = ClipboardHandle::for_test(Box::new(EmptyClipboard));

        assert!(clipboard.copy_image(&pixel()).is_err());
        assert_eq!(
            clipboard.image().expect_err("no image"),
            "The clipboard holds no image (empty)."
        );
    }
}
