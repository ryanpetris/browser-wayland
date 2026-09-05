//! Types shared between the compositor, the encoder and the web server.
//! Nothing here depends on Smithay or GStreamer.

use std::{any::Any, os::fd::OwnedFd, time::Duration};

pub use bytes::Bytes;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The output until the first viewer resizes it.
pub const INITIAL_OUTPUT: OutputGeometry = OutputGeometry { width_px: 1920, height_px: 1080, scale: 1.0, refresh_mhz: 60_000 };

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
    /// A viewer's encoder: every output frame goes to each of these (`None` stops one). `key` names it.
    ViewerStream { key: u64, sink: Option<Box<dyn FrameSink>> },
    /// Render a frame even if nothing changed (keyframe on connect).
    RequestFullFrame,
    /// The browser lost its pointer lock (Escape etc.): release the client's lock and don't re-lock until the next click.
    ReleasePointerLock,
    /// A window action or spawn from the viewer page or the HTTP API.
    Control(ControlMsg),
    /// Text or an image (`image/png`) from the browser or the API becomes the desktop clipboard.
    SetClipboard { mime: String, data: Vec<u8> },
    /// The browser is dragging local files over the desktop (the pointer is already where the drag is).
    Drag(Drag),
    /// A finger on the browser's touchscreen, as a `wl_touch` point (`slot` tells fingers apart); the
    /// pointer stays where it is.
    Touch { kind: TouchKind, slot: u32, x: f64, y: f64 },
    /// Pointer or keyboard input from the API or MCP, resolved on the compositor thread (window-relative
    /// coordinates against the live geometry, keys through the keymap) so a whole click lands as one unit.
    Input(InputMsg),
    /// Render one window (or the whole output) to pixels and hand them to `reply`.
    /// `scale` is relative to the output scale and only applies to windows.
    Snapshot { id: Option<u64>, scale: f64, reply: SnapshotReply },
    /// The icon a window's client set as pixels (xdg-toplevel-icon), the largest; `NoSuchWindow` when it set none.
    WindowIcon { id: u64, reply: SnapshotReply },
    /// Encode one window into its own stream through `sink` (`None` stops the stream). `key` names
    /// the stream, so two viewers of the same window don't disturb each other.
    WindowStream { key: u64, window: u64, sink: Option<Box<dyn FrameSink>> },
    Quit,
}

/// Straight-alpha RGBA, top row first.
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug)]
pub enum SnapshotError {
    NoSuchWindow,
    /// A GL step failed or the size is out of range; the compositor logged it.
    Render(String),
}

pub struct SnapshotReply(pub Box<dyn FnOnce(Result<Snapshot, SnapshotError>) + Send>);
impl std::fmt::Debug for SnapshotReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SnapshotReply")
    }
}

/// The compositor's own title bar, drawn above windows that don't draw their own (X11 windows, Wayland
/// toplevels that ask for server-side decorations or never bring the protocol up). It sits above the
/// window's geometry, so its elements have negative `y` relative to it, like everything reported
/// about a window. The layout lives here so the compositor (drawing, hit-testing) and the server (the
/// elements page) agree on it.
pub mod decoration {
    /// The bar's height in logical pixels.
    pub const BAR: i32 = 32;
    /// Each button's width; they are the bar's full height.
    pub const BUTTON: i32 = 32;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Button {
        Minimize,
        Maximize,
        Close,
    }

    impl Button {
        /// The accessible name, as the elements page reports it.
        pub fn name(self, maximized: bool) -> &'static str {
            match self {
                Button::Minimize => "Minimize",
                Button::Maximize if maximized => "Restore",
                Button::Maximize => "Maximize",
                Button::Close => "Close",
            }
        }
    }

    /// The buttons at the bar's right end for a window `w` wide: `(button, x)`, `x` relative to the
    /// geometry, each `BUTTON` wide at `y = -BAR`.
    pub fn buttons(w: i32) -> [(Button, i32); 3] {
        [(Button::Minimize, w - 3 * BUTTON), (Button::Maximize, w - 2 * BUTTON), (Button::Close, w - BUTTON)]
    }
}

/// One window as the desktop API reports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    /// X11: the WM_CLASS
    pub app_id: String,
    /// the icon name the client set (xdg-toplevel-icon); the picture is at `/api/windows/{id}/icon`, which falls back to the launcher's icon
    pub icon: Option<String>,
    /// what the client says it shows (content-type-v1): `photo`, `video` or `game`; null for ordinary windows
    pub content: Option<String>,
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
    /// height of the compositor's title bar above the geometry (the `decoration` module has its layout); 0 when the client draws its own
    pub decoration: i32,
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
#[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema)]
pub struct ControlMsg {
    #[serde(default)]
    pub id: u64,
    #[serde(flatten)]
    pub op: ControlOp,
}

#[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema)]
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
    /// Start an installed application by its id from `GET /api/applications` (its `.desktop` file's name)
    Launch { app: String },
    /// End browser-wayland: every window closes with it
    Quit,
}

