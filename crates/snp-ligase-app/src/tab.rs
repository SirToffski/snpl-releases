//! One open image: its document, its undo history, and how it is displayed
//! (SPEC section 7.1).
//!
//! Each capture and each opened image gets its own tab, and each tab owns
//! everything about that image: the [`Document`], the undo and redo stacks, the
//! file path, whether it has unsaved changes, the zoom and pan, the selection
//! and the sprite texture cache.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{Color32, ColorImage, TextureHandle, TextureOptions};
use image::RgbaImage;
use snp_ligase_core::codec;
use snp_ligase_core::geom::Rect;
use snp_ligase_core::model::{Annotation, Document};
use snp_ligase_core::render::{self, FlattenOptions, Sprite};
use snp_ligase_core::text::Fonts;

use crate::viewport::Viewport;

/// SPEC section 4: snapshot-based undo, capped at 100 entries. Do not build a
/// command-pattern undo system.
pub const UNDO_LIMIT: usize = 100;

/// Zoom limits (SPEC section 7.2): 10% to 1600%.
pub const ZOOM_MIN: f32 = 0.1;
pub const ZOOM_MAX: f32 = 16.0;

/// At or above this zoom, sampling is nearest-neighbour (SPEC section 7.2).
pub const NEAREST_FROM_ZOOM: f32 = 2.0;

/// The next tab id, process-wide and monotonic: two tabs never share one, even
/// across a close and a reopen (H6 needs a result to find its tab).
fn next_tab_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// One open image.
pub struct Tab {
    /// A stable identity, unlike the index: an async save result (H6) has to
    /// find this tab after the user switched to another one, and the index
    /// moves when tabs are added or closed.
    id: u64,
    /// The image and its annotations.
    pub document: Document,
    /// Zoom and pan of the canvas.
    pub viewport: Viewport,
    /// Auto-fit to the window (NEXT_STEPS step 13): while true the canvas
    /// re-fits when its size or the content changes. A manual zoom clears it
    /// for this tab only, so a zoom here cannot stop the next capture from
    /// fitting. A new tab takes the "Auto-fit images to the window" setting.
    pub auto_fit: bool,
    /// The selected annotation ids, in selection order.
    pub selection: Vec<u64>,
    /// Shown in place of the canvas when the image cannot be displayed
    /// (SPEC section 7.2).
    pub display_error: Option<String>,
    path: Option<PathBuf>,
    /// The title shown on the tab: the file name, or `Capture HH:MM:SS`.
    title: String,
    /// The document as it was last saved, so "unsaved changes" is exact rather
    /// than a flag that never clears itself.
    saved: Option<Document>,
    undo: Vec<Document>,
    redo: Vec<Document>,
    /// The snapshot an edit in flight will undo to. See [`Tab::stage_edit`].
    staged: Option<Document>,
    textures: TextureCache,
}

impl Tab {
    /// A tab for an image that is not from a file, such as a capture.
    #[must_use]
    pub fn new(document: Document, title: String) -> Self {
        Self {
            id: next_tab_id(),
            document,
            viewport: Viewport::new(),
            // The setting's default; `push_tab` replaces it with the setting
            // the user chose (NEXT_STEPS step 13).
            auto_fit: true,
            selection: Vec::new(),
            display_error: None,
            path: None,
            title,
            saved: None,
            undo: Vec::new(),
            redo: Vec::new(),
            staged: None,
            textures: TextureCache::default(),
        }
    }

    /// A tab for an image opened from a path.
    ///
    /// A file that was just opened has no unsaved changes, so the saved copy is
    /// the document itself.
    #[must_use]
    pub fn from_path(document: Document, path: PathBuf) -> Self {
        let mut tab = Self::new(document, String::new());
        tab.set_path(path);
        tab.saved = Some(tab.document.clone());
        tab
    }

    /// The tab title, with a `*` when there are unsaved changes (SPEC 7.1).
    #[must_use]
    pub fn title(&self) -> String {
        if self.dirty() {
            format!("{} *", self.title)
        } else {
            self.title.clone()
        }
    }

    /// True when the document differs from what was last saved.
    ///
    /// An image that has never been saved **is** unsaved: nothing about it is on
    /// disk yet, so closing the tab would throw it away (R22). That is why a tab
    /// with no path is dirty until [`Tab::save_to`] gives it one.
    #[must_use]
    pub fn dirty(&self) -> bool {
        match &self.saved {
            Some(saved) => *saved != self.document,
            None => true,
        }
    }

    /// Where this tab's image lives, if it has been saved.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// This tab's stable identity (see the field's comment).
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Records a new file path and takes the title from it.
    pub fn set_path(&mut self, path: PathBuf) {
        self.title = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        self.path = Some(path);
    }

