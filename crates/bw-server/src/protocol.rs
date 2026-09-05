//! Binary WebSocket messages, little-endian, byte 0 = type. Mirrored in web/src/viewer.js.

use bw_core::{Bytes, ControlMsg, CursorImage, EncodedFrame, StreamInfo, WindowInfo};

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
/// `[ROLE][u8]`: what this session may do: 0 watch only (the viewer token), 1 act but not drive (a control
/// token while someone else controls), 2 control (its pointer, keyboard and size are the desktop's).
pub const ROLE: u8 = 0x08;
/// `[NOTICE][utf-8 text]`: something the page should tell its user about what it just did.
pub const NOTICE: u8 = 0x09;
/// A desktop application copied something that isn't text; the payload is its mime type (`image/png`)
/// and the bytes are at `GET /api/clipboard`.
pub const CLIPBOARD_DATA: u8 = 0x0A;
// client -> server
/// `[AUTH][token as UTF-8]`: must be the first message on a new socket; nothing else is processed before it.
pub const AUTH: u8 = 0x80;
/// `[HELLO][u8 hw][u8 sw]`: codec families the browser decodes (bit0 H.264, bit1 HEVC, bit2 VP9), with/without hardware.
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

/// What a session may do, as sent in `ROLE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Viewer = 0,
    Participant = 1,
    Controller = 2,
}

pub fn role(role: Role) -> Bytes {
    Bytes::from(vec![ROLE, role as u8])
}

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

pub fn notice(text: &str) -> Bytes {
    let mut b = Vec::with_capacity(1 + text.len());
    b.push(NOTICE);
    b.extend_from_slice(text.as_bytes());
    b.into()
}

pub fn clipboard(text: &str) -> Bytes {
    let mut b = Vec::with_capacity(1 + text.len());
    b.push(CLIPBOARD);
    b.extend_from_slice(text.as_bytes());
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
    Hello { hw: u8, sw: u8 },
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
}

/// Malformed messages decode to `None` and are ignored.
pub fn decode(b: &[u8]) -> Option<ClientMsg> {
    let u8_at = |i: usize| b.get(i).copied();
    let u16_at = |i: usize| Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?));
    let f32_at = |i: usize| Some(f32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?));
    Some(match u8_at(0)? {
        HELLO => ClientMsg::Hello { hw: u8_at(1)?, sw: u8_at(2)? },
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
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(decode(&[0x8D]), Some(ClientMsg::TakeControl));
        assert_eq!(role(Role::Controller).as_ref(), &[0x08, 2]);
        let control = |json: &str| decode(&[&[CONTROL][..], json.as_bytes()].concat());
        assert_eq!(
            control(r#"{"id":3,"op":"move","x":10,"y":-2}"#),
            Some(ClientMsg::Control(bw_core::ControlMsg { id: 3, op: bw_core::ControlOp::Move { x: 10, y: -2 } }))
        );
        assert_eq!(control(r#"{"op":"spawn","cmd":"foot"}"#), Some(ClientMsg::Control(bw_core::ControlMsg { id: 0, op: bw_core::ControlOp::Spawn { cmd: "foot".into() } })));
        assert_eq!(control(r#"{"op":"dance"}"#), None);
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
