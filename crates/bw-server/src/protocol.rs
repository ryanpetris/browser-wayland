//! Binary WebSocket messages, little-endian, byte 0 = type. Mirrored in web/src/viewer.js.

use bw_core::{Bytes, Codec, ControlMsg, CursorImage, EncodedFrame, InputMsg, Quality, StreamInfo, WindowInfo};

// server -> client
pub const CONFIG: u8 = 0x01;
pub const VIDEO: u8 = 0x02;
pub const CURSOR: u8 = 0x03;
pub const POINTER_LOCK: u8 = 0x04;
pub const AUDIO: u8 = 0x05;
/// `[WINDOWS][JSON array of WindowInfo]`
pub const WINDOWS: u8 = 0x06;
/// UTF-8 text a desktop application put on the clipboard.
pub const CLIPBOARD: u8 = 0x07;
/// `[ROLE][u8 role][u8 features]`: what this session may do: 0 watch only (the viewer token), 1 act but
/// not drive (a control token while someone else controls), 2 control (its pointer, keyboard and size are
/// the desktop's); then what the desktop takes from the browser: bit 0 its microphone (`MIC`), bit 1 its
/// webcam (`CAM`).
pub const ROLE: u8 = 0x08;
/// `[NOTICE][utf-8 text]`: something the page should tell its user about what it just did.
pub const NOTICE: u8 = 0x09;
/// A desktop application copied something that isn't text; the payload is its mime type (`image/png`)
/// and the bytes are at `GET /api/clipboard`.
pub const CLIPBOARD_DATA: u8 = 0x0A;
/// The open desktop notifications, as a JSON array of `Notification`, whenever they change (and in the replay).
pub const NOTIFICATIONS: u8 = 0x0B;
/// JSON `{"codec","auto_codec","preset","ceiling_kbps","medium_kbps","bitrate_kbps","max_fps"}`: what this session's encoder does right now;
/// after every `Config` and whenever the rate controller steps the quality.
pub const STREAM_STATE: u8 = 0x0C;
/// `[RTC][JSON]`: WebRTC signalling, server side: `{"ice_servers": [...]}` once the session is up (the
/// browser may then offer), `{"answer": "<sdp>"}` to its offer.
pub const RTC: u8 = 0x0D;
/// `[FRAGMENT][u32 id][u16 index][u16 count][bytes]`: on the data channel only, a piece of one WebSocket
/// message (a video frame), reassembled by id.
pub const FRAGMENT: u8 = 0x0E;
/// JSON snapshot of the private session mixer.
pub const MIXER_STATE: u8 = 0x0F;
/// JSON array of per-object scalar peaks.
pub const MIXER_LEVELS: u8 = 0x10;
/// UTF-8 mixer command error.
pub const MIXER_ERROR: u8 = 0x11;
/// `[SESSION][u64 id]`: this desktop connection, for conditional presentation handoff.
pub const SESSION: u8 = 0x12;
/// `[FILE_RESULT][JSON]`: a staged batch rescued to the transfer folder, with saved paths and failures.
pub const FILE_RESULT: u8 = 0x13;
// client -> server
/// `[AUTH][token as UTF-8]`: must be the first message on a new socket; nothing else is processed before it.
pub const AUTH: u8 = 0x80;
/// `[HELLO][u8 hw][u8 sw][u8 codec][u8 quality]`: decoder masks, codec choice and quality ceiling.
pub const HELLO: u8 = 0x81;
pub const RESIZE: u8 = 0x82;
pub const MOTION_ABS: u8 = 0x83;
pub const MOTION_REL: u8 = 0x84;
pub const BUTTON: u8 = 0x85;
pub const AXIS: u8 = 0x86;
pub const KEY: u8 = 0x87;
pub const REQUEST_KEYFRAME: u8 = 0x88;
pub const BLUR: u8 = 0x89;
pub const POINTER_LOCK_LOST: u8 = 0x8A;
/// `[CONTROL][JSON ControlMsg]`
pub const CONTROL: u8 = 0x8B;
/// UTF-8 text the browser pasted: becomes the desktop clipboard.
pub const SET_CLIPBOARD: u8 = 0x8C;
/// A session with a control token asks to become the controller.
pub const TAKE_CONTROL: u8 = 0x8D;
/// JSON `{"id":N,"action":"key"}`: the viewer clicked a notification (`default`) or one of its actions; without `action` it dismissed it. Control token only.
pub const NOTIFY: u8 = 0x8E;
/// JSON `{"codec": "auto" | name, "quality": "very-low" | "low" | "medium" | "high" | "max"}`, either field
/// optional: this session's choice, applied live. Any session.
pub const STREAM: u8 = 0x8F;
/// `[DRAG][JSON]`: the browser drags local files over the desktop: `{"op": "start"}` where the pointer is,
/// `{"op": "drop", "batch": "…", "names": [...]}` with the batch the files were staged in (`PUT
/// /api/drop/{batch}/{name}` first) and their names, or `{"op": "cancel"}`, with the batch when files
/// were staged for a drop that isn't happening. Controlling session only.
pub const DRAG: u8 = 0x90;
/// `[INPUT][JSON]`: one input action as `POST /api/input` takes it (`{"type": "text", "text": …}`,
/// `{"type": "key", "keys": …}`, …), resolved on the compositor thread; the on-screen keyboard types with
/// it, in order with the rest of the session's input. Controlling session only.
pub const INPUT: u8 = 0x91;
/// `[TOUCH][u8 kind: 0 down, 1 motion, 2 up][u8 id][f32 x][f32 y]`: a finger on the browser's touchscreen,
/// passed on as a `wl_touch` point (`id` tells the fingers down at once apart; x, y in logical px).
/// Controlling session only.
pub const TOUCH: u8 = 0x92;
/// `[MIC][Opus packet]`: 20 ms of the browser's microphone, played into the desktop's virtual source.
/// Controlling session only.
pub const MIC: u8 = 0x93;
/// `[CAM][VP8 frame]`: one encoded frame of the browser's webcam, played into the loopback camera.
/// Controlling session only.
pub const CAM: u8 = 0x94;
/// `[RTC][JSON]`: WebRTC signalling, browser side: `{"offer": "<sdp>", "g": n}` to connect the video data
/// channel (`g` numbers the attempt and comes back with the answer, so a late one is known for what it is),
/// `{"close": true}` to go back to the socket. Any session (a window tab too), each its own connection only.
pub const RTC_CLIENT: u8 = 0x95;
/// `u16 delay_ms` `u16 dropped`: the page's last second of video, once a second while frames come: how much
/// later they arrived than at their best lately (the link queueing, which comes before it loses) and how
/// many its decoder dropped. The rate controller's ear on the far end.
pub const REPORT: u8 = 0x96;
/// JSON typed session-mixer command, at most 4096 bytes.
pub const MIXER_CLIENT: u8 = 0x97;
/// `[HANDOFF][u64 target]`: only the current controller may transfer to a live control session.
pub const HANDOFF: u8 = 0x98;

