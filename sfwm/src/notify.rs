//! The built-in `org.freedesktop.Notifications` D-Bus service — sfwm's dunst
//! replacement. dunst-on-Wayland uses wlr-layer-shell, which can't composite
//! under sfwm, so the WM hosts the service itself and draws popups with the same
//! shell-surface engine as the bar/wallpaper.
//!
//! It runs on a dedicated thread (zbus' blocking connection drives D-Bus I/O on
//! its own internal tasks). Each `Notify`/`CloseNotification` is forwarded to the
//! WM's calloop thread over a channel; the main thread owns all rendering.
//!
//! KISS scope: `Notify` / `CloseNotification` / `GetCapabilities` /
//! `GetServerInformation` — enough for `notify-send` and ordinary apps. Actions
//! and the `NotificationClosed`/`ActionInvoked` signals are a deliberate TODO.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use calloop::channel::Sender;
use zbus::zvariant::Value;

/// A message from the D-Bus thread to the WM main thread.
pub enum NotifEvent {
    Show {
        id: u32,
        summary: String,
        body: String,
        /// freedesktop urgency hint: 0 low, 1 normal, 2 critical.
        urgency: u8,
        /// milliseconds; -1 = server default, 0 = never expire.
        expire_timeout: i32,
    },
    Close(u32),
}

struct NotifService {
    tx: Sender<NotifEvent>,
    next: AtomicU32,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotifService {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        _app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        hints: HashMap<String, Value<'_>>,
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next.fetch_add(1, Ordering::Relaxed)
        };
        let urgency = hints
            .get("urgency")
            .and_then(|v| u8::try_from(v).ok())
            .unwrap_or(1);
        let _ = self.tx.send(NotifEvent::Show {
            id,
            summary,
            body,
            urgency,
            expire_timeout,
        });
        id
    }

    fn close_notification(&self, id: u32) {
        let _ = self.tx.send(NotifEvent::Close(id));
    }

    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".to_string()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "sfwm".to_string(),
            "sommerflusswm".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
            "1.2".to_string(),
        )
    }
}

/// Start the notification service on a background thread. Tolerant: if there's no
/// session bus, or another notifier already owns the name, it logs and gives up
/// (the WM keeps running fine without notifications).
pub fn spawn_notification_service(tx: Sender<NotifEvent>) {
    let _ = std::thread::Builder::new()
        .name("sfwm-notifications".to_string())
        .spawn(move || match build(tx) {
            // Keep the connection alive for the process lifetime; zbus services
            // the bus on its own internal tasks.
            Ok(_conn) => loop {
                std::thread::park();
            },
            Err(e) => {
                eprintln!("sfwm: notifications: D-Bus unavailable ({e}); notify-send won't work")
            }
        });
}

fn build(tx: Sender<NotifEvent>) -> zbus::Result<zbus::blocking::Connection> {
    let svc = NotifService {
        tx,
        next: AtomicU32::new(1),
    };
    zbus::blocking::connection::Builder::session()?
        .name("org.freedesktop.Notifications")?
        .serve_at("/org/freedesktop/Notifications", svc)?
        .build()
}
