//! Saving off the UI thread (H6).
//!
//! The 4K encode timings measured on this machine (release build, a
//! capture-like 3840×2160 image) are PNG 7 ms, WebP 12 ms, JPEG XL lossless
//! 25 ms, JPEG 113 ms — and **AVIF ~3.0 s**. A save on the UI thread would
//! freeze the window for seconds, so the encode and the atomic write run here,
//! on a worker thread, and the UI is told when they are done.
//!
//! `flatten` stays on the UI thread: it needs the app's one [`Fonts`], which is
//! shared with the canvas and does not travel. What crosses to the worker is
//! the flattened image, the path, the format and the options.
//!
//! [`Fonts`]: snp_ligase_core::text::Fonts

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use snp_ligase_core::codec::{self, EncodeOptions};
use snp_ligase_core::model::Document;

use crate::controller::WindowGate;

/// One save the UI has asked for.
pub struct SaveJob {
    /// The tab the image came from. Tabs have a stable id, because the result
    /// has to find its tab after the user switched to another one.
    pub tab: u64,
    /// Where the bytes go.
    pub path: PathBuf,
    /// Exactly what was flattened. A document that changes while the save runs
    /// must not be reported as saved, so this is what the tab records as its
    /// saved state when the result arrives.
    pub document: Document,
    /// The flattened image.
    pub image: image::RgbaImage,
    /// The format, from the path's extension.
    pub format: codec::Format,
    /// The per-format quality settings (SPEC section 9's Options).
    pub options: EncodeOptions,
    /// True for SPEC section 10's auto-save, so the status bar can keep
    /// saying "Auto-saved".
    pub auto: bool,
}

/// What the worker says about a job.
pub struct SaveOutcome {
    /// The tab that asked for the save.
    pub tab: u64,
    /// Where the bytes were written.
    pub path: PathBuf,
    /// The document the pixels came from, for [`crate::tab::Tab::note_saved`].
    pub document: Document,
    /// True for an auto-save (see [`SaveJob::auto`]).
    pub auto: bool,
    /// The file size and timings, or a message for the status bar.
    pub result: Result<SaveStats, String>,
}

/// How long the two halves of a save took and how big the file is.
#[derive(Debug, Clone, Copy)]
pub struct SaveStats {
    /// The size of the file that was written.
    pub bytes: u64,
    /// `codec::encode`, the half that can take seconds (AVIF).
    pub encode_ms: f64,
    /// The atomic write (temp file + rename).
    pub write_ms: f64,
}

/// The worker thread and the UI's ends of its two channels.
pub struct SaveWorker {
    jobs: Sender<SaveJob>,
    outcomes: Receiver<SaveOutcome>,
    /// Test support: a sender to the same channel, so a test can inject a
    /// result the worker would have produced.
    #[cfg(test)]
    outcomes_tx: Sender<SaveOutcome>,
}

impl SaveWorker {
    /// Starts the worker thread.
    ///
    /// It ends when the UI end is dropped, which is what happens when the
    /// window is closed for good.
    ///
    /// `wake` is the UI's window gate: every finished save asks for a repaint,
    /// because the window may be hidden (an auto-save after a capture runs
    /// while the UI is doing something else) and no frame would run otherwise,
    /// leaving the status bar saying "Saving…" until the next input event.
    #[must_use]
    pub fn spawn(wake: std::sync::Arc<WindowGate>) -> Self {
        let (job_sender, job_receiver) = std::sync::mpsc::channel();
        let (outcome_sender, outcome_receiver) = std::sync::mpsc::channel();
        // The clone has to happen before the sender moves into the thread.
        #[cfg(test)]
        let outcome_tx = outcome_sender.clone();

        let spawned = std::thread::Builder::new()
            .name("save-worker".to_owned())
            .spawn(move || run_worker(job_receiver, outcome_sender, wake));
        if let Err(err) = spawned {
            // Saving is dead from here on; the UI says so when the send fails.
            tracing::error!(error = %err, "could not start the save worker thread");
        }

        Self {
            jobs: job_sender,
            outcomes: outcome_receiver,
            #[cfg(test)]
            outcomes_tx: outcome_tx,
        }
    }

    /// Hands the worker a save. A closed worker is reported, so the user is
    /// not left believing an image was written.
    pub fn submit(&self, job: SaveJob) {
        tracing::info!(
            tab = job.tab,
            path = %job.path.display(),
            format = job.format.label(),
            auto = job.auto,
            "save handed to the save worker"
        );
        if self.jobs.send(job).is_err() {
            tracing::error!("the save worker is gone; the save was dropped");
        }
    }

    /// Everything the worker has finished since the last frame.
    #[must_use]
    pub fn take_outcomes(&self) -> Vec<SaveOutcome> {
        self.outcomes.try_iter().collect()
    }

    /// Injects an outcome as if the worker had sent it.
    ///
    /// Test support for the branches a real encode cannot be made to take —
    /// a missing tab, a failed write — without racing the real thread.
    #[cfg(test)]
    pub fn inject_outcome(&self, outcome: SaveOutcome) {
        if self.outcomes_tx.send(outcome).is_err() {
            tracing::warn!("an injected test outcome was dropped");
        }
    }
}

/// The worker's loop: one save at a time, in order, so the numbering of
/// auto-save names cannot be overtaken by a later job.
fn run_worker(
    jobs: Receiver<SaveJob>,
    outcomes: Sender<SaveOutcome>,
    wake: std::sync::Arc<WindowGate>,
) {
    while let Ok(job) = jobs.recv() {
        let outcome = run_job(&job);
        // A repaint before the send: if the UI is mid-frame it will pick the
        // outcome up next frame either way.
        wake.wake();
        if outcomes.send(outcome).is_err() {
            break;
        }
    }
    tracing::debug!("the save worker thread is done");
}

/// Encodes one job and writes it, with SPEC section 9's atomic-write rule.
fn run_job(job: &SaveJob) -> SaveOutcome {
    let encode_started = std::time::Instant::now();
    let encoded = codec::encode(&job.image, job.format, &job.options)
        .map_err(|err| format!("Could not encode the image: {err}"));
    let encode_ms = encode_started.elapsed().as_secs_f64() * 1000.0;

    let mut write_ms = 0.0;
    let result = match encoded {
        Ok(bytes) => {
            let write_started = std::time::Instant::now();
            let written = crate::tab::write_atomically(&job.path, &bytes);
            write_ms = write_started.elapsed().as_secs_f64() * 1000.0;
            match written {
                Ok(()) => Ok(SaveStats {
                    bytes: bytes.len() as u64,
                    encode_ms,
                    write_ms,
                }),
                Err(message) => Err(message),
            }
        }
        Err(message) => Err(message),
    };

    // The same fields as the synchronous path in `Tab::save_to`, so a log from
    // either route reads the same way.
    match &result {
        Ok(stats) => tracing::info!(
            path = %job.path.display(),
            format = job.format.label(),
            width = job.image.width(),
            height = job.image.height(),
            bytes = stats.bytes,
            encode_ms,
            write_ms,
            "image saved"
        ),
        Err(message) => tracing::warn!(
            path = %job.path.display(),
            format = job.format.label(),
            width = job.image.width(),
            height = job.image.height(),
            encode_ms,
            error = %message,
            "the save failed"
        ),
    }

    SaveOutcome {
        tab: job.tab,
        path: job.path.clone(),
        document: job.document.clone(),
        auto: job.auto,
        result,
    }
}
