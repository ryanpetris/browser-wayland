//! WebSocket sessions: the viewers of the desktop (any number, each with its own encoder; one of them,
//! the controller, drives the pointer and keyboard and sizes the output) and the per-window streams.

use std::{
    sync::{Arc, atomic::{AtomicU64, Ordering}},
    time::Duration,
};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use bw_core::{AxisSource, Bytes, Codec, Command, ControlMsg, ControlOp, Event, InputMsg, OutputGeometry, StreamMsg};
use tokio::sync::mpsc;

use crate::{App, Key, ViewerSession, Viewers, api, protocol::{self, ClientMsg, Role}};

/// Close codes the page understands.
const UNAUTHORIZED: u16 = 4001;
/// A stream that can't (re)start: no such window, the window closed, no encoder could be made.
const GONE: u16 = 4003;

/// The clients' Opus packets to every viewer; a dropped packet is a 20 ms glitch.
pub async fn distribute_audio(app: Arc<App>, mut rx: mpsc::Receiver<StreamMsg>) {
    while let Some(msg) = rx.recv().await {
        if let StreamMsg::Audio { pts_us, data } = msg {
            for s in app.viewers.lock().unwrap().sessions.values_mut() {
                let seq = s.audio_seq;
                s.audio_seq = seq.wrapping_add(1);
                let _ = s.audio.try_send(protocol::audio(pts_us, &data, seq));
            }
        }
    }
}

/// Compositor events (cursor, pointer lock, window list, clipboard) to every viewer and window session.
pub async fn forward_events(app: Arc<App>, mut rx: mpsc::UnboundedReceiver<Event>) {
    while let Some(mut ev) = rx.recv().await {
        // Window lists supersede each other: a slow viewer gets the newest one, not the whole history.
        while let Event::Windows(_) = ev {
            match rx.try_recv() {
                Ok(next @ Event::Windows(_)) => ev = next,
                _ => break,
            }
        }
        let mut v = app.viewers.lock().unwrap();
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
            Event::Clipboard { mime, data } => {
                let (msg, data) = if mime == api::PNG {
                    (protocol::clipboard_data(&mime), data)
                } else {
                    let text = String::from_utf8_lossy(&data).into_owned(); // a legacy STRING owner may hand us Latin-1
                    (protocol::clipboard(&text), Bytes::from(text))
                };
                v.clipboard = Some((mime, data));
                msg
            }
        };
        drop(v);
        app.broadcast(msg);
    }
}

/// The first message must be AUTH with a token; until then this socket is nobody. A wrong token, or
/// five seconds of silence, ends it. Returns the token the session came in with and which one it is.
async fn authenticate(socket: &mut WebSocket, app: &App) -> Option<(String, Key)> {
    let auth = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.recv().await {
                Some(Ok(Message::Binary(b))) => {
                    let t = std::str::from_utf8(b.get(1..).unwrap_or_default()).unwrap_or("");
                    return (b.first() == Some(&protocol::AUTH)).then(|| app.key_for(t).map(|k| (t.to_string(), k))).flatten();
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                _ => {}
            }
        }
    })
    .await
    .ok()
    .flatten();
    if auth.is_none() {
        let _ = socket.send(Message::Close(Some(CloseFrame { code: UNAUTHORIZED, reason: "unauthorized".into() }))).await;
    }
    auth
}

/// Hello, which picks the codec, before the pipeline exists; five seconds of silence ends the socket.
async fn hello(socket: &mut WebSocket) -> Option<(u8, u8)> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.recv().await? {
                Ok(Message::Binary(b)) => {
                    if let Some(ClientMsg::Hello { hw, sw }) = protocol::decode(&b) {
                        return Some((hw, sw));
                    }
                }
                Ok(Message::Close(_)) | Err(_) => return None,
                _ => {}
            }
        }
    })
    .await
    .ok()
    .flatten()
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
}

/// A send that gives up on a peer that stopped reading, so its session ends (and with it the encoder
/// waiting on it) instead of sitting on a full socket for good.
async fn send(socket: &mut WebSocket, msg: Bytes) -> bool {
    tokio::time::timeout(Duration::from_secs(10), socket.send(Message::Binary(msg))).await.is_ok_and(|r| r.is_ok())
}

