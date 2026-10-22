//! Activate the existing eframe surface on winit's own Wayland connection.
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::{Backend, ObjectId},
    protocol::{wl_callback, wl_pointer, wl_registry, wl_seat, wl_surface::WlSurface},
};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::{self, XdgActivationV1},
};
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};

#[derive(Default)]
struct State {
    activation: Option<u32>,
    observe_input: bool,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    serial: Option<u32>,
    token: Option<String>,
}
impl Dispatch<wl_callback::WlCallback, super::roundtrip::Done> for State {
    fn event(
        _: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        done: &super::roundtrip::Done,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        done.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "xdg_activation_v1" {
                state.activation = Some(name);
            }
            if state.observe_input && interface == "wl_seat" && state.seat.is_none() {
                state.seat =
                    Some(registry.bind::<wl_seat::WlSeat, (), Self>(name, version.min(5), qh, ()));
            }
        }
    }
}
impl Dispatch<XdgActivationV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &XdgActivationV1,
        _: xdg_activation_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Raw editor handles. The app installs these during `logic` and clears them
/// under the same mutex in `on_exit`/Drop, before eframe destroys the window.
/// Activation and clearing are serialized by WindowGate's mutex.
#[derive(Debug)]
pub struct EditorSurface {
    surface: usize,
    connection: Connection,
    activation: XdgActivationV1,
    registry: wl_registry::WlRegistry,
}

impl EditorSurface {
    pub fn capture(window: &(impl HasWindowHandle + HasDisplayHandle)) -> Result<Self, String> {
        let display = window.display_handle().map_err(|e| e.to_string())?;
        let surface = window.window_handle().map_err(|e| e.to_string())?;
        let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) =
            (display.as_raw(), surface.as_raw())
        else {
            return Err("the editor has no Wayland surface".into());
        };
        // SAFETY: called by the UI thread while eframe owns this window, before
        // publishing the handles. No surface mutex or teardown can run here.
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let mut queue = connection.new_event_queue::<State>();
        let registry = connection.display().get_registry(&queue.handle(), ());
        let mut state = State::default();
        let result = super::roundtrip::roundtrip(
            &connection,
            &mut queue,
            &mut state,
            std::time::Duration::from_millis(250),
        );
        let activation = result.and_then(|()| {
            let name = state
                .activation
                .ok_or("the compositor has no xdg_activation_v1")?;
            Ok(registry.bind::<XdgActivationV1, (), State>(name, 1, &queue.handle(), ()))
        });
        let activation = match activation {
            Ok(activation) => activation,
            Err(error) => {
                let _ = connection.backend().destroy_object(&registry.id());
                return Err(error);
            }
        };
        Ok(Self {
            surface: surface.surface.as_ptr() as usize,
            connection,
            activation,
            registry,
        })
    }

    /// Only queues and flushes requests under the surface lifetime lock; no roundtrip.
    pub fn activate(&self, token: String) -> Result<(), String> {
        // SAFETY: WindowGate's lock keeps the stored wl_surface alive; the proxy
        // borrows it and never owns or destroys it.
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), self.surface as *mut _) }
            .map_err(|e| e.to_string())?;
        let surface = WlSurface::from_id(&self.connection, id).map_err(|e| e.to_string())?;
        self.activation.activate(token, &surface);
        self.connection.flush().map_err(|e| e.to_string())?;
        tracing::info!(
            "requested activation of the existing editor surface; compositor decides whether to focus it"
        );
        Ok(())
    }
}

impl Drop for EditorSurface {
    fn drop(&mut self) {
        // WindowGate clears this before eframe tears down its foreign display.
        self.activation.destroy();
        let _ = self
            .connection
            .backend()
            .destroy_object(&self.registry.id());
    }
}