    /// The image size in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        self.document.size()
    }

    // ---------------------------------------------------------------------
    // Selection (SPEC sections 7.3 and 7.4)
    // ---------------------------------------------------------------------

    /// The selected annotation, when exactly one is selected.
    #[must_use]
    pub fn selected(&self) -> Option<&Annotation> {
        match self.selection.as_slice() {
            [id] => self.document.find(*id),
            _ => None,
        }
    }

    /// The selected annotation, mutably, when exactly one is selected.
    pub fn selected_mut(&mut self) -> Option<&mut Annotation> {
        let id = *self.selection.first()?;
        self.selection.retain(|selected| *selected == id);
        self.document.find_mut(id)
    }

    /// Selects one annotation, replacing the selection. `None` deselects.
    pub fn select_only(&mut self, id: Option<u64>) {
        self.selection = id.into_iter().collect();
    }

    /// Adds or removes an annotation from the selection (Shift+click, SPEC 7.3).
    pub fn toggle_selection(&mut self, id: u64) {
        if let Some(position) = self.selection.iter().position(|selected| *selected == id) {
            self.selection.remove(position);
        } else {
            self.selection.push(id);
        }
    }

    /// Selects every annotation (`Ctrl+A`, SPEC 7.4).
    pub fn select_all(&mut self) {
        self.selection = self
            .document
            .annotations
            .iter()
            .map(|annotation| annotation.id)
            .collect();
    }

    /// The selected ids, in the order they are moved and deleted.
    #[must_use]
    pub fn selection(&self) -> &[u64] {
        &self.selection
    }

    // ---------------------------------------------------------------------
    // Undo (SPEC section 4)
    // ---------------------------------------------------------------------

    /// Takes a snapshot before a gesture changes anything.
    ///
    /// Called once when a gesture starts, not on every mouse move (SPEC 7.3),
    /// so a whole drag is one undo step. The snapshot is *staged*: it is pushed
    /// by [`Tab::commit_edit`] only if the document really changed, so a press
    /// that turned out to be a plain click leaves the undo stack — and the redo
    /// stack — alone (R20). It also hinges back into the R21 cancel path.
    pub fn stage_edit(&mut self) {
        if self.staged.is_none() {
            self.staged = Some(self.document.clone());
        }
    }

    /// Ends an in-flight edit, pushing the staged snapshot if it differs from
    /// what the document has become.
    ///
    /// No snapshot, or an identical one, means the gesture changed nothing and
    /// must not cost the user an undo step.
    pub fn commit_edit(&mut self) {
        let Some(staged) = self.staged.take() else {
            return;
        };
        if staged != self.document {
            self.push_undo(staged);
        }
    }

    /// Undoes an in-flight edit by putting the staged snapshot back, which is
    /// what cancelling a move or a handle drag does (R21).
    ///
    /// Returns true when a document was restored.
    pub fn cancel_edit(&mut self) -> bool {
        match self.staged.take() {
            Some(staged) => {
                self.document = staged;
                self.drop_missing_selection();
                true
            }
            None => false,
        }
    }

    /// True when an edit is in flight that can still be cancelled.
    #[must_use]
    pub fn edit_in_flight(&self) -> bool {
        self.staged.is_some()
    }

    /// How many undo steps there are, for tests.
    #[cfg(test)]
    #[must_use]
    pub fn undo_depth_for_test(&self) -> usize {
        self.undo.len()
    }

    /// Pushes one undo snapshot, dropping the oldest past [`UNDO_LIMIT`] and
    /// clearing the redo stack.
    fn push_undo(&mut self, snapshot: Document) {
        // Belt and braces: a snapshot equal to the current document is not a
        // step, whoever asked for it.
        if self.undo.last() == Some(&self.document) {
            self.redo.clear();
            return;
        }
        self.undo.push(snapshot);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Pushes an undo snapshot now, for a change that happens in one frame.
    ///
    /// Prefer [`Tab::stage_edit`] for anything spread over several frames.
    pub fn begin_edit(&mut self) {
        let snapshot = self.document.clone();
        if self.undo.last() == Some(&snapshot) {
            return;
        }
        self.push_undo(snapshot);
    }

    /// Pushes an undo snapshot that was taken earlier, for a change completed
    /// in one go (the text editor session, SPEC 7.5).
    ///
    /// [`Tab::stage_edit`] cannot be used for that: it commits on every frame
    /// the pointer is up, which would make a typing session one undo step per
    /// keystroke.
    pub fn push_snapshot(&mut self, snapshot: Document) {
        self.push_undo(snapshot);
    }

    /// True when there is something to undo.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// True when there is something to redo.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Steps back one edit.
    pub fn undo(&mut self) -> bool {
        // An edit still in flight is discarded: Ctrl+Z during a drag means "go
        // back", not "cancel this change and then go back".
        self.staged = None;
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.redo
            .push(std::mem::replace(&mut self.document, previous));
        self.drop_missing_selection();
        // Undo can take annotations away; their textures would otherwise stay
        // cached until the tab closes (R30).
        self.forget_deleted();
        true
    }

    /// Steps forward one edit.
    pub fn redo(&mut self) -> bool {
        self.staged = None;
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(&mut self.document, next));
        self.drop_missing_selection();
        self.forget_deleted();
        true
    }

    /// Undoing can delete the thing that was selected, so the selection is
    /// trimmed to what still exists.
    fn drop_missing_selection(&mut self) {
        self.selection
            .retain(|id| self.document.find(*id).is_some());
    }

    // ---------------------------------------------------------------------
    // Saving (SPEC section 9)
    // ---------------------------------------------------------------------

    /// The flattened image: the base with every annotation composited on top,
    /// with `options` deciding what happens to annotations outside the image
    /// (NEXT_STEPS step 10 — with `FlattenOptions::CUT` this is the pre-step-10
    /// behaviour, byte for byte).
    #[must_use]
    pub fn flattened(&self, fonts: &mut Fonts, options: &FlattenOptions) -> RgbaImage {
        render::flatten_with(&self.document, fonts, options)
    }

    /// The rectangle the exported image would cover, in image coordinates:
    /// the union of the image and the **painted pixels** of every annotation
    /// with `expand`, the image itself otherwise (NEXT_STEPS steps 10 and 11).
    ///
    /// J3: with `expand` the rectangle comes from the **sprite cache** — the
    /// painted bounds stored when each sprite was rasterized — rather than
    /// from `bounds()`, which lays out every Text annotation with cosmic-text.
    /// The canvas and the status bar call this every frame, so the layout cost
    /// is gone. A sprite that has not been rasterized yet (a brand-new
    /// annotation) is simply not in the rectangle until the next frame; an
    /// annotation that is being *moved* reuses its cached pixels (H4), so its
    /// painted rectangle follows the same offset.
    #[must_use]
    pub fn export_rect(&self, expand: bool) -> Rect {
        let (width, height) = self.size();
        let mut area = Rect::new(0.0, 0.0, width as f32, height as f32);
        if !expand {
            return area;
        }
        for annotation in &self.document.annotations {
            // Blur and Highlight only ever draw where the base image or a
            // pasted image already is (NEXT_STEPS step 12), so they can never
            // grow it (J1).
            if annotation.kind.is_effect() {
                continue;
            }
            if let Some(painted) = self.textures.painted_rect(annotation, &self.document.base) {
                area = area.union(painted.to_rect());
            }
        }
        area
    }

    /// Saves to `path`, writing through a temporary file and renaming, so a
    /// failed save never destroys what is already there (SPEC section 9).
    ///
    /// The format comes from the extension, and an extension this build cannot
    /// write is an **error**, not a reason to write PNG bytes under a `.jpg`
    /// name (R11). Since M6 every format of SPEC section 9's table can be
    /// written, so Ctrl+S on an opened `.jpg` really writes a JPEG.
    ///
    /// `flatten` is the "annotations outside the image" setting (NEXT_STEPS
    /// step 10): the synchronous path has to obey it exactly like the save
    /// worker does.
    ///
    /// This is the **synchronous** path, used where the caller has to wait for
    /// the file before it can go on: the Save/Discard/Cancel prompt. Ctrl+S,
    /// Save As, Save All and auto-save go through the save worker instead
    /// (H6), because an AVIF 4K encode takes seconds.
    pub fn save_to(
        &mut self,
        fonts: &mut Fonts,
        path: &Path,
        options: &codec::EncodeOptions,
        flatten: &FlattenOptions,
    ) -> Result<(), String> {
        let format = format_for_path(path)?;

        let encode_started = std::time::Instant::now();
        let bytes = codec::encode(&self.flattened(fonts, flatten), format, options)
            .map_err(|err| format!("Could not encode the image: {err}"))?;
        let encode_ms = encode_started.elapsed().as_secs_f64() * 1000.0;
        let write_started = std::time::Instant::now();
        write_atomically(path, &bytes)?;
        // SPEC 12 M7's "saving a 4K PNG ≤ 1 s" budget: the two halves are
        // logged separately, so a slow encoder and a slow disk are told apart.
        tracing::info!(
            path = %path.display(),
            format = format.label(),
            width = self.size().0,
            height = self.size().1,
            bytes = bytes.len(),
            encode_ms,
            write_ms = write_started.elapsed().as_secs_f64() * 1000.0,
            "image saved"
        );

        self.note_saved(path.to_path_buf(), self.document.clone());
        Ok(())
    }

    /// Records a save that has finished, whichever thread wrote it (H6).
    ///
    /// `document` is the exact snapshot whose pixels were written, so a change
    /// made while the save ran leaves the tab with unsaved changes: "saved"
    /// compares against the snapshot, not against whatever the document has
    /// become in the meantime.
    pub fn note_saved(&mut self, path: PathBuf, document: Document) {
        self.saved = Some(document);
        self.set_path(path);
    }

    // ---------------------------------------------------------------------
    // Textures (SPEC section 7.2)
    // ---------------------------------------------------------------------

    /// The base image texture, loaded on first use and reloaded when it
    /// changes.
    ///
    /// Returns `None` and sets [`Self::display_error`] when the image is larger
    /// than the GPU's maximum texture side (SPEC 7.2).
    ///
    /// `zoom` decides the sampling the texture is **created** with: uploading
    /// LINEAR and re-uploading next frame because the zoom wanted nearest (or
    /// the other way round) wastes a full-size upload on the very first frame
    /// (R30).
    pub fn base_texture(
        &mut self,
        ctx: &egui::Context,
        max_texture_side: usize,
        zoom: f32,
    ) -> Option<&TextureHandle> {
        let (width, height) = self.size();
        if width == 0 || height == 0 {
            return None;
        }
        if width as usize > max_texture_side || height as usize > max_texture_side {
            // SPEC 7.2: say so instead of crashing. Tiling is P2.
            self.display_error = Some(format!(
                "This image is {width} × {height} px, larger than the {max_texture_side} px \
                 texture limit of this machine, so it cannot be shown here. Saving still works."
            ));
            return None;
        }
        self.display_error = None;
        self.textures.base(ctx, &self.document.base, zoom)
    }

    /// The texture for one annotation, re-rasterized only when that annotation
    /// changes (SPEC 7.2).
    ///
    /// The cached copy of the annotation is compared by value, which SPEC 7.2
    /// allows, so no revision counter is needed. Blur and Highlight sprites are
    /// made from a different base: a Blur reads the base image, a Highlight
    /// reads the base with every Blur already applied, so the screen matches
    /// what `flatten` writes (R18, and DECISIONS.md for why that differs from
    /// SPEC 4.2).
    ///
    /// A **moved** annotation reuses its sprite (SPEC 12 M7): a translation
    /// does not change the pixels, so the texture stays and only the origin
    /// moves. Without that, dragging a full-image-sized annotation re-rasterized
    /// and re-uploaded tens of megabytes every frame.
    ///
    /// The reuse lasts only while the move does: `moving` is the app's
    /// [`Gesture::is_moving`], and once the gesture is over a sprite whose
    /// cached source differs is rasterized again (H4). The pixels drawn during
    /// a fractional move are the cached ones shifted by a fraction — a
    /// sub-pixel approximation SPEC 12's note accepts — and they are **not**
    /// what a save writes, so they must not stay on screen.
    ///
    /// `zoom` is passed on to the cache for the same reason as in
    /// [`Tab::base_texture`]: a new sprite is uploaded with the sampling the
    /// current zoom asks for.
    ///
    /// `expand` is the "annotations outside the image" mode: with it the effect
    /// layers reach the pasted images, so a Blur or Highlight outside the
    /// original screenshot has pixels to read (NEXT_STEPS step 12).
    ///
    /// [`Gesture::is_moving`]: crate::tools::Gesture::is_moving
    pub fn sprite_texture(
        &mut self,
        ctx: &egui::Context,
        id: u64,
        fonts: &mut Fonts,
        zoom: f32,
        moving: bool,
        expand: bool,
    ) -> Option<SpriteTexture<'_>> {
        let annotation = self.document.find(id)?.clone();
        let base = match annotation.kind {
            // A Highlight reads the base with the images and blurs on it (R18).
            snp_ligase_core::model::Kind::Highlight(_) => self.effects_base(fonts, expand),
            // A Blur reads the base with the pasted images, so a blur over a
            // pasted image blurs the pasted pixels (NEXT_STEPS step 11).
            snp_ligase_core::model::Kind::Blur(_) => self.images_base(fonts, expand),
            _ => render::Layer::whole(Arc::clone(&self.document.base)),
        };
        self.textures.sprite(
            ctx,
            id,
            SpriteSource {
                annotation: &annotation,
                base: &base,
                zoom,
                moving,
            },
            fonts,
        )
    }

    /// The base image the effects are computed from: [`Document::base`] with
    /// every Image annotation composited, and that with every Blur on top.
    ///
    /// A Blur sprite is made from the first layer (the pasted images), a
    /// Highlight sprite from the second (NEXT_STEPS step 11): dragging a
    /// highlight over a blurred password never reveals it, and a blur over a
    /// pasted image really blurs the pasted pixels.
    ///
    /// With `expand` both layers also reach the pasted images outside the
    /// original, which is what lets an effect work there (NEXT_STEPS step 12);
    /// with Cut they are the base image alone, like the saved file.
    ///
    /// Both layers are cached together, keyed by the base image's address, the
    /// list of Image and Blur annotations by value and the `expand` mode: they
    /// have to be rebuilt whenever anything they depend on changes.
    pub fn effects_base(&mut self, fonts: &mut Fonts, expand: bool) -> render::Layer {
        self.refresh_layers(fonts, expand);
        self.textures
            .layers
            .as_ref()
            .map(|cached| cached.blurred.clone())
            .unwrap_or_else(|| render::Layer::whole(Arc::clone(&self.document.base)))
    }

    /// [`Tab::effects_base`]'s first layer: the base with the pasted images.
    pub fn images_base(&mut self, fonts: &mut Fonts, expand: bool) -> render::Layer {
        self.refresh_layers(fonts, expand);
        self.textures
            .layers
            .as_ref()
            .map(|cached| cached.with_images.clone())
            .unwrap_or_else(|| render::Layer::whole(Arc::clone(&self.document.base)))
    }

    /// Rebuilds the two layers when what they are made from has changed.
    fn refresh_layers(&mut self, fonts: &mut Fonts, expand: bool) {
        let base = Arc::clone(&self.document.base);
        let images: Vec<Annotation> = self
            .document
            .annotations
            .iter()
            .filter(|annotation| matches!(annotation.kind, snp_ligase_core::model::Kind::Image(_)))
            .cloned()
            .collect();
        let blurs: Vec<Annotation> = self
            .document
            .annotations
            .iter()
            .filter(|annotation| matches!(annotation.kind, snp_ligase_core::model::Kind::Blur(_)))
            .cloned()
            .collect();

        let fresh = self.textures.layers.as_ref().is_some_and(|cached| {
            Arc::ptr_eq(&cached.image, &base)
                && cached.images == images
                && cached.blurs == blurs
                && cached.expand == expand
        });
        if fresh {
            return;
        }

        let with_images = render::images_base(&self.document, fonts, expand);
        let blurred = render::effects_base_from(&self.document, &with_images, fonts);
        self.textures.layers = Some(CachedLayers {
            image: base,
            images,
            blurs,
            expand,
            with_images,
            blurred,
        });
    }

    /// Re-uploads any texture whose sampling no longer matches the zoom.
    ///
    /// egui's filtering is decided when a texture is uploaded, not when it is
    /// drawn, so crossing 200% (SPEC 7.2) means re-uploading with the other
    /// filter. The pixels are kept for exactly this.
    pub fn apply_sampling(&mut self, zoom: f32) {
        self.textures.apply_sampling(zoom);
    }

    /// A manual zoom: auto-fit stands down for **this** tab (NEXT_STEPS step
    /// 13).
    ///
    /// Every zoom the user drives by hand — Ctrl+wheel, View > Zoom In/Out,
    /// Actual Size — goes through here, so no path can miss turning auto-fit
    /// off. Panning is not a zoom and does not.
    pub fn user_zoomed(&mut self) {
        if self.auto_fit {
            self.auto_fit = false;
            // Once per tab: the status bar dropping "Auto-fit" is the visible
            // half, and this is the half a bug report needs (rule 11).
            tracing::info!(
                target: "snp_ligase::ui",
                "a manual zoom turned auto-fit off for this tab"
            );
        }
    }

    /// Drops cached textures for annotations that no longer exist.
    ///
    /// Dropping a [`TextureHandle`] frees the texture, so this is also how the
    /// GPU memory of deleted annotations goes back.
    pub fn forget_deleted(&mut self) {
        let mut live = Vec::with_capacity(self.document.annotations.len());
        for annotation in &self.document.annotations {
            live.push(annotation.id);
        }
        self.textures.retain_sprites(|id| live.contains(&id));
    }

    /// Puts a whole document back, as `Esc` does during a text session (M7).
    ///
    /// The selection is trimmed to what still exists, the textures of the
    /// annotations that are gone are freed, and any staged edit is dropped:
    /// the restored document is the truth, not a snapshot to push later.
    pub fn restore_document(&mut self, document: Document) {
        self.document = document;
        self.staged = None;
        self.drop_missing_selection();
        self.forget_deleted();
    }

    /// Drops every cached texture, for when a tab is closed.
    pub fn forget_all_textures(&mut self) {
        self.textures = TextureCache::default();
    }
}