/// A viewer of the desktop. The first one with a control token controls; the rest watch the same
/// desktop scaled to their own window, and one with a control token may take control.
pub async fn session(mut socket: WebSocket, app: Arc<App>) {
    let Some((token, key)) = authenticate(&mut socket, &app).await else { return };
    let Some((hw, sw)) = hello(&mut socket).await else { return };
    let (tx, mut rx) = mpsc::channel::<StreamMsg>(16);
    let (sink, control) = match (app.sinks)(tx) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("viewer stream: {e:#}");
            return close(&mut socket, GONE, "no encoder").await;
        }
    };
    control.set_codec(app.choose_codec(hw, sw));
    let (etx, mut erx) = mpsc::channel::<Bytes>(32);
    let (atx, mut arx) = mpsc::channel::<Bytes>(4);
    let notifications = protocol::notifications(&app.notifications()); // its own lock: never inside the viewers'
    let (id, replay) = {
        // registered under the lock a rotation clears, so a session that came in with an old token is
        // either cleared by it or refused here
        let mut v = app.viewers.lock().unwrap();
        if app.key_for(&token).is_none() {
            drop(v);
            return close(&mut socket, UNAUTHORIZED, "token rotated").await;
        }
        let id = v.next_id;
        v.next_id += 1;
        v.sessions.insert(id, ViewerSession { key, events: etx, audio: atx, audio_seq: 0, size: None, control });
        if key == Key::Control && v.controller.is_none() {
            v.controller = Some(id);
        }
        let replay: Vec<Bytes> = [v.cursor.clone(), v.windows.clone(), v.locked.then(|| Bytes::from(vec![protocol::POINTER_LOCK, 1])), Some(protocol::role(v.role_of(id))), Some(notifications.clone())].into_iter().flatten().collect();
        (id, replay)
    };
    for msg in replay {
        let _ = socket.send(Message::Binary(msg)).await;
    }
    let _ = app.commands.send(Command::ViewerStream { key: id, sink: Some(sink) });

    let (mut info, mut seq, mut failed) = (None::<bw_core::StreamInfo>, 0u16, false);
    let mut ping = tokio::time::interval(Duration::from_secs(5));
    let mut unanswered = 0;
    let ended = loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(StreamMsg::Info(i)) => {
                    seq = 0;
                    failed = false;
                    if !send(&mut socket, protocol::config(&i)).await { break None }
                    info = Some(i);
                }
                Some(StreamMsg::Frame(f)) => {
                    if info.as_ref().is_some_and(|i| i.stream_id == f.stream_id) {
                        // sent in order, waiting for the socket: the pipeline drops raw frames upstream of the encoder
                        // while we do, so nothing goes missing between encoder and page (no keyframe dance)
                        if !send(&mut socket, protocol::video(&f, seq)).await { break None }
                        seq = seq.wrapping_add(1);
                    }
                }
                // a dead pipeline is dropped and the next frame builds a new one; a rebuild that fails too
                // ends the session instead of looping, and the page reconnects
                Some(StreamMsg::Failed) if failed => break None,
                Some(StreamMsg::Failed) => {
                    failed = true;
                    if let Some(s) = app.viewers.lock().unwrap().sessions.get(&id) {
                        s.control.request_keyframe(); // drops the dead pipeline, and with it the leases the frame needs
                    }
                    let _ = app.commands.send(Command::RequestFullFrame);
                }
                None => break None,
                Some(StreamMsg::Audio { .. }) => {}
            },
            ev = erx.recv() => match ev {
                Some(b) => if !send(&mut socket, b).await { break None },
                None => break Some((UNAUTHORIZED, "token rotated")), // rotate_tokens dropped every session
            },
            Some(b) = arx.recv() => if !send(&mut socket, b).await { break None },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if app.key_for(&token).is_none() {
                        break Some((UNAUTHORIZED, "token rotated")); // a queued command must not get through after a rotation
                    }
                    match protocol::decode(&b) {
                        Some(ClientMsg::Notify(n)) if key == Key::Control => app.spawn_notification_action(n),
                        Some(m) => app.viewer_message(id, key, m),
                        None => {}
                    }
                }
                Some(Ok(Message::Pong(_))) => unanswered = 0,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break None,
                _ => {}
            },
            _ = ping.tick() => {
                if unanswered >= 3 || socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break None; // dead peer
                }
                unanswered += 1;
            }
        }
    };
    {
        let mut v = app.viewers.lock().unwrap();
        v.sessions.remove(&id);
        if v.controller == Some(id) {
            // the oldest remaining control-token session takes over
            let next = v.sessions.iter().filter(|(_, s)| s.key == Key::Control).map(|(id, _)| *id).min();
            app.set_controller(&mut v, next);
        }
    }
    let _ = app.commands.send(Command::ViewerStream { key: id, sink: None });
    if let Some((code, reason)) = ended {
        close(&mut socket, code, reason).await;
    }
}