impl Command {
    /// Wheel scroll by lines in the viewer's units: 15 logical px and 120 "v120" per line.
    pub fn wheel(dx: f64, dy: f64) -> Command {
        Command::PointerAxis { source: AxisSource::Wheel, dx: dx * 15.0, dy: dy * 15.0, v120: Some(((dx * 120.0) as i32, (dy * 120.0) as i32)) }
    }
}

/// One input action (`POST /api/input`, MCP tools). Coordinates are logical pixels on the output, or
/// relative to a window's geometry when `window` is given, like element rectangles.
#[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InputMsg {
    /// Move the pointer.
    Move { x: f64, y: f64, #[serde(default)] window: Option<u64> },
    /// Move the pointer there and click `count` times (default 1).
    Click {
        x: f64,
        y: f64,
        #[serde(default)]
        window: Option<u64>,
        #[serde(default)]
        button: Button,
        #[serde(default)]
        #[schemars(range(min = 1, max = 3))]
        count: Option<u32>,
    },
    /// Press or release a button where the pointer is (drags).
    Button { button: Button, pressed: bool },
    /// Scroll by wheel lines; positive `dy` scrolls down.
    Scroll { #[serde(default)] dx: f64, #[serde(default)] dy: f64 },
    /// A key chord, `+`-separated: `ctrl+shift+t`, `Return`, `alt+F4`. Modifiers first, the key last; all released after.
    Key { keys: String },
    /// Type text through the keyboard layout.
    Text { text: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    #[default]
    Left,
    Right,
    Middle,
}

impl Button {
    /// Linux `BTN_*` code.
    pub fn code(self) -> u32 {
        match self {
            Button::Left => 0x110,
            Button::Right => 0x111,
            Button::Middle => 0x112,
        }
    }
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
    /// A desktop application put text (a `text/*` mime) or a PNG on the clipboard.
    Clipboard { mime: String, data: Bytes },
    /// A drag from the browser was dropped: the application under the pointer took it, or nobody did.
    DragEnded { taken: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchKind {
    Down,
    Motion,
    /// Lifted, or taken by the browser for a gesture of its own: the same to the application.
    Up,
}

/// A drag from the browser, carried by the compositor as its own drag-and-drop offering `text/uri-list`.
#[derive(Debug)]
pub enum Drag {
    /// Take the pointer with a drag; the application under it is told what is coming.
    Start,
    /// The files are on the desktop now: this is their URI list; drop it on the application under the pointer.
    Drop(Vec<u8>),
    /// Let go over nothing.
    Cancel,
}

#[derive(Debug)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    /// Hotspot in logical pixels.
    pub hot_x: i32,
    pub hot_y: i32,
    /// The size the cursor is shown at, in logical pixels; the bitmap is larger for a client's HiDPI
    /// cursor (buffer scale or a viewport), equal for theme cursors.
    pub logical_w: u32,
    pub logical_h: u32,
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
    /// The picture didn't change since the last frame: this one is for the encoder to spend bits on
    /// what the motion before left rough (sent once, a moment after the last change).
    pub refine: bool,
    /// Whatever keeps the buffer alive; dropping it frees the slot.
    pub lease: Box<dyn Any + Send + Sync>,
}

/// What one viewer's encoder aims for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quality {
    pub bitrate_kbps: u32,
    /// Frames per second at most; the compositor's clock is the ceiling.
    pub max_fps: u32,
}

/// What became of a submitted frame: handed to the encoder, or held back on purpose (a rate cap), in
/// which case the compositor offers the next frame whole, like after an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submit {
    Encoded,
    Held,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Vp9,
    Av1,
    /// software only
    Vp8,
}

/// What the server may ask of the running encoder.
pub trait StreamControl: Send + Sync {
    fn request_keyframe(&self);
    /// Switch codecs; the stream restarts with a new id.
    fn set_codec(&self, codec: Codec);
    /// Encode at this size (the frames are scaled to it) or, with none, at the frames' own; the stream
    /// restarts with a new id when it changes.
    fn set_size(&self, size: Option<(u32, u32)>);
    /// Bitrate and frame rate, applied to the running encoder where it allows, at the next restart otherwise.
    fn set_quality(&self, quality: Quality);
}

pub type SinkError = Box<dyn std::error::Error + Send + Sync>;

pub trait FrameSink: Send {
    /// Must not block. `Err` means the frame was not handed to the encoder and something is wrong.
    fn submit(&mut self, frame: DmabufFrame) -> Result<Submit, SinkError>;
    fn output_changed(&mut self, geo: OutputGeometry, fourcc: u32, modifier: u64);
}

impl std::fmt::Debug for dyn FrameSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameSink")
    }
}

/// Encoder -> server.
pub enum StreamMsg {
    /// A (re)started stream; always followed by a keyframe.
    Info(StreamInfo),
    Frame(EncodedFrame),
    /// The pipeline died and was dropped; the next frame submitted builds a new one.
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
