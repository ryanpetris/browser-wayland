//! The operations behind the HTTP API and the MCP tools, implemented once. Routes and tools only
//! translate requests in and results or errors out.

use std::time::Duration;

use anyhow::Result;
use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bw_core::{Bytes, Command, ControlMsg, ControlOp, InputMsg, Snapshot, SnapshotError, SnapshotReply, WindowInfo};

/// The clipboard mimes the bridge carries: text (offered to clients under every text mime) and PNG.
pub const TEXT: &str = "text/plain;charset=utf-8";
pub const PNG: &str = "image/png";

pub fn clipboard_limit(mime: &str) -> usize {
    if mime == PNG { 16 << 20 } else { 1 << 20 }
}

use crate::{App, apps, elements::Page};

#[derive(Debug)]
pub enum ApiError {
    /// The feature is switched off (`--elements`).
    Disabled(&'static str),
    /// The presented token isn't the current one (rotation).
    Unauthorized,
    /// The viewer token: it can look, not act.
    Forbidden,
    /// No such window.
    NotFound,
    /// No such application (`launch`, icons).
    NoSuchApp,
    /// Another snapshot is in flight.
    Busy,
    /// The compositor or the accessibility bus didn't answer.
    Unavailable(String),
    /// The request body is over the limit.
    TooLarge,
    /// Something on our side broke (a GL step of a snapshot, PNG encoding).
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::Disabled(_) => StatusCode::NOT_IMPLEMENTED,
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden => StatusCode::FORBIDDEN,
            ApiError::NotFound | ApiError::NoSuchApp => StatusCode::NOT_FOUND,
            ApiError::Busy => StatusCode::TOO_MANY_REQUESTS,
            ApiError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Disabled(what) => f.write_str(what),
            ApiError::Unauthorized => f.write_str("not the current token"),
            ApiError::Forbidden => f.write_str("read-only token"),
            ApiError::NotFound => f.write_str("no such window"),
            ApiError::NoSuchApp => f.write_str("no such application"),
            ApiError::Busy => f.write_str("another snapshot is in flight"),
            ApiError::TooLarge => f.write_str("over the size limit"),
            ApiError::Unavailable(why) | ApiError::Internal(why) => f.write_str(why),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status(), [(header::CACHE_CONTROL, "no-store")], Json(serde_json::json!({ "error": self.to_string() }))).into_response()
    }
}

pub const X11_EDGE: &str = "That part of the window is past the desktop's edge, where X11 programs can't take clicks: move or shrink the window, or enlarge the desktop.";

impl App {
    /// The window list the viewers were last sent.
    pub fn windows(&self) -> Vec<WindowInfo> {
        self.viewers.lock().unwrap().window_list.clone()
    }

    /// One window and the current output scale.
    fn window(&self, id: u64) -> Result<(WindowInfo, f64), ApiError> {
        let v = self.viewers.lock().unwrap();
        let win = v.window_list.iter().find(|w| w.id == id).cloned().ok_or(ApiError::NotFound)?;
        Ok((win, v.output.scale))
    }

    /// The window's UI elements from its accessibility tree (see `elements.rs`).
    pub async fn elements(&self, id: u64) -> Result<Page, ApiError> {
        if !self.elements {
            return Err(ApiError::Disabled("started without --elements"));
        }
        let (win, scale) = self.window(id)?;
        match tokio::time::timeout(Duration::from_secs(2), crate::elements::elements(&win, scale)).await {
            Ok(Ok(page)) => Ok(page),
            Ok(Err(e)) => Err(ApiError::Unavailable(format!("{e:#}"))),
            Err(_) => Err(ApiError::Unavailable("timed out reading the tree".into())),
        }
    }

    /// PNG of one window (`scale` 0.05..=2 relative to the output scale) or, with `None`, of the whole output.
    pub async fn snapshot(&self, id: Option<u64>, scale: f64) -> Result<Vec<u8>, ApiError> {
        // One at a time: the compositor renders these on its own thread and a queued request can't be cancelled.
        let Ok(_busy) = self.snapshot_lock.try_acquire() else { return Err(ApiError::Busy) };
        let scale = scale.clamp(0.05, 2.0);
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Snapshot, SnapshotError>>();
        let reply = SnapshotReply(Box::new(move |s| {
            let _ = tx.send(s);
        }));
        self.send(Command::Snapshot { id, scale, reply })?;
        let snap = match tokio::time::timeout(Duration::from_secs(2), rx).await {
            Ok(Ok(Ok(s))) => s,
            Ok(Ok(Err(SnapshotError::NoSuchWindow))) => return Err(ApiError::NotFound),
            Ok(Ok(Err(SnapshotError::Render(e)))) => return Err(ApiError::Internal(e)),
            _ => return Err(ApiError::Unavailable("the compositor didn't answer".into())),
        };
        encode_png(snap).await
    }

    /// The snapshot scale that fits a window's (or, with `None`, the output's) long side in `px` device pixels; at most 1.
    pub fn fit_scale(&self, id: Option<u64>, px: f64) -> f64 {
        let long_side = match id {
            Some(id) => self.window(id).map(|(w, scale)| w.w.max(w.h) as f64 * scale).ok(),
            None => Some({
                let out = self.viewers.lock().unwrap().output;
                out.width_px.max(out.height_px) as f64
            }),
        };
        long_side.map_or(1.0, |side| (px / side.max(1.0)).min(1.0))
    }

    /// What was last put on the clipboard, by an application, the browser or the API: its mime and bytes.
    pub fn clipboard(&self) -> Option<(String, Bytes)> {
        self.viewers.lock().unwrap().clipboard.clone()
    }