/// The format a path's extension names, or the message listing what this build
/// can write.
///
/// Shared by the synchronous save ([`Tab::save_to`], the close prompt) and the
/// async one (`crate::save`), so the two cannot disagree about what is
/// savable — and so no bytes are ever written under an extension this build
/// cannot encode (R11).
pub(crate) fn format_for_path(path: &Path) -> Result<codec::Format, String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    codec::Format::from_extension(extension).ok_or_else(|| {
        format!(
            "Saving as .{extension} is not supported by this build. Use Save As and choose \
             one of: {}.",
            codec::Format::ALL
                .iter()
                .map(|format| format.extension())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Distinguishes the temporary files of concurrent writes inside one process
/// (M1). The process id also in the name keeps two instances apart (R30).
static TEMP_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The temporary file one atomic write uses: a sibling of the real file,
/// named `.<name>.<pid>.<serial>.tmp`.
///
/// A process id alone is not enough: the save worker and a synchronous save of
/// the same path (the close prompt) run in the same process, and with one
/// shared name the two writes could interleave in the file that the rename
/// puts in place (M1). The serial is per call, so every write has its own
/// temporary file.
fn atomic_temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "image".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let serial = TEMP_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(
        ".{name}.{pid}.{serial}.tmp",
        pid = std::process::id()
    ))
}

/// Writes `bytes` to `path` through a temporary file in the same folder, then
/// renames it into place (SPEC section 9).
///
/// The temporary name is a sibling of the real file, unique per instance (the
/// process id) and per call (the serial), so it can never be mistaken for a
/// file the user has and no two writers can share it (R30, M1). The settings
/// file uses this too, so a half-written config cannot replace a good one.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = atomic_temp_path(path);
    std::fs::write(&temporary, bytes)
        .map_err(|err| format!("Could not write {}: {err}", temporary.display()))?;
    if let Err(err) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("Could not save {}: {err}", path.display()));
    }
    Ok(())
}

