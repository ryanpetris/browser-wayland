//! Binary WebSocket messages, little-endian, byte 0 = type. Mirrored in web/app.js.

use bw_core::{Bytes, CursorImage, EncodedFrame, StreamInfo};

// server -> client
pub const CONFIG: u8 = 0x01;
pub const VIDEO: u8 = 0x02;
pub const CURSOR: u8 = 0x03;
pub const POINTER_LOCK: u8 = 0x04;
// client -> server
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

/// `[VIDEO][flags: bit0 keyframe][pts_us: u64][annex-b access unit]`
pub fn video(f: &EncodedFrame) -> Bytes {
    let mut b = Vec::with_capacity(10 + f.data.len());
    b.push(VIDEO);
    b.push(f.keyframe as u8);
    b.extend_from_slice(&f.pts_us.to_le_bytes());
    b.extend_from_slice(&f.data);
    b.into()
}

/// `[CURSOR][u16 w][u16 h][i16 hot_x][i16 hot_y][straight RGBA]`; `w == 0` hides the pointer.
pub fn cursor(img: Option<&CursorImage>) -> Bytes {
    let Some(img) = img else { return Bytes::from_static(&[CURSOR, 0, 0, 0, 0, 0, 0, 0, 0]) };
    let mut b = Vec::with_capacity(9 + img.rgba.len());
    b.push(CURSOR);
    b.extend_from_slice(&(img.width as u16).to_le_bytes());
    b.extend_from_slice(&(img.height as u16).to_le_bytes());
    b.extend_from_slice(&(img.hot_x as i16).to_le_bytes());
    b.extend_from_slice(&(img.hot_y as i16).to_le_bytes());
    b.extend_from_slice(&img.rgba);
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
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_matches_js_layout() {
        // These byte strings are also what web/app.js produces.
        assert_eq!(
            decode(&[0x82, 0x80, 0x07, 0x38, 0x04, 0x00, 0x00, 0x00, 0x40]),
            Some(ClientMsg::Resize { css_w: 1920, css_h: 1080, dpr: 2.0 })
        );
        assert_eq!(decode(&[0x85, 0x10, 0x01, 0x01]), Some(ClientMsg::Button { button: 0x110, pressed: true }));
        assert_eq!(decode(&[0x87, 0x1e, 0x00, 0x00]), Some(ClientMsg::Key { evdev: 0x1e, pressed: false }));
        assert_eq!(decode(&[0x86, 0x01, 0, 0, 0, 0, 0, 0, 0x40, 0x40]), Some(ClientMsg::Axis { mode: 1, dx: 0.0, dy: 3.0 }));
        assert_eq!(decode(&[0x89]), Some(ClientMsg::Blur));
        assert_eq!(decode(&[0x85, 0x10]), None);
        assert_eq!(decode(&[]), None);
    }
}
