//! The Wayland session's layout: what the backend reports as monitors, in
//! the portal image's own pixel space (LINUX_SUPPORT.md section 6, L3b).
//!
//! Two parts:
//!
//! - [`probe`] walks the Wayland globals on a short-lived connection: per
//!   output the `xdg_output` logical position and size, the physical mode
//!   size from `wl_output`, and the output's name. It is re-probed for every
//!   capture because monitors move. It uses no GUI connection at all — in
//!   particular not winit's monitor list, which is wrong under mixed scales
//!   (Spikes S1 and S2: winit reports the 150% output at x=3840 with scale 2,
//!   while its logical position is x=1920 and its real scale 1.5).
//! - [`build`] is a pure function from those facts (plus, on a real capture,
//!   the portal image's size) to the virtual layout: every monitor's rect in
//!   the portal image's pixel space and its frame's *native scale*, how much
//!   a frame pixel was rescaled away from the output's own pixels.
//!
//! The portal composes the whole desktop at the largest output's scale, so
//! the image's scale `S` is not an output's own density: a 100% output next
//! to a 150% one arrives 1.5× too big. A frame's native scale undoes that
//! on the way out ([`crate::VirtualCapture::crop`]).
//!
//! Rotate the physical mode axes before measuring scale. Positions are relative
//! to the complete logical desktop bounds on both capture paths.
//!
//! Sizes come from `physical × S / the output's scale`, not from
//! `logical × S`: the compositor's logical size is only exact to a pixel,
//! and multiplying it with a snapped scale would amplify that rounding.
//! With the VM's real numbers, a reported logical width of 1706 *or* 1707
//! both land the 150% output's virtual width exactly at its physical 2560
//! this way.
//!
//! Tests pin the VM's real numbers: a 1920×1080 output at 100% at logical
//! (0,0), a 2560×1440 output at 150% at logical (1920,0), and the portal
//! image 5441×1620 that KDE composes for them (LINUX_SUPPORT.md, Spike S1).

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_output, wl_registry},
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

use crate::{Error, RectI, Result};

/// What [`probe`] learned about one output.
///
/// Deliberately plain data: [`build`] and the tests work on these, and the
/// spike example prints them, so a number pinned in the tests is provably
/// real.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbedOutput {
    /// `wl_output` v4's own name, e.g. `Virtual-1`. Empty when the compositor
    /// only speaks `wl_output` v1..3.
    pub wl_name: String,
    /// The `xdg_output` name, e.g. `Virtual-1`: what winit's
    /// `MonitorHandle::name()` must say for the overlay child to match it.
    pub xdg_name: String,
    /// The logical position: compositor space, fractional-scale aware.
    pub logical_x: i32,
    pub logical_y: i32,
    /// The logical size: physical / the output's real scale, rounded by the
    /// compositor.
    pub logical_w: i32,
    pub logical_h: i32,
    /// The current mode's physical pixels, from a `wl_output` mode event.
    pub physical_w: i32,
    pub physical_h: i32,
    /// Raw wl_output transform; 0 is normal, 1/3/5/7 swap the mode axes.
    pub transform: u32,
    /// The `wl_output` scale event's integer factor (1 or 2), when sent. Log
    /// only: on a fractional-scale desktop this is *not* the real scale, the
    /// protocol is integer-only and KDE rounds 1.5 up to 2.
    pub wl_scale: Option<u32>,
    /// The xdg_output description, e.g. `Virtual-1`. Logged only.
    pub description: String,
}

impl ProbedOutput {
    /// Mode dimensions after the compositor's rotation/reflection.
    fn oriented_size(&self) -> (i32, i32) {
        if matches!(self.transform, 1 | 3 | 5 | 7) {
            (self.physical_h, self.physical_w)
        } else {
            (self.physical_w, self.physical_h)
        }
    }

    /// The connector name: `xdg_output`'s, else `wl_output`'s, else empty.
    ///
    /// The name is what the overlay matches windows by, so an empty name in a
    /// probe result is a probe failure, not a fallback.
    #[must_use]
    fn name(&self) -> String {
        if !self.xdg_name.is_empty() {
            self.xdg_name.clone()
        } else {
            self.wl_name.clone()
        }
    }
}

