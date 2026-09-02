use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use bw_core::{AxisSource, Bytes, Command, OutputGeometry, StreamMsg};
use tokio::sync::mpsc;

use crate::{App, protocol::{self, ClientMsg}};

/// Forwards encoder output to whoever is the current viewer.
pub async fn distribute(app: Arc<App>, mut rx: mpsc::Receiver<StreamMsg>) {
    while let Some(msg) = rx.recv().await {
        let mut v = app.viewer.lock().unwrap();
        match msg {
            StreamMsg::Info(info) => {
                if let Some(tx) = &v.tx {
                    let _ = tx.try_send(protocol::config(&info));
                }
                v.info = Some(info);
                v.need_key = true;
            }
            StreamMsg::Frame(f) => {
                let Some(tx) = &v.tx else { continue };
                if v.need_key && !f.keyframe {
                    continue;
                }
                match tx.try_send(protocol::video(&f)) {
                    Ok(()) => v.need_key = false,
                    Err(_) => {
                        // Viewer is slow: drop until the next keyframe, never send a delta after a gap.
                        v.need_key = true;
                        drop(v);
                        app.rekey();
                    }
                }
            }
        }
    }
}

pub async fn session(mut socket: WebSocket, app: Arc<App>) {
    let (tx, mut rx) = mpsc::channel::<Bytes>(8);
    let config = {
        let mut v = app.viewer.lock().unwrap();
        v.tx = Some(tx.clone()); // replaces (and thereby closes) any previous viewer
        v.need_key = true;
        v.info.as_ref().map(protocol::config)
    };
    let _ = app.commands.send(Command::ViewerConnected);
    if let Some(c) = config {
        let _ = socket.send(Message::Binary(c)).await;
    }
    app.rekey();

    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(b) => if socket.send(Message::Binary(b)).await.is_err() { break },
                None => break, // replaced by another viewer
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if let Some(cmd) = protocol::decode(&b).and_then(|m| app.command_for(m)) {
                        let _ = app.commands.send(cmd);
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
        }
    }

    let mut v = app.viewer.lock().unwrap();
    if v.tx.as_ref().is_some_and(|t| t.same_channel(&tx)) {
        v.tx = None;
    }
    drop(v);
    let _ = app.commands.send(Command::ReleaseAllKeys);
    let _ = app.commands.send(Command::ViewerDisconnected);
}

impl App {
    /// Ask for a keyframe. The compositor only renders on damage, so also force a frame.
    pub fn rekey(&self) {
        (self.request_keyframe)();
        let _ = self.commands.send(Command::RequestFullFrame);
    }

    fn command_for(&self, m: ClientMsg) -> Option<Command> {
        Some(match m {
            ClientMsg::Resize { css_w, css_h, dpr } => Command::Resize(geometry(css_w, css_h, dpr as f64)),
            ClientMsg::MotionAbs { x, y } => Command::PointerMotionAbsolute { x: x as f64, y: y as f64 },
            ClientMsg::MotionRel { dx, dy } => Command::PointerMotionRelative { dx: dx as f64, dy: dy as f64 },
            ClientMsg::Button { button, pressed } => Command::PointerButton { button: button as u32, pressed },
            ClientMsg::Axis { mode: 1, dx, dy } => Command::PointerAxis {
                source: AxisSource::Wheel,
                dx: dx as f64 * 15.0,
                dy: dy as f64 * 15.0,
                v120: Some(((dx * 120.0) as i32, (dy * 120.0) as i32)),
                stop: false,
            },
            // ponytail: pixel (and page) deltas go out as finger scroll with no axis_stop;
            // add a stop timer if clients need kinetic scrolling.
            ClientMsg::Axis { dx, dy, .. } => Command::PointerAxis {
                source: AxisSource::Finger,
                dx: dx as f64,
                dy: dy as f64,
                v120: None,
                stop: false,
            },
            ClientMsg::Key { evdev, pressed } => Command::Key { evdev: evdev as u32, pressed },
            ClientMsg::Blur => Command::ReleaseAllKeys,
            ClientMsg::RequestKeyframe => {
                self.viewer.lock().unwrap().need_key = true;
                self.rekey();
                return None;
            }
        })
    }
}

/// CSS size × devicePixelRatio, rounded down to even (4:2:0 encoders).
fn geometry(css_w: u16, css_h: u16, dpr: f64) -> OutputGeometry {
    let px = |css: u16| ((css as f64 * dpr).round() as u32 & !1).max(2);
    OutputGeometry { width_px: px(css_w), height_px: px(css_h), scale: dpr, refresh_mhz: 60_000 }
}