/// One window as its own stream (`/ws/window/{id}`): the same messages as `/ws`, except that pointer
/// positions are relative to the window's geometry, a Resize resizes the window rather than the
/// output, there is no audio, and a control token drives regardless of who controls the desktop. Any
/// number of these can run; each has its own encoder, which the compositor stops when the session ends
/// or the window goes away.
pub async fn window_session(mut socket: WebSocket, app: Arc<App>, id: u64) {
    let Some((token, key)) = authenticate(&mut socket, &app).await else { return };
    if !app.viewers.lock().unwrap().window_list.iter().any(|w| w.id == id) {
        return close(&mut socket, GONE, "no such window").await;
    }
    let Some((hw, sw)) = hello(&mut socket).await else { return };
    let (tx, mut rx) = mpsc::channel::<StreamMsg>(16);
    let (sink, control) = match (app.sinks)(tx) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("window stream: {e:#}");
            return close(&mut socket, GONE, "no encoder").await;
        }
    };
    control.set_codec(app.choose_codec(hw, sw));
    static KEY: AtomicU64 = AtomicU64::new(1);
    let stream = KEY.fetch_add(1, Ordering::Relaxed);
    let (etx, mut erx) = mpsc::channel::<Bytes>(32);
    {
        let mut viewers = app.window_viewers.lock().unwrap();
        if app.key_for(&token).is_none() {
            drop(viewers);
            return close(&mut socket, UNAUTHORIZED, "token rotated").await;
        }
        viewers.insert(stream, etx.clone());
    }
    let replay: Vec<Bytes> = {
        let v = app.viewers.lock().unwrap();
        let role = if key == Key::Control { Role::Controller } else { Role::Viewer };
        [v.cursor.clone(), v.windows.clone(), v.locked.then(|| Bytes::from(vec![protocol::POINTER_LOCK, 1])), Some(protocol::role(role))].into_iter().flatten().collect()
    };
    for msg in replay {
        let _ = socket.send(Message::Binary(msg)).await;
    }
    let _ = app.commands.send(Command::WindowStream { key: stream, window: id, sink: Some(sink) });

    let (mut info, mut seq, mut failed) = (None::<bw_core::StreamInfo>, 0u16, false);
    let mut pointer = None; // the last window-relative position, for the edge notice
    let mut ping = tokio::time::interval(Duration::from_secs(5));
    let mut unanswered = 0;
    let ended = loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(StreamMsg::Info(i)) => {
                    seq = 0;
                    if !send(&mut socket, protocol::config(&i)).await { break None }
                    info = Some(i);
                }
                Some(StreamMsg::Frame(f)) => {
                    if info.as_ref().is_some_and(|i| i.stream_id == f.stream_id) {
                        if !send(&mut socket, protocol::video(&f, seq)).await { break None }
                        seq = seq.wrapping_add(1);
                    }
                }
                Some(StreamMsg::Failed) if failed => break None,
                Some(StreamMsg::Failed) => {
                    failed = true;
                    control.request_keyframe();
                    let _ = app.commands.send(Command::RequestFullFrame);
                }
                Some(StreamMsg::Audio { .. }) => {}
                None => break Some((GONE, "window closed")), // the compositor dropped the stream: the window is gone
            },
            ev = erx.recv() => match ev {
                Some(b) => if !send(&mut socket, b).await { break None },
                None => break Some((UNAUTHORIZED, "token rotated")),
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if app.key_for(&token).is_none() {
                        break Some((UNAUTHORIZED, "token rotated"));
                    }
                    let decoded = protocol::decode(&b);
                    if let Some(ClientMsg::MotionAbs { x, y }) = &decoded {
                        pointer = Some((*x as f64, *y as f64));
                    }
                    // a press on the part of an X11 window past the output's edge goes nowhere: say so,
                    // through the session's own queue so the press itself isn't held back
                    if let (Some(ClientMsg::Button { pressed: true, .. }), Some((x, y))) = (&decoded, pointer)
                        && key == Key::Control
                        && let Some(w) = app.x11_edge_warning(id, x, y)
                    {
                        let _ = etx.try_send(protocol::notice(w));
                    }
                    let cmd = match decoded {
                        Some(ClientMsg::RequestKeyframe) => {
                            control.request_keyframe();
                            Some(Command::RequestFullFrame)
                        }
                        Some(_) if key != Key::Control => None, // the viewer token watches
                        Some(ClientMsg::Control(m)) => app.command_for(m).ok(),
                        // window-relative, resolved against the live geometry on the compositor thread
                        Some(ClientMsg::MotionAbs { x, y }) => Some(Command::Input(InputMsg::Move { x: x as f64, y: y as f64, window: Some(id) })),
                        Some(ClientMsg::Resize { css_w, css_h, .. }) => Some(Command::Control(ControlMsg { id, op: ControlOp::Resize { w: css_w as i32, h: css_h as i32 } })),
                        Some(ClientMsg::SetClipboard(text)) => Some(Command::SetClipboard { mime: api::TEXT.into(), data: text.into() }),
                        Some(ClientMsg::Notify(n)) => {
                            app.spawn_notification_action(n);
                            None
                        }
                        Some(m) => input_command(m),
                        None => None,
                    };
                    if let Some(cmd) = cmd {
                        // under the lock a rotation clears, so nothing slips through behind one
                        let live = app.window_viewers.lock().unwrap();
                        if !live.contains_key(&stream) {
                            break Some((UNAUTHORIZED, "token rotated"));
                        }
                        let _ = app.commands.send(cmd);
                    }
                }
                Some(Ok(Message::Pong(_))) => unanswered = 0,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break None,
                _ => {}
            },
            _ = ping.tick() => {
                if unanswered >= 3 || socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break None;
                }
                unanswered += 1;
            }
        }
    };
    app.window_viewers.lock().unwrap().remove(&stream);
    let _ = app.commands.send(Command::WindowStream { key: stream, window: id, sink: None });
    if key == Key::Control {
        let _ = app.commands.send(Command::ReleaseAllInput);
    }
    if let Some((code, reason)) = ended {
        close(&mut socket, code, reason).await;
    }
}

