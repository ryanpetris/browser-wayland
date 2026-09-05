// Wire format, mirrored in crates/bw-server/src/protocol.rs (little-endian, byte 0 = type).
export const CONFIG = 0x01, VIDEO = 0x02, CURSOR = 0x03, POINTER_LOCK = 0x04, AUDIO = 0x05, WINDOWS = 0x06, CLIPBOARD = 0x07, ROLE = 0x08, NOTICE = 0x09, CLIPBOARD_DATA = 0x0a, NOTIFICATION = 0x0b;
export const AUTH = 0x80, HELLO = 0x81, RESIZE = 0x82, MOTION_ABS = 0x83, MOTION_REL = 0x84, BUTTON = 0x85, AXIS = 0x86, KEY = 0x87,
  REQUEST_KEYFRAME = 0x88, BLUR = 0x89, POINTER_LOCK_LOST = 0x8A, CONTROL = 0x8B, SET_CLIPBOARD = 0x8C, TAKE_CONTROL = 0x8D, NOTIFY = 0x8E;
// ROLE payload: what this session may do
export const ROLES = ['viewer', 'participant', 'controller'];
// PointerEvent.button -> BTN_LEFT, MIDDLE, RIGHT, SIDE, EXTRA
export const BTN = [0x110, 0x112, 0x111, 0x113, 0x114];
