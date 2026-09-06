//! Desktop notifications. With no panel there is no notification daemon, so elsewhere serves
//! `org.freedesktop.Notifications` on the session bus (unless another daemon owns the name) and shows
//! the notifications as toasts in every viewer; a viewer's click, action or dismissal goes back to the
//! application as the protocol's signals.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use elsewhere_core::{Command, ControlMsg, ControlOp, Snapshot};
use schemars::JsonSchema;
use serde::Serialize;
use zbus::{fdo::RequestNameFlags, interface, object_server::SignalEmitter, zvariant::OwnedValue};

use crate::{App, api::{ApiError, encode_png}, apps, protocol};

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// What a notification without its own timeout stays for.
const DEFAULT_TIMEOUT: u32 = 5000;

/// One notification, as the viewers and `GET /api/notifications` see it.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Notification {
    pub id: u32,
    /// counts up when the application replaces the notification under the same id
    pub rev: u64,
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
    File(PathBuf, &'static str),
    Png(Vec<u8>),
}

/// An open notification and what the API needs beyond its public shape.
pub struct Open {
    pub info: Notification,
    icon: Option<Icon>,
    /// the `desktop-entry` hint: which windows a click brings forward
    desktop_entry: Option<String>,
}

pub struct Daemon(pub Arc<App>);