/// What a session may do, as sent in `ROLE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Viewer = 0,
    Participant = 1,
    Controller = 2,
}

/// `[ROLE][role][features]`: what the session may do, and what the desktop takes (`FEATURE_*` bits).
pub fn role(role: Role, features: u8) -> Bytes {
    Bytes::from(vec![ROLE, role as u8, features])
}
pub fn session(id: u64) -> Bytes {
    let mut b = vec![SESSION];
    b.extend_from_slice(&id.to_le_bytes());
    b.into()
}
pub const FEATURE_MIC: u8 = 1;
pub const FEATURE_CAM: u8 = 2;
pub const FEATURE_AUDIO: u8 = 4;

pub fn config(info: &StreamInfo) -> Bytes {
    let json = format!(
        r#"{{"streamId":{},"codec":"{}","width":{},"height":{},"scale":{}}}"#,
        info.stream_id, info.codec, info.width, info.height, info.scale
    );
    let mut b = Vec::with_capacity(1 + json.len());
    b.push(CONFIG);
    b.extend_from_slice(json.as_bytes());
    b.into()
}

/// `[VIDEO][flags: bit0 keyframe][seq: u16][pts_us: u64][annex-b access unit]`; `seq` numbers the frames
/// of a stream from 0 in the order they are sent, so the page can tell when one went missing.
pub fn video(f: &EncodedFrame, seq: u16) -> Bytes {
    let mut b = Vec::with_capacity(12 + f.data.len());
    b.push(VIDEO);
    b.push(f.keyframe as u8);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&f.pts_us.to_le_bytes());
    b.extend_from_slice(&f.data);
    b.into()
}

