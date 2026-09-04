//! Types shared between the compositor, the encoder and the web server.
//! Nothing here depends on Smithay or GStreamer.

use std::{any::Any, os::fd::OwnedFd, time::Duration};

pub use bytes::Bytes;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputGeometry {
    pub width_px: u32,
    pub height_px: u32,
    pub scale: f64,
    pub refresh_mhz: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisSource {
    Wheel,
    Finger,
}

/// Server -> compositor.
#[derive(Debug)]
pub enum Command {
    Key { evdev: u32, pressed: bool },
    /// Release every held key and pointer button (viewer blurred or went away).
    ReleaseAllInput,
    /// Logical pixels (== browser CSS pixels).
    PointerMotionAbsolute { x: f64, y: f64 },
    PointerMotionRelative { dx: f64, dy: f64 },
    /// Linux `BTN_*` code.
    PointerButton { button: u32, pressed: bool },
    PointerAxis { source: AxisSource, dx: f64, dy: f64, v120: Option<(i32, i32)> },
    Resize(OutputGeometry),
    ViewerConnected,
    ViewerDisconnected,
    /// Render a frame even if nothing changed (keyframe on connect).
    RequestFullFrame,
    /// The browser lost its pointer lock (Escape etc.): release the client's lock and don't re-lock until the next click.
    ReleasePointerLock,
    /// A window action or spawn from the viewer page or the HTTP API.
    Control(ControlMsg),
    /// Render one window (or the whole output) to pixels and hand them to `reply`.
    /// `scale` is relative to the output scale and only applies to windows.
    Snapshot { id: Option<u64>, scale: f64, reply: SnapshotReply },
    Quit,
}

/// Straight-alpha RGBA, top row first.
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct SnapshotReply(pub Box<dyn FnOnce(Option<Snapshot>) + Send>);
impl std::fmt::Debug for SnapshotReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SnapshotReply")
    }
}

/// One window as the desktop API reports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    /// X11: the WM_CLASS
    pub app_id: String,
    pub x11: bool,
    pub pid: Option<u32>,
    /// xdg geometry in logical px
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// where the geometry sits inside the client's surface (its shadow margin); 0 for X11
    pub geo_x: i32,
    pub geo_y: i32,
    /// open popups (menus, combo lists) as `(x, y, w, h)` relative to the geometry; Wayland only
    pub popups: Vec<(i32, i32, i32, i32)>,
    /// stacking index, 0 = bottom; `None` while minimized
    pub z: Option<u32>,
    pub maximized: bool,
    pub fullscreen: bool,
    pub minimized: bool,
    pub focused: bool,
    /// last commit, ms on the compositor clock
    pub updated_ms: u64,
}

/// `{"id":3,"op":"move","x":10,"y":20}`, `{"op":"spawn","cmd":"foot"}`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ControlMsg {
    #[serde(default)]
    pub id: u64,
    #[serde(flatten)]
    pub op: ControlOp,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum ControlOp {
    Activate,
    Close,
    Minimize,
    Unminimize,
    Maximize,
    Unmaximize,
    Fullscreen,
    Unfullscreen,
    Move { x: i32, y: i32 },
    Resize { w: i32, h: i32 },
    /// `sh -c`, with the same environment as `--exec`
    Spawn { cmd: String },
}

/// Compositor -> server.
#[derive(Debug)]
pub enum Event {
    /// The pointer image changed; `None` hides it. Drawn by the browser, not composited.
    Cursor(Option<CursorImage>),
    /// A client locked (or released) the pointer; the browser should mirror it with the Pointer Lock API.
    PointerLock(bool),
    /// The window list changed (full list, bottom to top, minimized last).
    Windows(Vec<WindowInfo>),
}

#[derive(Debug)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    /// Straight (non-premultiplied) RGBA.
    pub rgba: Vec<u8>,
}

/// One composited frame, handed from the compositor to the encoder.
pub struct DmabufFrame {
    /// A dup the sink owns.
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
    /// Stable per swapchain slot; the sink caches its import per slot.
    pub slot_id: u32,
    pub pts: Duration,
    pub seq: u64,
    /// Whatever keeps the buffer alive; dropping it frees the slot.
    pub lease: Box<dyn Any + Send + Sync>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Vp9,
}

/// What the server may ask of the running encoder.
pub trait StreamControl: Send + Sync {
    fn request_keyframe(&self);
    /// Switch codecs; the stream restarts with a new id.
    fn set_codec(&self, codec: Codec);
}

pub type SinkError = Box<dyn std::error::Error + Send + Sync>;

pub trait FrameSink: Send {
    /// Must not block. `Err` means the frame was not handed to the encoder.
    fn submit(&mut self, frame: DmabufFrame) -> Result<(), SinkError>;
    fn output_changed(&mut self, geo: OutputGeometry, fourcc: u32, modifier: u64);
    /// `(fourcc, modifier)` pairs the encoder can import zero-copy.
    fn accepted_formats(&self) -> Vec<(u32, u64)>;
}

/// Encoder -> server.
pub enum StreamMsg {
    /// A (re)started stream; always followed by a keyframe.
    Info(StreamInfo),
    Frame(EncodedFrame),
    /// The pipeline died; whoever drives it should ask for a keyframe so it gets rebuilt.
    Failed,
    /// One 20 ms Opus packet from the clients' audio sink.
    Audio { pts_us: u64, data: Bytes },
}

pub struct EncodedFrame {
    pub stream_id: u32,
    pub keyframe: bool,
    pub pts_us: u64,
    pub data: Bytes,
}

#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub stream_id: u32,
    /// WebCodecs codec string, e.g. `avc1.640029`.
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}