#[cfg(test)]
#[test]
fn w1_activation_setup_roundtrip_is_bounded_on_a_silent_socket() {
    let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    let (release, stop) = std::sync::mpsc::channel::<()>();
    let peer = std::thread::spawn(move || {
        let _ = stop.recv_timeout(std::time::Duration::from_millis(700));
        drop(server);
    });
    let connection = Connection::from_socket(client).unwrap();
    let mut queue = connection.new_event_queue::<State>();
    let started = std::time::Instant::now();
    let result = super::roundtrip::roundtrip(
        &connection,
        &mut queue,
        &mut State::default(),
        std::time::Duration::from_millis(50),
    );
    let elapsed = started.elapsed();
    drop(release);
    peer.join().unwrap();
    assert!(result.unwrap_err().contains("budget"));
    assert!(
        elapsed < std::time::Duration::from_millis(400),
        "{elapsed:?}"
    );
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: wayland_client::WEnum::Value(caps),
        } = event
            && caps.contains(wl_seat::Capability::Pointer)
            && state.pointer.is_none()
        {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button {
            serial,
            button: 0x110, // Linux BTN_LEFT: the release that completed the selection.
            state: wayland_client::WEnum::Value(wl_pointer::ButtonState::Released),
            ..
        } = event
        {
            state.serial = Some(serial);
        }
    }
}
impl Dispatch<XdgActivationTokenV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.token = Some(token);
        }
    }
}

/// Observes the overlay client's own pointer serial, before its final gesture.
/// The foreign connection and proxies must be dropped before its windows.
pub struct OverlayActivation {
    editor: EditorSurface,
    queue: wayland_client::EventQueue<State>,
    state: State,
}
impl OverlayActivation {
    pub fn capture(window: &(impl HasWindowHandle + HasDisplayHandle)) -> Result<Self, String> {
        let editor = EditorSurface::capture(window)?;
        let mut queue = editor.connection.new_event_queue::<State>();
        let mut state = State {
            observe_input: true,
            ..State::default()
        };
        let registry = editor
            .connection
            .display()
            .get_registry(&queue.handle(), ());
        for _ in 0..2 {
            super::roundtrip::roundtrip(
                &editor.connection,
                &mut queue,
                &mut state,
                std::time::Duration::from_millis(250),
            )?;
        }
        let _ = editor.connection.backend().destroy_object(&registry.id());
        Ok(Self {
            editor,
            queue,
            state,
        })
    }
    pub fn finish(&mut self, window: &impl HasWindowHandle) -> Result<String, String> {
        let budget = std::time::Duration::from_millis(250);
        super::roundtrip::roundtrip(
            &self.editor.connection,
            &mut self.queue,
            &mut self.state,
            budget,
        )?;
        let serial = self
            .state
            .serial
            .take()
            .ok_or("overlay has no final pointer serial")?;
        let seat = self
            .state
            .seat
            .as_ref()
            .ok_or("overlay has no input seat")?;
        let RawWindowHandle::Wayland(handle) =
            window.window_handle().map_err(|e| e.to_string())?.as_raw()
        else {
            return Err("overlay has no Wayland surface".into());
        };
        // SAFETY: the caller retains this window throughout the bounded request.
        let id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), handle.surface.as_ptr().cast()) }
                .map_err(|e| e.to_string())?;
        let surface = WlSurface::from_id(&self.editor.connection, id).map_err(|e| e.to_string())?;
        let token = self
            .editor
            .activation
            .get_activation_token(&self.queue.handle(), ());
        token.set_serial(serial, seat);
        token.set_surface(&surface);
        token.set_app_id("snp-ligase".into());
        token.commit();
        let started = std::time::Instant::now();
        let result = loop {
            if let Some(value) = self.state.token.take() {
                break Ok(value);
            }
            let remaining = budget.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break Err("overlay activation token deadline expired".into());
            }
            if let Err(error) = super::roundtrip::roundtrip(
                &self.editor.connection,
                &mut self.queue,
                &mut self.state,
                remaining,
            ) {
                break Err(error);
            }
        };
        token.destroy();
        tracing::info!(
            serial,
            success = result.is_ok(),
            "overlay requested fresh activation permission"
        );
        result
    }
}
impl Drop for OverlayActivation {
    fn drop(&mut self) {
        if let Some(pointer) = self.state.pointer.take() {
            pointer.release();
        }
        if let Some(seat) = self.state.seat.take() {
            seat.release();
        }
    }
}
