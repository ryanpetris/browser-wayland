use std::{sync::Arc, time::Duration};

use axum::extract::ws::{Message, WebSocket};
use bw_core::{AxisSource, Bytes, Codec, Command, Event, OutputGeometry, StreamMsg};
use tokio::sync::mpsc;

use crate::{App, Viewer, protocol::{self, ClientMsg}};

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
            StreamMsg::Audio { pts_us, data } => {
                if let Some(tx) = &v.audio_tx {
                    let _ = tx.try_send(protocol::audio(pts_us, &data)); // a dropped packet is a 20 ms glitch
                }
            }
            StreamMsg::Failed => {
                v.need_key = true;
                drop(v);
                app.rekey(); // drops the dead pipeline and forces a frame, which rebuilds it
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
                        // Viewer is slow: never send a delta after a gap. Ask for a keyframe once per gap,
                        // and again only if the keyframe itself had to be dropped.
                        tracing::debug!(keyframe = f.keyframe, "viewer queue full, frame dropped");
                        let ask = !v.need_key || f.keyframe;
                        v.need_key = true;
                        drop(v);
                        if ask {
                            app.rekey();
                        }
                    }
                }
            }
        }
    }
}

/// Compositor events (cursor changes) to the current viewer.
pub async fn forward_events(app: Arc<App>, mut rx: mpsc::UnboundedReceiver<Event>) {
    while let Some(ev) = rx.recv().await {
        let (msg, tx) = {
            let mut v = app.viewer.lock().unwrap();
            let msg = match ev {
                Event::Cursor(img) => {
                    let msg = protocol::cursor(img.as_ref());
                    v.cursor = Some(msg.clone());
                    msg
                }
                Event::PointerLock(locked) => {
                    v.locked = locked;
                    Bytes::from(vec![protocol::POINTER_LOCK, locked as u8])
                }
            };
            (msg, v.tx.clone())
        };
        // State, not a frame: wait for room rather than drop it. A replaced viewer's sender just fails.
        if let Some(tx) = tx {
            let _ = tx.send(msg).await;
        }
    }
}

pub async fn session(mut socket: WebSocket, app: Arc<App>) {
    let (tx, mut rx) = mpsc::channel::<Bytes>(8);
    let (atx, mut arx) = mpsc::channel::<Bytes>(4);
    // Taking over drops the previous viewer's only sender, which ends its session below.
    let (my_gen, cursor, locked) = {
        let mut v = app.viewer.lock().unwrap();
        if v.tx.is_some() {
            let _ = app.commands.send(Command::ReleaseAllInput); // whatever the old viewer still held
        }
        v.generation += 1;
        v.tx = Some(tx);
        v.audio_tx = Some(atx);
        v.announced = None;
        v.need_key = true;
        (v.generation, v.cursor.clone(), v.locked)
    };
    if let Some(c) = cursor {
        let _ = socket.send(Message::Binary(c)).await;
    }
    if locked {
        let _ = socket.send(Message::Binary(Bytes::from(vec![protocol::POINTER_LOCK, 1]))).await;
    }
    // Frames start once Hello has picked the codec (see command_for).

    let mut ping = tokio::time::interval(Duration::from_secs(5));
    let mut unanswered = 0;
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(b) => if socket.send(Message::Binary(b)).await.is_err() { break },
                None => break, // replaced by a newer viewer
            },
            Some(b) = arx.recv() => if socket.send(Message::Binary(b)).await.is_err() { break },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    // Hold the viewer lock from the generation check through the send, so a takeover
                    // can't slip its ReleaseAllInput in between.
                    let mut v = app.viewer.lock().unwrap();
                    if v.generation != my_gen {
                        break; // replaced: stop acting on this socket at all
                    }
                    let (cmd, rekey) = protocol::decode(&b).map(|m| app.command_for(m, &mut v)).unwrap_or((None, false));
                    if let Some(cmd) = cmd {
                        let _ = app.commands.send(cmd);
                    }
                    drop(v);
                    if rekey {
                        app.rekey();
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
        v.audio_tx = None;
        drop(v);
        let _ = app.commands.send(Command::ReleaseAllInput);
        let _ = app.commands.send(Command::ViewerDisconnected);
    }
}

impl App {
    /// Ask for a keyframe. The compositor only renders on damage, so also force a frame.
    pub fn rekey(&self) {
        self.control.request_keyframe();
        let _ = self.commands.send(Command::RequestFullFrame);
    }

    /// Pick the codec for a browser whose `hw` mask passed the prefer-hardware probe and `sw` the plain one
    /// (bit0 H.264, bit1 HEVC, bit2 VP9).
    fn choose_codec(&self, hw: u8, sw: u8) -> Codec {
        let bit = |c: Codec| match c {
            Codec::H264 => 1,
            Codec::Hevc => 2,
            Codec::Vp9 => 4,
        };
        let preferred = [Codec::Hevc, Codec::Vp9, Codec::H264];
        match self.policy {
            Some(c) if sw & bit(c) != 0 => c,
            Some(c) => {
                tracing::warn!(?c, "browser can't decode the requested codec; using H.264");
                Codec::H264
            }
            None => preferred
                .into_iter()
                .find(|&c| hw & bit(c) != 0)
                .or_else(|| preferred.into_iter().find(|&c| sw & bit(c) != 0))
                .unwrap_or(Codec::H264),
        }
    }

    /// Runs with the viewer lock held; returns the command to forward and whether to rekey after unlocking.
    fn command_for(&self, m: ClientMsg, v: &mut Viewer) -> (Option<Command>, bool) {
        let cmd = match m {
            ClientMsg::Hello { hw, sw } => {
                let codec = self.choose_codec(hw, sw);
                tracing::info!(?codec, hw, sw, "viewer codec");
                self.control.set_codec(codec); // non-blocking: the old pipeline is dropped elsewhere
                v.need_key = true;
                return (Some(Command::ViewerConnected), true);
            }
            // dpr bounds keep a bogus value from turning into a giant dmabuf allocation
            ClientMsg::Resize { css_w, css_h, dpr } if (0.5..=8.0).contains(&dpr) => {
                Command::Resize(geometry(css_w, css_h, dpr as f64))
            }
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
            ClientMsg::PointerLockLost => Command::ReleasePointerLock,
            ClientMsg::RequestKeyframe => {
                v.need_key = true;
                return (None, true);
            }
            ClientMsg::Resize { .. } => return (None, false),
        };
        (Some(cmd), false)
    }
}

/// CSS size × devicePixelRatio, rounded down to even (4:2:0 encoders), capped at 8K.
fn geometry(css_w: u16, css_h: u16, dpr: f64) -> OutputGeometry {
    let px = |css: u16| (((css as f64 * dpr).round() as u32).min(8192) & !1).max(2);
    OutputGeometry { width_px: px(css_w), height_px: px(css_h), scale: dpr, refresh_mhz: 60_000 }
}