/// `[CURSOR][u16 w][u16 h][i16 hot_x][i16 hot_y][u16 logical_w][u16 logical_h][straight RGBA]`; `w == 0` hides the pointer.
pub fn cursor(img: Option<&CursorImage>) -> Bytes {
    let Some(img) = img else { return Bytes::from_static(&[CURSOR, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]) };
    let mut b = Vec::with_capacity(13 + img.rgba.len());
    b.push(CURSOR);
    b.extend_from_slice(&(img.width as u16).to_le_bytes());
    b.extend_from_slice(&(img.height as u16).to_le_bytes());
    b.extend_from_slice(&(img.hot_x as i16).to_le_bytes());
    b.extend_from_slice(&(img.hot_y as i16).to_le_bytes());
    b.extend_from_slice(&(img.logical_w as u16).to_le_bytes());
    b.extend_from_slice(&(img.logical_h as u16).to_le_bytes());
    b.extend_from_slice(&img.rgba);
    b.into()
}

pub fn rtc(v: &serde_json::Value) -> Bytes {
    let mut b = vec![RTC];
    serde_json::to_writer(&mut b, v).expect("json serializes");
    b.into()
}

pub fn mixer_state(state: &bw_core::audio::Snapshot) -> Bytes {
    let mut packet = vec![MIXER_STATE];
    serde_json::to_writer(&mut packet, state).expect("mixer state");
    packet.into()
}

pub fn mixer_levels(levels: &[bw_core::audio::Level]) -> Bytes {
    let mut packet = vec![MIXER_LEVELS];
    serde_json::to_writer(&mut packet, levels).expect("mixer levels");
    packet.into()
}

pub fn mixer_error(text: &str) -> Bytes {
    let mut packet = vec![MIXER_ERROR];
    packet.extend_from_slice(text.as_bytes());
    packet.into()
}

/// A warning: something didn't happen, or happened instead.
pub fn notice(text: &str) -> Bytes {
    notice_kind(0, text)
}

/// Good news, shown as such.
pub fn success(text: &str) -> Bytes {
    notice_kind(1, text)
}

fn notice_kind(kind: u8, text: &str) -> Bytes {
    let mut b = Vec::with_capacity(2 + text.len());
    b.push(NOTICE);
    b.push(kind);
    b.extend_from_slice(text.as_bytes());
    b.into()
}

pub fn clipboard(text: &str) -> Bytes {
    let mut b = Vec::with_capacity(1 + text.len());
    b.push(CLIPBOARD);
    b.extend_from_slice(text.as_bytes());
    b.into()
}

/// The codec families' names on the wire and in the API, and their `Hello` ids (0 is Auto).
pub const CODECS: [(Codec, &str); 5] = [(Codec::H264, "h264"), (Codec::Hevc, "hevc"), (Codec::Vp9, "vp9"), (Codec::Av1, "av1"), (Codec::Vp8, "vp8")];

pub fn codec_name(c: Codec) -> &'static str {
    CODECS.iter().find(|(k, _)| *k == c).map(|(_, n)| *n).unwrap()
}

pub fn codec_named(name: &str) -> Option<Codec> {
    CODECS.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
}

/// A viewer's adaptive bitrate ceiling (`--bitrate` configures Medium).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Preset {
    VeryLow = 1,
    Low = 2,
    Medium = 3,
    High = 4,
    #[default]
    Max = 5,
}

