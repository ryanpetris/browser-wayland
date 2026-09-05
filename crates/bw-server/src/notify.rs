//! Desktop notifications. With no panel there is no notification daemon, so browser-wayland serves
//! `org.freedesktop.Notifications` on the session bus (unless another daemon owns the name) and shows
//! the notifications as toasts in every viewer; a viewer's click, action or dismissal goes back to the
//! application as the protocol's signals.

use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use bw_core::{Command, ControlMsg, ControlOp, Snapshot};
use schemars::JsonSchema;
use serde::Serialize;
use zbus::{interface, object_server::SignalEmitter, zvariant::OwnedValue};

use crate::{App, api::{ApiError, encode_png}, apps, protocol};

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// What a notification without its own timeout stays for.
const DEFAULT_TIMEOUT: u32 = 5000;

/// One notification, as the viewers and `GET /api/notifications` see it.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Notification {
    pub id: u32,
    /// the application's name as it gave it
    pub app: String,
    pub summary: String,
    pub body: String,
    /// whether `GET /api/notifications/{id}/icon` has a picture
    pub icon: bool,
    /// `[key, label]` pairs the application offers; a plain click means `default` when that key is among them
    pub actions: Vec<(String, String)>,
    /// how long it is shown, ms; 0 means until closed
    pub timeout_ms: u32,
}

enum Icon {
    Name(String),
    Path(String),
    Png(Vec<u8>),
}

/// An open notification and what the API needs beyond its public shape.
pub struct Open {
    pub info: Notification,
    icon: Option<Icon>,
    /// the `desktop-entry` hint: which windows a click brings forward, and the fallback icon
    desktop_entry: Option<String>,
    /// bumps when the id is reused (`replaces_id`), so an old expiry timer does nothing
    generation: u64,
}

pub struct Daemon(pub Arc<App>);

#[interface(name = "org.freedesktop.Notifications")]
impl Daemon {
    fn get_capabilities(&self) -> Vec<&'static str> {
        vec!["actions", "body", "icon-static", "persistence"]
    }

    fn get_server_information(&self) -> (&'static str, &'static str, &'static str, &'static str) {
        ("browser-wayland", "browser-wayland", self.0.version, "1.2")
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(&self, app_name: String, replaces_id: u32, app_icon: String, summary: String, body: String, actions: Vec<String>, hints: HashMap<String, OwnedValue>, expire_timeout: i32) -> u32 {
        let desktop_entry = hints.get("desktop-entry").and_then(|v| <&str>::try_from(v).ok()).map(|s| s.trim_end_matches(".desktop").to_string());
        let mut icon = match app_icon.as_str() {
            "" => None,
            p if p.starts_with("file://") => Some(Icon::Path(p[7..].to_string())),
            p if p.starts_with('/') => Some(Icon::Path(p.to_string())),
            n => Some(Icon::Name(n.to_string())),
        };
        if icon.is_none()
            && let Some(pixels) = ["image-data", "image_data", "icon_data"].iter().find_map(|k| hints.get(*k)).and_then(|v| image_data(v))
        {
            icon = encode_png(pixels).await.ok().map(Icon::Png);
        }
        if icon.is_none()
            && let Some(p) = hints.get("image-path").or(hints.get("image_path")).and_then(|v| <&str>::try_from(v).ok())
        {
            icon = Some(if p.starts_with('/') { Icon::Path(p.to_string()) } else { Icon::Name(p.trim_start_matches("file://").to_string()) });
        }
        let actions: Vec<(String, String)> = actions.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect();
        let timeout_ms = match expire_timeout {
            t if t < 0 => DEFAULT_TIMEOUT,
            t => t as u32,
        };
        self.0.open_notification(replaces_id, app_name, summary, body, icon, actions, timeout_ms, desktop_entry)
    }

    async fn close_notification(&self, id: u32, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        if self.0.take_notification(id, None).is_some() {
            Self::notification_closed(&emitter, id, 3).await?; // 3: closed by a call
        }
        Ok(())
    }

    #[zbus(signal)]
    async fn notification_closed(emitter: &SignalEmitter<'_>, id: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(emitter: &SignalEmitter<'_>, id: u32, action_key: String) -> zbus::Result<()>;
}

/// The `image-data` hint (`iiibiiay`: width, height, rowstride, has alpha, bits per sample, channels,
/// pixels) as straight RGBA rows.
fn image_data(v: &OwnedValue) -> Option<Snapshot> {
    let (w, h, stride, alpha, bps, channels, data): (i32, i32, i32, bool, i32, i32, Vec<u8>) = v.clone().try_into().ok()?;
    if bps != 8 || w <= 0 || h <= 0 || channels < 3 {
        return None;
    }
    let (w, h, stride, channels) = (w as usize, h as usize, stride as usize, channels as usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let p = data.get(y * stride + x * channels..y * stride + x * channels + channels)?;
            rgba.extend_from_slice(&p[..3]);
            rgba.push(if alpha { p[3] } else { 255 });
        }
    }
    Some(Snapshot { width: w as u32, height: h as u32, rgba })
}

