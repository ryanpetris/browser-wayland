# Protocol and HTTP API

The server speaks two things: a binary WebSocket protocol for the viewer page and a JSON/PNG HTTP API
for scripts. Both are guarded by the one shared token from the data directory (`token`).

## Authentication

- **Viewer page** (`/`, `/app.js`, `/desktop.js`, `/keycodes.js`): public. The page reads the token from
  its own URL (`/?token=…`, the URL the server prints) and uses it below.
- **WebSocket** (`/ws`): the first message must be `AUTH` with the token. Until then the socket is
  nobody: nothing is processed, and it cannot take the stream over. A wrong token, or five seconds of
  silence, closes it with code **4001** `unauthorized`.
- **HTTP API** (`/api/...`): `Authorization: Bearer <token>`. Nothing else is accepted, so the token
  never appears in a URL the server or a proxy logs.

No cookies are used anywhere.

## WebSocket messages

Binary frames, little-endian, byte 0 is the type. Mirrored in `crates/bw-server/src/protocol.rs` and
`web/app.js`.

### Server → client

| Type | Name | Payload |
|---|---|---|
| `0x01` | Config | JSON `{streamId, codec, width, height, scale}`. Sent before the first frame of every (re)started stream; the viewer resets its decoder on a new `streamId`. `codec` is a WebCodecs string (`avc1…`, `hev1…`, `vp09…`). |
| `0x02` | Video | `u8 flags` (bit 0 keyframe) `u64 pts_us` then one Annex B access unit. |
| `0x03` | Cursor | `u16 w` `u16 h` `i16 hot_x` `i16 hot_y` then straight-alpha RGBA; `w == 0` hides the pointer. |
| `0x04` | PointerLock | `u8 locked`: a client locked or released the pointer; the page mirrors it with the Pointer Lock API. |
| `0x05` | Audio | `u8 0` `u64 pts_us` then one 20 ms Opus packet. |
| `0x06` | Windows | JSON array of window objects (see below), the whole list, whenever anything in it changed. Replayed to a new viewer. |

### Client → server

| Type | Name | Payload |
|---|---|---|
| `0x80` | Auth | the token as UTF-8. First message. |
| `0x81` | Hello | `u8 hw` `u8 sw`: codec families the browser decodes with hardware, and at all (bit 0 H.264, bit 1 HEVC, bit 2 VP9). Picks the codec and starts the stream. |
| `0x82` | Resize | `u16 css_w` `u16 css_h` `f32 dpr`. Output = CSS size × dpr, rounded down to even, capped at 8K. |
| `0x83` | MotionAbs | `f32 x` `f32 y` in logical (CSS) pixels. |
| `0x84` | MotionRel | `f32 dx` `f32 dy` while pointer-locked. |
| `0x85` | Button | `u16 button` (Linux `BTN_*`: 0x110 left, 0x111 right, 0x112 middle, 0x113 side, 0x114 extra) `u8 pressed`. |
| `0x86` | Axis | `u8 deltaMode` (0 pixels, 1 lines, 2 pages) `f32 dx` `f32 dy`. Lines become wheel clicks (v120 = 120 per line); pixels become finger scrolling. |
| `0x87` | Key | `u16 evdev` `u8 pressed`. From `KeyboardEvent.code`; repeats are never sent. |
| `0x88` | RequestKeyframe | none. |
| `0x89` | Blur | none. Window blur, page hidden: releases every held key and button. |
| `0x8A` | PointerLockLost | none. The browser lost its lock (Escape): the client's lock is released and not re-taken until the next click. |
| `0x8B` | Control | JSON control message (below). |

### Close codes

| Code | Meaning |
|---|---|
| 4001 | unauthorized: no or wrong token within five seconds |
| 4002 | replaced by another viewer (one at a time; the newest wins) |

The page shows both as plain text and stops retrying; on any other close it reconnects after a second.

## HTTP API

```sh
T=$(cat ~/.config/browser-wayland/token)
curl -s -H "Authorization: Bearer $T" https://host:8443/api/windows
curl -X POST -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
     https://host:8443/api/control -d '{"id":3,"op":"minimize"}'
curl -X POST -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
     https://host:8443/api/control -d '{"op":"spawn","cmd":"firefox"}'
curl -o w.png -H "Authorization: Bearer $T" 'https://host:8443/api/windows/3/snapshot.png?scale=0.5'
curl -o screen.png -H "Authorization: Bearer $T" https://host:8443/api/screenshot.png
curl -s -H "Authorization: Bearer $T" https://host:8443/api/windows/3/elements      # needs --elements
```