impl Preset {
    pub const NAMES: [(Preset, &'static str); 5] = [(Preset::VeryLow, "very-low"), (Preset::Low, "low"), (Preset::Medium, "medium"), (Preset::High, "high"), (Preset::Max, "max")];

    pub fn named(name: &str) -> Option<Preset> {
        Self::NAMES.iter().find(|(_, n)| *n == name).map(|(p, _)| *p)
    }

    pub fn name(self) -> &'static str {
        Self::NAMES.iter().find(|(p, _)| *p == self).unwrap().1
    }

    fn from_id(id: u8) -> Preset {
        match id {
            1 => Preset::VeryLow,
            2 => Preset::Low,
            3 => Preset::Medium,
            4 => Preset::High,
            5 => Preset::Max,
            _ => Preset::default(),
        }
    }

    /// The stream starts at this ceiling; under 3 Mbit/s its frame rate is capped at 30.
    pub fn quality(self, medium_kbps: u32) -> Quality {
        let bitrate_kbps = match self {
            Preset::VeryLow => 2000,
            Preset::Low => 5000,
            Preset::Medium => medium_kbps,
            Preset::High => 12000,
            Preset::Max => 25000,
        };
        Quality { bitrate_kbps, max_fps: if bitrate_kbps < 3000 { 30 } else { 0 } }
    }
}

/// The selected ceiling and current encoder target, separate from measured throughput.
pub fn stream_state(codec: Codec, auto_codec: bool, quality: Quality, preset: Preset, medium_kbps: u32) -> Bytes {
    let json = serde_json::json!({ "codec": codec_name(codec), "auto_codec": auto_codec,
        "preset": preset.name(), "ceiling_kbps": preset.quality(medium_kbps).bitrate_kbps, "medium_kbps": medium_kbps,
        "bitrate_kbps": quality.bitrate_kbps, "max_fps": quality.max_fps });
    let mut b = vec![STREAM_STATE];
    b.extend_from_slice(json.to_string().as_bytes());
    b.into()
}

pub fn notifications(list: &[crate::notify::Notification]) -> Bytes {
    let mut b = vec![NOTIFICATIONS];
    b.extend_from_slice(serde_json::to_string(list).unwrap().as_bytes());
    b.into()
}

pub fn clipboard_data(mime: &str) -> Bytes {
    let mut b = Vec::with_capacity(1 + mime.len());
    b.push(CLIPBOARD_DATA);
    b.extend_from_slice(mime.as_bytes());
    b.into()
}

pub fn windows(list: &[WindowInfo]) -> Bytes {
    let mut b = vec![WINDOWS];
    serde_json::to_writer(&mut b, list).expect("WindowInfo serializes");
    b.into()
}

/// `[AUDIO][0][seq: u16][pts_us: u64][opus packet]`, same header shape as video.
pub fn audio(pts_us: u64, data: &[u8], seq: u16) -> Bytes {
    let mut b = Vec::with_capacity(12 + data.len());
    b.push(AUDIO);
    b.push(0);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_us.to_le_bytes());
    b.extend_from_slice(data);
    b.into()
}

#[derive(Debug, PartialEq)]
pub enum ClientMsg {
    /// The browser's decoders and codec and quality choices.
    Hello { hw: u8, sw: u8, codec: Option<Codec>, quality: Preset },
    Resize { css_w: u16, css_h: u16, dpr: f32 },
    MotionAbs { x: f32, y: f32 },
    MotionRel { dx: f32, dy: f32 },
    Button { button: u16, pressed: bool },
    /// `mode` is the DOM `deltaMode`: 0 pixels, 1 lines, 2 pages.
    Axis { mode: u8, dx: f32, dy: f32 },
    Key { evdev: u16, pressed: bool },
    RequestKeyframe,
    Blur,
    PointerLockLost,
    Control(ControlMsg),
    SetClipboard(String),
    TakeControl,
    Handoff(u64),
    Notify(NotifyMsg),
    Stream(StreamChoice),
    Drag(DragMsg),
    Input(InputMsg),
    Touch { kind: u8, id: u8, x: f32, y: f32 },
    Mixer(Result<bw_core::audio::Command, &'static str>),
    Mic(Bytes),
    Cam(Bytes),
    /// `{"offer": "<sdp>", "g": 1}` or `{"close": true, "g": 1}` (see `RTC_CLIENT`).
    Rtc { g: u64, message: serde_json::Value },
    /// The page's second of video (see `REPORT`).
    Report { delay_ms: u16, dropped: u16 },
}

#[derive(Debug, PartialEq, serde::Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum DragMsg {
    Start,
    Drop { batch: String, names: Vec<String> },
    Cancel { batch: Option<String> },
}

