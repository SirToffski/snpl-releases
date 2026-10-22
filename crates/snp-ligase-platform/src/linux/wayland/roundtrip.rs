//! A bounded setup roundtrip on a live connection, without a detached raw-handle worker.
use std::{
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::{Connection, Dispatch, EventQueue, protocol::wl_callback};

pub(super) type Done = Arc<AtomicBool>;

pub(super) fn roundtrip<S>(
    connection: &Connection,
    queue: &mut EventQueue<S>,
    state: &mut S,
    budget: Duration,
) -> Result<(), String>
where
    S: Dispatch<wl_callback::WlCallback, Done> + 'static,
{
    let started = Instant::now();
    let done = Done::default();
    connection.display().sync(&queue.handle(), done.clone());
    while !done.load(Ordering::Relaxed) {
        if started.elapsed() >= budget {
            return Err("Wayland setup roundtrip did not answer within its budget".into());
        }
        connection.flush().map_err(|e| e.to_string())?;
        queue.dispatch_pending(state).map_err(|e| e.to_string())?;
        if done.load(Ordering::Relaxed) {
            break;
        }
        if let Some(guard) = queue.prepare_read() {
            let remaining = budget.saturating_sub(started.elapsed());
            let mut fd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one initialized pollfd, valid while the read guard borrows its connection.
            let ready = unsafe {
                libc::poll(
                    &mut fd,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.to_string());
            }
            if ready == 0 {
                return Err("Wayland setup roundtrip did not answer within its budget".into());
            }
            guard.read().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