    /// Text (`TEXT`) or a PNG becomes the desktop clipboard; fire-and-forget like control. The compositor
    /// reports it back like any clipboard change, which is what `clipboard()` and the viewers then see.
    pub fn set_clipboard(&self, mime: &str, data: Bytes) -> Result<(), ApiError> {
        if data.len() > clipboard_limit(mime) {
            return Err(ApiError::TooLarge);
        }
        self.send(Command::SetClipboard { mime: mime.to_string(), data: data.to_vec() })
    }

    /// A window action, spawn, launch or quit. Fire-and-forget: the compositor ignores unknown ids and
    /// impossible requests.
    pub fn control(&self, msg: ControlMsg) -> Result<(), ApiError> {
        let cmd = self.command_for(msg)?;
        self.send(cmd)
    }

    /// The compositor's command for a control request: a launcher becomes its Exec line, quit ends the
    /// desktop, the rest is the window action itself.
    pub fn command_for(&self, msg: ControlMsg) -> Result<Command, ApiError> {
        Ok(match msg.op {
            ControlOp::Launch { app } => Command::Control(ControlMsg { id: 0, op: ControlOp::Spawn { cmd: apps::exec(&app).ok_or(ApiError::NoSuchApp)? } }),
            ControlOp::Quit => Command::Quit,
            _ => Command::Control(msg),
        })
    }

    /// Xwayland's screen is the output, and the X server pins its pointer to it: a click on the part of an
    /// X11 window that hangs past the output's edge lands on the edge instead. Says so for such a click
    /// (one inside the client's own area; the title bar and resize band around it are ours).
    pub fn x11_edge_warning(&self, window: u64, x: f64, y: f64) -> Option<&'static str> {
        let v = self.viewers.lock().unwrap();
        let w = v.window_list.iter().find(|w| w.id == window).filter(|w| w.x11)?;
        let inside = (0.0..w.w as f64).contains(&x) && (0.0..w.h as f64).contains(&y);
        let (ax, ay) = (w.x as f64 + x, w.y as f64 + y);
        let (ow, oh) = (v.output.width_px as f64 / v.output.scale, v.output.height_px as f64 / v.output.scale);
        (inside && (ax < 0.0 || ay < 0.0 || ax >= ow || ay >= oh)).then_some(X11_EDGE)
    }

    /// The installed applications, by name. A few hundred small files: read off the async workers.
    pub async fn applications(&self) -> Vec<apps::AppInfo> {
        tokio::task::spawn_blocking(apps::list).await.unwrap_or_default()
    }

    /// An application's icon as bytes and media type.
    pub async fn application_icon(&self, id: String) -> Result<(Vec<u8>, &'static str), ApiError> {
        tokio::task::spawn_blocking(move || {
            let (path, mime) = apps::icon(&id).ok_or(ApiError::NoSuchApp)?;
            Ok((std::fs::read(path).map_err(|e| ApiError::Internal(e.to_string()))?, mime))
        })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    }

    /// A window's icon as bytes and media type: the name its client set, else the pixels it set, else
    /// its launcher's icon.
    pub async fn window_icon(&self, id: u64) -> Result<(Vec<u8>, &'static str), ApiError> {
        let (WindowInfo { icon, app_id, .. }, _) = self.window(id)?;
        if let Some(name) = icon
            && let Some(found) = read_icon(move || apps::named_icon(&name)).await?
        {
            return Ok(found);
        }
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Snapshot, SnapshotError>>();
        let reply = SnapshotReply(Box::new(move |s| {
            let _ = tx.send(s);
        }));
        self.send(Command::WindowIcon { id, reply })?;
        match tokio::time::timeout(Duration::from_secs(2), rx).await {
            Ok(Ok(Ok(snap))) => Ok((encode_png(snap).await?, "image/png")),
            Ok(Ok(Err(SnapshotError::NoSuchWindow))) => read_icon(move || apps::launcher_icon(&app_id)).await?.ok_or(ApiError::NotFound),
            Ok(Ok(Err(SnapshotError::Render(e)))) => Err(ApiError::Internal(e)),
            _ => Err(ApiError::Unavailable("the compositor didn't answer".into())),
        }
    }

    /// Pointer and keyboard input. `window` makes coordinates relative to that window's geometry; the
    /// compositor resolves it against the live geometry, this only answers 404 for an unknown id.
    pub fn input(&self, msg: InputMsg) -> Result<(), ApiError> {
        if let InputMsg::Move { window: Some(id), .. } | InputMsg::Click { window: Some(id), .. } = &msg {
            self.window(*id)?;
        }
        self.send(Command::Input(msg))
    }

    pub(crate) fn send(&self, cmd: Command) -> Result<(), ApiError> {
        self.commands.send(cmd).map_err(|_| ApiError::Unavailable("the compositor is gone".into()))
    }
}

/// An icon file found by `find`, read on the blocking pool.
async fn read_icon(find: impl FnOnce() -> Option<(std::path::PathBuf, &'static str)> + Send + 'static) -> Result<Option<(Vec<u8>, &'static str)>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let Some((path, mime)) = find() else { return Ok(None) };
        Ok(Some((std::fs::read(path).map_err(|e| ApiError::Internal(e.to_string()))?, mime)))
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
}

/// Straight-alpha RGBA rows to PNG, on the blocking pool.
pub(crate) async fn encode_png(snap: Snapshot) -> Result<Vec<u8>, ApiError> {
    let png = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, snap.width, snap.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&snap.rgba)?;
        Ok(out)
    })
    .await;
    match png {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(e)) => Err(ApiError::Internal(format!("png: {e}"))),
        Err(e) => Err(ApiError::Internal(format!("png: {e}"))),
    }
}