/// `{"codec": "auto" | "h264" | …, "quality": "very-low" | "low" | …}`, either optional (unchanged).
#[derive(Debug, PartialEq, Default, serde::Deserialize)]
pub struct StreamChoice {
    pub codec: Option<String>,
    pub quality: Option<String>,
}

#[derive(Debug, PartialEq, serde::Deserialize)]
pub struct NotifyMsg {
    pub id: u32,
    /// `default`, an action key, or nothing for a dismissal
    #[serde(default)]
    pub action: Option<String>,
}

/// Malformed messages decode to `None` and are ignored.
pub fn decode(b: &[u8]) -> Option<ClientMsg> {
    let u8_at = |i: usize| b.get(i).copied();
    let u16_at = |i: usize| Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?));
    let f32_at = |i: usize| Some(f32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?));
    Some(match u8_at(0)? {
        HELLO => ClientMsg::Hello {
            hw: u8_at(1)?,
            sw: u8_at(2)?,
            codec: u8_at(3)?.checked_sub(1).and_then(|id| CODECS.get(id as usize)).map(|(c, _)| *c),
            quality: Preset::from_id(u8_at(4)?),
        },
        RESIZE => ClientMsg::Resize { css_w: u16_at(1)?, css_h: u16_at(3)?, dpr: f32_at(5)? },
        MOTION_ABS => ClientMsg::MotionAbs { x: f32_at(1)?, y: f32_at(5)? },
        MOTION_REL => ClientMsg::MotionRel { dx: f32_at(1)?, dy: f32_at(5)? },
        BUTTON => ClientMsg::Button { button: u16_at(1)?, pressed: u8_at(3)? != 0 },
        AXIS => ClientMsg::Axis { mode: u8_at(1)?, dx: f32_at(2)?, dy: f32_at(6)? },
        KEY => ClientMsg::Key { evdev: u16_at(1)?, pressed: u8_at(3)? != 0 },
        REQUEST_KEYFRAME => ClientMsg::RequestKeyframe,
        BLUR => ClientMsg::Blur,
        POINTER_LOCK_LOST => ClientMsg::PointerLockLost,
        CONTROL => ClientMsg::Control(serde_json::from_slice(&b[1..]).ok()?),
        SET_CLIPBOARD => ClientMsg::SetClipboard(String::from_utf8(b[1..].to_vec()).ok()?),
        TAKE_CONTROL => ClientMsg::TakeControl,
        HANDOFF if b.len() == 9 => ClientMsg::Handoff(u64::from_le_bytes(b[1..].try_into().ok()?)),
        NOTIFY => ClientMsg::Notify(serde_json::from_slice(&b[1..]).ok()?),
        STREAM => ClientMsg::Stream(serde_json::from_slice(&b[1..]).ok()?),
        DRAG => ClientMsg::Drag(serde_json::from_slice(&b[1..]).ok()?),
        INPUT => ClientMsg::Input(serde_json::from_slice(&b[1..]).ok()?),
        TOUCH => ClientMsg::Touch { kind: u8_at(1)?, id: u8_at(2)?, x: f32_at(3)?, y: f32_at(7)? },
        MIC if (2..=65_537).contains(&b.len()) => ClientMsg::Mic(Bytes::copy_from_slice(&b[1..])),
        CAM => ClientMsg::Cam(Bytes::copy_from_slice(b.get(1..)?)),
        MIXER_CLIENT => ClientMsg::Mixer(if b.len() <= 4097 { serde_json::from_slice(&b[1..]).map_err(|_| "Malformed mixer command.") } else { Err("Mixer command is too large.") }),
        RTC_CLIENT => {
            let message: serde_json::Value = serde_json::from_slice(&b[1..]).ok()?;
            let g = message.get("g")?.as_u64()?;
            ClientMsg::Rtc { g, message }
        },
        REPORT => ClientMsg::Report { delay_ms: u16_at(1)?, dropped: u16_at(3)? },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_levels_and_handshake_defaults() {
        for (id, name, preset, kbps) in [(1, "very-low", Preset::VeryLow, 2000), (2, "low", Preset::Low, 5000),
            (3, "medium", Preset::Medium, 8000), (4, "high", Preset::High, 12000), (5, "max", Preset::Max, 25000)] {
            assert_eq!(Preset::named(name), Some(preset));
            assert_eq!(preset.name(), name);
            assert_eq!(preset as u8, id);
            assert_eq!(preset.quality(8000).bitrate_kbps, kbps);
            assert_eq!(decode(&[HELLO, 0, 16, 5, id]), Some(ClientMsg::Hello { hw: 0, sw: 16, codec: Some(Codec::Vp8), quality: preset }));
            for medium in [2500, 3000, 40000] {
                let quality = preset.quality(medium);
                assert_eq!(quality.bitrate_kbps, if preset == Preset::Medium { medium } else { kbps });
                assert_eq!(quality.max_fps, if quality.bitrate_kbps < 3000 { 30 } else { 0 });
                let packet = stream_state(Codec::Vp8, true, Quality { bitrate_kbps: 1000, max_fps: 30 }, preset, medium);
                let state: serde_json::Value = serde_json::from_slice(&packet[1..]).unwrap();
                assert_eq!(state["preset"], name);
                assert_eq!(state["ceiling_kbps"], quality.bitrate_kbps);
                assert_eq!(state["medium_kbps"], medium);
                assert_eq!(state["bitrate_kbps"], 1000);
                assert_eq!(state["auto_codec"], true);
            }
        }
        assert_eq!(decode(&[HELLO, 0, 16]), None);
        assert_eq!(decode(&[HELLO, 0, 16, 0]), None);
        assert_eq!(Preset::named("auto"), None);
        assert_eq!(Preset::default(), Preset::Max);
        for packet in [vec![HELLO, 0, 16, 0, 0], vec![HELLO, 0, 16, 0, 255]] {
            assert_eq!(decode(&packet), Some(ClientMsg::Hello { hw: 0, sw: 16, codec: None, quality: Preset::Max }));
        }
    }

    #[test]
    fn rtc_requires_an_integer_generation() {
        for operation in [serde_json::json!({ "offer": "sdp" }), serde_json::json!({ "close": true })] {
            let packet = |v: &serde_json::Value| {
                let mut b = vec![RTC_CLIENT];
                b.extend(serde_json::to_vec(v).unwrap());
                b
            };
            assert_eq!(decode(&packet(&operation)), None);
            for invalid in [serde_json::json!(null), serde_json::json!("1"), serde_json::json!(-1), serde_json::json!(1.5), serde_json::json!(true)] {
                let mut v = operation.clone();
                v["g"] = invalid;
                assert_eq!(decode(&packet(&v)), None);
            }
            let mut v = operation;
            v["g"] = 1.into();
            assert_eq!(decode(&packet(&v)), Some(ClientMsg::Rtc { g: 1, message: v }));
        }
    }

    #[test]
    fn mixer_packets_and_commands() {
        let packet = mixer_state(&bw_core::audio::Snapshot::default());
        assert_eq!(packet[0], MIXER_STATE);
        assert!(!serde_json::from_slice::<bw_core::audio::Snapshot>(&packet[1..]).unwrap().available);
        assert_eq!(mixer_levels(&[]).as_ref(), &[MIXER_LEVELS, b'[', b']']);
        assert_eq!(mixer_error("busy").as_ref(), &[MIXER_ERROR, b'b', b'u', b's', b'y']);
        for (body, valid) in [(r#"{"op":"subscribe","enabled":true}"#, true),
            (r#"{"op":"mute","id":"1:2","value":true,"server":"host"}"#, false)] {
            let mut packet = vec![MIXER_CLIENT];
            packet.extend_from_slice(body.as_bytes());
            assert!(matches!(decode(&packet), Some(ClientMsg::Mixer(result)) if result.is_ok() == valid));
        }
        assert!(matches!(decode(&vec![MIXER_CLIENT; 4098]), Some(ClientMsg::Mixer(Err(_)))));
    }

    #[test]
    fn microphone_payload_bounds() {
        for length in [0, 1, 65_536, 65_537] {
            let mut packet = vec![0; length + 1];
            packet[0] = MIC;
            assert_eq!(matches!(decode(&packet), Some(ClientMsg::Mic(_))), (1..=65_536).contains(&length));
        }
    }

    #[test]
    fn decode_matches_js_layout() {
        // These byte strings are also what web/src/viewer.js produces.
        assert_eq!(
            decode(&[0x82, 0x80, 0x07, 0x38, 0x04, 0x00, 0x00, 0x00, 0x40]),
            Some(ClientMsg::Resize { css_w: 1920, css_h: 1080, dpr: 2.0 })
        );
        assert_eq!(decode(&[0x85, 0x10, 0x01, 0x01]), Some(ClientMsg::Button { button: 0x110, pressed: true }));
        assert_eq!(decode(&[0x87, 0x1e, 0x00, 0x00]), Some(ClientMsg::Key { evdev: 0x1e, pressed: false }));
        assert_eq!(decode(&[0x86, 0x01, 0, 0, 0, 0, 0, 0, 0x40, 0x40]), Some(ClientMsg::Axis { mode: 1, dx: 0.0, dy: 3.0 }));
        assert_eq!(decode(&[0x89]), Some(ClientMsg::Blur));
        assert_eq!(decode(&[0x92, 0, 3, 0, 0, 0x80, 0x3f, 0, 0, 0, 0x40]), Some(ClientMsg::Touch { kind: 0, id: 3, x: 1.0, y: 2.0 }));
        assert_eq!(decode(&[0x8D]), Some(ClientMsg::TakeControl));
        let target = 0x1234_5678_9abc_def0_u64;
        let mut handoff = vec![HANDOFF];
        handoff.extend_from_slice(&target.to_le_bytes());
        assert_eq!(decode(&handoff), Some(ClientMsg::Handoff(target)));
        assert_eq!(decode(&handoff[..8]), None);
        handoff.push(0);
        assert_eq!(decode(&handoff), None);
        assert_eq!(&session(target)[1..], &target.to_le_bytes());
        assert_eq!(decode(&[0x96, 0x64, 0, 2, 0]), Some(ClientMsg::Report { delay_ms: 100, dropped: 2 }));
        assert_eq!(role(Role::Controller, FEATURE_CAM).as_ref(), &[0x08, 2, 2]);
        let control = |json: &str| decode(&[&[CONTROL][..], json.as_bytes()].concat());
        assert_eq!(
            control(r#"{"id":3,"op":"move","x":10,"y":-2}"#),
            Some(ClientMsg::Control(bw_core::ControlMsg { id: 3, op: bw_core::ControlOp::Move { x: 10, y: -2 } }))
        );
        assert_eq!(control(r#"{"op":"spawn","cmd":"foot"}"#), Some(ClientMsg::Control(bw_core::ControlMsg { id: 0, op: bw_core::ControlOp::Spawn { cmd: "foot".into() } })));
        assert_eq!(control(r#"{"op":"dance"}"#), None);
        let drag = |json: &str| decode(&[&[DRAG][..], json.as_bytes()].concat());
        assert_eq!(drag(r#"{"op":"drop","batch":"b1","names":["a.txt"]}"#), Some(ClientMsg::Drag(DragMsg::Drop { batch: "b1".into(), names: vec!["a.txt".into()] })));
        assert_eq!(drag(r#"{"op":"cancel"}"#), Some(ClientMsg::Drag(DragMsg::Cancel { batch: None })));
        assert_eq!(decode(&[0x85, 0x10]), None);
        assert_eq!(decode(&[]), None);
    }

    /// Server → client media headers as `viewer.js` reads them: type, flags, u16 seq, u64 pts, payload from byte 12.
    #[test]
    fn media_layout() {
        let v = video(&bw_core::EncodedFrame { stream_id: 7, keyframe: true, pts_us: 0x0102030405060708, data: bw_core::Bytes::from_static(&[9, 9]) }, 0xBEEF);
        assert_eq!(&v[..], &[VIDEO, 1, 0xEF, 0xBE, 8, 7, 6, 5, 4, 3, 2, 1, 9, 9]);
        let a = audio(0x11, &[4, 5, 6], 3);
        assert_eq!(&a[..], &[AUDIO, 0, 3, 0, 0x11, 0, 0, 0, 0, 0, 0, 0, 4, 5, 6]);
    }
}
