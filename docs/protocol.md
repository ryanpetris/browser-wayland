# Protocol and HTTP API

The server speaks two things: a binary WebSocket protocol for the viewer page and a JSON/PNG HTTP API
for scripts. Both are guarded by two shared tokens from the data directory: `token`, the control token,
which may do everything, and `viewer-token`, which may only look (the server prints its URL as "view
only"): the video, the window list, elements, snapshots and the clipboard's text, but no input, window
actions, programs or clipboard writes.

## Authentication

- **Viewer page** (`/`, `/app.js`, `/app.css`): public. The token arrives once in the
  URL fragment (`/#token=…`, the URLs the server prints; `?token=` is accepted too), is moved into
  `sessionStorage` and stripped from the address bar; a page with no token shows a dialog asking for one.
- **WebSocket** (`/ws`): the first message must be `AUTH` with a token. Until then the socket is
  nobody: nothing is processed. A wrong token, or five seconds of silence, closes it with code **4001**
  `unauthorized`. Which token it was decides the session's role (below).
- **Window streams** (`/ws/window/{id}`): authenticated like `/ws`; see below.
- **HTTP API** (`/api/...`): `Authorization: Bearer <token>`. Nothing else is accepted, so the token
  never appears in a URL the server or a proxy logs. The viewer token gets `403` from the routes that act.
- `POST /api/token/rotate` (control token) replaces both tokens everywhere at once.

No cookies are used anywhere.

## WebSocket messages

Binary frames, little-endian, byte 0 is the type. Mirrored in `crates/bw-server/src/protocol.rs` and
`web/src/viewer.js` (with the constants in `web/src/protocol.js`).

### Server → client

| Type | Name | Payload |
|---|---|---|
| `0x01` | Config | JSON `{streamId, codec, width, height, scale}`. Sent before the first frame of every (re)started stream; the viewer resets its decoder on a new `streamId`. `codec` is a WebCodecs string (`avc1…`, `hev1…`, `vp09…`). |
| `0x02` | Video | `u8 flags` (bit 0 keyframe) `u16 seq` `u64 pts_us` then one Annex B access unit. `seq` numbers the frames of a stream from 0 in the order they are sent, restarting with each new stream; the page treats a gap as a lost frame and waits for the next keyframe. |
| `0x03` | Cursor | `u16 w` `u16 h` `i16 hot_x` `i16 hot_y` `u16 logical_w` `u16 logical_h` then straight-alpha RGBA; `w == 0` hides the pointer. The bitmap is `w × h`; it is shown at `logical_w × logical_h` logical pixels (larger for a client's HiDPI cursor, by buffer scale or viewport), the hotspot is logical, so the page uses `image-set(… (w/logical_w)x)`. |
| `0x04` | PointerLock | `u8 locked`: a client locked or released the pointer; the page mirrors it with the Pointer Lock API. |
| `0x05` | Audio | `u8 0` `u16 seq` `u64 pts_us` then one 20 ms Opus packet. `seq` counts every packet, sent or not. |
| `0x06` | Windows | JSON array of window objects (see below), the whole list, whenever anything in it changed. Replayed to a new viewer. |
| `0x07` | Clipboard | UTF-8 text a desktop application put on the clipboard (at most 1 MiB). Not replayed: a viewer that reconnects keeps its browser clipboard. |
| `0x08` | Role | `u8`: what this session may do. 0 watch only (the viewer token); 1 act but not drive (a control token while another session controls); 2 control: its pointer, keyboard and window size are the desktop's. Sent with the replay after `Hello` and whenever it changes. |
| `0x09` | Notice | UTF-8 text about the session's last action, for the page to show briefly. Sent to a window stream whose press aims past the desktop's edge at an X11 window: Xwayland's screen is the desktop, and the X server pins the pointer to it, so that click cannot arrive. |
| `0x0A` | ClipboardData | A desktop application put something other than text on the clipboard; the payload is its mime type (`image/png`). The bytes are at `GET /api/clipboard`; the page fetches them and writes them to the browser clipboard. |
| `0x0B` | Notifications | JSON array of the open desktop notifications, oldest first, whenever they change and in the replay: each has `id`, `rev` (counts up when the application replaces it), `app`, `summary`, `body`, `icon` (whether `GET /api/notifications/{id}/icon` has a picture), `actions` as `[key, label]` pairs, and `timeout_ms` (0: until closed). |

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
| `0x8C` | SetClipboard | UTF-8 text the browser pasted; it becomes the desktop clipboard, offered to Wayland and X11 clients. Control token only. A pasted image goes by `PUT /api/clipboard` instead. |
| `0x8E` | Notify | JSON `{"id": N, "action": "default" \| "<key>"}`: the viewer clicked a notification or one of its actions; without `action` it dismissed it. Control token only. |
| `0x8D` | TakeControl | none. A control-token session becomes the controller; the desktop takes its size. |

### Close codes

| Code | Meaning |
|---|---|
| 4001 | unauthorized: no or wrong token within five seconds, or the tokens were rotated |
| 4003 | a stream that can't run: no such window, the window closed, or no encoder could be made |

The page shows these (a token dialog for 4001, a card for 4003) and stops retrying; on any other close
it reconnects after a second.

### Viewers

Any number of sessions may watch the desktop at once; each has its own encoder, so each gets the
codec its browser decodes best and a stream scaled to fit its own window. One session at a time is the
**controller**: the first control-token session to connect, until it leaves (then the oldest
remaining control-token session) or another control-token session sends `TakeControl`. The
controller's pointer and keyboard messages drive the desktop, and its `Resize` sizes the output; the
others' pointer and keyboard messages are ignored and their `Resize` only sets the size their own
stream is scaled to (the output's aspect, never enlarged; the page letterboxes it). A control-token
session that isn't controlling still acts through `Control` and `SetClipboard`, and through the API;
a viewer-token session can't. Control changes send `Role` to the two sessions concerned and release
whatever the old controller held.

### Window streams (`/ws/window/{id}`)

One application window as its own video, for a tab or popup that shows just that window (the ↗ button in
the viewer's panel opens one, sized to the window). The same messages as `/ws`, with these differences:

- No `Resize` is needed: the stream is the window's geometry at the output's scale (even-rounded), and
  follows it. A `Resize` the page sends resizes the *window* to the given CSS size (a floating window
  only, like `resize` below).
- Pointer positions are relative to the window's geometry, as in the input message (they are forwarded
  as one, resolved against the live geometry). Keys and buttons go where they always go (the focused
  window, the pointer). Any control-token session drives its popup, whoever controls the desktop; a
  viewer-token session only watches. Focusing the tab activates the window.
- `Cursor`, `PointerLock`, `Windows` and `Clipboard` arrive as on `/ws`; there is no audio. The page
  uses the window list only for the tab title. `Notice` arrives only here: a press on the part of an
  X11 window that hangs past the desktop's edge, which Xwayland pins to the edge.
- Any number can run beside the viewer, each with its own encoder (one `--bitrate` each). The
  compositor renders a window stream only when that window changed; it drops the stream when the
  window closes (close code 4003) or the socket ends. `Hello` still picks the codec. A token rotation
  ends them all with 4001. A tab that stops reading for ten seconds is dropped, as is a viewer.

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
| `GET /api/windows/{id}/snapshot.png?scale=` | PNG of that window. `scale` 0.05–2, relative to the output scale, default 1. `404` unknown id, `429` another snapshot is in flight, `500` the render failed (logged), `503` the compositor didn't answer within 2 s. |
| `GET /api/screenshot.png?scale=` | PNG of the whole output (layers included, cursor excluded); `scale` as for a window; `429`, `500`, `503` as for a window. |
| `POST /api/input` | Body: an input message (below). `202`, with `{"warning": …}` when a click aims past the desktop's edge at an X11 window; `404` unknown window; `503` compositor gone. |
| `GET /api/notifications` | The open notifications, oldest first, as the `Notifications` message carries them. |
| `POST /api/notifications/{id}` | Body `{"action": "default" \| "<key>"}`, or `{}` to dismiss. `202`; `404` unknown id. |
| `GET /api/notifications/{id}/icon` | The notification's picture: what the application named or sent, else its launcher's icon. `404` none. |
| `GET /api/clipboard` | What a desktop application last copied: `text/plain`, or `image/png` (the Content-Type says which); `204` before any. |
| `PUT /api/clipboard` | Body: UTF-8 text, or a PNG with `Content-Type: image/png`; it becomes the desktop clipboard. `202`; `413` over 1 MiB of text or 16 MiB of image. |
| `POST /api/token/rotate` | Replaces both tokens: written to the data directory, printed as new URLs, returned as `{"token": …, "viewer_token": …}`; every session is closed with `4001 token rotated` and the old tokens stop working. Control token only; not an MCP tool. |
| `GET /api/windows/{id}/elements` | The window's UI elements (below). `501` the server runs without `--elements`, `503` the tree couldn't be read: no D-Bus session or accessibility bus, the application went away, or 2 s passed (body: `{"error": …}`), `404` unknown id. |

Status codes: `401` (empty body) missing or wrong bearer token; `403` `read-only token` from `POST
/api/control`, `POST /api/input`, `PUT /api/clipboard` and `POST /api/token/rotate` with the viewer
token; the statuses above come with `{"error": "..."}`. A body axum can't read is rejected with a plain-text message: `400` invalid JSON,
`415` missing `Content-Type: application/json`, `422` wrong shape; bodies are limited to 2 MiB.

Also on the server: `POST /mcp` (MCP over Streamable HTTP, same bearer token; see [mcp.md](mcp.md)) and
`GET /skill/SKILL.md`, `GET /skill/reference.md` (the agent documentation, no token). The generated
`skills/browser-wayland/reference.md` holds the JSON schemas of every body and tool.

### Window object

```json
{"id": 3, "title": "…", "app_id": "org.gnome.Calculator", "icon": "org.gnome.Calculator", "content": null, "x11": false, "pid": 4242,
 "x": 70, "y": 70, "w": 360, "h": 616, "geo_x": 26, "geo_y": 23, "popups": [[12, 40, 200, 310]], "decoration": 0, "z": 1,
 "maximized": false, "fullscreen": false, "minimized": false, "focused": true,
 "updated_ms": 34044000}
```

- `id` is stable for the window's life and never reused.
- `app_id` is the X11 `WM_CLASS` for X11 windows; `pid` comes from the socket credentials (Wayland) or `_NET_WM_PID` (X11), when known.
- `x y w h` is the xdg geometry in logical pixels. For a minimized window it is where the window will come back.
- `geo_x geo_y` is where that geometry sits inside the client's own surface (the width of its client-side shadow), 0 for X11 windows.
- `popups` lists the window's open popups (menus, combo box lists, tooltips) as `[x, y, w, h]` relative to `x y`; always empty for X11 windows.
- `decoration` is the height of the title bar the compositor draws above `x y w h` (32), or 0 when the
  application draws its own. That bar and its buttons are part of the window's elements (below).
- `z` is the stacking index, 0 = bottom, over the listed windows; `null` while minimized. Menus and tooltips (X11 override-redirect) are not listed.
- `focused` is the compositor's intent: the window last activated by a click, the taskbar or the API.
- `updated_ms` is the time of the window's last commit on the compositor's monotonic clock, whole seconds, so a client redrawing at 60 fps does not produce sixty lists a second.

### Input message

```json
{"type": "click", "window": 3, "x": 549, "y": 47}          {"type": "click", "x": 700, "y": 400, "button": "right", "count": 2}
{"type": "move", "x": 10, "y": 10}                          {"type": "button", "button": "left", "pressed": true}
{"type": "scroll", "dy": 3}                                 {"type": "key", "keys": "ctrl+shift+t"}
{"type": "text", "text": "hello\n"}
```

- Coordinates are output logical pixels, or relative to the window's geometry when `window` is given
  (the same origin as element rectangles). `click` moves the pointer first; `count` is 1 to 3.
- `key` is a `+`-separated chord: `ctrl`, `shift`, `alt`, `super`, then any xkb keysym name (`Return`,
  `Escape`, `F5`, `Prior`) or a single character (letters are case-insensitive). Pressed in order,
  released in reverse; keys a viewer already holds stay held; a chord with a key the layout lacks does nothing.
- `text` is typed through the compositor's keyboard layout, Shift or AltGr where the layout needs it;
  `\n` is Return; characters the layout can't produce are skipped with a warning in the log.
- `scroll` is in wheel lines (positive `dy` down), sent like the viewer's wheel.

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
  Items of an open menu are placed at the menu's popup (see `popups` on the window object).
  When the compositor decorates the window (`decoration` > 0), the list ends with its title bar (role
  `title bar`, the window's title as `name`) and its three buttons (`push button`: `Minimize`,
  `Maximize` or `Restore`, `Close`), at `y = -32` above the geometry; they are there at every `level`.

### Control message

`{"id": <window id>, "op": "<op>", ...}`; `id` is omitted for `spawn`, `launch` and `quit`.

| `op` | Effect |
|---|---|
| `activate` | unminimize if needed, raise, focus |
| `close` | ask the client to close (`xdg_toplevel.close` / `WM_DELETE_WINDOW`) |
| `minimize`, `unminimize` | |
| `maximize`, `unmaximize`, `fullscreen`, `unfullscreen` | through the same paths as the client's own requests; a minimized window is restored first |
| `move` (`x`, `y`) | floating windows only (mapped, not maximized or fullscreen) |
| `resize` (`w`, `h`) | floating windows only; a size hint for Wayland clients, a configure for X11 |
| `spawn` (`cmd`) | `sh -c cmd` with the `--exec` environment: `WAYLAND_DISPLAY`, `DISPLAY`, `PULSE_SINK`, `XDG_SESSION_TYPE` and the toolkits' backend switches |
| `launch` (`app`) | start an installed application: `app` is an `id` from `GET /api/applications`, its `Exec` line runs like `spawn`; `404` over HTTP for an unknown id |
| `quit` | browser-wayland exits, every window with it |

Unknown ids and impossible requests are ignored. `spawn` is remote code execution by design; the token
is the boundary.

### Applications

`GET /api/applications` lists the launchers of the `.desktop` files in the XDG data directories
(`$XDG_DATA_HOME`, `$XDG_DATA_DIRS`; the first directory that has a file wins, so a user's copy hides a
system entry): `id` (the file name without `.desktop`), `name`, `comment`, `categories`. Entries that a
menu would not show are left out: `NoDisplay`, `Hidden`, `OnlyShowIn` (meant for one desktop), `Terminal`
(nothing to run them in), and `TryExec` binaries that aren't installed. `GET /api/applications/{id}/icon`
is the entry's icon as SVG or PNG, from the icon themes (hicolor first) or `pixmaps`; `404` without one.

## Browser console

`window.bw()` returns viewer statistics. `bw.windows()`, `bw.activate(id)`, `bw.control({...})`,
`bw.spawn(cmd)`, `bw.launch(app)`, `bw.quit()`, `bw.snapshot(id, scale)` (a `Blob`) and `bw.elements(id)` wrap the
same messages and routes.
