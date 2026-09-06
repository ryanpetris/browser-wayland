// Wire format, mirrored in crates/bw-server/src/protocol.rs (little-endian, byte 0 = type).
export const CONFIG = 0x01, VIDEO = 0x02, CURSOR = 0x03, POINTER_LOCK = 0x04, AUDIO = 0x05, WINDOWS = 0x06, CLIPBOARD = 0x07, ROLE = 0x08, NOTICE = 0x09, CLIPBOARD_DATA = 0x0a, NOTIFICATIONS = 0x0b, STREAM_STATE = 0x0c, RTC = 0x0d, FRAGMENT = 0x0e;
export const AUTH = 0x80, HELLO = 0x81, RESIZE = 0x82, MOTION_ABS = 0x83, MOTION_REL = 0x84, BUTTON = 0x85, AXIS = 0x86, KEY = 0x87,
  REQUEST_KEYFRAME = 0x88, BLUR = 0x89, POINTER_LOCK_LOST = 0x8A, CONTROL = 0x8B, SET_CLIPBOARD = 0x8C, TAKE_CONTROL = 0x8D, NOTIFY = 0x8E, STREAM = 0x8F, DRAG = 0x90, INPUT = 0x91, TOUCH = 0x92, MIC = 0x93, CAM = 0x94, RTC_CLIENT = 0x95, REPORT = 0x96;
// ROLE payload: what this session may do, then what the desktop takes: bit 0 the microphone, bit 1 the webcam
export const ROLES = ['viewer', 'participant', 'controller'];
// codec families in Hello bit / id order (id = index + 1; 0 is Auto), and the quality presets (id = index)
export const CODEC_FAMILIES = ['h264', 'hevc', 'vp9', 'av1', 'vp8'];
export const PRESETS = ['auto', 'low', 'medium', 'high', 'max'];
// how the video travels; the socket carries it unless a viewer picks the WebRTC data channel
export const TRANSPORTS = ['websocket', 'webrtc'];
// PointerEvent.button -> BTN_LEFT, MIDDLE, RIGHT, SIDE, EXTRA
export const BTN = [0x110, 0x112, 0x111, 0x113, 0x114];