/// A sprite's texture and where it sits in the image, as the canvas needs it.
pub struct SpriteTexture<'a> {
    /// Top-left in image pixels.
    pub origin: egui::Pos2,
    pub texture: &'a TextureHandle,
}

/// One tab's textures: the base image plus one per annotation.
#[derive(Default)]
struct TextureCache {
    base: Option<CachedBase>,
    sprites: std::collections::HashMap<u64, CachedSprite>,
    /// The two layers effect sprites are made from: the base with the pasted
    /// images, and that with the blurs (see [`Tab::refresh_layers`]).
    layers: Option<CachedLayers>,
}

/// The two effect layers and what they were built from, so they can be reused
/// until something they depend on changes.
struct CachedLayers {
    /// The document's base image, compared by address.
    image: Arc<RgbaImage>,
    /// The Image annotations the first layer was built from, by value.
    images: Vec<Annotation>,
    /// The Blur annotations the second layer was built from, by value.
    blurs: Vec<Annotation>,
    /// The "annotations outside the image" mode the layers were built for:
    /// Expand makes them reach the pasted images (NEXT_STEPS step 12).
    expand: bool,
    /// The base with the pasted images.
    with_images: render::Layer,
    /// That with the blurs: what Highlight sprites read.
    blurred: render::Layer,
}

/// The pixels behind a texture, kept so it can be re-uploaded when the sampling
/// changes (SPEC 7.2).
type Pixels = ColorImage;

/// The base image texture, its pixels, and what they were made from.
struct CachedBase {
    texture: TextureHandle,
    pixels: Pixels,
    image: Arc<RgbaImage>,
    nearest: bool,
}

/// An annotation's texture, with what it was rasterized from and where it sits.
struct CachedSprite {
    texture: TextureHandle,
    pixels: Pixels,
    /// Top-left of the sprite in image pixels, from `bounds()`.
    origin: (i32, i32),
    /// The bounding box of the pixels really painted (alpha > 0), in image
    /// coordinates: the export rectangle follows this rather than `bounds()`
    /// (NEXT_STEPS step 11, J2 and J3).
    painted: Option<snp_ligase_core::geom::RectI>,
    source: Annotation,
    base: render::Layer,
    nearest: bool,
}

impl CachedSprite {
    /// True when this texture still matches what the annotation asks for.
    ///
    /// The layer is compared by address: a Blur or Highlight sprite is made
    /// from it, and two equal images still rasterize the same, but only the
    /// same buffer can be assumed to.
    fn matches(&self, annotation: &Annotation, base: &render::Layer) -> bool {
        self.source == *annotation && self.base.shares_pixels(base)
    }
}

/// The sampling for a zoom level: nearest at 200% and above so pixels stay
/// crisp, linear below that so they do not look ragged (SPEC section 7.2).
#[must_use]
pub fn sampling_options(zoom: f32) -> TextureOptions {
    if zoom >= NEAREST_FROM_ZOOM {
        TextureOptions::NEAREST
    } else {
        TextureOptions::LINEAR
    }
}

/// True when a zoom level wants nearest-neighbour sampling.
#[must_use]
pub fn wants_nearest(zoom: f32) -> bool {
    zoom >= NEAREST_FROM_ZOOM
}

/// What one sprite is drawn from, so the cache call does not grow a long
/// parameter list as the cache learns new things.
struct SpriteSource<'a> {
    /// The annotation as it is now.
    annotation: &'a Annotation,
    /// The layer its pixels come from (the base, the base with the pasted
    /// images for a Blur, or that with the blurs for a Highlight).
    base: &'a render::Layer,
    /// The sampling changes at 200% (SPEC 7.2).
    zoom: f32,
    /// True while the selection is being moved: only then may a translated
    /// sprite's pixels be reused (H4).
    moving: bool,
}

impl TextureCache {
    fn base(
        &mut self,
        ctx: &egui::Context,
        image: &Arc<RgbaImage>,
        zoom: f32,
    ) -> Option<&TextureHandle> {
        let loaded = self
            .base
            .as_ref()
            .is_some_and(|cached| Arc::ptr_eq(&cached.image, image));
        if !loaded {
            let pixels = color_image_from(image);
            let options = sampling_options(zoom);
            let texture = ctx.load_texture(
                format!("base:{:p}", Arc::as_ptr(image)),
                pixels.clone(),
                options,
            );
            self.base = Some(CachedBase {
                texture,
                pixels,
                image: Arc::clone(image),
                nearest: wants_nearest(zoom),
            });
        }
        self.base.as_ref().map(|cached| &cached.texture)
    }

    fn sprite(
        &mut self,
        ctx: &egui::Context,
        id: u64,
        source: SpriteSource<'_>,
        fonts: &mut Fonts,
    ) -> Option<SpriteTexture<'_>> {
        let SpriteSource {
            annotation,
            base,
            zoom,
            moving,
        } = source;
        let stale = self
            .sprites
            .get(&id)
            .is_none_or(|cached| !cached.matches(annotation, base));

        // A translation-only change keeps the pixels **while the move is in
        // progress** (SPEC 12 M7): the texture is reused and only the origin
        // moves. `translation_between` is the proof — it translates the cached
        // annotation and compares, so a change of shape can never slip through.
        // Effects are excluded there: their pixels come from the base image
        // *under* the rectangle.
        //
        // H4: the reuse ends with the gesture. A fractional offset drawn from
        // the cache is resampled and differs from what `flatten` writes, so
        // once `moving` is false a stale sprite is rasterized at its real
        // position again.
        let mut reuse = None;
        if stale
            && moving
            && let Some(cached) = self.sprites.get(&id)
            && let Some(offset) =
                snp_ligase_core::edit::translation_between(&cached.source, annotation)
        {
            reuse = Some(offset);
        }

        if stale && reuse.is_none() {
            let sprite: Sprite = render::rasterize_layer(annotation, base, fonts)?;
            let pixels = color_image_from_sprite(&sprite);
            let texture = ctx.load_texture(
                format!("annotation:{id}"),
                pixels.clone(),
                sampling_options(zoom),
            );
            tracing::debug!(
                id,
                width = sprite.pixmap.width(),
                height = sprite.pixmap.height(),
                "sprite rasterized"
            );
            self.sprites.insert(
                id,
                CachedSprite {
                    texture,
                    pixels,
                    origin: sprite.origin,
                    painted: render::painted_bounds(&sprite),
                    source: annotation.clone(),
                    base: base.clone(),
                    nearest: wants_nearest(zoom),
                },
            );
        }
        let cached = self.sprites.get(&id)?;
        let (dx, dy) = reuse.unwrap_or((0.0, 0.0));
        if reuse.is_some() {
            // `trace`, not `debug`: this fires for every frame of every drag.
            tracing::trace!(id, dx, dy, "sprite reused for a move");
        }
        Some(SpriteTexture {
            origin: egui::pos2(cached.origin.0 as f32 + dx, cached.origin.1 as f32 + dy),
            texture: &cached.texture,
        })
    }

    fn retain_sprites(&mut self, keep: impl Fn(u64) -> bool) {
        self.sprites.retain(|id, _| keep(*id));
    }

    /// The painted rectangle of one annotation's cached sprite, in image
    /// coordinates, or `None` when the cache cannot answer for it (NEXT_STEPS
    /// step 11, J3).
    ///
    /// `None` means the sprite is missing or no longer describes the
    /// annotation — it will be rasterized again during this frame's canvas
    /// draw, and the export rectangle catches up then.
    ///
    /// While an annotation is being **moved**, the cache reuses the cached
    /// pixels at an offset (H4); the painted rectangle follows the same offset
    /// so the export area grows with the drag instead of only after the
    /// release.
    fn painted_rect(
        &self,
        annotation: &Annotation,
        base: &Arc<RgbaImage>,
    ) -> Option<snp_ligase_core::geom::RectI> {
        let cached = self.sprites.get(&annotation.id)?;
        if !cached.base.is_buffer(base) {
            return None;
        }
        if cached.source == *annotation {
            return cached.painted;
        }
        let (dx, dy) = snp_ligase_core::edit::translation_between(&cached.source, annotation)?;
        cached
            .painted
            .map(|rect| rect.translated(dx.round() as i32, dy.round() as i32))
    }

    /// Re-uploads the textures whose sampling no longer matches the zoom.
    fn apply_sampling(&mut self, zoom: f32) {
        let nearest = wants_nearest(zoom);
        let options = sampling_options(zoom);
        if let Some(base) = &mut self.base
            && base.nearest != nearest
        {
            let pixels = base.pixels.clone();
            base.texture.set(pixels, options);
            base.nearest = nearest;
        }
        for sprite in self.sprites.values_mut() {
            if sprite.nearest != nearest {
                let pixels = sprite.pixels.clone();
                sprite.texture.set(pixels, options);
                sprite.nearest = nearest;
            }
        }
    }
}