| Route | Result |
|---|---|
| `GET /api/windows` | JSON array of window objects (the list the viewer was last sent; `[]` before any). |
| `POST /api/control` | Body: a control message. `202 Accepted`; fire-and-forget. |
| `GET /api/windows/{id}/snapshot.png?scale=` | PNG of that window. `scale` 0.05–2, relative to the output scale, default 1. `404` unknown id, `429` another snapshot is in flight, `503` the compositor didn't answer within 2 s. |
| `GET /api/screenshot.png` | PNG of the whole output at its own scale (layers included, cursor excluded). |
| `GET /api/windows/{id}/elements` | The window's UI elements (below). `501` the server runs without `--elements`, `503` the tree couldn't be read: no D-Bus session or accessibility bus, the application went away, or 2 s passed (body: `{"error": …}`), `404` unknown id. |

Status codes: `401` missing or wrong bearer token; the JSON body is limited to 2 MiB by axum.

### Window object

```json
{"id": 3, "title": "…", "app_id": "org.gnome.Calculator", "x11": false, "pid": 4242,
 "x": 70, "y": 70, "w": 360, "h": 616, "geo_x": 26, "geo_y": 23, "z": 1,
 "maximized": false, "fullscreen": false, "minimized": false, "focused": true,
 "updated_ms": 34044000}
```

- `id` is stable for the window's life and never reused.
- `app_id` is the X11 `WM_CLASS` for X11 windows; `pid` comes from the socket credentials (Wayland) or `_NET_WM_PID` (X11), when known.
- `x y w h` is the xdg geometry in logical pixels. For a minimized window it is where the window will come back.
- `geo_x geo_y` is where that geometry sits inside the client's own surface (the width of its client-side shadow), 0 for X11 windows.
- `z` is the stacking index, 0 = bottom, over the listed windows; `null` while minimized. Menus and tooltips (X11 override-redirect) are not listed.
- `focused` is the compositor's intent: the window last activated by a click, the taskbar or the API.
- `updated_ms` is the time of the window's last commit on the compositor's monotonic clock, whole seconds, so a client redrawing at 60 fps does not produce sixty lists a second.

### Elements object

```json
{"level": "full", "toolkit": "GTK",
 "elements": [{"role": "button", "name": "Save As…", "x": 549, "y": 47, "w": 107, "h": 34}, …]}
```

- `level`: `none` (the application publishes no tree), `app` (it does, but no toplevel of it matches this
  window), `frame` (the toplevel is there but empty; Chromium without `--force-renderer-accessibility`),
  `full`. `elements` is empty below `full`.
- `toolkit` as the application names it (`GTK`, `gtk`, `Gecko`, `Chromium`, …), when it says.
- `role` is one of `button`, `toggle`, `switch`, `checkbox`, `radio`, `link`, `entry`, `text`,
  `password`, `combobox`, `menu`, `menuitem`, `tab`, `slider`, `spinbutton`, `listitem`, `treeitem`,
  `scrollbar`, `heading`. Containers and static text are not listed.
- `x y w h` are logical pixels relative to the window's `x y`, so `x + window.x` is where to click. Only
  elements that are showing and have a size are listed; at most 500, from a walk of at most 3000 nodes.

### Control message

`{"id": <window id>, "op": "<op>", ...}`; `id` is omitted for `spawn`.

| `op` | Effect |
|---|---|
| `activate` | unminimize if needed, raise, focus |
| `close` | ask the client to close (`xdg_toplevel.close` / `WM_DELETE_WINDOW`) |
| `minimize`, `unminimize` | |
| `maximize`, `unmaximize`, `fullscreen`, `unfullscreen` | through the same paths as the client's own requests; a minimized window is restored first |
| `move` (`x`, `y`) | floating windows only (mapped, not maximized or fullscreen) |
| `resize` (`w`, `h`) | floating windows only; a size hint for Wayland clients, a configure for X11 |
| `spawn` (`cmd`) | `sh -c cmd` with the `--exec` environment: `WAYLAND_DISPLAY`, `DISPLAY`, `BW_WIDTH`/`BW_HEIGHT`, `PULSE_SINK`, toolkit backends |

Unknown ids and impossible requests are ignored. `spawn` is remote code execution by design; the token
is the boundary.

## Browser console

`window.bw()` returns viewer statistics. `bw.windows()`, `bw.activate(id)`, `bw.control({...})`,
`bw.spawn(cmd)`, `bw.snapshot(id, scale)` (a `Blob`) and `bw.elements(id)` wrap the same messages and routes.