impl Viewers {
    pub(crate) fn role_of(&self, id: u64) -> Role {
        if self.controller == Some(id) {
            Role::Controller
        } else if self.sessions.get(&id).is_some_and(|s| s.key == Key::Control) {
            Role::Participant
        } else {
            Role::Viewer
        }
    }
}

impl App {
    /// A viewer's click on a notification, answered on the bus in the background.
    fn spawn_notification_action(self: &Arc<Self>, n: protocol::NotifyMsg) {
        let app = self.clone();
        tokio::spawn(async move {
            let _ = app.notification_action(n.id, n.action.as_deref()).await;
        });
    }

    /// A state message to every viewer and window session.
    /// ponytail: a session that can't keep up misses a state change (it is dropped after ten seconds anyway)
    pub(crate) fn broadcast(&self, msg: Bytes) {
        for s in self.viewers.lock().unwrap().sessions.values() {
            let _ = s.events.try_send(msg.clone());
        }
        for tx in self.window_viewers.lock().unwrap().values() {
            let _ = tx.try_send(msg.clone());
        }
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

    /// A message from a viewer session: its size always counts (the output's if it controls, its own
    /// stream's scale otherwise); input only from the controller; window actions and the clipboard
    /// from any control token.
    fn viewer_message(&self, id: u64, key: Key, m: ClientMsg) {
        let mut v = self.viewers.lock().unwrap();
        if !v.sessions.contains_key(&id) {
            return; // a rotation cleared it under this lock; the session is about to end
        }
        let controls = v.controller == Some(id);
        let cmd = match m {
            // dpr bounds keep a bogus value from turning into a giant dmabuf allocation
            ClientMsg::Resize { css_w, css_h, dpr } if (0.5..=8.0).contains(&dpr) => {
                let geo = geometry(css_w, css_h, dpr as f64);
                if let Some(s) = v.sessions.get_mut(&id) {
                    s.size = Some(geo);
                }
                if controls {
                    v.output = geo;
                    self.retarget(&v);
                    Some(Command::Resize(geo))
                } else {
                    if let Some(s) = v.sessions.get(&id) {
                        s.control.set_size(Some(fit(&v.output, &geo)));
                    }
                    None
                }
            }
            ClientMsg::TakeControl => {
                if key == Key::Control {
                    self.set_controller(&mut v, Some(id));
                }
                None
            }
            ClientMsg::RequestKeyframe => {
                if let Some(s) = v.sessions.get(&id) {
                    s.control.request_keyframe();
                }
                Some(Command::RequestFullFrame)
            }
            ClientMsg::Control(m) if key == Key::Control => self.command_for(m).ok(),
            ClientMsg::SetClipboard(text) if key == Key::Control => Some(Command::SetClipboard { mime: api::TEXT.into(), data: text.into() }),
            m if controls => input_command(m),
            _ => None,
        };
        drop(v);
        if let Some(cmd) = cmd {
            let _ = self.commands.send(cmd);
        }
    }

    /// Hand control to `next` (none: nobody drives). The desktop takes the new controller's size, every
    /// stream is re-fitted, and the two sessions concerned learn their roles.
    fn set_controller(&self, v: &mut Viewers, next: Option<u64>) {
        let old = v.controller;
        if old == next {
            return;
        }
        v.controller = next;
        // whatever the old controller held; the application asks for its pointer lock again on the new one's click
        let _ = self.commands.send(Command::ReleaseAllInput);
        let _ = self.commands.send(Command::ReleasePointerLock);
        // targets first, so the frame the compositor renders for the new size finds them in place
        let size = next.and_then(|id| v.sessions.get(&id)).and_then(|s| s.size);
        if let Some(size) = size {
            v.output = size;
        }
        self.retarget(v);
        if let Some(size) = size {
            let _ = self.commands.send(Command::Resize(size));
        }
        for id in [old, next].into_iter().flatten() {
            if let Some(s) = v.sessions.get(&id) {
                let _ = s.events.try_send(protocol::role(v.role_of(id)));
            }
        }
    }

    /// Every viewer's encoder scales the output to that viewer's window; the controller's window is the
    /// output, so its encoder takes the frames as they are.
    // ponytail: set_size takes the sink's lock, which the compositor holds while it builds a pipeline
    // (~100 ms), under the viewers lock; hand the controls out as Arcs and call past the lock if that shows
    fn retarget(&self, v: &Viewers) {
        for (id, s) in &v.sessions {
            if let Some(size) = s.size {
                s.control.set_size((v.controller != Some(*id)).then(|| fit(&v.output, &size)));
            }
        }
    }
}

/// Pointer, keyboard and window messages as compositor commands (the ones any driving session sends the same way).
fn input_command(m: ClientMsg) -> Option<Command> {
    Some(match m {
        ClientMsg::MotionAbs { x, y } => Command::PointerMotionAbsolute { x: x as f64, y: y as f64 },
        ClientMsg::MotionRel { dx, dy } => Command::PointerMotionRelative { dx: dx as f64, dy: dy as f64 },
        ClientMsg::Button { button, pressed } => Command::PointerButton { button: button as u32, pressed },
        ClientMsg::Axis { mode: 1, dx, dy } => Command::wheel(dx as f64, dy as f64),
        // ponytail: pixel (and page) deltas go out as finger scroll with no axis_stop;
        // add a stop timer if clients need kinetic scrolling.
        ClientMsg::Axis { dx, dy, .. } => Command::PointerAxis { source: AxisSource::Finger, dx: dx as f64, dy: dy as f64, v120: None },
        ClientMsg::Key { evdev, pressed } => Command::Key { evdev: evdev as u32, pressed },
        ClientMsg::Blur => Command::ReleaseAllInput,
        ClientMsg::PointerLockLost => Command::ReleasePointerLock,
        _ => return None,
    })
}

/// CSS size × devicePixelRatio, rounded down to even (4:2:0 encoders), capped at 8K.
fn geometry(css_w: u16, css_h: u16, dpr: f64) -> OutputGeometry {
    let px = |css: u16| (((css as f64 * dpr).round() as u32).min(8192) & !1).max(2);
    OutputGeometry { width_px: px(css_w), height_px: px(css_h), scale: dpr, refresh_mhz: 60_000 }
}

/// The output scaled to fit a viewer's window (never up), even-sized for the encoders.
fn fit(output: &OutputGeometry, stage: &OutputGeometry) -> (u32, u32) {
    let k = (stage.width_px as f64 / output.width_px as f64).min(stage.height_px as f64 / output.height_px as f64).min(1.0);
    let even = |px: f64| ((px.round() as u32) & !1).max(2);
    (even(output.width_px as f64 * k), even(output.height_px as f64 * k))
}
