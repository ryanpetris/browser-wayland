//! WebSocket sessions: the viewers of the desktop (any number, each with its own encoder; one of them,
//! the controller, drives the pointer and keyboard and sizes the output) and the per-window streams.

use std::{
    sync::{Arc, atomic::{AtomicU64, Ordering}},
    time::{Duration, Instant},
};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use bw_core::{AxisSource, Bytes, Codec, Command, ControlMsg, ControlOp, Event, InputMsg, OutputGeometry, StreamMsg, TouchKind};
use tokio::sync::mpsc;

use crate::{apps, App, Key, ViewerSession, Viewers, api, protocol::{self, ClientMsg, Preset, Role}};

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
                let (msg, data) = if mime == api::PNG || mime == api::URI_LIST {
                    (protocol::clipboard_data(&mime), data)
                } else {
                    let text = String::from_utf8_lossy(&data).into_owned(); // a legacy STRING owner may hand us Latin-1
                    (protocol::clipboard(&text), Bytes::from(text))
                };
                v.clipboard = Some((mime, data));
                msg
            }
            Event::DragEnded { taken, target, batch } => {
                // files nobody took go to the transfer folder; then the controller, if still here, hears how it went
                let events = v.controller.and_then(|c| v.sessions.get(&c)).map(|s| s.events.clone());
                let app = app.clone();
                tokio::spawn(async move {
                    let saved = taken || app.rescue(&batch).await.unwrap_or(false);
                    let word = match (taken, saved) {
                        (true, _) => protocol::success(&format!("Copied to {}", target.map_or("the desktop".to_string(), |id| apps::display_name(&id)))),
                        (false, true) => protocol::notice("no application took the files; they are in the desktop's transfer folder"),
                        (false, false) => protocol::notice("no application took the files, and they could not be saved to the transfer folder"),
                    };
                    if let Some(events) = events {
                        let _ = events.try_send(word);
                    }
                });
                continue;
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
async fn hello(socket: &mut WebSocket) -> Option<(u8, u8, Option<Codec>, Preset)> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.recv().await? {
                Ok(Message::Binary(b)) => {
                    if let Some(ClientMsg::Hello { hw, sw, codec, quality }) = protocol::decode(&b) {
                        return Some((hw, sw, codec, quality));
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
    let Some((hw, sw, want_codec, preset)) = hello(&mut socket).await else { return };
    let (tx, mut rx) = mpsc::channel::<StreamMsg>(16);
    let (sink, control) = match (app.sinks)(tx) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("viewer stream: {e:#}");
            return close(&mut socket, GONE, "no encoder").await;
        }
    };
    let Some(codec) = app.pick_codec(want_codec, hw, sw) else { return close(&mut socket, GONE, "no codec in common").await };
    control.set_codec(codec);
    let quality = preset.quality(app.bitrate_kbps);
    control.set_quality(quality);
    let mut auto = AutoRate::new(quality.bitrate_kbps);
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
        v.sessions.insert(id, ViewerSession { key, events: etx.clone(), audio: atx, audio_seq: 0, size: None, control, hw, sw, want_codec, codec, quality, cam_wait_key: false });
        if key == Key::Control && v.controller.is_none() {
            v.controller = Some(id);
        }
        let replay: Vec<Bytes> = [v.cursor.clone(), v.windows.clone(), v.locked.then(|| Bytes::from(vec![protocol::POINTER_LOCK, 1])), Some(protocol::role(v.role_of(id), app.features())), Some(notifications.clone())].into_iter().flatten().collect();
        (id, replay)
    };
    for msg in replay {
        let _ = socket.send(Message::Binary(msg)).await;
    }
    if let Some(hub) = &app.rtc {
        let _ = socket.send(Message::Binary(protocol::rtc(&serde_json::json!({ "ice_servers": *hub.ice_servers })))).await; // the page may offer now
    }
    let _ = app.commands.send(Command::ViewerStream { key: id, sink: Some(sink) });

    let (mut info, mut seq, mut failed) = (None::<bw_core::StreamInfo>, 0u16, false);
    let mut ping = tokio::time::interval(Duration::from_secs(1));
    let (mut unanswered, started) = (0, Instant::now());
    let ended = loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(StreamMsg::Info(i)) => {
                    seq = 0;
                    failed = false;
                    if !send(&mut socket, protocol::config(&i)).await { break None }
                    info = Some(i);
                    let Some(state) = app.stream_state(id) else { break Some((UNAUTHORIZED, "token rotated")) };
                    if !send(&mut socket, state).await { break None }
                }
                Some(StreamMsg::Frame(f)) => {
                    if info.as_ref().is_some_and(|i| i.stream_id == f.stream_id) {
                        // sent in order, waiting for the socket: the pipeline drops raw frames upstream of the encoder
                        // while we do, so nothing goes missing between encoder and page (no keyframe dance)
                        let (backlog, pressure) = (rx.len(), app.rtc.as_ref().and_then(|hub| hub.pressure(id))); // the channel's drops and queue are congestion too
                        let t = Instant::now();
                        match (&app.rtc, pressure) {
                            // the data channel, while the page has one open
                            (Some(hub), Some(_)) => hub.frame(id, protocol::video(&f, seq)),
                            _ => if !send(&mut socket, protocol::video(&f, seq)).await { break None },
                        }
                        seq = seq.wrapping_add(1);
                        let (dropped, queued) = pressure.unwrap_or((0, 0));
                        if let Some(q) = auto.frame(backlog + queued, dropped, t.elapsed()) {
                            app.set_quality(id, q);
                            let Some(state) = app.stream_state(id) else { break Some((UNAUTHORIZED, "token rotated")) };
                            if !send(&mut socket, state).await { break None }
                        }
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
                        Some(ClientMsg::Stream(choice)) => {
                            let (new_codec, preset) = app.apply_choice(id, &choice);
                            if let Some(preset) = preset {
                                auto = AutoRate::new(preset.quality(app.bitrate_kbps).bitrate_kbps);
                            }
                            if new_codec {
                                let _ = app.commands.send(Command::RequestFullFrame); // the new pipeline starts with a frame
                            }
                            let Some(state) = app.stream_state(id) else { break Some((UNAUTHORIZED, "token rotated")) };
                            if !send(&mut socket, state).await { break None }
                        }
                        Some(ClientMsg::Rtc(v)) => match (&app.rtc, v.get("offer").and_then(|o| o.as_str())) {
                            (Some(hub), Some(sdp)) => hub.offer(id, sdp.to_string(), v.get("g").cloned().unwrap_or_default(), etx.clone()).await,
                            (Some(hub), None) => hub.close(id).await,
                            (None, _) => {}
                        },
                        Some(ClientMsg::Report { delay_ms, dropped }) => auto.report(delay_ms, dropped),
                        Some(m) => app.viewer_message(id, key, m),
                        None => {}
                    }
                }
                Some(Ok(Message::Pong(p))) => {
                    unanswered = 0;
                    if let Some(q) = rtt_of(&p, started).and_then(|rtt| auto.rtt(rtt)) {
                        app.set_quality(id, q);
                        let Some(state) = app.stream_state(id) else { break Some((UNAUTHORIZED, "token rotated")) };
                        if !send(&mut socket, state).await { break None }
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break None,
                _ => {}
            },
            _ = ping.tick() => {
                if app.key_for(&token).is_none() {
                    break Some((UNAUTHORIZED, "token rotated")); // an idle session, which no message would end
                }
                // the pong comes back behind whatever video is queued in the socket: its time is the backlog's
                if unanswered >= 10 || socket.send(Message::Ping(ping_payload(started))).await.is_err() {
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
    if let Some(hub) = &app.rtc {
        hub.close(id).await;
    }
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
    let Some((hw, sw, mut want_codec, preset)) = hello(&mut socket).await else { return };
    let (tx, mut rx) = mpsc::channel::<StreamMsg>(16);
    let (sink, control) = match (app.sinks)(tx) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("window stream: {e:#}");
            return close(&mut socket, GONE, "no encoder").await;
        }
    };
    let Some(mut codec) = app.pick_codec(want_codec, hw, sw) else { return close(&mut socket, GONE, "no codec in common").await };
    control.set_codec(codec);
    let mut quality = preset.quality(app.bitrate_kbps);
    control.set_quality(quality);
    let mut auto = AutoRate::new(quality.bitrate_kbps);
    let state = |codec, quality, want: Option<Codec>| protocol::stream_state(codec, want.is_none(), quality);
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
        [v.cursor.clone(), v.windows.clone(), v.locked.then(|| Bytes::from(vec![protocol::POINTER_LOCK, 1])), Some(protocol::role(role, 0))].into_iter().flatten().collect()
    };
    for msg in replay {
        let _ = socket.send(Message::Binary(msg)).await;
    }
    let rtc_key = stream | 1 << 63; // the hub's sessions: desktop ids below, window streams above
    if let Some(hub) = &app.rtc {
        let _ = socket.send(Message::Binary(protocol::rtc(&serde_json::json!({ "ice_servers": *hub.ice_servers })))).await;
    }
    let _ = app.commands.send(Command::WindowStream { key: stream, window: id, sink: Some(sink) });

    let (mut info, mut seq, mut failed) = (None::<bw_core::StreamInfo>, 0u16, false);
    let mut pointer = None; // the last window-relative position, for the edge notice
    let mut ping = tokio::time::interval(Duration::from_secs(1));
    let (mut unanswered, started) = (0, Instant::now());
    let ended = loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(StreamMsg::Info(i)) => {
                    seq = 0;
                    if !send(&mut socket, protocol::config(&i)).await { break None }
                    info = Some(i);
                    if !send(&mut socket, state(codec, quality, want_codec)).await { break None }
                }
                Some(StreamMsg::Frame(f)) => {
                    if info.as_ref().is_some_and(|i| i.stream_id == f.stream_id) {
                        let (backlog, pressure) = (rx.len(), app.rtc.as_ref().and_then(|hub| hub.pressure(rtc_key)));
                        let t = Instant::now();
                        match (&app.rtc, pressure) {
                            (Some(hub), Some(_)) => hub.frame(rtc_key, protocol::video(&f, seq)),
                            _ => if !send(&mut socket, protocol::video(&f, seq)).await { break None },
                        }
                        seq = seq.wrapping_add(1);
                        let (dropped, queued) = pressure.unwrap_or((0, 0));
                        if let Some(q) = auto.frame(backlog + queued, dropped, t.elapsed()) {
                            quality = q;
                            control.set_quality(q);
                            if !send(&mut socket, state(codec, quality, want_codec)).await { break None }
                        }
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
                        Some(ClientMsg::Stream(choice)) => {
                            let mut cmd = None;
                            if let Some(c) = &choice.codec {
                                want_codec = protocol::codec_named(c);
                                if let Some(picked) = app.pick_codec(want_codec, hw, sw) {
                                    cmd = (picked != codec).then_some(Command::RequestFullFrame); // the new pipeline starts with a frame
                                    codec = picked;
                                    control.set_codec(codec);
                                }
                            }
                            if let Some(p) = choice.quality.as_deref().and_then(Preset::named) {
                                quality = p.quality(app.bitrate_kbps);
                                auto = AutoRate::new(quality.bitrate_kbps);
                                control.set_quality(quality);
                            }
                            let _ = etx.try_send(state(codec, quality, want_codec));
                            cmd
                        }
                        Some(ClientMsg::Rtc(v)) => {
                            match (&app.rtc, v.get("offer").and_then(|o| o.as_str())) {
                                (Some(hub), Some(sdp)) => hub.offer(rtc_key, sdp.to_string(), v.get("g").cloned().unwrap_or_default(), etx.clone()).await,
                                (Some(hub), None) => hub.close(rtc_key).await,
                                (None, _) => {}
                            }
                            None
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
                        Some(ClientMsg::Touch { .. }) => None, // tab coordinates aren't the desktop's; the page sends fingers as a pointer here
                        Some(ClientMsg::Report { delay_ms, dropped }) => {
                            auto.report(delay_ms, dropped);
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
                Some(Ok(Message::Pong(p))) => {
                    unanswered = 0;
                    if let Some(q) = rtt_of(&p, started).and_then(|rtt| auto.rtt(rtt)) {
                        quality = q;
                        control.set_quality(q);
                        if !send(&mut socket, state(codec, quality, want_codec)).await { break None }
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break None,
                _ => {}
            },
            _ = ping.tick() => {
                if app.key_for(&token).is_none() {
                    break Some((UNAUTHORIZED, "token rotated")); // an idle session, which no message would end
                }
                if unanswered >= 10 || socket.send(Message::Ping(ping_payload(started))).await.is_err() {
                    break None;
                }
                unanswered += 1;
            }
        }
    };
    app.window_viewers.lock().unwrap().remove(&stream);
    if let Some(hub) = &app.rtc {
        hub.close(rtc_key).await;
    }
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

    /// What the desktop takes from the browser (`Role`'s second byte).
    pub(crate) fn features(&self) -> u8 {
        let cam = self.cam.is_some() && !self.cam_dead.load(std::sync::atomic::Ordering::Relaxed);
        (self.mic.is_some() as u8) * protocol::FEATURE_MIC | (cam as u8) * protocol::FEATURE_CAM | (self.audio_available as u8) * protocol::FEATURE_AUDIO
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

    /// The viewer's own choice when both sides can, else the automatic pick.
    fn pick_codec(&self, want: Option<Codec>, hw: u8, sw: u8) -> Option<Codec> {
        match want {
            Some(c) if sw & bit(c) != 0 && self.codecs.contains(&c) => Some(c),
            _ => self.choose_codec(hw, sw),
        }
    }

    /// The automatic controller changed a session's quality.
    fn set_quality(&self, id: u64, q: bw_core::Quality) {
        if let Some(s) = self.viewers.lock().unwrap().sessions.get_mut(&id) {
            s.quality = q;
            s.control.set_quality(q);
        }
    }

    /// A desktop session's codec or quality choice: whether the codec changed, and the new preset when
    /// the quality did.
    fn apply_choice(&self, id: u64, choice: &protocol::StreamChoice) -> (bool, Option<Preset>) {
        let mut v = self.viewers.lock().unwrap();
        let Some(s) = v.sessions.get_mut(&id) else { return (false, None) };
        let mut new_codec = false;
        if let Some(c) = &choice.codec {
            s.want_codec = protocol::codec_named(c);
            if let Some(picked) = self.pick_codec(s.want_codec, s.hw, s.sw) {
                new_codec = picked != s.codec;
                s.codec = picked;
                s.control.set_codec(picked);
            }
        }
        let preset = choice.quality.as_deref().and_then(Preset::named);
        if let Some(p) = preset {
            s.quality = p.quality(self.bitrate_kbps);
            s.control.set_quality(s.quality);
        }
        (new_codec, preset)
    }

    /// What a session's encoder does right now, for the page's labels; `None` once a rotation cleared it.
    fn stream_state(&self, id: u64) -> Option<Bytes> {
        let v = self.viewers.lock().unwrap();
        let s = v.sessions.get(&id)?;
        Some(protocol::stream_state(s.codec, s.want_codec.is_none(), s.quality))
    }

    /// Pick the codec for a browser whose `hw` mask passed the prefer-hardware probe and `sw` the plain one
    /// (bit0 H.264, bit1 HEVC, bit2 VP9, bit3 AV1, bit4 VP8), among those the encoder side produces (best first):
    /// `--codec` if both sides can, else the first the browser decodes in hardware, else at all; none is `None`.
    fn choose_codec(&self, hw: u8, sw: u8) -> Option<Codec> {
        let usable = |mask: u8| self.codecs.iter().copied().find(|&c| mask & bit(c) != 0);
        match self.policy {
            Some(c) if sw & bit(c) != 0 && self.codecs.contains(&c) => Some(c),
            Some(c) => {
                tracing::warn!(?c, "the browser can't decode the requested codec or the encoder can't produce it; picking another");
                usable(hw).or_else(|| usable(sw))
            }
            None => usable(hw).or_else(|| usable(sw)),
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
                let geo = geometry(css_w, css_h, dpr as f64, v.output.refresh_mhz);
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
            ClientMsg::Drag(d) if controls => Some(self.drag_command(d)),
            ClientMsg::Input(m) if controls => Some(Command::Input(m)),
            ClientMsg::Mic(packet) if controls => {
                if let Some(mic) = &self.mic {
                    let _ = mic.try_send(packet); // a full queue drops the packet: the sink is behind anyway
                }
                None
            }
            ClientMsg::Cam(frame) if controls => {
                // a VP8 frame tag's low bit is clear on a keyframe; after a drop only one of those makes sense
                let key = frame.first().is_some_and(|b| b & 1 == 0);
                if let (Some(cam), Some(s)) = (&self.cam, v.sessions.get_mut(&id))
                    && (key || !s.cam_wait_key)
                {
                    match cam.try_send(frame) {
                        Ok(()) => s.cam_wait_key = false,
                        Err(mpsc::error::TrySendError::Full(_)) => s.cam_wait_key = true,
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            // the pipeline died (the log says why); nobody gets the button any more, this
                            // session hears why its camera does nothing
                            if !self.cam_dead.swap(true, std::sync::atomic::Ordering::Relaxed) {
                                let _ = s.events.try_send(protocol::notice("the webcam device stopped taking frames; see the server's log"));
                            }
                        }
                    }
                }
                None
            }
            m if controls => input_command(m),
            _ => None,
        };
        // sent under the lock: a handover's ReleaseAllInput then follows everything the old controller got in
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
                let _ = s.events.try_send(protocol::role(v.role_of(id), self.features()));
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
        ClientMsg::Touch { kind, id, x, y } => {
            let kind = match kind {
                0 => TouchKind::Down,
                1 => TouchKind::Motion,
                _ => TouchKind::Up,
            };
            Command::Touch { kind, slot: id as u32, x: x as f64, y: y as f64 }
        }
        ClientMsg::Blur => Command::ReleaseAllInput,
        ClientMsg::PointerLockLost => Command::ReleasePointerLock,
        _ => return None,
    })
}

/// A ping carries when it left (ms since the session started); the pong echoes it.
fn ping_payload(started: Instant) -> Bytes {
    Bytes::copy_from_slice(&(started.elapsed().as_millis() as u64).to_le_bytes())
}

fn rtt_of(pong: &[u8], started: Instant) -> Option<Duration> {
    let sent = u64::from_le_bytes(pong.try_into().ok()?);
    Some(Duration::from_millis(started.elapsed().as_millis() as u64 - sent))
}

/// A codec's bit in the `Hello` masks.
fn bit(c: Codec) -> u8 {
    match c {
        Codec::H264 => 1,
        Codec::Hevc => 2,
        Codec::Vp9 => 4,
        Codec::Av1 => 8,
        Codec::Vp8 => 16,
    }
}

/// The rate controller: a viewer's quality is a ceiling, and the bitrate lives under it by what the link
/// and the browser show. A second with a third of its frames congested (two waiting behind one, a channel
/// drop or a channel queue, a slow send), a pong 200 ms over the link's best, or the page reporting frames
/// a hundred milliseconds later than they were or its decoder dropping some, halves the bitrate, which then
/// holds two seconds; five clean seconds raise it by a quarter. Every change is a keyframe with the VA
/// encoders (a new rate opens a new GOP), so the steps are few and large rather than many and small.
struct AutoRate {
    ceiling: u32,
    quality: bw_core::Quality,
    frames: u32,
    congested: u32,
    /// Signs of a slow path this second: a late pong, the page's delay, the page's drops.
    slow: u32,
    best_rtt: Duration,
    clean_secs: u32,
    /// Seconds still to wait after a step down before the next verdict.
    hold: u32,
    window: Instant,
}

impl AutoRate {
    fn new(ceiling: u32) -> AutoRate {
        AutoRate { ceiling, quality: Preset::Auto.quality(ceiling), frames: 0, congested: 0, slow: 0, best_rtt: Duration::MAX, clean_secs: 0, hold: 0, window: Instant::now() }
    }

    /// One frame went out; `backlog` frames were waiting behind it and the send took `took`.
    fn frame(&mut self, backlog: usize, dropped: u32, took: Duration) -> Option<bw_core::Quality> {
        self.frames += 1;
        if backlog >= 2 || dropped > 0 || took > Duration::from_millis(33) {
            self.congested += 1;
        }
        self.evaluate()
    }

    /// A ping was answered after `rtt`.
    fn rtt(&mut self, rtt: Duration) -> Option<bw_core::Quality> {
        self.best_rtt = self.best_rtt.min(rtt);
        if rtt > self.best_rtt + Duration::from_millis(200) {
            self.slow += 1;
        }
        self.evaluate()
    }

    /// The page's second: how much later its frames arrived than at their best lately, and how many it dropped.
    fn report(&mut self, delay_ms: u16, dropped: u16) {
        if delay_ms > 100 || dropped > 0 {
            self.slow += 1;
        }
    }

    /// Once a second: step the quality by what the second showed.
    fn evaluate(&mut self) -> Option<bw_core::Quality> {
        if self.window.elapsed() < Duration::from_secs(1) {
            return None;
        }
        let (frames, congested, slow) = (std::mem::take(&mut self.frames), std::mem::take(&mut self.congested), std::mem::take(&mut self.slow));
        self.window = Instant::now();
        tracing::debug!(frames, congested, slow, kbps = self.quality.bitrate_kbps, "auto quality: a second of frames");
        if self.hold > 0 {
            self.hold -= 1; // the step before is still taking effect
            return None;
        }
        let mut q = self.quality;
        if congested * 3 > frames || slow > 0 {
            self.clean_secs = 0;
            q.bitrate_kbps = (q.bitrate_kbps / 2).max(1000.min(self.ceiling));
            self.hold = 2;
        } else if frames > 0 {
            self.clean_secs += 1;
            if self.clean_secs >= 5 && q.bitrate_kbps < self.ceiling {
                q.bitrate_kbps = (q.bitrate_kbps * 5 / 4).min(self.ceiling);
                self.clean_secs = 0;
            }
        }
        q.max_fps = if q.bitrate_kbps < 3000 { 30 } else { 0 };
        (q != self.quality).then(|| {
            self.quality = q;
            q
        })
    }
}


/// CSS size × devicePixelRatio, rounded down to even (4:2:0 encoders), capped at 8K.
fn geometry(css_w: u16, css_h: u16, dpr: f64, refresh_mhz: i32) -> OutputGeometry {
    let px = |css: u16| (((css as f64 * dpr).round() as u32).min(8192) & !1).max(2);
    OutputGeometry { width_px: px(css_w), height_px: px(css_h), scale: dpr, refresh_mhz }
}

/// The output scaled to fit a viewer's window (never up), even-sized for the encoders.
fn fit(output: &OutputGeometry, stage: &OutputGeometry) -> (u32, u32) {
    let k = (stage.width_px as f64 / output.width_px as f64).min(stage.height_px as f64 / output.height_px as f64).min(1.0);
    let even = |px: f64| ((px.round() as u32) & !1).max(2);
    (even(output.width_px as f64 * k), even(output.height_px as f64 * k))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second of `n` frames, `bad` of them congested, evaluated at its end.
    fn second(a: &mut AutoRate, n: u32, bad: u32) -> Option<bw_core::Quality> {
        let mut changed = None;
        for i in 0..n {
            if i == n - 1 {
                a.window = Instant::now() - Duration::from_secs(2); // the window is over with this frame
            }
            changed = a.frame(if i < bad { 2 } else { 0 }, 0, Duration::ZERO).or(changed);
        }
        changed
    }

    #[test]
    fn auto_rate_backs_off_and_recovers() {
        let mut a = AutoRate::new(8000);
        assert_eq!(second(&mut a, 60, 30).unwrap().bitrate_kbps, 4000); // half the frames waited: halved
        assert!(second(&mut a, 60, 30).is_none()); // the two seconds after a step are the step's own
        assert!(second(&mut a, 60, 30).is_none());
        assert!(second(&mut a, 60, 10).is_none()); // a sixth: fine
        // five clean seconds climb a quarter; the sixth waits again
        for _ in 0..3 {
            assert!(second(&mut a, 60, 0).is_none());
        }
        assert_eq!(second(&mut a, 60, 0).unwrap().bitrate_kbps, 5000);
        assert!(second(&mut a, 60, 0).is_none());
        // the floor and the frame cap under 3 Mbit/s
        let mut a = AutoRate::new(2000);
        let q = second(&mut a, 1, 1).unwrap();
        assert_eq!((q.bitrate_kbps, q.max_fps), (1000, 30));
        // a pong slower than the link's best by 200 ms congests the second on its own; a steady slow link doesn't
        let mut a = AutoRate::new(8000);
        a.rtt(Duration::from_millis(300));
        a.window = Instant::now() - Duration::from_secs(2);
        assert!(a.rtt(Duration::from_millis(320)).is_none());
        a.window = Instant::now() - Duration::from_secs(2);
        assert_eq!(a.rtt(Duration::from_millis(600)).unwrap().bitrate_kbps, 4000);
        // the page's word counts the same: frames a hundred milliseconds later than they were, or dropped
        let mut a = AutoRate::new(8000);
        a.report(50, 0);
        assert!(second(&mut a, 60, 0).is_none());
        a.report(0, 1);
        assert_eq!(second(&mut a, 60, 0).unwrap().bitrate_kbps, 4000);
        // the floor never passes a low ceiling: nothing to step down to
        let mut a = AutoRate::new(500);
        assert!(second(&mut a, 1, 1).is_none());
        assert_eq!((a.quality.bitrate_kbps, a.quality.max_fps), (500, 30));
    }
}
