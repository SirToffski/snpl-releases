//! Highlight and Blur: the two annotations whose pixels come from the base
//! image rather than from vector drawing (SPEC section 4.2).
//!
//! There is nothing to draw here on purpose. Both effects are computed by
//! [`crate::effects`] from the base image and handed to the compositor as an
//! opaque sprite, which is what lets `flatten` use one source-over pass for
//! everything and keeps an effect from hiding another annotation.
//!
//! The tests that pin that behaviour down at the render level (SPEC section
//! 11.1 — a highlight over white is the highlight colour, over black is black,
//! and a blurred rectangle is soft inside its edges and sharp outside them)
//! live with the functions they test: `crate::effects` and
//! [`crate::render::tests`]. This module used to re-export `effects::blur` and
//! `effects::highlight` as `blur`/`highlight`; nothing used the wrappers, so
//! they are gone (R29).
