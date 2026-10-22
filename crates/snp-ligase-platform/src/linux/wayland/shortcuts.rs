//! GlobalShortcuts session, owned by a dedicated async-io thread.
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use ashpd::desktop::CreateSessionOptions;
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use futures_lite::StreamExt;

/// Desktop-reported bindings, also used by read-only Settings.
static BOUND: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

pub fn bound() -> Vec<(String, String)> {
    BOUND.lock().map(|rows| rows.clone()).unwrap_or_default()
}

fn store(rows: Vec<(String, String)>) {
    if let Ok(mut state) = BOUND.lock() {
        *state = rows;
    }
}

pub enum Event {
    Activated { id: String, token: Option<String> },
    Status(String),
}

/// Dropping the guard closes the session on its owner thread.
pub struct Shortcuts(mpsc::Sender<()>);
impl Drop for Shortcuts {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

pub fn start(
    shortcuts: Vec<(String, String, Option<String>)>,
    mut emit: impl FnMut(Event) + Send + 'static,
) -> std::io::Result<Shortcuts> {
    let (stop, stopped) = mpsc::channel();
    std::thread::Builder::new().name("portal-shortcuts".into()).spawn(move || {
        async_io::block_on(async {
            let run = async {
                let proxy = deadline(GlobalShortcuts::new(), 15).await?;
                let session = deadline(proxy.create_session(CreateSessionOptions::default()), 15).await.map_err(|e| {
                    tracing::warn!(error = %e, "GlobalShortcuts create_session failed");
                    format!("Global hotkeys unavailable ({e}); start snp-ligase from its menu entry — install it with packaging/linux/install.sh — for global hotkeys")
                })?;
                let session_path = serde_json::to_value(&session).map_err(|e| e.to_string())?;
                let requested: Vec<_> = shortcuts.iter().map(|(id, label, trigger)| NewShortcut::new(id, label).preferred_trigger(trigger.as_deref())).collect();
                // Subscribe before binding, so an immediate activation is not lost.
                let mut activated = deadline(proxy.receive_activated(), 15).await?;
                let mut changed = deadline(proxy.receive_shortcuts_changed(), 15).await?;
                let request = deadline(proxy.bind_shortcuts(&session, &requested, None, BindShortcutsOptions::default()), 120).await?;
                let response = request.response().map_err(|e| e.to_string())?;
                let rows: Vec<_> = response.shortcuts().iter().map(|s| {
                    tracing::info!(id = s.id(), trigger = s.trigger_description(), "desktop stored global shortcut");
                    (s.id().to_owned(), s.trigger_description().to_owned())
                }).collect();
                let warn = needs_spectacle_warning(&rows);
                store(rows);
                // KDE can report a stored trigger even when Spectacle still owns it.
                if warn { emit(Event::Status("If Print or Shift+Print opens Spectacle, free it in System Settings > Shortcuts > Spectacle; then change snp-ligase in System Settings > Shortcuts.".into())); }
                let outcome = listen(&mut activated, &mut changed, &stopped,
                    |event| {
                        if Some(event.session_handle().as_str()) != session_path.as_str() { return; }
                        let keys: Vec<_> = event.options().keys().collect();
                        tracing::info!(id = event.shortcut_id(), ?keys, "portal hotkey pressed");
                        let token = event.options().get("activation_token").and_then(|v| <&str>::try_from(v).ok()).map(str::to_owned);
                        emit(Event::Activated { id: event.shortcut_id().to_owned(), token });
                    },
                    |event| {
                        if Some(event.session_handle().as_str()) == session_path.as_str() {
                            store(event.shortcuts().iter().map(|s| (s.id().into(), s.trigger_description().into())).collect());
                        }
                    }).await;
                if let Err(error) = deadline(session.close(), 15).await {
                    tracing::warn!(%error, "closing GlobalShortcuts session failed");
                }
                outcome?;
                Ok::<(), String>(())
            };
            finish_session(run.await, &mut emit);
        });
    })?;
    Ok(Shortcuts(stop))
}

/// EOF is a terminal session failure, not an immediately-ready empty poll.
async fn listen<A, C>(
    activated: &mut A,
    changed: &mut C,
    stopped: &mpsc::Receiver<()>,
    mut on_activated: impl FnMut(A::Item),
    mut on_changed: impl FnMut(C::Item),
) -> Result<(), String>
where
    A: futures_lite::Stream + Unpin,
    C: futures_lite::Stream + Unpin,
{
    loop {
        if !matches!(stopped.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            return Ok(());
        }
        let poll = futures_lite::future::race(
            async {
                let event = activated.next().await.ok_or(
                    "the desktop's global shortcut activation stream ended; global hotkeys stopped",
                )?;
                on_activated(event);
                Ok(())
            },
            async {
                let event = changed.next().await.ok_or(
                    "the desktop's global shortcut binding stream ended; global hotkeys stopped",
                )?;
                on_changed(event);
                Ok(())
            },
        );
        futures_lite::future::race(poll, async {
            async_io::Timer::after(Duration::from_millis(100)).await;
            Ok::<(), String>(())
        })
        .await?;
    }
}

fn finish_session(outcome: Result<(), String>, emit: &mut impl FnMut(Event)) {
    store(Vec::new());
    if let Err(message) = outcome {
        tracing::warn!(%message, "GlobalShortcuts session stopped");
        emit(Event::Status(message));
    }
}

#[cfg(test)]
#[test]
fn w1_either_closed_stream_ends_session_and_clears_bindings_once() {
    for activation_ends in [true, false] {
        store(vec![("region".into(), "Ctrl+Print".into())]);
        let (stop, stopped) = mpsc::channel();
        let (answer, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = async_io::block_on(async {
                let mut ended = futures_lite::stream::empty::<()>();
                let mut open = futures_lite::stream::pending::<()>();
                if activation_ends {
                    listen(&mut ended, &mut open, &stopped, |_| panic!(), |_| panic!()).await
                } else {
                    listen(&mut open, &mut ended, &stopped, |_| panic!(), |_| panic!()).await
                }
            });
            let _ = answer.send(result);
        });
        // A separate thread bounds even an EOF spin that never yields to an async timer.
        let result = received.recv_timeout(Duration::from_millis(300));
        drop(stop);
        worker.join().unwrap();
        let result = result.expect("closed shortcut stream spun instead of stopping");
        assert!(result.as_ref().unwrap_err().contains("stream ended"));
        let mut messages = Vec::new();
        finish_session(result, &mut |event| {
            if let Event::Status(message) = event {
                messages.push(message);
            }
        });
        assert!(bound().is_empty());
        assert_eq!(messages.len(), 1);
    }
}

/// Dialogs get two minutes; ordinary service calls get fifteen seconds.
async fn deadline<T, E: std::fmt::Display>(
    future: impl std::future::Future<Output = Result<T, E>>,
    seconds: u64,
) -> Result<T, String> {
    futures_lite::future::race(async { future.await.map_err(|e| e.to_string()) }, async {
        async_io::Timer::after(Duration::from_secs(seconds)).await;
        Err("the desktop's global shortcut service did not answer".into())
    })
    .await
}

fn needs_spectacle_warning(rows: &[(String, String)]) -> bool {
    rows.iter().any(|(_, trigger)| {
        matches!(
            trigger.to_ascii_lowercase().replace(' ', "").as_str(),
            "print" | "shift+print"
        )
    })
}

#[cfg(test)]
#[test]
fn l3d_warns_only_for_spectacle_triggers() {
    assert!(!needs_spectacle_warning(&[(
        "region".into(),
        "Ctrl+Print".into()
    )]));
    assert!(needs_spectacle_warning(&[(
        "region".into(),
        "Print".into()
    )]));
    assert!(needs_spectacle_warning(&[(
        "region".into(),
        "Shift+Print".into()
    )]));
}
