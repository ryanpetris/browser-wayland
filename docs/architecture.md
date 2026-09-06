# Architecture

browser-wayland is a headless Wayland compositor whose display is a browser tab. Clients render on
the GPU as usual; the composited frame is hardware-encoded (VA-API through GStreamer) and streamed
over a WebSocket; the browser decodes it with WebCodecs and paints it on a canvas. Mouse, keyboard
and (optionally) audio travel the same socket; the video moves to a WebRTC data channel (`bw-server`'s
`rtc.rs`, str0m) when a viewer picks that transport, which is what reaches a server across NAT through a
TURN relay (the README compares the two under loss). The compositor is also the window manager, and it
exposes what it knows and can do as an HTTP/WebSocket API (see [desktop-api.md](desktop-api.md)).

Other documents: [protocol.md](protocol.md) (wire formats and HTTP API), [panels.md](panels.md)
(layer-shell, taskbars, minimize), [desktop-api.md](desktop-api.md) (window metadata, control,
snapshots, browser UI).

## Decisions

| Question | Decision |
|---|---|
| Stack | Rust + Smithay (git master, pinned by commit in `crates/bw-compositor/Cargo.toml`; 0.7.0 kills a client that destroys a toplevel icon before its buffer, as Chromium 152 does); no wlroots, no C. GStreamer (via gstreamer-rs) for encoding. axum for HTTP/WebSocket. |
| Transport | WebSocket + WebCodecs. WebCodecs needs a secure context, so the server speaks HTTPS with a self-signed certificate unless `--no-tls` (localhost development). |
| Windowing | Floating desktop: stacking, click-to-focus, decorations by the client or, for those that draw none, by the compositor, xdg move/resize, maximize/fullscreen, minimize, layer-shell panels. `--kiosk` fullscreens every window for nested desktops. |
| Viewers | Any number, each with its own encoder at its own size and codec. One controls (input and output size): the first control-token session, or whoever took control last. A second, read-only token lets people watch. |
| Cursor | Drawn by the browser (CSS cursor from the compositor's image), never composited: pointer motion costs no frames. Clients name a shape through cursor-shape-v1 or upload a surface; either ends as the same image. |
| Rendering cadence | Damage-driven. No commit, no frame, no bandwidth. |
| Auth | Two shared tokens (control and view-only), handed to a viewer once in the URL fragment and kept in `sessionStorage`; rotatable through the API. WebSocket authenticates with its first message; HTTP API uses `Authorization: Bearer`. No cookies. |

## Process layout

One process, three thread domains joined by channels:

```
 Wayland clients ──► $WAYLAND_DISPLAY socket           Xwayland (rootless; we are its window manager)
                              │
  ┌───────────────────────────▼──────────────────┐  Frame (dmabuf + lease, or pixels)  ┌─────────────────────┐
  │ compositor thread (calloop, Smithay)         │ ────────────────────────────► │ GStreamer, one pipeline   │
  │  wl_compositor · shm · linux-dmabuf · xdg     │      (every viewer's)          │ per viewer and window:    │
  │  layer-shell · foreign-toplevel · seat · ...  │ ◄── last lease dropped ⇒ free ─│  appsrc → vapostproc      │
  │  desktop::Space<Window>  (floating WM)        │                                │  (scale) → va*enc → sink  │
  │  GlesRenderer on GBM/EGL (render node)        │                                └────────────┬─────────────┘
  │  OutputDamageTracker → 4-slot dmabuf swapchain│                                             │ StreamMsg
  └──────────────▲───────────────────────────────┘                                             ▼
                 │ Command (calloop channel)   Event (tokio mpsc) ▲
  ┌──────────────┴──────────────────────────────────────────────┴──────────────────────────────────────────┐
  │ tokio thread: axum · HTTPS (rustls, rcgen self-signed) · /ws · /ws/window · /api · /mcp · web/dist      │
  └──────────────────────────────────────────────────────────────▲──────────────────────────────────────────┘
                                                                 │ wss (binary frames both ways)
  ┌──────────────────────────────────────────────────────────────┴──────────────────────────────────────────┐
  │ browsers: VideoDecoder → canvas · AudioDecoder → Web Audio · the controller's input → messages · React UI │
  └─────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Zero-copy video path: client dmabuf → GLES composite into a GBM-allocated dmabuf → the VA-API
post-processor and encoder import that same dmabuf → bitstream → browser GPU decode. No CPU pixel
copies. Without a GPU (`--render-node none`, or no node) the renderer is llvmpipe on Mesa's surfaceless
EGL platform, frames are rendered into one texture and read back (`gpu::Targets::Texture`,
`FrameBuffer::Memory`) for the software encoders, and there is no dmabuf or explicit-sync global: clients
draw into shared memory.

Audio uses a private PipeWire server, pipewire-pulse and WirePlumber per desktop. A native null sink
receives application playback; a native mono loopback publishes the browser microphone. Hardware
discovery and host routing state are excluded. The main process owns service startup and cleanup.
A supervised helper process runs GStreamer `pipewiresrc` capture → stereo Opus and browser Opus →
`pipewiresink` microphone injection, connected through explicit native socket descriptors. Framed
Opus and typed mixer messages cross the helper's pipes; browser audio packet framing remains separate.
The helper's native management loop subscribes to graph state and creates shared passive meters only
while viewers subscribe. Controller epochs use a shared atomic mapping so delayed helper input cannot
authorize queued commands after handoff. Authenticated desktop sockets receive authoritative snapshots
and subscribed scalar levels through latest-value channels. The helper lets the
owner bound GStreamer initialization and stop pipelines before destroying services, even when plugin
startup blocks. The application launch path exports private native/Pulse selectors and clears inherited
device overrides. See [session audio](session-audio.md) for readiness, failure and compatibility checks.

The webcam comes back as VP8 frames decoded into a `v4l2loopback` device (`--webcam`), a camera to applications.

## Crates

The compositor crate never depends on GStreamer and the stream crate never on Smithay; both depend on a
small shared-types crate. That boundary keeps encoders and transports pluggable and compile times sane.

| Crate | Role |
|---|---|
| `bw-core` | Plain types shared by everything: `Command` (server → compositor), `Event` (compositor → server), `Frame`/`FrameBuffer`, `FrameSink`, `StreamMsg`, `WindowInfo`, `ControlMsg`, `InputMsg`, `Snapshot`, the decoration layout. Serde and JSON schemas on the API types. |
| `bw-compositor` | Smithay. `lib.rs` (state, loop, output, resize, spawn), `handlers.rs` (protocol delegates), `input.rs` (browser and API input → seat, focus, decorations), `render.rs` (frame), `gpu.rs` (render node, GBM, EGL and dmabuf swapchains, or the surfaceless platform and a texture read back), `grabs.rs` (move/resize), `decor.rs` (title bars), `xwayland.rs`, `foreign_toplevel.rs`, `workspace.rs`, `desktop.rs` (window list, control, snapshots), `window_stream.rs`, `clipboard.rs`, `cursor.rs`. |
| `bw-stream` | GStreamer. `GstSink: FrameSink` (dmabuf import or memory frames, pipeline build/rebuild, keyframes, codec and size switch), `lease.rs` (a custom `GstMeta` whose `free` drops the swapchain lease), the Opus audio source, the microphone and webcam sinks. |
| `bw-server` | axum. TLS and token bootstrap, the viewer assets (`web/dist`, embedded with `include_str!`; its build script insists on a web build first), `/ws` (viewer sessions, roles) and `/ws/window/{id}` sessions, `rtc.rs` (the WebRTC data-channel transport: str0m peers on one UDP socket per address, the video's other pipe), `/api` (`api.rs` holds the operations, `elements.rs` the accessibility walk), `/mcp` (`mcp.rs`), audio and event broadcast. |
| `bw` | The `browser-wayland` binary: clap CLI, thread spawning, channel wiring, the audio devices, the render node or its absence. |

`web/` is the viewer: React 19 and Tailwind CSS 4, built by Vite into `web/dist` by `make web` (the
`Makefile` runs it before cargo; the Dockerfile and the release workflow do the same; `web/dist` is
not tracked). `src/viewer.js` is the engine (the
WebSocket, WebCodecs decoding onto the canvas, input, clipboard, audio, the `bw` console helpers); it
publishes its state on a small store and React only renders the chrome around the canvas
(`src/App.jsx` and `src/components/`). `src/keycodes.js` maps DOM `code` to evdev (generated from
Chromium's table).

## Compositor

**GPU without KMS.** `DrmNode` for the render node → `DrmDeviceFd` (unprivileged; the "unable to become
drm master" log is expected) → `GbmDevice` → `EGLDisplay`/`EGLContext` → `GlesRenderer`. Frames render
into a 4-slot `Swapchain<Dmabuf>` allocated through GBM with a modifier negotiated at startup: the
intersection of the renderer's dmabuf formats and what `vapostproc` accepts, tiled preferred over linear.
Each slot's `Dmabuf` is bound directly (the renderer caches FBOs by dmabuf identity).

**Frame loop.** A calloop timer at the output refresh, plus on-demand rendering right after input or
commits once a frame period has passed. `render_frame` renders only if something is dirty; if the
damage tracker reports no damage, or no viewer is connected, nothing is encoded; otherwise the frame
goes to every viewer's encoder, each holding a share of the slot's lease (carried by copies that still
hold the dmabuf, not by the converted frames the VPP or the CPU make of it, which the encoders keep). After rendering the
GPU is waited on (`SyncPoint::wait`) before any early return, because the next commit releases client
buffers. The rendered slot leaves as a `Frame` whose lease (the `Slot`) is attached to the
`gst::Buffer` as a custom meta; the slot is free again when GStreamer drops the buffer.

**Client buffer safety.** A pre-commit hook blocks the commit until the client's GPU work is done:
the explicit-sync acquire point when the client uses linux-drm-syncobj (GTK's Vulkan renderer puts no
implicit fences on its dmabufs), else the dmabuf's implicit fences. Clients started with `--exec` also
get `GSK_RENDERER=ngl`, because GTK 4.22's default Vulkan renderer intermittently drew hairline
slivers from window corners into our stream.

**Output.** One `Output` ("BROWSER-1") whose mode is the browser's canvas in device pixels and whose
scale is the browser's `devicePixelRatio`, so logical pixels equal CSS pixels and pointer coordinates
need no conversion. `Command::Resize` changes the mode, resizes the swapchain, re-arranges layers,
re-fits windows (`relayout`), and rebuilds the encoder pipeline with a new stream id. Sizes are rounded
down to even for 4:2:0 encoders.

**Windows.** `desktop::Space<Window>` holds Wayland toplevels and X11 windows alike. New windows
cascade inside the work area (the output minus panel exclusive zones); maximize fills the work area,
fullscreen the output. Focus, raising and activation go through one function (`focus_window`), which
the click handler, the taskbar protocol, the desktop API and minimize all use. Minimized windows leave
the space into a list (no rendering, hit-testing or frame callbacks) and come back through `relayout`.
Client buffers are wl_shm, linux-dmabuf or single-pixel-buffer-v1 (GTK4's solid backgrounds), and
alpha-modifier-v1 fades a surface without redrawing it. A toplevel with an xdg parent (set_parent, or another client's window through xdg-foreign v2, as
GTK4 and Qt portal dialogs do; GTK3 exports through v1, which Smithay doesn't offer) opens centred on
the parent and is raised with it; xdg-activation lets a client
bring a window forward (a link opened from another program, a second instance of an application).
Windows that don't draw their own decorations (X11 windows, Wayland toplevels that ask for server-side
decorations or bind neither xdg-decoration nor KDE's server-decoration protocol) get a title bar drawn
by the compositor (`decor.rs`, see [desktop-api.md](desktop-api.md)). Super/Alt + left drag moves any
window besides.

**Frame pacing.** A frame clock at the output's refresh rate renders when something is dirty. After
every tick, rendered or not, the compositor releases the fifo barriers (fifo-v1) and the commit timers
due by then (commit-timing-v1) of every surface a client has, mapped or not, so a client that queues
one frame per refresh never stalls and a hidden one keeps going; the commits this lets through render
on the next tick, or at once after an idle one. content-type-v1 is recorded per window for the API.

**Input.** Browser keys arrive as evdev codes (xkb keycode = evdev + 8); the compositor never
auto-repeats (clients do, via `repeat_info`) and ignores repeats. Pointer motion hit-tests overlay/top
layers, then windows, then bottom/background layers. Pointer locks (relative-pointer + pointer-
constraints) are mirrored to the browser's Pointer Lock API; the browser then sends raw deltas.

**Xwayland.** Started at boot; its `DISPLAY` is printed and passed to `--exec` children. X11 windows
are ordinary space elements; clipboard and primary selection are bridged both ways; `WM_CHANGE_STATE`
iconify goes through the same minimize code. The keyboard focuses an X11 window as an `X11Surface`
(`KeyboardFocus` in `handlers.rs`), so Smithay applies the window's focus model from `WM_HINTS` and
`WM_TAKE_FOCUS` (X input focus, the take-focus message, or both); an X11 client that only ever saw its
surface focused gets no `FocusIn`, and Chromium, for one, then opens no menus. Clicks on
override-redirect windows (X11 menus, tooltips) leave the focus alone, and an unmapped window hands it
to the top-most one left. The pointer is not clamped to the output: a window that hangs past its edge
(a popped-out one sized by its own tab) takes clicks there, except an X11 one, whose Xwayland screen is
the output; the server tells the popup and the API caller when a click aims at such a spot. The frame
that moves the pointer onto a new surface carries no
relative-pointer delta: Xwayland warps its device to the entry point and would apply the delta on top,
which put a synthesized click far from its target.

**Panels and taskbars.** wlr-layer-shell and a hand-written wlr-foreign-toplevel-management (v2)
make waybar and xfce4-panel work as ordinary clients. Details in [panels.md](panels.md).

## Streaming

One `GstSink` per viewer (and per window stream) builds, per frame size, target size and codec, a
pipeline of the shape

```
appsrc (memory:DMABuf, DMA_DRM caps) ! vapostproc ! video/x-raw(memory:VAMemory),format=NV12,width=W,height=H
  ! va{h264,h265,vp9,av1}enc ! {h264,h265,vp9,av1}parse ! appsink
```

with low-latency encoder settings (constant bitrate at the session's quality, no B-frames, one reference);
`vapostproc` scales the shared frame to the viewer's size on the GPU. With `--software-encoding` the
compositor renders into linear dmabufs instead, the pipeline maps them as plain raw video
(`videoconvertscale` on the CPU) and encodes with vp8enc, x264enc (or openh264enc), vp9enc, x265enc or
svtav1enc, whichever are installed; the compositor's clock runs at 30 Hz in that mode, so every rendered
frame reaches the encoders at a rate they can take. The two modes never mix. Frames are sent to each viewer in
order; while a socket is busy, `appsrc` drops raw frames before the encoder, so no delta ever refers to
a frame the viewer missed. A decoder error on the page asks for a keyframe (`UpstreamForceKeyUnitEvent`)
plus `Command::RequestFullFrame`, since without damage no frame would be produced. Codec choice comes
from each browser's `VideoDecoder.isConfigSupported` probes, among the codecs this machine encodes: on
the GPU the VA elements the driver registered for the render node (low-power variants included), best
first (AV1, HEVC, VP9, H.264); with `--software-encoding` the CPU encoders installed, cheapest first (VP8,
H.264, VP9, HEVC, AV1). `--codec` wins when both sides can, else the first the browser decodes in
hardware, else any it decodes; a browser with none in common is closed. The AV1 and VP9 codec strings carry a level chosen from the picture size, not read
from the stream. A resize, size or codec change tears the pipeline down and rebuilds it with a new
stream id; the page resets its decoder when it sees a new id. Each session also has a quality: a preset's
bitrate (Auto's is `--bitrate`), the ceiling a rate controller works under. Per second of frames it halves
the bitrate when more than a third of the frames found the transport behind (a backlog in the encoder's
channel or the data channel's queue, a channel drop, or a send over two frame times), when a ping's
answer came back 200 ms later than the quickest (it queues behind the video the kernel still holds), or
when the page's once-a-second report (`0x96`) says frames arrived a hundred milliseconds later than at
their best over ten seconds or its decoder dropped some; the new rate then holds two seconds. Five clean
seconds with frames raise it a quarter, up to the ceiling; under 3 Mbit/s the rate is capped at 30 fps.
The steps are few and large because the VA encoders open a new GOP on any rate change, so each is a
keyframe, and keyframes otherwise come on request, the encoders' periodic one pushed as far out as
each allows (the VA encoders' to 1024 frames). The refine frame is encoded at four times the bitrate only by the CPU encoders. The bitrate
changes on the running encoder where the element allows (the VA encoders, x264, x265, libvpx); the
frame cap holds frames in the sink (`Submit::Held`), which the compositor treats like a failed frame. 150 ms after the picture settles the compositor renders it once
more as a refine frame, which the sink encodes at four times the bitrate before restoring it. Pipeline errors reach the server through a bus
sync handler (freed with the pipeline; a watching thread would outlive it).

Window streams (`/ws/window/{id}`, see [desktop-api.md](desktop-api.md)) are further `GstSink`s, one
per streamed window, fed from per-window swapchains in the compositor.

Audio: `pulsesrc` on the null sink's monitor → Opus (20 ms packets) → appsink → the WebSocket. The
browser decodes with `AudioDecoder` and schedules the packets on an `AudioContext` a small lead ahead
of its clock as a jitter buffer.

## Server

axum with rustls. On first start the server writes a self-signed certificate (every local address and
`localhost` as SANs, so the fingerprint it prints can be compared in the browser), its key and a random
token into the data directory (`$XDG_CONFIG_HOME/browser-wayland`, or `~/.config/...`). ALPN is pinned to
HTTP/1.1 because WebSocket upgrades need it.

A session becomes a viewer only after it sends a token (see [protocol.md](protocol.md)); which of the
two tokens decides whether it may act. Each session forwards its own encoder's output with a
ten-second send deadline; state messages and audio are broadcast to every session. Encoder output
belonging to a superseded stream id is discarded. The controller and the sizing rules are in
[desktop-api.md](desktop-api.md).

## Web viewer

`VideoDecoder` with `optimizeForLatency`; frames are drawn onto a 2D canvas as they decode. A WebGPU
external-texture path exists behind `?renderer=webgpu` but is opt-in because Chromium on Linux
occasionally presented a blank frame with it. The canvas fills the stage, the area between the top
bar, the side panel and the status bar; the desktop's output takes the stage's size (a `ResizeObserver`
sends a debounced `Resize`), and the old picture is stretched until the new stream arrives. Fullscreen
is requested on the stage, so the chrome disappears and the output becomes the screen's size; the
Keyboard Lock API then lets shortcuts like Ctrl+W reach the desktop. The status bar shows fps, bandwidth,
input-to-frame latency and loss counters; the Statistics tab of the side panel adds per-stage timings
(receive to decoded, decoded to paint, paint interval as p50/p95), decode queue depth, keyframe cadence
and audio lead once a second, collected only while it is shown; `bw()` in the console returns the same
as JSON.

## Running and deployment

See the README for flags and the `Dockerfile` for a complete Arch Linux image with the Xfce applications
and panel, Firefox, Chromium, PipeWire and Mesa's GL and Vulkan drivers (`make docker-run`). The desktop
needs no panel: the viewer's application menu (`GET /api/applications`, from the `.desktop` files) and
power menu, with its window list, stand in for one. Practical notes: `--exec` runs at startup, with a
Wayland session's environment; nested desktops need `--kiosk`; the data directory should be persisted in
containers or every start prints a new token.

## Known limitations

- One workspace, one output; one pointer and keyboard, driven by one viewer at a time.
- Window streams carry no audio.
