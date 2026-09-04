use std::{sync::Arc, time::Duration};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
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
                v.video_seq = 0; // a new stream starts its count over (the page resets on Config)
            }
            StreamMsg::Audio { pts_us, data } => {
                let seq = v.audio_seq;
                v.audio_seq = seq.wrapping_add(1);
                if let Some(tx) = &v.audio_tx {
                    let _ = tx.try_send(protocol::audio(pts_us, &data, seq)); // a dropped packet is a 20 ms glitch
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
                let seq = v.video_seq;
                v.video_seq = seq.wrapping_add(1);
                if v.need_key && !f.keyframe {
                    continue; // a keyframe request is outstanding; the page sees the gap in seq
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
                match tx.try_send(protocol::video(&f, seq)) {
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
    while let Some(mut ev) = rx.recv().await {
        // Window lists supersede each other: a slow viewer gets the newest one, not the whole history.
        while let Event::Windows(_) = ev {
            match rx.try_recv() {
                Ok(next @ Event::Windows(_)) => ev = next,
                _ => break,
            }
        }
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
                Event::Windows(list) => {
                    let msg = protocol::windows(&list);
                    v.windows = Some(msg.clone());
                    v.window_list = list;
                    msg
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

/// Close codes the page understands.
const UNAUTHORIZED: u16 = 4001;
const REPLACED: u16 = 4002;

pub async fn session(mut socket: WebSocket, app: Arc<App>) {
    // The first message must be AUTH with the token; until then this socket is nobody and can't
    // take the stream over. A wrong token, or five seconds of silence, ends it.
    let authed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.recv().await {
                Some(Ok(Message::Binary(b))) => return b.first() == Some(&protocol::AUTH) && app.token_ok(std::str::from_utf8(&b[1..]).unwrap_or("")),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return false,
                _ => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    if !authed {
        let _ = socket.send(Message::Close(Some(CloseFrame { code: UNAUTHORIZED, reason: "unauthorized".into() }))).await;
        return;
    }

    let (tx, mut rx) = mpsc::channel::<Bytes>(8);
    let (atx, mut arx) = mpsc::channel::<Bytes>(4);
    // Taking over drops the previous viewer's only sender, which ends its session below.
    let (my_gen, cursor, locked, windows) = {
        let mut v = app.viewer.lock().unwrap();
        if v.tx.is_some() {
            let _ = app.commands.send(Command::ReleaseAllInput); // whatever the old viewer still held
        }
        v.generation += 1;
        v.tx = Some(tx);
        v.audio_tx = Some(atx);
        v.announced = None;
        v.need_key = true;
        (v.generation, v.cursor.clone(), v.locked, v.windows.clone())
    };
    if let Some(c) = cursor {
        let _ = socket.send(Message::Binary(c)).await;
    }
    if let Some(w) = windows {
        let _ = socket.send(Message::Binary(w)).await;
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
                None => {
                    // replaced by a newer viewer: tell the page so it stops retrying
                    let _ = socket.send(Message::Close(Some(CloseFrame { code: REPLACED, reason: "replaced by another viewer".into() }))).await;
                    break;
                }
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
            ClientMsg::Axis { mode: 1, dx, dy } => Command::wheel(dx as f64, dy as f64),
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
            ClientMsg::Control(m) => Command::Control(m),
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