#[interface(name = "org.freedesktop.Notifications")]
impl Daemon {
    fn get_capabilities(&self) -> Vec<&'static str> {
        vec!["actions", "body", "icon-static"]
    }

    fn get_server_information(&self) -> (&'static str, &'static str, &'static str, &'static str) {
        ("Elsewhere", "Elsewhere", self.0.version, "1.2")
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(&self, app_name: String, replaces_id: u32, app_icon: String, summary: String, body: String, actions: Vec<String>, hints: HashMap<String, OwnedValue>, expire_timeout: i32) -> u32 {
        let str_hint = |k: &str| hints.get(k).and_then(|v| <&str>::try_from(v).ok()).filter(|s| !s.is_empty());
        let desktop_entry = str_hint("desktop-entry").map(|s| s.trim_end_matches(".desktop").to_string());
        // the picture, in the specification's order of preference
        let pixels = ["image-data", "image_data"].iter().find_map(|k| hints.get(*k)).and_then(image_data);
        let icon = match pixels {
            Some(p) => encode_png(p).await.ok().map(Icon::Png),
            None => {
                let named: Vec<String> = [str_hint("image-path"), str_hint("image_path"), Some(app_icon.as_str())].into_iter().flatten().map(|s| s.trim_start_matches("file://").to_string()).collect();
                let entry = desktop_entry.clone();
                tokio::task::spawn_blocking(move || named.iter().find_map(|n| apps::icon_file(n)).or_else(|| apps::launcher_icon(entry.as_deref()?)).map(|(p, m)| Icon::File(p, m))).await.ok().flatten()
            }
        };
        let icon = match icon {
            None => match hints.get("icon_data").and_then(image_data) {
                Some(p) => encode_png(p).await.ok().map(Icon::Png),
                None => None,
            },
            some => some,
        };
        let actions: Vec<(String, String)> = actions.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect();
        let critical = hints.get("urgency").and_then(|v| u8::try_from(v).ok()) == Some(2);
        let timeout_ms = match expire_timeout {
            _ if critical => 0,
            t if t < 0 => DEFAULT_TIMEOUT,
            t => t as u32,
        };
        self.0.open_notification(replaces_id, app_name, summary, body, icon, actions, timeout_ms, desktop_entry)
    }

    async fn close_notification(&self, id: u32, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        if self.0.take_notification(id, None).is_none() {
            return Err(zbus::fdo::Error::Failed("no such notification".into()));
        }
        Self::notification_closed(&emitter, id, 3).await?; // 3: closed by a call
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
    let (w, h, stride, channels) = (usize::try_from(w).ok()?, usize::try_from(h).ok()?, usize::try_from(stride).ok()?, usize::try_from(channels).ok()?);
    if bps != 8 || w == 0 || h == 0 || channels != if alpha { 4 } else { 3 } || w.checked_mul(channels)? > stride || (h - 1).checked_mul(stride)?.checked_add(w * channels)? > data.len() {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in data.chunks(stride).take(h) {
        for p in row[..w * channels].chunks(channels) {
            rgba.extend_from_slice(&p[..3]);
            rgba.push(if alpha { p[3] } else { 255 });
        }
    }
    Some(Snapshot { width: w as u32, height: h as u32, rgba })
}

/// Own the notification name on the session bus and serve until the process ends. Another daemon
/// owning it, or no bus, means we're not the one showing notifications; nothing else changes.
pub async fn serve(app: Arc<App>) {
    let served = async {
        let conn = zbus::connection::Builder::session()?.serve_at(PATH, Daemon(app.clone()))?.build().await?;
        conn.request_name_with_flags(NAME, RequestNameFlags::DoNotQueue.into()).await?; // never queue behind an owner
        Ok::<_, zbus::Error>(conn)
    }
    .await;
    match served {
        Ok(conn) => {
            tracing::info!("notifications: serving {NAME}; applications' notifications appear in the viewers");
            let _ = app.notify_bus.set(conn);
        }
        Err(zbus::Error::NameTaken) => tracing::info!("notifications: another daemon owns {NAME}; leaving them to it"),
        Err(e) => tracing::debug!("notifications: not serving ({e})"),
    }
}

impl App {
    /// Store a notification (a nonzero `replaces_id` is the id, as the protocol requires), send every
    /// viewer the new list, arm its expiry.
    #[allow(clippy::too_many_arguments)]
    fn open_notification(self: &Arc<Self>, replaces_id: u32, app: String, summary: String, body: String, icon: Option<Icon>, actions: Vec<(String, String)>, timeout_ms: u32, desktop_entry: Option<String>) -> u32 {
        let mut open = self.notifications.lock().unwrap();
        let id = if replaces_id != 0 { replaces_id } else { self.next_notification.fetch_add(1, Ordering::Relaxed) };
        let rev = open.get(&id).map_or(0, |o| o.info.rev + 1);
        let info = Notification { id, rev, app, summary, body, icon: icon.is_some(), actions, timeout_ms };
        open.insert(id, Open { info, icon, desktop_entry });
        let msg = protocol::notifications(&sorted(&open));
        drop(open);
        self.broadcast(msg);
        if timeout_ms > 0 {
            let app = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(timeout_ms as u64)).await;
                app.close_notification(id, Some(rev), 1).await; // 1: expired
            });
        }
        id
    }

    /// Remove a notification (only the given revision, if one is given) and tell the viewers.
    fn take_notification(&self, id: u32, rev: Option<u64>) -> Option<Open> {
        let mut open = self.notifications.lock().unwrap();
        if rev.is_some_and(|r| open.get(&id).is_some_and(|o| o.info.rev != r)) {
            return None;
        }
        let taken = open.remove(&id)?;
        let msg = protocol::notifications(&sorted(&open));
        drop(open);
        self.broadcast(msg);
        Some(taken)
    }

    /// Close a notification with the protocol's reason (1 expired, 2 dismissed by the user, 3 by a call).
    async fn close_notification(&self, id: u32, rev: Option<u64>, reason: u32) {
        if self.take_notification(id, rev).is_some()
            && let Some(e) = self.emitter()
            && let Err(e) = Daemon::notification_closed(&e, id, reason).await
        {
            tracing::debug!("notification signal: {e}");
        }
    }

    fn emitter(&self) -> Option<SignalEmitter<'static>> {
        SignalEmitter::new(self.notify_bus.get()?, PATH).ok()
    }

    /// The open notifications, oldest first.
    pub fn notifications(&self) -> Vec<Notification> {
        sorted(&self.notifications.lock().unwrap())
    }

    /// A viewer acted on a notification: no action closes it (dismissed); an action key the
    /// application offered is invoked and closes it; `default` without such an action brings the
    /// application's newest window forward instead.
    pub async fn notification_action(&self, id: u32, action: Option<&str>) -> Result<(), ApiError> {
        let (rev, offered, names) = {
            let open = self.notifications.lock().unwrap();
            let o = open.get(&id).ok_or(ApiError::NotFound)?;
            let offered = action.is_some_and(|a| o.info.actions.iter().any(|(k, _)| k == a));
            (o.info.rev, offered, [o.desktop_entry.clone().unwrap_or_default().to_lowercase(), o.info.app.to_lowercase()])
        };
        if offered
            && let Some(e) = self.emitter()
            && let Err(e) = Daemon::action_invoked(&e, id, action.unwrap_or_default().to_string()).await
        {
            tracing::debug!("notification signal: {e}");
        } else if action == Some("default") {
            let target = self.viewers.lock().unwrap().window_list.iter().filter(|w| names.contains(&w.app_id.to_lowercase())).map(|w| w.id).max();
            if let Some(window) = target {
                let _ = self.send(Command::Control(ControlMsg { id: window, op: ControlOp::Activate }));
            }
        }
        self.close_notification(id, Some(rev), 2).await; // 2: dismissed by the user
        Ok(())
    }

    /// A notification's picture: what the application sent or named, else its launcher's icon.
    pub async fn notification_icon(&self, id: u32) -> Result<(Vec<u8>, &'static str), ApiError> {
        let (path, mime) = {
            let open = self.notifications.lock().unwrap();
            match &open.get(&id).ok_or(ApiError::NotFound)?.icon {
                Some(Icon::Png(png)) => return Ok((png.clone(), "image/png")),
                Some(Icon::File(p, m)) => (p.clone(), *m),
                None => return Err(ApiError::NotFound),
            }
        };
        tokio::task::spawn_blocking(move || std::fs::read(path).map(|b| (b, mime)).map_err(|e| ApiError::Internal(e.to_string())))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
    }
}

fn sorted(open: &HashMap<u32, Open>) -> Vec<Notification> {
    let mut list: Vec<Notification> = open.values().map(|o| o.info.clone()).collect();
    list.sort_by_key(|n| n.id);
    list
}
