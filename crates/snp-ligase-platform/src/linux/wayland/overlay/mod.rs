//! The Wayland overlay child (LINUX_SUPPORT.md section 6, L3b).
//!
//! The X11 overlay's *parent* machinery is shared — the run directory, the
//! layout line, the answer line on stdout are the protocol both children
//! speak — so the Wayland side is only the child's single monitor windows:
//! one [`winit`]-window per output, matched by **output name**
//! (`MonitorHandle::name()` against the layout's `xdg_output` name, never
//! by position, which dishonest under mixed scale), painted by
//! [`paint`] in the window's own physical pixels, and mapped in and out of
//! virtual space by [`map`].
//!
//! Everything the user decides still goes through the shared pure state
//! machine ([`crate::linux::x11::overlay::state`]); the child only turns
//! Wayland events into calls, with coordinates mapped ([`map`]).
//!
//! The Linux safety nets need no copy of their own to be listed in a design:
//! the child arms `PR_SET_PDEATHSIG` and checks its parent the way the X11
//! child does, because a `SIGKILL`ed parent must not leave its
//! screen-covering windows behind.

pub mod map;
pub mod paint;

mod child;

/// Runs the Wayland overlay child and returns the process's exit code.
pub use child::run_child;