/// One output's events while dispatching, keyed by its `wl_output` slot.
#[derive(Debug, Default)]
struct OutputData {
    probed: ProbedOutput,
    /// The mode `wl_output` flagged CURRENT, so far.
    mode: Option<(i32, i32)>,
}

/// The dispatch state across the probe's roundtrips.
#[derive(Debug, Default)]
struct Probe {
    /// In the same order as the `wl_output` globals were bound.
    outputs: Vec<OutputData>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
    /// `registry_queue_init` records globals itself; late adds and removals
    /// are irrelevant in a probe that lives exactly two roundtrips.
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, usize> for Probe {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let Some(data) = state.outputs.get_mut(*index) else {
            tracing::warn!(
                index = *index,
                "an event for an output outside the known set"
            );
            return;
        };
        match event {
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                let flags: wl_output::Mode = flags
                    .into_result()
                    .unwrap_or_else(|_| wl_output::Mode::empty());
                if flags.contains(wl_output::Mode::Current) {
                    data.mode = Some((width, height));
                }
            }
            wl_output::Event::Scale { factor } => data.probed.wl_scale = Some(factor as u32),
            wl_output::Event::Name { name } => data.probed.wl_name = name,
            wl_output::Event::Geometry { transform, .. } => {
                data.probed.transform = transform.into();
            }
            wl_output::Event::Done => {}
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for Probe {
    fn event(
        state: &mut Self,
        _proxy: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let Some(data) = state.outputs.get_mut(*index) else {
            tracing::warn!(
                index = *index,
                "an xdg_output event for an output outside the known set"
            );
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                data.probed.logical_x = x;
                data.probed.logical_y = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                data.probed.logical_w = width;
                data.probed.logical_h = height;
            }
            zxdg_output_v1::Event::Name { name } => data.probed.xdg_name = name,
            zxdg_output_v1::Event::Description { description } => {
                data.probed.description = description;
            }
            zxdg_output_v1::Event::Done => {}
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for Probe {
    /// The manager has no events.
    fn event(
        _state: &mut Self,
        _proxy: &ZxdgOutputManagerV1,
        _event: wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

/// Walks the compositor once and reports the plain facts about every output.
///
/// The connection and its proxies are all gone when this returns; a Wayland
/// problem in the probe therefore cannot poison anything later.
pub fn probe() -> Result<Vec<ProbedOutput>> {
    let conn = Connection::connect_to_env().map_err(|err| Error::Capture {
        message: format!("the Wayland session could not be connected: {err:?}"),
    })?;
    probe_connection(conn)
}

pub(super) fn probe_connection(conn: Connection) -> Result<Vec<ProbedOutput>> {
    let (globals, mut queue) = registry_queue_init(&conn).map_err(|err| Error::Capture {
        message: format!("the Wayland globals could not be read: {err:?}"),
    })?;
    let registry = globals.registry().clone();
    let qhandle = queue.handle();

    // The compositor's advertised order is its own; the logical positions in
    // [`build`] are authoritative, this order only keys the event handlers.
    let output_globals: Vec<_> = globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|global| global.interface == "wl_output")
        .collect();
    if output_globals.is_empty() {
        return Err(Error::Capture {
            message: "no monitor is switched on or connected".to_owned(),
        });
    }

    let mut probe = Probe {
        outputs: output_globals
            .iter()
            .map(|_| OutputData::default())
            .collect(),
    };
    let mut output_proxies = Vec::with_capacity(output_globals.len());
    for (index, global) in output_globals.iter().enumerate() {
        let proxy = registry.bind::<wl_output::WlOutput, usize, Probe>(
            global.name,
            global.version.min(4),
            &qhandle,
            index,
        );
        tracing::debug!(
            proxy_id = %proxy.id(),
            name = global.name,
            version = global.version,
            "wl_output bound for the layout probe"
        );
        output_proxies.push(proxy);
    }

    // xdg_output: one manager, then one child proxy per wl_output, in order.
    let manager_global = globals
        .contents()
        .clone_list()
        .into_iter()
        .find(|global| global.interface == "zxdg_output_manager_v1")
        .ok_or_else(|| Error::Capture {
            message: "the compositor does not offer xdg-output, which the layout needs".to_owned(),
        })?;
    let manager = registry.bind::<ZxdgOutputManagerV1, (), Probe>(
        manager_global.name,
        manager_global.version.min(3),
        &qhandle,
        (),
    );
    for (index, output_proxy) in output_proxies.iter().enumerate() {
        manager.get_xdg_output(output_proxy, &qhandle, index);
    }

    queue.roundtrip(&mut probe).map_err(|err| Error::Capture {
        message: format!("dispatching the layout probe: {err:?}"),
    })?;
    queue.roundtrip(&mut probe).map_err(|err| Error::Capture {
        message: format!("dispatching the layout probe again: {err:?}"),
    })?;

    let mut outputs = Vec::with_capacity(probe.outputs.len());
    for data in &probe.outputs {
        // The mode events carry the physical size; without one the output's
        // own resolution is unknowable and it must not be pretended.
        let (physical_w, physical_h) = data.mode.ok_or_else(|| Error::Capture {
            message: format!(
                "the compositor reported no mode for output {:?}, so its physical size is unknown",
                data.probed.name()
            ),
        })?;
        let mut probed = data.probed.clone();
        probed.physical_w = physical_w;
        probed.physical_h = physical_h;

        if probed.name().is_empty() {
            return Err(Error::Capture {
                message: format!(
                    "the compositor reports no name for an output (xdg {:?}, wl {:?})",
                    probed.xdg_name, probed.wl_name,
                ),
            });
        }

        tracing::info!(
            name = %probed.name(),
            xdg_description = %probed.description,
            logical = format!("{}x{} at {},{}", probed.logical_w, probed.logical_h, probed.logical_x, probed.logical_y),
            physical = format!("{}x{}", probed.physical_w, probed.physical_h),
            wl_scale = probed.wl_scale,
            transform = probed.transform,
            "output probed"
        );
        outputs.push(probed);
    }
    Ok(outputs)
}

/// A scale value, as the probe would measure it and as snapping makes it.
///
/// Quarter steps (1.0, 1.25, 1.5, 1.75) are what a desktop is really set to;
/// the raw measurement of one differs by a hair because the compositor
/// rounds the logical size to a whole pixel. `0.02` of a step is far below
/// anything a real desktop would be set to and far above that rounding.
#[must_use]
pub fn snap(value: f64) -> (f64, bool) {
    let snapped = (value * 4.0).round() / 4.0;
    if (value - snapped).abs() <= 0.02 {
        (snapped, true)
    } else {
        (value, false)
    }
}

/// One output's place in the virtual layout: the portal's pixel space.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualMonitor {
    /// The connector name, e.g. `Virtual-1`.
    pub name: String,
    /// The rectangle in image-space pixels.
    pub rect: RectI,
    /// Frame pixels as a fraction of image-space pixels, per axis
    /// ([`crate::MonitorFrame::native_scale_x`]).
    pub native_scale_x: f64,
    pub native_scale_y: f64,
    /// For logs and the UI only, never maths: e.g. 1.0, 1.5.
    pub scale_factor: f32,
}

/// The whole desktop, as the portal image composes it.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualLayout {
    pub monitors: Vec<VirtualMonitor>,
    /// Image-space pixels per logical pixel (`S`): 1.5 for the VM.
    pub scale_x: f64,
    pub scale_y: f64,
    /// The monitors' union; whitespace (the portal's rounding column at
    /// x=5440) is not in it, so it stays uncovered by any frame.
    pub union: RectI,
    /// What the build wants said out loud: un-snapped scales, a size the
    /// image's scaleXs disagree about. The caller logs these lines.
    pub warnings: Vec<String>,
}

/// Builds the virtual layout from probed outputs and, on a real capture, the
/// portal image's size.
///
/// See the module docs for why the sizes come from the physical size and the
/// measured scales rather than the reported logical size.
pub fn build(probe: &[ProbedOutput], image: Option<(u32, u32)>) -> Result<VirtualLayout> {
    let mut warnings = Vec::new();

    // An output with no logical size (or no mode) cannot sit anywhere: skip
    // it rather than inventing coordinates for it.
    let mut usable = Vec::with_capacity(probe.len());
    for output in probe {
        if output.logical_w > 0
            && output.logical_h > 0
            && output.physical_w > 0
            && output.physical_h > 0
        {
            usable.push(output);
        } else {
            warnings.push(format!(
                "output {:?} has no usable geometry (logical {}x{} at {},{}, physical {}x{}) and \
is skipped",
                output.name(),
                output.logical_w,
                output.logical_h,
                output.logical_x,
                output.logical_y,
                output.physical_w,
                output.physical_h,
            ));
        }
    }
    if usable.is_empty() {
        return Err(Error::Capture {
            message: "the compositor's outputs have no usable geometry".to_owned(),
        });
    }

    // Per output, its real scale: physical / compositor-reported logical,
    // snapped to the quarter step the desktop was actually set to.
    let measured: Vec<(f64, f64, bool)> = usable
        .iter()
        .map(|output| {
            let (physical_w, physical_h) = output.oriented_size();
            let raw_x = physical_w as f64 / output.logical_w as f64;
            let raw_y = physical_h as f64 / output.logical_h as f64;
            let (scale_x, snapped_x) = snap(raw_x);
            let (scale_y, snapped_y) = snap(raw_y);
            if !snapped_x || !snapped_y {
                warnings.push(format!(
                    "output {:?}'s scale is {:.4},{:.4}, not a quarter step; using it unsnapped",
                    output.name(),
                    scale_x,
                    scale_y,
                ));
            } else if (raw_x - raw_y).abs() / raw_x > 0.02 {
                warnings.push(format!(
                    "output {:?}'s measured scale differs per axis: {:.4} vs {:.4}, using {:.4},{:.4}",
                    output.name(), raw_x, raw_y, scale_x, scale_y,
                ));
            }
            (scale_x, scale_y, snapped_x && snapped_y)
        })
        .collect();

    // The output grid's extents in logical pixels, for measuring S off the
    // portal image: the image is the whole logical desktop at S.
    let (mut min_x, mut min_y, mut max_right, mut max_bottom) =
        (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for output in &usable {
        min_x = min_x.min(output.logical_x);
        min_y = min_y.min(output.logical_y);
        max_right = max_right.max(output.logical_x + output.logical_w);
        max_bottom = max_bottom.max(output.logical_y + output.logical_h);
    }
    let logical_w = i64::from(max_right) - i64::from(min_x);
    let logical_h = i64::from(max_bottom) - i64::from(min_y);
    let (scale_x, scale_y) = match image {
        Some((image_w, image_h)) if logical_w > 0 && logical_h > 0 => {
            let raw_x = image_w as f64 / logical_w as f64;
            let raw_y = image_h as f64 / logical_h as f64;
            let (snapped_x, x_ok) = snap(raw_x);
            let (snapped_y, y_ok) = snap(raw_y);
            if (raw_y - raw_x).abs() / raw_x > 0.01 {
                warnings.push(format!(
                    "the portal image measures different scales per axis: {:.4} by width vs {:.4} by \
height, using {:.4},{:.4}",
                    raw_x, raw_y, snapped_x, snapped_y,
                ));
            }
            if !x_ok || !y_ok {
                warnings.push(format!(
                    "the portal image's scale {:.4},{:.4} is not a quarter step; using it unsnapped",
                    snapped_x, snapped_y,
                ));
            }
            (snapped_x, snapped_y)
        }
        // No image yet: the portal composes at the largest output's scale, so
        // that is the best estimate (the spike prints the probe this way).
        _ => {
            let x = measured.iter().map(|(x, _, _)| *x).fold(f64::MIN, f64::max);
            let y = measured.iter().map(|(_, y, _)| *y).fold(f64::MIN, f64::max);
            warnings.push(format!(
                "no portal image given: the image scale is assumed from the largest output's \
({x:.4},{y:.4})"
            ));
            (x, y)
        }
    };

    let mut monitors = Vec::with_capacity(usable.len());
    for (output, (scale_out_x, scale_out_y, _snapped)) in usable.iter().zip(&measured) {
        // Positions: (logical - desktop origin) × S, shared by native and portal.
        // Sizes: oriented physical × S / the output's own
        // scale, so a compositor's ±1 logical rounding does not move the
        // monitor's own edge (see the module docs).
        let x = ((i64::from(output.logical_x) - i64::from(min_x)) as f64 * scale_x).round() as i32;
        let y = ((i64::from(output.logical_y) - i64::from(min_y)) as f64 * scale_y).round() as i32;
        let (physical_w, physical_h) = output.oriented_size();
        let w = (physical_w as f64 * scale_x / scale_out_x).round() as i32;
        let h = (physical_h as f64 * scale_y / scale_out_y).round() as i32;
        if w <= 0 || h <= 0 {
            return Err(Error::Capture {
                message: format!(
                    "the layout would demand a {w}x{h} rectangle for output {:?}",
                    output.name()
                ),
            });
        }
        let rect = RectI::new(x, y, w, h);
        tracing::trace!(monitor = %output.name(), rect = ?rect, "a monitor in the portal's pixel space");
        monitors.push(VirtualMonitor {
            name: output.name(),
            rect,
            // How much the portal's image-space pixels rescaled this output:
            // the output's own density relative to the image's scale.
            native_scale_x: scale_out_x / scale_x,
            native_scale_y: scale_out_y / scale_y,
            scale_factor: (*scale_out_x) as f32,
        });
    }

    // The desktop's bounds: the smallest origin and the largest corner, so a
    // rounding column (the portal image is 5441 wide, the desktop 5440) is
    // not part of anything, and a selection across outputs is still one rect.
    let (mut left, mut top, mut right, mut bottom) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for monitor in &monitors {
        let rect = monitor.rect;
        left = left.min(rect.x);
        top = top.min(rect.y);
        right = right.max(rect.right());
        bottom = bottom.max(rect.bottom());
    }
    if right < left || bottom < top {
        return Err(Error::Capture {
            message: "the virtual layout bounds collapsed".to_owned(),
        });
    }
    let union = RectI::new(left, top, right - left, bottom - top);

    Ok(VirtualLayout {
        monitors,
        scale_x,
        scale_y,
        union,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One output's reported facts, keyed for the test pinning later.
    fn out(name: &str, logical: (i32, i32, i32, i32), physical: (i32, i32)) -> ProbedOutput {
        ProbedOutput {
            wl_name: name.to_owned(),
            xdg_name: name.to_owned(),
            logical_x: logical.0,
            logical_y: logical.1,
            logical_w: logical.2,
            logical_h: logical.3,
            physical_w: physical.0,
            physical_h: physical.1,
            ..ProbedOutput::default()
        }
    }

    /// The VM's real probe, as the `layout` spike command read it on Fedora
    /// 44 / Plasma 6.7.5 (LINUX_SUPPORT.md, Spike S1): the 100% output is
    /// named `Virtual-2` and comes first in the compositor's own order, the
    /// 150% one is `Virtual-1` at logical (1920,0) — logical height 960
    /// (1440 physical at 1.5), logical width 1707 from the compositor's
    /// rounding. Names are what the overlay wins by; the reverse window
    /// creation therefore puts the last monitor's window last.
    fn vm_probe(logical_w: i32) -> Vec<ProbedOutput> {
        vec![
            out("Virtual-2", (0, 0, 1920, 1080), (1920, 1080)),
            out("Virtual-1", (1920, 0, logical_w, 960), (2560, 1440)),
        ]
    }

    /// The spike-measured portal image (Spike S1, every run).
    const PORTAL_IMAGE: (u32, u32) = (5441, 1620);

    #[test]
    fn w1_rotated_and_flipped_modes_build_native_expected_sizes() {
        for transform in 0..8 {
            let portrait = matches!(transform, 1 | 3 | 5 | 7);
            let size = if portrait { (1080, 1920) } else { (1920, 1080) };
            let mut rotated = out("rotated", (1920, 0, size.0, size.1), (1920, 1080));
            rotated.transform = transform;
            for image in [None, Some((size.0 as u32, size.1 as u32))] {
                let layout = build(std::slice::from_ref(&rotated), image).unwrap();
                assert_eq!(layout.monitors[0].rect, RectI::new(0, 0, size.0, size.1));
                assert_eq!(
                    super::super::kwin::expected_size(&layout.monitors[0]),
                    (size.0 as u32, size.1 as u32)
                );
            }
            let probes = [out("landscape", (0, 0, 1920, 1080), (1920, 1080)), rotated];
            for image in [None, Some(((1920 + size.0) as u32, size.1 as u32))] {
                let layout = build(&probes, image).unwrap();
                let monitor = &layout.monitors[1];
                assert_eq!(
                    monitor.rect,
                    RectI::new(1920, 0, size.0, size.1),
                    "transform {transform}"
                );
                assert_eq!(
                    (monitor.native_scale_x, monitor.native_scale_y),
                    (1.0, 1.0),
                    "transform {transform}"
                );
                assert_eq!(
                    super::super::kwin::expected_size(monitor),
                    (size.0 as u32, size.1 as u32)
                );
            }
        }
    }

    #[test]
    fn w1_nonzero_desktop_origin_matches_portal_and_native() {
        for (x, y) in [(-1920, 0), (-1920, -240), (200, 300)] {
            let probes = [
                out("left", (x, y, 1920, 1080), (1920, 1080)),
                out("right", (x + 1920, y + 240, 1920, 1080), (1920, 1080)),
            ];
            for image in [None, Some((3840, 1320))] {
                let layout = build(&probes, image).unwrap();
                assert_eq!((layout.scale_x, layout.scale_y), (1.0, 1.0));
                assert_eq!(layout.monitors[0].rect, RectI::new(0, 0, 1920, 1080));
                assert_eq!(layout.monitors[1].rect, RectI::new(1920, 240, 1920, 1080));
                assert_eq!(layout.union, RectI::new(0, 0, 3840, 1320));
            }
        }
    }

    #[test]
    fn snap_pulls_compositor_rounding_to_the_quarter_step() {
        // The real measurements: 2560/1706 and 2560/1707.
        assert_eq!(snap(2560.0 / 1706.0), (1.5, true));
        assert_eq!(snap(2560.0 / 1707.0), (1.5, true));
        assert_eq!(snap(1.0), (1.0, true));
        assert_eq!(snap(1.25), (1.25, true));
        // Between quarters but clearly within tolerance: healed to it.
        assert_eq!(snap(1.015), (1.0, true));
        // Too far from a quarter step: kept, and the caller is told.
        assert_eq!(snap(1.4), (1.4, false), "0.1 away is past the 0.02 limit");
    }

    #[test]
    fn the_vms_two_outputs_build_the_portals_pixel_space() {
        let layout = build(&vm_probe(1707), Some(PORTAL_IMAGE)).unwrap();

        assert_eq!(layout.monitors.len(), 2);
        assert_eq!(layout.scale_x, 1.5, "5441 / 3627 snapped to a quarter");
        assert_eq!(layout.scale_y, 1.5, "1620 / 1080 snapped too");

        // The 100% output: S-space is 1.5× its pixels, native ⅔. Its name is
        // Virtual-2: the compositor's order and numbering are its own.
        let first = &layout.monitors[0];
        assert_eq!(first.name, "Virtual-2");
        assert_eq!(first.rect, RectI::new(0, 0, 2880, 1620));
        assert_eq!(first.native_scale_x, 2.0 / 3.0);
        assert_eq!(first.native_scale_y, 2.0 / 3.0);
        assert_eq!(first.scale_factor, 1.0);

        // The 150% output: the image's pixels are already its own, native 1.0.
        let second = &layout.monitors[1];
        assert_eq!(second.name, "Virtual-1");
        assert_eq!(second.rect, RectI::new(2880, 0, 2560, 1440));
        assert_eq!(second.native_scale_x, 1.0);
        assert_eq!(second.native_scale_y, 1.0);
        assert_eq!(second.scale_factor, 1.5);

        // The desktop: 5440 wide, not the image's 5441 — the rounding column
        // is outside the union, so no frame ever claims it.
        assert_eq!(layout.union, RectI::new(0, 0, 5440, 1620));
        assert!(layout.warnings.is_empty(), "{:?}", layout.warnings);
    }

    #[test]
    fn a_compositor_rounding_error_keeps_the_same_layout() {
        // 1707 reported, 1706 reported: identical virtual width, because the
        // sizes come from the physical size and the snapped scale, not from
        // the logical size. This is the design's reason for the snap.
        let from_1707 = build(&vm_probe(1707), Some(PORTAL_IMAGE)).unwrap();
        let from_1706 = build(&vm_probe(1706), Some(PORTAL_IMAGE)).unwrap();
        assert_eq!(from_1707.monitors[1].rect, from_1706.monitors[1].rect);
        assert_eq!(from_1706.monitors[1].rect, RectI::new(2880, 0, 2560, 1440));
        assert!(from_1706.warnings.is_empty());
    }

    #[test]
    fn a_scale_that_is_not_a_quarter_step_is_kept_with_a_warning() {
        // A logical width of 1707 was the real one; setting an odd fractional
        // scale would report here as 2560/1737 = 1.4738, far from 1.5.
        let mut probe = vm_probe(1707);
        probe[1].logical_w = 1737;
        let layout = build(&probe, Some(PORTAL_IMAGE)).unwrap();

        assert_eq!(
            layout.warnings.len(),
            1,
            "expected one warning, got {:?}",
            layout.warnings
        );
        assert!(
            layout.warnings[0].contains("not a quarter step"),
            "{layout:?}"
        );
        // Still built from the physical size this output really has: the
        // maths lands on 1737×1.5 = 2605.5 exactly, which rounds up to 2606.
        assert_eq!(layout.monitors[1].rect.w, 2606, "{layout:?}");
    }

    #[test]
    fn a_portal_image_whose_axes_disagree_is_warned_about() {
        // The image's height would give S = 1.4... while its width gives
        // ½ a logical pixel off: more than the 1% cross-check allows.
        let mut probe = vm_probe(1707);
        probe[1].physical_w = 2128; // makes raw_per_output = 2128/1707 ≈ 1.247
        let layout = build(&probe, Some((5441, 1540))).unwrap();
        assert!(
            layout
                .warnings
                .iter()
                .any(|warning| warning.contains("different scales")),
            "{layout:?}"
        );
    }

    #[test]
    fn without_an_image_the_scale_is_the_largest_outputs() {
        let layout = build(&vm_probe(1707), None).unwrap();
        assert_eq!(layout.scale_x, 1.5);
        assert_eq!(layout.scale_y, 1.5);
        assert_eq!(layout.monitors[1].rect, RectI::new(2880, 0, 2560, 1440));
        assert_eq!(layout.monitors[0].rect, RectI::new(0, 0, 2880, 1620));
        assert_eq!(layout.monitors[0].native_scale_x, 2.0 / 3.0);
        assert_eq!(layout.union, RectI::new(0, 0, 5440, 1620));
    }

    #[test]
    fn an_output_without_geometry_is_skipped_with_a_warning() {
        let mut probe = vm_probe(1707);
        // A third, unplugged-ish output reported with no sizes to speak of.
        probe.push(out("Virtual-3", (0, 0, 0, 0), (0, 0)));
        let layout = build(&probe, Some(PORTAL_IMAGE)).unwrap();
        assert_eq!(layout.monitors.len(), 2, "{layout:?}");
        assert_eq!(layout.warnings.len(), 1);
        assert!(layout.warnings[0].contains("Virtual-3"), "{layout:?}");
    }

    #[test]
    fn nothing_usable_is_an_error() {
        let probe = vec![out("Virtual-3", (0, 0, 0, 0), (0, 0))];
        assert!(build(&probe, Some(PORTAL_IMAGE)).is_err());
    }
}