/// Own the notification name on the session bus and serve until the process ends. Another daemon
/// owning it, or no bus, means we're not the one showing notifications; nothing else changes.
pub async fn serve(app: Arc<App>) {
    let built = async {
        zbus::connection::Builder::session()?.name(NAME)?.serve_at(PATH, Daemon(app.clone()))?.build().await
    }
    .await;
    match built {
        Ok(conn) => {
            tracing::info!("notifications: serving {NAME}; applications' notifications appear in the viewers");
            let _ = app.notify_bus.set(conn);
        }
        Err(zbus::Error::NameTaken) => tracing::info!("notifications: another daemon owns {NAME}; leaving them to it"),
        Err(e) => tracing::debug!("notifications: not serving ({e})"),
    }
}

impl App {
    /// Store a notification (a `replaces_id` takes that id's place), tell every viewer, arm its expiry.
    #[allow(clippy::too_many_arguments)]
    fn open_notification(self: &Arc<Self>, replaces_id: u32, app: String, summary: String, body: String, icon: Option<Icon>, actions: Vec<(String, String)>, timeout_ms: u32, desktop_entry: Option<String>) -> u32 {
        let mut open = self.notifications.lock().unwrap();
        let (id, generation) = match open.get(&replaces_id) {
            Some(o) if replaces_id != 0 => (replaces_id, o.generation + 1),
            _ => (self.next_notification.fetch_add(1, Ordering::Relaxed), 0),
        };
        let info = Notification { id, app, summary, body, icon: icon.is_some(), actions, timeout_ms };
        let msg = protocol::notification(&info);
        open.insert(id, Open { info, icon, desktop_entry, generation });
        drop(open);
        self.broadcast(msg);
        if timeout_ms > 0 {
            let app = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(timeout_ms as u64)).await;
                app.close_notification(id, Some(generation), 1).await; // 1: expired
            });
        }
        id
    }

    /// Remove a notification (only the given generation, if one is given) and tell the viewers.
    fn take_notification(&self, id: u32, generation: Option<u64>) -> Option<Open> {
        let mut open = self.notifications.lock().unwrap();
        if generation.is_some_and(|g| open.get(&id).is_some_and(|o| o.generation != g)) {
            return None;
        }
        let taken = open.remove(&id)?;
        drop(open);
        self.broadcast(protocol::notification_closed(id));
        Some(taken)
    }

    /// Close a notification with the protocol's reason (1 expired, 2 dismissed by the user, 3 by a call).
    async fn close_notification(&self, id: u32, generation: Option<u64>, reason: u32) {
        if self.take_notification(id, generation).is_some() {
            self.emit(|e| async move { Daemon::notification_closed(&e, id, reason).await }).await;
        }
    }

    async fn emit<F, Fut>(&self, f: F)
    where
        F: FnOnce(SignalEmitter<'static>) -> Fut,
        Fut: Future<Output = zbus::Result<()>>,
    {
        let Some(conn) = self.notify_bus.get() else { return };
        let Ok(iface) = conn.object_server().interface::<_, Daemon>(PATH).await else { return };
        if let Err(e) = f(iface.signal_emitter().to_owned()).await {
            tracing::debug!("notification signal: {e}");
        }
    }

    /// The open notifications, oldest first.
    pub fn notifications(&self) -> Vec<Notification> {
        let mut list: Vec<Notification> = self.notifications.lock().unwrap().values().map(|o| o.info.clone()).collect();
        list.sort_by_key(|n| n.id);
        list
    }

    /// A viewer acted on a notification: `dismiss` closes it; an action key is invoked and closes it;
    /// `default` without such an action brings the application's newest window forward instead.
    pub async fn notification_action(&self, id: u32, action: &str) -> Result<(), ApiError> {
        let (has_action, target) = {
            let open = self.notifications.lock().unwrap();
            let o = open.get(&id).ok_or(ApiError::NotFound)?;
            let has_action = o.info.actions.iter().any(|(k, _)| k == action);
            let names = [o.desktop_entry.clone().unwrap_or_default().to_lowercase(), o.info.app.to_lowercase()];
            let target = (action == "default" && !has_action).then(|| {
                self.viewers.lock().unwrap().window_list.iter().filter(|w| names.contains(&w.app_id.to_lowercase())).map(|w| w.id).max()
            }).flatten();
            (has_action, target)
        };
        if has_action {
            let key = action.to_string();
            self.emit(|e| async move { Daemon::action_invoked(&e, id, key).await }).await;
        } else if let Some(window) = target {
            let _ = self.send(Command::Control(ControlMsg { id: window, op: ControlOp::Activate }));
        }
        self.close_notification(id, None, 2).await; // 2: dismissed by the user
        Ok(())
    }

    /// A notification's picture: what the application named or sent, else its launcher's icon.
    pub async fn notification_icon(&self, id: u32) -> Result<(Vec<u8>, &'static str), ApiError> {
        let found = {
            let open = self.notifications.lock().unwrap();
            let o = open.get(&id).ok_or(ApiError::NotFound)?;
            match &o.icon {
                Some(Icon::Png(png)) => return Ok((png.clone(), "image/png")),
                Some(Icon::Path(p)) => apps::icon_file(p),
                Some(Icon::Name(n)) => apps::named_icon(n),
                None => None,
            }
            .or_else(|| apps::launcher_icon(o.desktop_entry.as_deref()?))
        };
        let (path, mime) = found.ok_or(ApiError::NotFound)?;
        tokio::task::spawn_blocking(move || std::fs::read(path).map(|b| (b, mime)).map_err(|e| ApiError::Internal(e.to_string())))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
    }
}
