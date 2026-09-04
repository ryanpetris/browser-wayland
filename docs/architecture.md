# Architecture

browser-wayland is a headless Wayland compositor whose display is a browser tab. Clients render on
the GPU as usual; the composited frame is hardware-encoded (VA-API through GStreamer) and streamed
over a WebSocket; the browser decodes it with WebCodecs and paints it on a canvas. Mouse, keyboard
and (optionally) audio travel the same socket. The compositor is also the window manager, and it
exposes what it knows and can do as an HTTP/WebSocket API (see [desktop-api.md](desktop-api.md)).

Other documents: [protocol.md](protocol.md) (wire formats and HTTP API), [panels.md](panels.md)
(layer-shell, taskbars, minimize), [desktop-api.md](desktop-api.md) (window metadata, control,
snapshots, browser UI).

## Decisions

| Question | Decision |
|---|---|
| Stack | Rust + Smithay 0.7; no wlroots, no C. GStreamer (via gstreamer-rs) for encoding. axum for HTTP/WebSocket. |
| Transport | WebSocket + WebCodecs. WebCodecs needs a secure context, so the server speaks HTTPS with a self-signed certificate unless `--no-tls` (localhost development). |
| Windowing | Floating desktop: stacking, click-to-focus, client-side decorations, xdg move/resize, maximize/fullscreen, minimize, layer-shell panels. `--kiosk` fullscreens every window for nested desktops. |
| Viewers | One at a time; a new connection takes over and the old one is told so. |
| Cursor | Drawn by the browser (CSS cursor from the compositor's image), never composited: pointer motion costs no frames. |
| Rendering cadence | Damage-driven. No commit, no frame, no bandwidth. |
| Auth | One shared token, handed to the viewer once in the URL fragment and kept in `sessionStorage`; rotatable through the API. WebSocket authenticates with its first message; HTTP API uses `Authorization: Bearer`. No cookies. |

## Process layout

One process, three thread domains joined by channels:

```
 Wayland clients ──► $WAYLAND_DISPLAY socket           Xwayland (rootless; we are its window manager)
                              │
  ┌───────────────────────────▼──────────────────┐  DmabufFrame (dmabuf + lease)  ┌──────────────────────────┐
  │ compositor thread (calloop, Smithay)         │ ────────────────────────────► │ GStreamer streaming       │
  │  wl_compositor · shm · linux-dmabuf · xdg     │                                │  appsrc(memory:DMABuf)    │
  │  layer-shell · foreign-toplevel · seat · ...  │ ◄── lease dropped ⇒ slot free ─│  → vapostproc → va*enc    │
  │  desktop::Space<Window>  (floating WM)        │                                │  → parse → appsink        │
  │  GlesRenderer on GBM/EGL (render node)        │                                └────────────┬─────────────┘
  │  OutputDamageTracker → 4-slot dmabuf swapchain│                                             │ StreamMsg
  └──────────────▲───────────────────────────────┘                                             ▼
                 │ Command (calloop channel)   Event (tokio mpsc) ▲
  ┌──────────────┴──────────────────────────────────────────────┴──────────────────────────────────────────┐
  │ tokio thread: axum · HTTPS (rustls, rcgen self-signed) · /ws · /api · embedded web/                     │
  └──────────────────────────────────────────────────────────────▲──────────────────────────────────────────┘
                                                                 │ wss (binary frames both ways)
  ┌──────────────────────────────────────────────────────────────┴──────────────────────────────────────────┐
  │ browser: VideoDecoder → canvas · AudioDecoder → Web Audio · input → binary messages · desktop UI (JS)    │
  └─────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Zero-copy video path: client dmabuf → GLES composite into a GBM-allocated dmabuf → the VA-API
post-processor and encoder import that same dmabuf → bitstream → browser GPU decode. No CPU pixel
copies. Audio: clients play into a private PulseAudio/PipeWire null sink; its monitor is captured and
encoded as Opus.

## Crates

The compositor crate never depends on GStreamer and the stream crate never on Smithay; both depend on a
small shared-types crate. That boundary keeps encoders and transports pluggable and compile times sane.

| Crate | Role |
|---|---|
| `bw-core` | Plain types shared by everything: `Command` (server → compositor), `Event` (compositor → server), `DmabufFrame`, `FrameSink`, `StreamMsg`, `WindowInfo`, `ControlMsg`, `Snapshot`. Serde on the API types. |
| `bw-compositor` | Smithay. `lib.rs` (state, loop, output, resize, spawn), `handlers.rs` (protocol delegates), `input.rs` (browser input → seat, focus), `render.rs` (frame), `gpu.rs` (render node, GBM, EGL, swapchain), `grabs.rs` (move/resize), `xwayland.rs`, `foreign_toplevel.rs`, `desktop.rs` (window list, control, snapshots), `cursor.rs`. |
| `bw-stream` | GStreamer. `GstSink: FrameSink` (dmabuf import, pipeline build/rebuild, keyframes, codec switch), `lease.rs` (a custom `GstMeta` whose `free` drops the swapchain lease), the Opus audio source, and a `videotestsrc` fake source for `--fake-source`. |
| `bw-server` | axum. TLS and token bootstrap, the viewer assets (`web/dist`, embedded with `include_str!`; its build script insists on a web build first), `/ws` sessions, `/api`, frame/audio/event distribution to the current viewer. |
| `bw` | The `browser-wayland` binary: clap CLI, thread spawning, channel wiring, the audio null sink. |

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
damage tracker reports no damage, or no viewer is connected, nothing is encoded. After rendering the
GPU is waited on (`SyncPoint::wait`) before any early return, because the next commit releases client
buffers. The rendered slot leaves as a `DmabufFrame` whose lease (the `Slot`) is attached to the
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
Super/Alt + left drag moves any window, which is how undecorated X11 windows are moved.

**Input.** Browser keys arrive as evdev codes (xkb keycode = evdev + 8); the compositor never
auto-repeats (clients do, via `repeat_info`) and ignores repeats. Pointer motion hit-tests overlay/top
layers, then windows, then bottom/background layers. Pointer locks (relative-pointer + pointer-
constraints) are mirrored to the browser's Pointer Lock API; the browser then sends raw deltas.

**Xwayland.** Started at boot; its `DISPLAY` is printed and passed to `--exec` children. X11 windows
are ordinary space elements; clipboard and primary selection are bridged both ways; `WM_CHANGE_STATE`
iconify goes through the same minimize code.

**Panels and taskbars.** wlr-layer-shell and a hand-written wlr-foreign-toplevel-management (v2)
make waybar and xfce4-panel work as ordinary clients. Details in [panels.md](panels.md).

## Streaming

`GstSink` builds, per output size and codec, a pipeline of the shape

```
appsrc (memory:DMABuf, DMA_DRM caps) ! vapostproc ! video/x-raw(memory:VAMemory),format=NV12
  ! va{h264,h265,vp9}enc ! {h264,h265,vp9}parse ! appsink
```

with low-latency encoder settings (constant bitrate from `--bitrate`, no B-frames, one reference).
The appsink never blocks and never drops: every encoded access unit reaches the server, which alone
decides what to drop. A viewer gap is followed by a keyframe request (`UpstreamForceKeyUnitEvent`)
plus `Command::RequestFullFrame`, since without damage no frame would be produced. Codec choice comes
from the browser's `VideoDecoder.isConfigSupported` probes: hardware HEVC, then VP9, then H.264, unless
`--codec` pins one. A resize or codec change tears the pipeline down and rebuilds it with a new stream
id; the viewer resets its decoder when it sees a new id. Pipeline errors reach the server through a bus
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

A session becomes the viewer only after it sends the token (see [protocol.md](protocol.md)). The
current viewer's queue is small (8 frames); when it is full, frames are dropped and a keyframe is
requested, so a delta is never sent after a gap. State messages (cursor, pointer lock, window list) wait
for room instead. Encoder output belonging to a superseded stream id is discarded.

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

See the README for flags and the `Dockerfile` for a complete Arch Linux image with the Xfce panel,
Xfce applications, Firefox, Chromium, PipeWire and Mesa's GL and Vulkan drivers (`make docker-run`). Practical notes: `--exec` runs at startup, with `BW_WIDTH`/`BW_HEIGHT`
set to the output size (1920×1080 until the first viewer resizes it); nested desktops need `--kiosk`; the data
directory should be persisted in containers or every start prints a new token.

## Known limitations

- One workspace, one output, one viewer (plus any number of window streams).
- Window streams carry no audio.