/// Uploads a straight-alpha image. egui textures are premultiplied, so
/// `from_rgba_unmultiplied` is the conversion that keeps colours correct.
fn color_image_from(image: &Arc<RgbaImage>) -> ColorImage {
    ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    )
}

/// Uploads a sprite. Sprite pixmaps are already premultiplied, so the bytes go
/// across unchanged.
fn color_image_from_sprite(sprite: &Sprite) -> ColorImage {
    let width = sprite.pixmap.width() as usize;
    let height = sprite.pixmap.height() as usize;
    let (chunks, _) = sprite.pixmap.data().as_chunks::<4>();
    let pixels: Vec<Color32> = chunks
        .iter()
        .map(|pixel| Color32::from_rgba_premultiplied(pixel[0], pixel[1], pixel[2], pixel[3]))
        .collect();
    ColorImage::new([width, height], pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools;
    use snp_ligase_core::geom::{Point, Rect};
    use snp_ligase_core::model::{Kind, PathA, RectA, Stroke};

    fn tab() -> Tab {
        Tab::new(Document::new(RgbaImage::new(200, 100)), "test".to_owned())
    }

    /// Stands in for a drawing gesture: one undo snapshot, then the change.
    fn rectangle(tab: &mut Tab) -> u64 {
        tab.begin_edit();
        tab.document.push(Kind::Rect(RectA {
            rect: Rect::new(10.0, 10.0, 40.0, 30.0),
            stroke: Some(Stroke {
                color: [255, 0, 0, 255],
                width: 2.0,
            }),
            fill: None,
            corner_radius: 0.0,
        }))
    }

    fn line(tab: &mut Tab) -> u64 {
        tab.begin_edit();
        tab.document.push(Kind::Path(PathA {
            points: vec![Point::new(5.0, 5.0), Point::new(150.0, 90.0)],
            curved: false,
            stroke: Stroke {
                color: [0, 0, 255, 255],
                width: 3.0,
            },
            head_start: false,
            head_end: false,
        }))
    }

    #[test]
    fn a_new_tab_is_unsaved_until_it_has_a_file() {
        // A capture or a pasted image that has never been saved must not close
        // silently: SPEC 6.3 marks it unsaved (R22).
        let mut tab = tab();
        assert!(tab.dirty(), "a tab with no file is not unsaved");
        assert!(tab.title().ends_with(" *"));

        let path = std::env::temp_dir().join("snp-ligase-no-path-test.png");
        tab.save_to(
            &mut Fonts::bundled(),
            &path,
            &codec::EncodeOptions::default(),
            &FlattenOptions::CUT,
        )
        .expect("save should work");
        assert!(!tab.dirty(), "saving did not clear the unsaved mark");
        assert_eq!(tab.title(), "snp-ligase-no-path-test.png");
        let _ = std::fs::remove_file(&path);
    }

    /// NEXT_STEPS step 11, J3: the export rectangle comes from the sprite
    /// cache's **painted pixels**. The old code called `render::export_bounds`
    /// every frame, which laid out every Text annotation with cosmic-text; this
    /// path never touches the font system.
    #[test]
    fn the_export_rect_comes_from_the_cached_sprite_pixels() {
        let mut tab = tab();
        let text = tab.document.push(Kind::Text(snp_ligase_core::model::TextA {
            pos: Point::new(40.0, -64.0),
            text: "Hi".to_owned(),
            font_family: "Inter".to_owned(),
            size: 28.0,
            bold: true,
            italic: false,
            color: [255, 255, 255, 255],
            background: Some([0, 0, 255, 255]),
            padding: 6.0,
            outline: None,
        }));
        let outside = tab.document.push(Kind::Rect(RectA {
            rect: Rect::new(-30.0, 20.0, 20.0, 20.0),
            stroke: None,
            fill: Some([255, 0, 0, 255]),
            corner_radius: 0.0,
        }));

        // With nothing cached the export is the image: the canvas has not
        // drawn the annotations yet.
        assert_eq!(tab.export_rect(true), Rect::new(0.0, 0.0, 200.0, 100.0));

        // The canvas rasterizes both sprites.
        let ctx = egui::Context::default();
        let mut fonts = Fonts::bundled();
        tab.sprite_texture(&ctx, text, &mut fonts, 1.0, false, false)
            .expect("a text sprite");
        tab.sprite_texture(&ctx, outside, &mut fonts, 1.0, false, false)
            .expect("a rectangle sprite");

        // From now on the rectangle includes the painted pixels: the box top
        // at -64, not the two pixels of sprite slack the old `bounds()` added,
        // and the rectangle's 30 px overhang exactly.
        let expanded = tab.export_rect(true);
        assert_eq!(
            expanded.x, -30.0,
            "the outside rectangle did not grow the export"
        );
        assert_eq!(
            expanded.y, -64.0,
            "the export should start at the painted box top"
        );
        assert_eq!((expanded.right(), expanded.bottom()), (200.0, 100.0));

        // "Cut" is still the image itself.
        assert_eq!(tab.export_rect(false), Rect::new(0.0, 0.0, 200.0, 100.0));
    }

    /// NEXT_STEPS step 12: the canvas makes an effect sprite from the cached
    /// layer, and the export composites the same pixels. A Blur over a pasted
    /// image outside the base is the case step 11 got wrong, so this pins the
    /// two paths together there.
    #[test]
    fn the_cached_effect_layer_and_the_export_agree_outside_the_base() {
        let mut tab = tab();
        let pasted: RgbaImage = RgbaImage::from_fn(60, 40, |x, _y| {
            if (x / 6) % 2 == 0 {
                image::Rgba([0, 0, 0, 255])
            } else {
                image::Rgba([255, 255, 255, 255])
            }
        });
        tab.document
            .push(Kind::Image(snp_ligase_core::model::ImageA {
                pos: Point::new(220.0, 20.0),
                image: Arc::new(pasted),
            }));
        let blur = tab.document.push(Kind::Blur(snp_ligase_core::model::BlurA {
            rect: Rect::new(220.0, 20.0, 60.0, 40.0),
            radius: 8.0,
        }));

        let mut fonts = Fonts::bundled();
        let options = FlattenOptions {
            expand: true,
            fill: Some([255, 255, 255, 255]),
        };

        // The canvas path: the cached layer, rasterized against it.
        let layer = tab.images_base(&mut fonts, options.expand);
        let annotation = tab.document.find(blur).expect("the blur").clone();
        let sprite = render::rasterize_layer(&annotation, &layer, &mut fonts)
            .expect("a blur sprite to the right of the base");

        // The export path, from the same tab.
        let saved = tab.flattened(&mut fonts, &options);

        assert_eq!(sprite.origin, (220, 20));
        for row in 0..sprite.pixmap.height() {
            for column in 0..sprite.pixmap.width() {
                let at = (row as usize * sprite.pixmap.width() as usize + column as usize) * 4;
                let pixel = &sprite.pixmap.data()[at..at + 4];
                assert_eq!(pixel[3], 255, "an effect sprite is opaque");
                let (x, y) = (220 + column, 20 + row);
                assert_eq!(
                    &saved.get_pixel(x, y).0[..3],
                    &pixel[..3],
                    "the canvas and the export disagree at ({x}, {y})"
                );
            }
        }
    }

    /// J3's measurement: what the export rectangle costs per frame with 20
    /// text annotations, before and after. Ignored by default; run with
    /// `cargo test -p snp-ligase-app --bin snp-ligase measure_export_rect -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not an assertion; run it by hand"]
    fn measure_export_rect_per_frame() {
        let mut tab = tab();
        let mut fonts = Fonts::bundled();
        for index in 0..20 {
            tab.document.push(Kind::Text(snp_ligase_core::model::TextA {
                pos: Point::new(
                    (index % 4) as f32 * 40.0 + 5.0,
                    (index / 4) as f32 * 24.0 - 80.0,
                ),
                text: format!("Label {index}"),
                font_family: "Inter".to_owned(),
                size: 18.0,
                bold: false,
                italic: false,
                color: [0, 0, 0, 255],
                background: Some([255, 255, 0, 255]),
                padding: 4.0,
                outline: None,
            }));
        }

        // The new path needs the sprites cached, which the canvas does.
        let ctx = egui::Context::default();
        let ids: Vec<u64> = tab
            .document
            .annotations
            .iter()
            .map(|annotation| annotation.id)
            .collect();
        for id in ids {
            tab.sprite_texture(&ctx, id, &mut fonts, 1.0, false, false)
                .expect("a sprite");
        }

        let iterations = 200u32;
        // The old per-frame work: `bounds()` for every annotation, which lays
        // each Text out with cosmic-text (this was `export_bounds`).
        let old_started = std::time::Instant::now();
        for _ in 0..iterations {
            for annotation in &tab.document.annotations {
                let _ = render::bounds(annotation, &mut fonts);
            }
        }
        let old = old_started.elapsed() / iterations;

        // The new one: painted rectangles out of the sprite cache.
        let new_started = std::time::Instant::now();
        for _ in 0..iterations {
            let _ = tab.export_rect(true);
        }
        let new = new_started.elapsed() / iterations;

        println!(
            "export rect with 20 text annotations: old (bounds + text layout) = {old:?}, \
             new (sprite cache) = {new:?}"
        );
    }

    #[test]
    fn editing_marks_the_tab_unsaved_and_saving_clears_it() {
        let mut tab = tab();
        rectangle(&mut tab);
        assert!(tab.dirty(), "adding an annotation did not dirty the tab");
        assert!(tab.title().ends_with(" *"));

        let path = std::env::temp_dir().join("snp-ligase-tab-test.png");
        tab.save_to(
            &mut Fonts::bundled(),
            &path,
            &codec::EncodeOptions::default(),
            &FlattenOptions::CUT,
        )
        .expect("save should work");
        assert!(!tab.dirty(), "saving did not clear the dirty mark");
        assert_eq!(tab.title(), "snp-ligase-tab-test.png");
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_opened_file_starts_clean() {
        let path = std::env::temp_dir().join("snp-ligase-opened-test.png");
        std::fs::write(&path, b"not really a png").expect("write should work");
        let document = Document::new(RgbaImage::new(10, 10));
        let tab = Tab::from_path(document, path.clone());
        assert!(!tab.dirty(), "a just-opened file has no unsaved changes");
        assert_eq!(tab.title(), "snp-ligase-opened-test.png");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn undo_and_redo_walk_the_document_back_and_forth() {
        let mut tab = tab();
        let before = tab.document.clone();
        rectangle(&mut tab);
        let after = tab.document.clone();

        assert!(tab.undo());
        assert_eq!(tab.document, before);
        assert!(tab.redo());
        assert_eq!(tab.document, after);
        assert!(!tab.can_redo(), "redo was not used up");
    }

    #[test]
    fn a_new_edit_clears_the_redo_stack() {
        let mut tab = tab();
        rectangle(&mut tab);
        tab.undo();
        assert!(tab.can_redo());
        line(&mut tab);
        assert!(!tab.can_redo(), "redo survived a new edit");
    }

    #[test]
    fn the_undo_stack_is_capped_at_one_hundred() {
        let mut tab = tab();
        for index in 0..150 {
            tab.begin_edit();
            line(&mut tab);
            let _ = index;
        }
        let mut steps = 0;
        while tab.undo() {
            steps += 1;
        }
        assert_eq!(steps, UNDO_LIMIT, "the cap is not {UNDO_LIMIT}");
    }

    #[test]
    fn undoing_a_deletion_fixes_the_selection() {
        let mut tab = tab();
        let id = rectangle(&mut tab);
        tab.select_only(Some(id));
        tab.begin_edit();
        tab.document.annotations.clear();
        tab.select_only(None);

        tab.undo();
        assert!(
            tab.selection().is_empty(),
            "the selection should stay empty"
        );
        assert_eq!(tab.document.annotations.len(), 1);
    }

    #[test]
    fn saving_writes_png_bytes() {
        let mut tab = tab();
        rectangle(&mut tab);
        let path = std::env::temp_dir().join("snp-ligase-bytes-test.png");
        tab.save_to(
            &mut Fonts::bundled(),
            &path,
            &codec::EncodeOptions::default(),
            &FlattenOptions::CUT,
        )
        .expect("save should work");
        let bytes = std::fs::read(&path).expect("read should work");
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "not a PNG"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn saving_a_jpg_name_writes_jpeg_bytes() {
        // R11 was lifted in M6: Ctrl+S on an opened JPEG used to refuse,
        // because the app had no JPEG encoder. It has one now, and the bytes
        // written are JPEG bytes — never PNG bytes under a .jpg name.
        let path = std::env::temp_dir().join("snp-ligase-jpeg-test.jpg");
        std::fs::write(&path, b"the original jpeg").expect("write should work");

        let mut tab = Tab::from_path(Document::new(RgbaImage::new(10, 10)), path.clone());
        rectangle(&mut tab);

        tab.save_to(
            &mut Fonts::bundled(),
            &path,
            &codec::EncodeOptions::default(),
            &FlattenOptions::CUT,
        )
        .expect("a .jpg save must work now");
        let bytes = std::fs::read(&path).expect("read should work");
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "not JPEG bytes");
        assert!(!tab.dirty(), "the save did not clear the unsaved mark");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_extension_this_build_cannot_write_is_still_refused() {
        // BMP and GIF decode but are not written (SPEC section 9's table), so
        // the R11 rule still holds for them: no PNG bytes under a .gif name.
        let path = std::env::temp_dir().join("snp-ligase-unsupported-test.gif");
        std::fs::write(&path, b"the original gif").expect("write should work");

        let mut tab = Tab::from_path(Document::new(RgbaImage::new(10, 10)), path.clone());
        rectangle(&mut tab);

        let error = tab
            .save_to(
                &mut Fonts::bundled(),
                &path,
                &codec::EncodeOptions::default(),
                &FlattenOptions::CUT,
            )
            .expect_err("a .gif save must be refused");
        assert!(error.contains(".gif"), "{error}");
        assert!(
            error.contains("png"),
            "the message should list what works: {error}"
        );
        let bytes = std::fs::read(&path).expect("read should work");
        assert_eq!(bytes, b"the original gif", "the file was overwritten");
        assert!(tab.dirty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_failed_save_leaves_the_old_file_alone() {
        // R29: the old version saved into a missing directory and never
        // touched a real file. The failure this test now forces is the one the
        // atomic write exists for: the bytes are written to a temporary file,
        // and the rename onto the target fails. The old file must survive it,
        // and no temporary file may be left behind.
        let path = std::env::temp_dir().join("snp-ligase-failed-save.png");
        std::fs::write(&path, b"the old bytes").expect("write should work");

        // A directory where the file wants to be makes the rename fail.
        let blocker = path.with_file_name("snp-ligase-failed-save-dir");
        let _ = std::fs::remove_dir_all(&blocker);
        std::fs::create_dir(&blocker).expect("create_dir should work");

        let mut tab = tab();
        rectangle(&mut tab);
        let error = tab
            .save_to(
                &mut Fonts::bundled(),
                &blocker,
                &codec::EncodeOptions::default(),
                &FlattenOptions::CUT,
            )
            .expect_err("a directory save must fail");
        assert!(!error.is_empty(), "the failure must say something");

        let bytes = std::fs::read(&path).expect("read should work");
        assert_eq!(bytes, b"the old bytes", "the old file was overwritten");

        // The temporary file was cleaned up: the only thing in the temp folder
        // starting with the blocker's name is the directory itself.
        let leftovers: Vec<String> = std::fs::read_dir(std::env::temp_dir())
            .expect("read_dir should work")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".snp-ligase-failed-save-dir"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files left behind: {leftovers:?}"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&blocker);
    }

    #[test]
    fn every_atomic_write_uses_its_own_temporary_file() {
        // M1: the save worker and a synchronous save (the close prompt) can
        // write the same path at the same time; one shared temporary name let
        // the two renames race. Every call gets a different name now.
        let path = std::env::temp_dir().join("snp-ligase-temp-name-test.png");
        let first = atomic_temp_path(&path);
        let second = atomic_temp_path(&path);

        assert_ne!(
            first, second,
            "two atomic writes picked the same temporary file"
        );
        for temporary in [&first, &second] {
            assert_ne!(*temporary, path, "a write would clobber the real file");
            assert_eq!(
                temporary.parent(),
                path.parent(),
                "the temporary file must be a sibling, so the rename cannot cross volumes"
            );
        }
    }

    #[test]
    fn two_threads_writing_the_same_path_both_produce_a_valid_file() {
        // M1: the race this guards against is two writers in one process; the
        // rename must put a whole write in place, never a mix of the two.
        let path =
            std::env::temp_dir().join(format!("snp-ligase-atomic-race-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let first = vec![0xA5u8; 2 * 1024 * 1024];
        let second = vec![0x5Au8; 1024 * 1024];
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let writers: Vec<std::thread::JoinHandle<Result<(), String>>> =
            [first.clone(), second.clone()]
                .into_iter()
                .map(|bytes| {
                    let path = path.clone();
                    let barrier = std::sync::Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        write_atomically(&path, &bytes)
                    })
                })
                .collect();
        for writer in writers {
            writer
                .join()
                .expect("a writer thread panicked")
                .expect("a write failed");
        }

        let written = std::fs::read(&path).expect("no file was written");
        assert!(
            written == first || written == second,
            "the file mixed the two writes: {} bytes",
            written.len()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_press_that_changes_nothing_costs_no_undo_step() {
        // R20: the old code pushed a snapshot on every press, so a plain click
        // to select gave the user an undo step that did nothing.
        let mut tab = tab();
        rectangle(&mut tab);
        tab.commit_edit();
        let steps = tab.undo_depth_for_test();

        tab.stage_edit();
        tab.select_only(Some(tab.document.annotations[0].id));
        tab.commit_edit();

        assert_eq!(tab.undo_depth_for_test(), steps, "a click pushed a step");
    }

    #[test]
    fn one_drag_is_one_undo_step() {
        let mut tab = tab();
        let id = rectangle(&mut tab);
        tab.select_only(Some(id));
        tab.commit_edit();
        let steps = tab.undo_depth_for_test();

        tab.stage_edit();
        for _ in 0..5 {
            tools::select::translate_selection(&mut tab, 1.0, 0.0);
        }
        tab.commit_edit();

        assert_eq!(tab.undo_depth_for_test(), steps + 1, "not one step");
        assert!(tab.undo());
        assert_eq!(tab.document.annotations.len(), 1);
    }

    #[test]
    fn cancelling_an_edit_restores_the_document_it_started_from() {
        // R21: Esc during a move used to leave the shape where it had been
        // dragged to, with the snapshot still on the undo stack.
        let mut tab = tab();
        let id = rectangle(&mut tab);
        tab.select_only(Some(id));
        tab.commit_edit();
        let before = tab.document.clone();
        let steps = tab.undo_depth_for_test();

        tab.stage_edit();
        tools::select::translate_selection(&mut tab, 40.0, 30.0);
        assert!(tab.cancel_edit(), "there was nothing to restore");
        assert_eq!(tab.document, before, "the move was not cancelled");
        assert!(!tab.edit_in_flight());
        assert_eq!(
            tab.undo_depth_for_test(),
            steps,
            "the cancelled edit left an undo step"
        );
    }

    /// SPEC 11.7: a sequence of edits followed by the same number of undos
    /// restores an equal `Document`.
    #[test]
    fn a_sequence_of_edits_undoes_back_to_the_start() {
        let mut tab = tab();
        let start = tab.document.clone();

        // The kind of session SPEC's M3 human checks describe: draw, select,
        // draw, reorder, delete.
        rectangle(&mut tab);
        line(&mut tab);
        line(&mut tab);
        // One annotation, not all of them: "Bring to Front" with everything
        // selected is correctly a no-op (R20) and would not be an edit.
        tab.select_only(Some(tab.document.annotations[0].id));
        tools::select::reorder(&mut tab, true);
        tools::select::delete_selection(&mut tab);
        let steps = 5;

        for _ in 0..steps {
            assert!(tab.undo(), "there was nothing left to undo");
        }
        assert_eq!(
            tab.document, start,
            "undoing every step did not restore the document"
        );
        assert!(
            !tab.can_undo(),
            "there are more steps than there were edits"
        );

        // The same number of redos gets back to where it was.
        for _ in 0..steps {
            assert!(tab.redo(), "there was nothing left to redo");
        }
    }

    #[test]
    fn redoing_a_sequence_replays_it() {
        let mut tab = tab();
        let start = tab.document.clone();
        rectangle(&mut tab);
        line(&mut tab);
        let after = tab.document.clone();

        assert!(tab.undo());
        assert!(tab.undo());
        assert_eq!(tab.document, start);
        assert!(tab.redo());
        assert!(tab.redo());
        assert_eq!(tab.document, after, "redo did not replay both steps");
    }

    /// SPEC M3 acceptance: opening a PNG, adding annotations, saving, and
    /// reopening shows the annotations baked in.
    ///
    /// This is the whole round trip through the file format: decode a real PNG,
    /// draw on it, write it, read it back, and look at the pixels.
    #[test]
    fn saving_bakes_the_annotations_into_the_file() {
        // A small PNG written to disk and read back through the real decoder.
        let source = std::env::temp_dir().join("snp-ligase-round-trip-source.png");
        let saved = std::env::temp_dir().join("snp-ligase-round-trip-saved.png");

        let picture = RgbaImage::from_pixel(64, 48, image::Rgba([0, 0, 255, 255]));
        std::fs::write(
            &source,
            codec::encode(
                &picture,
                codec::Format::Png,
                &codec::EncodeOptions::default(),
            )
            .expect("encode"),
        )
        .expect("write");

        let bytes = std::fs::read(&source).expect("read");
        let decoded = codec::decode(&bytes).expect("decode");
        assert_eq!(decoded.dimensions(), (64, 48), "wrong size from the file");
        let mut tab = Tab::from_path(Document::new(decoded), source.clone());

        // Draw a filled red rectangle over the middle of it.
        let id = tab.document.push(Kind::Rect(RectA {
            rect: Rect::new(16.0, 12.0, 32.0, 24.0),
            stroke: None,
            fill: Some([255, 0, 0, 255]),
            corner_radius: 0.0,
        }));
        tab.select_only(Some(id));
        assert!(tab.dirty(), "drawing did not mark the tab unsaved");

        tab.save_to(
            &mut Fonts::bundled(),
            &saved,
            &codec::EncodeOptions::default(),
            &FlattenOptions::CUT,
        )
        .expect("save");
        assert!(!tab.dirty(), "saving did not clear the unsaved mark");

        // Reopen it: the annotation is gone, but its pixels are in the file.
        let reopened = codec::decode(&std::fs::read(&saved).expect("read")).expect("decode");
        let centre = reopened.get_pixel(32, 24);
        assert_eq!(
            centre.0,
            [255, 0, 0, 255],
            "the rectangle was not baked into the saved file"
        );
        // And the blue that was not covered is still blue.
        assert_eq!(
            reopened.get_pixel(2, 2).0,
            [0, 0, 255, 255],
            "the untouched part of the image changed"
        );

        // A reopened tab holds the same picture, with no annotations left to
        // edit and nothing to undo.
        let reopened_tab = Tab::from_path(Document::new(reopened), saved.clone());
        assert!(
            reopened_tab.document.annotations.is_empty(),
            "the saved file still carried its annotations"
        );
        assert!(
            !reopened_tab.dirty(),
            "a just-opened file counts as unsaved?"
        );

        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_file(&saved);
    }

    /// The sprite path from an annotation to the pixels the GPU is handed.
    ///
    /// This is the step the canvas got wrong once: a sprite that rasterizes
    /// correctly must still have its pixels in the `ColorImage` that goes to
    /// egui, at the sprite's exact size, or nothing shows on screen.
    #[test]
    fn big_sprites_survive_the_upload_with_their_pixels() {
        let mut tab = tab();
        // A long arrow and a big rectangle: the shapes a real drawing session
        // produces, and both bigger than a small test sprite.
        let arrow = tab.document.push(Kind::Path(PathA {
            points: vec![Point::new(1109.0, 250.0), Point::new(2342.0, 489.0)],
            curved: true,
            stroke: Stroke {
                color: [232, 17, 35, 255],
                width: 4.0,
            },
            head_start: false,
            head_end: true,
        }));
        let big = tab.document.push(Kind::Rect(RectA {
            rect: Rect::new(1109.0, 695.0, 1040.0, 540.0),
            stroke: Some(Stroke {
                color: [232, 17, 35, 255],
                width: 4.0,
            }),
            fill: None,
            corner_radius: 0.0,
        }));

        let mut fonts = Fonts::bundled();
        for id in [arrow, big] {
            let annotation = tab.document.find(id).expect("there").clone();
            let sprite =
                snp_ligase_core::render::rasterize(&annotation, &tab.document.base, &mut fonts)
                    .unwrap_or_else(|| panic!("{id} did not rasterize"));

            // A pixmap is tightly packed: width * height * 4 bytes and no more.
            let raw = sprite.pixmap.data();
            let expected = sprite.pixmap.width() as usize * sprite.pixmap.height() as usize * 4;
            assert_eq!(raw.len(), expected, "{id}: the pixmap has a stride");

            let opaque = raw.as_chunks::<4>().0.iter().filter(|p| p[3] > 200).count();
            assert!(opaque > 100, "{id}: the sprite has no visible pixels");

            let image = color_image_from_sprite(&sprite);
            assert_eq!(
                [image.width(), image.height()],
                [
                    sprite.pixmap.width() as usize,
                    sprite.pixmap.height() as usize
                ],
                "{id}: the upload is the wrong size"
            );
            let visible = image
                .as_raw()
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[3] > 200)
                .count();
            assert_eq!(visible, opaque, "{id}: the upload lost pixels");
        }
    }
    #[test]
    fn a_rasterized_sprite_reaches_the_gpu_with_pixels() {
        let mut tab = tab();
        let id = rectangle(&mut tab);
        let mut fonts = Fonts::bundled();
        let sprite = snp_ligase_core::render::rasterize(
            &tab.document.find(id).expect("there").clone(),
            &tab.document.base,
            &mut fonts,
        )
        .expect("a rectangle must rasterize");

        let raw = sprite.pixmap.data();
        let opaque = raw.as_chunks::<4>().0.iter().filter(|p| p[3] > 200).count();
        println!(
            "sprite {}x{} opaque px = {opaque}",
            sprite.pixmap.width(),
            sprite.pixmap.height()
        );
        assert!(opaque > 100, "the rasterized sprite has no visible pixels");

        let image = color_image_from_sprite(&sprite);
        let visible = image
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] > 200)
            .count();

        assert!(
            visible > 100,
            "the ColorImage that goes to the GPU is empty"
        );
        assert_eq!(image.width(), sprite.pixmap.width() as usize);
        assert_eq!(image.height(), sprite.pixmap.height() as usize);
    }

    #[test]
    fn sampling_switches_at_two_hundred_percent() {
        assert!(!wants_nearest(1.0));
        assert!(!wants_nearest(1.99));
        assert!(wants_nearest(2.0));
        assert!(wants_nearest(16.0));
    }

    /// SPEC 12 M7: while an annotation is dragged, its sprite must not be
    /// rasterized and re-uploaded every frame. A move reuses the texture and
    /// shifts the origin; a change of shape rasterizes again.
    ///
    /// H4 adds the other half: the reuse lasts exactly as long as the move, so
    /// the first frame after the gesture shows pixels rasterized at the real
    /// position.
    #[test]
    fn moving_an_annotation_reuses_its_sprite_while_the_move_lasts() {
        let mut tab = tab();
        let ctx = egui::Context::default();
        let mut fonts = Fonts::bundled();
        let id = rectangle(&mut tab);
        tab.select_only(Some(id));

        let (first_texture, first_origin) = {
            let first = tab
                .sprite_texture(&ctx, id, &mut fonts, 1.0, false, false)
                .expect("a sprite");
            (first.texture.id(), first.origin)
        };

        tools::select::translate_selection(&mut tab, 7.0, 5.0);
        let moved = tab
            .sprite_texture(&ctx, id, &mut fonts, 1.0, true, false)
            .expect("a sprite");
        assert_eq!(
            moved.texture.id(),
            first_texture,
            "the sprite was rasterized again for a plain move"
        );
        assert_eq!(
            moved.origin,
            first_origin + egui::vec2(7.0, 5.0),
            "the origin did not follow the move"
        );

        // H4: the move is over — the stale sprite is rasterized again, at the
        // position the document now has.
        let (after_texture, after_origin) = {
            let after = tab
                .sprite_texture(&ctx, id, &mut fonts, 1.0, false, false)
                .expect("a sprite");
            (after.texture.id(), after.origin)
        };
        assert_ne!(
            after_texture, first_texture,
            "the reused pixels outlived the move"
        );
        assert_eq!(after_origin, first_origin + egui::vec2(7.0, 5.0));

        // A resize is not a move: the pixels really changed.
        if let Some(annotation) = tab.document.find_mut(id)
            && let Kind::Rect(rect) = &mut annotation.kind
        {
            rect.rect.w += 20.0;
        }
        let resized = tab
            .sprite_texture(&ctx, id, &mut fonts, 1.0, false, false)
            .expect("a sprite");
        assert_ne!(
            resized.texture.id(),
            after_texture,
            "a resized annotation kept the old pixels"
        );

        // An effect is never reused, even while moving: its pixels come from
        // the base image under the rectangle.
        let blur = tab.document.push(Kind::Blur(snp_ligase_core::model::BlurA {
            rect: Rect::new(20.0, 20.0, 40.0, 30.0),
            radius: 6.0,
        }));
        tab.select_only(Some(blur));
        let first_blur = tab
            .sprite_texture(&ctx, blur, &mut fonts, 1.0, false, false)
            .expect("a sprite")
            .texture
            .id();
        tools::select::translate_selection(&mut tab, 3.0, 3.0);
        let moved_blur = tab
            .sprite_texture(&ctx, blur, &mut fonts, 1.0, true, false)
            .expect("a sprite")
            .texture
            .id();
        assert_ne!(
            moved_blur, first_blur,
            "a moved blur kept pixels that belong under its old rectangle"
        );
    }

    /// H4: a move by a fraction of a pixel — any zoom other than 100% — is the
    /// case the reuse must not outlive. The cached pixels shifted by a fraction
    /// are resampled on screen and differ from what `flatten` writes, so the
    /// frame after the gesture must rasterize them again (the origin, which is
    /// the model's own position, is the same either way).
    #[test]
    fn a_fractional_move_is_rasterized_again_once_the_move_ends() {
        let mut tab = tab();
        let ctx = egui::Context::default();
        let mut fonts = Fonts::bundled();
        let id = rectangle(&mut tab);
        tab.select_only(Some(id));

        let first_texture = tab
            .sprite_texture(&ctx, id, &mut fonts, 1.0, false, false)
            .expect("a sprite")
            .texture
            .id();

        // H4: a move by a fraction of a pixel — any zoom other than 100% — is
        // the case the reuse must not outlive (see the test comment).
        tools::select::translate_selection(&mut tab, 7.5, 5.5);
        let during_origin = {
            let during = tab
                .sprite_texture(&ctx, id, &mut fonts, 1.0, true, false)
                .expect("a sprite");
            assert_eq!(
                during.texture.id(),
                first_texture,
                "a drag must reuse the sprite"
            );
            during.origin
        };

        let after = tab
            .sprite_texture(&ctx, id, &mut fonts, 1.0, false, false)
            .expect("a sprite");
        assert_ne!(
            after.texture.id(),
            first_texture,
            "the fractionally shifted pixels stayed on screen"
        );
        // The rasterized sprite sits at the annotation's own position; the
        // reused one was the cached origin plus the fractional offset, so the
        // two agree to within the sub-pixel the cache was approximating.
        assert!(
            (after.origin - during_origin).length() <= 1.0,
            "the two frames disagree about where the annotation is: {:?} vs {during_origin:?}",
            after.origin
        );
    }
}
