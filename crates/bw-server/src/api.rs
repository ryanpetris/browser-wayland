//! The operations behind the HTTP API and the MCP tools, implemented once. Routes and tools only
//! translate requests in and results or errors out.

use std::time::Duration;

use anyhow::Result;
use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bw_core::{AxisSource, Command, ControlMsg, InputMsg, Snapshot, SnapshotReply, WindowInfo};

use crate::{App, elements::Page};

#[derive(Debug)]
pub enum ApiError {
    /// The feature is switched off (`--elements`).
    Disabled(&'static str),
    /// No such window.
    NotFound,
    /// Another snapshot is in flight.
    Busy,
    /// The compositor or the accessibility bus didn't answer.
    Unavailable(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::Disabled(_) => StatusCode::NOT_IMPLEMENTED,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::Busy => StatusCode::TOO_MANY_REQUESTS,
            ApiError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Disabled(what) => f.write_str(what),
            ApiError::NotFound => f.write_str("no such window"),
            ApiError::Busy => f.write_str("another snapshot is in flight"),
            ApiError::Unavailable(why) => f.write_str(why),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status(), [(header::CACHE_CONTROL, "no-store")], Json(serde_json::json!({ "error": self.to_string() }))).into_response()
    }
}

impl App {
    /// The window list the viewer was last sent.
    pub fn windows(&self) -> Vec<WindowInfo> {
        self.viewer.lock().unwrap().window_list.clone()
    }

    /// One window and the current output scale.
    fn window(&self, id: u64) -> Result<(WindowInfo, f64), ApiError> {
        let v = self.viewer.lock().unwrap();
        let win = v.window_list.iter().find(|w| w.id == id).cloned().ok_or(ApiError::NotFound)?;
        Ok((win, v.info.as_ref().map_or(1.0, |i| i.scale)))
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
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<Snapshot>>();
        let reply = SnapshotReply(Box::new(move |s| {
            let _ = tx.send(s);
        }));
        self.send(Command::Snapshot { id, scale, reply })?;
        let snap = match tokio::time::timeout(Duration::from_secs(2), rx).await {
            Ok(Ok(Some(s))) => s,
            Ok(Ok(None)) => return Err(ApiError::NotFound),
            _ => return Err(ApiError::Unavailable("the compositor didn't answer".into())),
        };
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
            Ok(Err(e)) => Err(ApiError::Unavailable(format!("png: {e}"))),
            Err(e) => Err(ApiError::Unavailable(format!("png: {e}"))),
        }
    }

    /// A window action or spawn. Fire-and-forget: the compositor ignores unknown ids and impossible requests.
    pub fn control(&self, msg: ControlMsg) -> Result<(), ApiError> {
        self.send(Command::Control(msg))
    }

    /// Pointer and keyboard input; `window` makes coordinates relative to that window's geometry.
    pub fn input(&self, msg: InputMsg) -> Result<(), ApiError> {
        let at = |x: f64, y: f64, window: Option<u64>| -> Result<(f64, f64), ApiError> {
            match window {
                Some(id) => self.window(id).map(|(w, _)| (w.x as f64 + x, w.y as f64 + y)),
                None => Ok((x, y)),
            }
        };
        let mut cmds = Vec::new();
        match msg {
            InputMsg::Move { x, y, window } => {
                let (x, y) = at(x, y, window)?;
                cmds.push(Command::PointerMotionAbsolute { x, y });
            }
            InputMsg::Click { x, y, window, button, count } => {
                let (x, y) = at(x, y, window)?;
                cmds.push(Command::PointerMotionAbsolute { x, y });
                for _ in 0..count.unwrap_or(1).clamp(1, 3) {
                    cmds.push(Command::PointerButton { button: button.code(), pressed: true });
                    cmds.push(Command::PointerButton { button: button.code(), pressed: false });
                }
            }
            InputMsg::Button { button, pressed } => cmds.push(Command::PointerButton { button: button.code(), pressed }),
            // the same units as the viewer's wheel: 15 logical px and 120 "v120" per line
            InputMsg::Scroll { dx, dy } => cmds.push(Command::PointerAxis {
                source: AxisSource::Wheel,
                dx: dx * 15.0,
                dy: dy * 15.0,
                v120: Some(((dx * 120.0) as i32, (dy * 120.0) as i32)),
            }),
            InputMsg::Key { keys } => cmds.push(Command::Chord(keys.split('+').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect())),
            InputMsg::Text { text } => cmds.push(Command::Text(text)),
        }
        cmds.into_iter().try_for_each(|c| self.send(c))
    }

    fn send(&self, cmd: Command) -> Result<(), ApiError> {
        self.commands.send(cmd).map_err(|_| ApiError::Unavailable("the compositor is gone".into()))
    }
}
