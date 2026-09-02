use std::{sync::Arc, time::Duration};

use axum::extract::ws::{Message, WebSocket};
use bw_core::{AxisSource, Bytes, Command, Event, OutputGeometry, StreamMsg};
use tokio::sync::mpsc;

use crate::{App, protocol::{self, ClientMsg}};

/// Forwards encoder output to whoever is the current viewer.
pub async fn distribute(app: Arc<App>, mut rx: mpsc::Receiver<StreamMsg>) {
    while let Some(msg) = rx.recv().await {
        let mut v = app.viewer.lock().unwrap();
        match msg {
            StreamMsg::Info(info) => {
                v.info = Some(info);
                v.announced = None;
                v.need_key = true;
            }
            StreamMsg::Frame(f) => {
                let (Some(tx), Some(info)) = (v.tx.clone(), v.info.clone()) else { continue };
                if f.stream_id != info.stream_id {
                    continue; // output of a pipeline that has since been rebuilt
                }
                if v.need_key && !f.keyframe {
                    continue; // a keyframe request is outstanding
                }
                // Config must reach the viewer before the first frame of its stream.
                if v.announced != Some(f.stream_id) {
                    if tx.try_send(protocol::config(&info)).is_err() {
                        drop(v);
                        app.rekey();
                        continue;
                    }
                    v.announced = Some(f.stream_id);
                }
                match tx.try_send(protocol::video(&f)) {
                    Ok(()) => v.need_key = false,
                    Err(_) => {
                        // Viewer is slow: never send a delta after a gap.
                        v.need_key = true;
                        drop(v);
                        app.rekey();
                    }
                }
            }
        }
    }
}

/// Compositor events (cursor changes) to the current viewer.
pub async fn forward_events(app: Arc<App>, mut rx: mpsc::UnboundedReceiver<Event>) {
    while let Some(ev) = rx.recv().await {
        let Event::Cursor(img) = ev;
        let msg = protocol::cursor(img.as_ref());
        let mut v = app.viewer.lock().unwrap();
        if let Some(tx) = &v.tx {
            let _ = tx.try_send(msg.clone());
        }
        v.cursor = Some(msg);
    }
}

pub async fn session(mut socket: WebSocket, app: Arc<App>) {
    let (tx, mut rx) = mpsc::channel::<Bytes>(8);
    // Taking over drops the previous viewer's only sender, which ends its session below.
    let (my_gen, cursor) = {
        let mut v = app.viewer.lock().unwrap();
        v.generation += 1;
        v.tx = Some(tx);
        v.announced = None;
        v.need_key = true;
        (v.generation, v.cursor.clone())
    };
    if let Some(c) = cursor {
        let _ = socket.send(Message::Binary(c)).await;
    }
    let _ = app.commands.send(Command::ViewerConnected);
    app.rekey();

    let mut ping = tokio::time::interval(Duration::from_secs(5));
    let mut unanswered = 0;
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(b) => if socket.send(Message::Binary(b)).await.is_err() { break },
                None => break, // replaced by a newer viewer
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if let Some(cmd) = protocol::decode(&b).and_then(|m| app.command_for(m)) {
                        if app.viewer.lock().unwrap().generation == my_gen {
                            let _ = app.commands.send(cmd);
                        }
                    }
                }
                Some(Ok(Message::Pong(_))) => unanswered = 0,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
            _ = ping.tick() => {
                if unanswered >= 3 || socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break; // dead peer
                }
                unanswered += 1;
            }
        }
    }

    let mut v = app.viewer.lock().unwrap();
    if v.generation == my_gen {
        v.tx = None;
        drop(v);
        let _ = app.commands.send(Command::ReleaseAllInput);
        let _ = app.commands.send(Command::ViewerDisconnected);
    }
}

impl App {
    /// Ask for a keyframe. The compositor only renders on damage, so also force a frame.
    pub fn rekey(&self) {
        (self.request_keyframe)();
        let _ = self.commands.send(Command::RequestFullFrame);
    }

    fn command_for(&self, m: ClientMsg) -> Option<Command> {
        Some(match m {
            // dpr bounds keep a bogus value from turning into a giant dmabuf allocation
            ClientMsg::Resize { css_w, css_h, dpr } if (0.5..=8.0).contains(&dpr) => {
                Command::Resize(geometry(css_w, css_h, dpr as f64))
            }
            ClientMsg::Resize { .. } => return None,
            ClientMsg::MotionAbs { x, y } => Command::PointerMotionAbsolute { x: x as f64, y: y as f64 },
            ClientMsg::MotionRel { dx, dy } => Command::PointerMotionRelative { dx: dx as f64, dy: dy as f64 },
            ClientMsg::Button { button, pressed } => Command::PointerButton { button: button as u32, pressed },
            ClientMsg::Axis { mode: 1, dx, dy } => Command::PointerAxis {
                source: AxisSource::Wheel,
                dx: dx as f64 * 15.0,
                dy: dy as f64 * 15.0,
                v120: Some(((dx * 120.0) as i32, (dy * 120.0) as i32)),
            },
            // ponytail: pixel (and page) deltas go out as finger scroll with no axis_stop;
            // add a stop timer if clients need kinetic scrolling.
            ClientMsg::Axis { dx, dy, .. } => Command::PointerAxis {
                source: AxisSource::Finger,
                dx: dx as f64,
                dy: dy as f64,
                v120: None,
            },
            ClientMsg::Key { evdev, pressed } => Command::Key { evdev: evdev as u32, pressed },
            ClientMsg::Blur => Command::ReleaseAllInput,
            ClientMsg::RequestKeyframe => {
                self.viewer.lock().unwrap().need_key = true;
                self.rekey();
                return None;
            }
        })
    }
}

/// CSS size × devicePixelRatio, rounded down to even (4:2:0 encoders), capped at 8K.
fn geometry(css_w: u16, css_h: u16, dpr: f64) -> OutputGeometry {
    let px = |css: u16| (((css as f64 * dpr).round() as u32).min(8192) & !1).max(2);
    OutputGeometry { width_px: px(css_w), height_px: px(css_h), scale: dpr, refresh_mhz: 60_000 }
}
