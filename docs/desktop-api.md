# Desktop API and browser desktop UI

The compositor is the window manager, so it exposes what it knows and can do: the window list with
state and geometry, control of windows and process launching, PNG snapshots of windows, the UI elements
of a window, and a small desktop UI in the viewer built on the same data. Wire formats and routes are in
[protocol.md](protocol.md); this document covers the design.

## Decisions

| Question | Decision |
|---|---|
| State delivery | The full window list whenever anything in it changed. A handful of windows is a few hundred bytes of JSON; no delta protocol. |
| Window identity | `u64` from a counter, stored in the `Window` user data on first sight. Stable, never reused. |
| Where the page talks | The existing WebSocket: `Windows` (server → client) and `Control` (client → server) JSON messages. |
| Where scripts talk | `/api/...` on the same axum router, bearer token. |
| Two tokens | The control token acts; the viewer token (`viewer-token`, printed as "view only") reads: window list, elements, snapshots, clipboard text, the video. Acting routes answer `403`, acting MCP tools a tool error. |
| Several viewers | Each session has its own encoder (codec and size of its own); one controller at a time drives input and sizes the output, the first control-token session or whoever took control last; the rest watch letterboxed. |
| "Focused" | The compositor's intent: the window `focus_window` last activated (or that was just mapped), not the client-acknowledged xdg state, which lags a round trip and is wrong for a hung client. |
| Update timestamps | Whole-second resolution. It is part of the diffed list, so finer resolution would turn a 60 fps client into sixty lists a second. |
| Snapshot content | The window's xdg geometry (shadows clipped), popups included, minimized windows included, rendered offscreen at `scale` × output scale. The full screenshot builds the output's elements itself (`output_elements`) at the same kind of scale. |
| Snapshot format | PNG, straight alpha, encoded on the server's blocking pool; the compositor only renders and reads back. JPEG/WebP later if size matters. |
| Concurrency | One snapshot in flight; more get `429`. A queued request can't be cancelled once it is on the compositor's channel. |
| Elements | Behind `--elements`; read live from AT-SPI per request, never cached; the compositor is not involved beyond exporting the geometry offset, the open popups and its own decorations. |

## One implementation, several fronts

`api.rs` holds the operations as `App` methods: `windows`, `elements`, `snapshot`, `control`, `input`,
each returning a typed result or an `ApiError` (disabled, not found, busy, unavailable). The HTTP
routes in `lib.rs` parse and serialize; the MCP tools in `mcp.rs` do the same for an agent; both sit
behind one bearer middleware. Nothing that talks to the compositor lives in a handler, so the two
fronts cannot drift. See [mcp.md](mcp.md).

## Window list

`desktop.rs`. `window_info` builds a `WindowInfo` from a `Window`: title and app id from the xdg
toplevel data or the X11 surface, geometry from the space (or the saved position for a minimized
window), states from the acked xdg state or the X11 flags, `focused` from `State::active`, the pid from
the client's socket credentials or `_NET_WM_PID`, `icon` from the name the client set through
xdg-toplevel-icon (its picture, or the pixels it set instead, or its launcher's icon by app id, is at
`GET /api/windows/{id}/icon`), `content` from content-type-v1 (`photo`, `video`, `game`, else `null`),
`updated_ms` from a per-window `LastCommit` cell set
in the commit handler (also for minimized windows and for a window whose popup committed). `windows()`
walks the space bottom to top, skipping X11 override-redirect surfaces, then the minimized list.
`refresh_windows` runs once per loop iteration and sends `Event::Windows` only when the list differs from
the last one sent. The server caches the encoded message for `/api/windows` and replays it to a new
viewer; window lists to a slow viewer are coalesced to the newest.

`State::active` is written by `focus_window` and when a window is first mapped, and cleared when the
active window dies. Maximize and restore raise a window without activating it, so an API request on a
background window does not make it look focused.

## Control

`control()` resolves the id among mapped and minimized windows and dispatches to the functions the
compositor already had: `focus_window` (after `unminimize`), `send_close`/`x11.close()`, `minimize`,
`unminimize`, the fill/unfill paths for maximize and fullscreen, `map_element` for `move` (plus an X11
configure), a pending size plus configure for `resize`. Move and resize are ignored for windows that are
not floating. `spawn` reuses the `--exec` spawner and environment.

## Snapshots

`snapshot(id, scale)` renders one window's elements
(`Window::render_elements`, popups included) into an offscreen `GlesTexture` sized to the geometry at
`scale` × output scale, with the element origin at minus the geometry offset so the geometry lands at
(0, 0). A fresh `OutputDamageTracker::new(size, scale, Normal)` with age 0 redraws everything;
`copy_framebuffer` plus `map_texture` read the pixels back. Facts that matter, verified against Smithay's
GLES renderer:

- The GLES renderer multiplies its projection by a vertical flip, so logical row 0 is rendered at GL's
  bottom edge, which is the first row `glReadPixels` returns; the mapping reports `flipped() == true`
  meaning "row 0 is the top". So `flipped` → copy straight; the naive "flip when flipped" produced an
  upside-down image.
- GLES blends premultiplied (`ONE, ONE_MINUS_SRC_ALPHA`) onto a transparent clear; the pixels are
  un-premultiplied before PNG encoding. The screenshot clears with the compositor's opaque background.
- Binding an offscreen texture creates a throwaway FBO; the next normal frame re-binds the swapchain
  target itself and the main damage tracker is untouched, so the stream is unaffected.
- Sizes above 64 Mpx are refused (that is already 256 MiB of RGBA).

The HTTP handler sends `Command::Snapshot` with a oneshot reply, waits at most two seconds, encodes the
PNG with `spawn_blocking`, and answers `image/png` with `Cache-Control: no-store`.

## Decorations

`decor.rs`. A title bar (32 logical px, the layout in `bw_core::decoration` so the server agrees on
it) above windows that don't draw their own: X11 windows without the Motif "no decorations" hint,
Wayland toplevels that asked for server-side decorations through xdg-decoration (libdecor apps do) or
never said anything; GTK, Qt, Firefox and Chromium say they draw their own, GTK and Firefox through
KDE's server-decoration protocol, which the compositor offers for that answer alone. Fullscreen
windows have no bar. The bar is compositor chrome above the geometry: `WindowInfo` reports the client
area as before plus `decoration` (the bar's height, 0 when the client decorates), and elements,
snapshots and window streams keep meaning the client area; the border overlay includes the bar.

Rendering: one RGBA bitmap per bar at the output's resolution (title in a system sans-serif face found
with `fontdb`, glyphs from `ab_glyph`; buttons as line art), cached in the window's user data and redrawn
only when title, focus, maximized state or width change, drawn as a memory render element right above
the window's own elements. Layout: placement, clamping and maximizing leave room for the bar
(`fill_rect`, `clamp_to_output`).

Input (`input.rs`): `window_under` walks the windows top-most first and stops at the first that has
either a surface under the pointer (its own, a popup, a client-side resize handle) or, if we decorate
it, its bar or resize band (6 px around bar and window, not when maximized); pointer focus and the
decorations both come from that walk, so a higher window always wins. A press on the bar focuses and raises the window and starts the same move
grab an xdg move request uses; a second press within 400 ms toggles maximized; a press in the band
starts a resize grab with that edge; a button acts on release if the pointer is still on it (close,
minimize, maximize or restore, through `control`). Over the bar or band the compositor sets the cursor
itself (arrow or a resize arrow), since no client surface is under the pointer there.

Elements (`elements.rs`): when `decoration` > 0 the page ends with a `title bar` element and three
`push button`s (`Minimize`, `Maximize` or `Restore`, `Close`) at `y = -32`, at any `level` and even
when the accessibility bus is unreachable (then with `level: none`), so an agent targets them like the
application's own controls.

## UI elements

A Wayland compositor sees pixel buffers and a surface tree, never widgets. What does know about buttons,
links and text fields is the toolkit, and every major toolkit publishes that as an accessibility tree
over AT-SPI (GTK 3 and 4, Qt, Firefox, Chromium). `elements.rs` reads it so that scripts and agents can
target an element instead of interpreting a screenshot; the feature is named for what it returns, not for
the mechanism.

- **Bus.** `AT_SPI_BUS_ADDRESS`, else the session bus (`org.a11y.Bus.GetAddress`) of the D-Bus session
  browser-wayland runs in. Nothing is launched: without a session the route answers `503`, and the
  container's start script already wraps everything in `dbus-run-session`. Hand-declared `zbus` proxies
  for the four interfaces used (`Accessible`, `Component`, `Application`, the launcher), property caching
  off so that hundreds of short-lived proxies don't each subscribe to signals.
- **Matching.** The registry root lists the applications; the pid behind each connection (asked from the
  bus) is compared with the window's `pid`. Among the application's toplevels the one named like the
  window title wins; a lone toplevel needs no match. That gives the `level`: `none`, `app`, `frame`
  (toplevel without children) or `full`. AT-SPI has no surface handle, so title is the best key there is.
- **Walk.** Depth first from the toplevel, in document order; a node whose state lacks SHOWING is skipped
  with its whole subtree (this cuts a Firefox window from about 1700 nodes to about 400). Nodes whose
  role is interactive (buttons, toggles, links, text fields, menus, tabs, sliders, list and tree items,
  scroll bars, headings) and whose extents are non-empty are returned; at most 500 from at most 3000
  visited. One request takes tens of milliseconds locally; the handler gives up after 2 s.
- **Coordinates.** Only window-relative extents are usable: the screen variant is all zeros on Wayland,
  since clients don't know where they are. Toolkits disagree on what "window" means. GTK 4 measures from
  the xdg geometry; GTK 3 and Chromium from the whole surface including the client-side shadow; Firefox
  from the surface too, but reports its toplevel at the geometry's position. So the window list carries
  the geometry's offset inside the surface (`geo_x`, `geo_y`, from `Window::geometry().loc`) and the rule
  is: if the toplevel's extents have the geometry's size, its position is the origin; otherwise the
  surface is, and the offset is subtracted. Verified pixel-exact against window snapshots for all four.
- **Chromium's web content** (everything under a `document web` node) comes in device pixels at the
  output scale while its own toolbar is logical; those subtrees are divided by the stream's scale.
- **Menus** live in their own popup surfaces, and toolkits report a menu's items relative to that
  surface, so they would land at the window's top-left. The window list therefore carries the open popups
  (`popups`, from `PopupManager::popups_for_surface`, positioned relative to the geometry like the
  positioner defines them), and a `menu` node whose size equals a not yet matched open popup takes that
  popup's position for itself and its subtree, each popup once. GTK 3 menubar menus are different again:
  the items hang straight off the menubar item, in the coordinates of the items' popup, so they fall
  outside the item; the group of them has the popup's width and about its height, and is centred on the
  matching popup. Submenus match their own popup the same way. GTK 3 hangs each open context menu off the
  application as a separate borderless `window` toplevel rather than under the frame, so those are walked
  too while the window has popups open, and their nodes are reported only once placed on a popup (an
  application's other windows may have menus of their own). GTK 4 popovers and Firefox menus already
  report window coordinates; their sizes include a shadow, so the size match leaves them alone.
- **Getting trees at all.** GTK always connects to the bus. Firefox connects when the bus reports
  accessibility enabled or `GNOME_ACCESSIBILITY=1` is set; Qt has `QT_LINUX_ACCESSIBILITY_ALWAYS_ON`.
  Both variables are added to the `--exec` environment when the flag is on, and they propagate to
  everything a panel launches. Chromium registers only an empty toplevel unless started with
  `--force-renderer-accessibility` (the bus's screen-reader flag does nothing for it), so that stays a
  documented requirement rather than something browser-wayland tries to inject (the Docker image sets
  it in Chromium's flags file).

## Clipboard

`clipboard.rs`. When a client takes the clipboard with a text mime type (`new_selection`, or the
Xwayland path in `xwayland.rs`), the compositor asks the owner for the text through a pipe on the next
loop idle (the request is deferred out of the selection handler and any read still in progress is
dropped), reads it on the event loop (calloop `Generic` on the non-blocking read end, 1 MiB cap) and
sends `Event::Clipboard`; the server keeps the last text for `GET /api/clipboard` and the `Clipboard`
message (not replayed to a connecting viewer, whose browser clipboard may be newer). `Command::SetClipboard` makes a compositor-owned selection whose
user data carries the text; `send_selection` writes it from a calloop source on the non-blocking pipe,
so a slow reader never blocks the compositor, and X11 clients are offered it too. The selection user
data distinguishes relayed X11 selections from our own. Setting the clipboard drops a read still in
flight, so an application's older text can't land after the new one. Either pipe is closed after ten
seconds if its peer stalls. Text only; primary selection and images are not bridged; a write over 1 MiB
gets 413.

The page writes received text to the browser clipboard at once when it may, otherwise on the next
gesture. Ctrl+V and Shift+Insert are not forwarded immediately: the browser's `paste` event (which
needs no permission) delivers the text, that goes to the desktop as `SetClipboard`, and the key
press and release follow, so the application pastes the browser's content. If no paste event comes
within 150 ms the key goes through on its own.

## Viewers

`ws.rs`. `Viewers` holds every session (`ViewerSession`: which token, its event and audio senders, its
last size, and the `StreamControl` of its own encoder) and the shared state they all see (cursor,
pointer lock, window list, clipboard text, the output as the controller last sized it). A session
authenticates, sends `Hello` (which picks its codec), gets an encoder from the server's `SinkFactory`
and hands the sink to the compositor with `Command::ViewerStream`; the compositor submits every
output frame to every viewer sink (each with its own dup of the dmabuf fd and a share of the swapchain
lease, so the slot is free when the last encoder is done) and encodes nothing while there is none.
Frames go from each session's own channel to its socket in order, with a ten-second send deadline, the
way window streams do; audio and events are broadcast with `try_send`.

The controller's `Resize` becomes `Command::Resize` and re-fits everyone else (`retarget`); another
session's `Resize` only sets its own encoder's size (`fit`: the output's aspect within its window, never
enlarged, even-sized). `set_size` on a `GstSink` puts a caps filter after `vapostproc`, which scales on
the GPU on the way to NV12, and the stream's `scale` becomes `output scale × target / output width`, so
the page's logical mapping still holds; the controller's encoder has no target and takes the output as
it is, so a resize rebuilds its pipeline once, through the compositor. `TakeControl` from a control-token session, or the controller
leaving (the oldest remaining control-token session inherits), goes through `set_controller`: release
all input and the pointer lock, re-fit, resize the output to the new controller's size, tell both
sessions their `Role`. An encoder that fails is rebuilt by the next full frame; one that fails again
before producing a stream ends the session, and the page reconnects. A token rotation drops every
session's senders, which ends them with `4001`.

## Window streams

`window_stream.rs`. `Command::WindowStream { key, window, sink }` starts (or, with no sink, stops) one
stream: a `WindowStream` holds the window, a dmabuf swapchain and damage tracker of its own, and the
encoder sink the server made for it (`SinkFactory` in `bw-server`, a `GstSink` per stream). After every
output frame, `render_window_streams` renders each streamed window's elements (popups too, within the
geometry) with the geometry's corner at the origin, at the output's scale, into its swapchain, and
submits the frame only if its damage tracker saw a change, so an idle window costs nothing; a size
change, once it has held for 150 ms (an interactive resize commits one per frame), resizes the swapchain
and tells the sink, which rebuilds its pipeline with a new stream id. Sizes under 16 px (an unmapped
window) are skipped. A frame that found no
free buffer or that the sink refused marks the stream `pending`, which keeps the loop ticking and
redraws it whole. Streams of windows no longer on the desktop are dropped, which drops their sinks and
so their pipelines; the server session sees its channel close and ends with `4003`.

Server: `ws::window_session` authenticates, takes `Hello` for the codec, builds the sink, and forwards
frames straight from its own channel to the socket, in order (the pipeline drops raw frames upstream
of the encoder while the socket is busy, so nothing is lost between encoder and page and there is no
keyframe dance). Events go to window sessions as well as the viewer, by `try_send`. Pointer positions
are forwarded as window-relative `Input` moves, resolved on the compositor thread; a `Resize` becomes a
`resize` control for the window. Page: `?window=ID` (see `docs/protocol.md`); the panel's ↗ button opens a popup the window's
size, and `sessionStorage` (the token) is copied into it by the browser.

## Browser UI (`web/src`)

React and Tailwind, built by Vite into `web/dist` by `make web` and embedded (see the README). The engine
`viewer.js` owns the canvas and the connection and publishes state through `store.js`; the components
read it with `useSyncExternalStore` and send actions back through the engine.

- **Layout** (`App.jsx`): a top bar (name, the application menu, connection status, codec and size, the
  toggles, fullscreen, the power menu),
  the stage (`Stage.jsx`: the canvas, centred and fitted to it, plus the overlays and the status banners), the
  side panel (`Sidebar.jsx`) and a status bar (`StatusBar.jsx`: fps, bandwidth, input-to-paint latency,
  loss counters, clipboard, pointer lock, audio). The controller's stage sizes the desktop's output; the
  other sessions get it fitted into theirs. Fullscreen
  is requested on the stage element, so the chrome is gone while it lasts. Toggles are remembered in
  `localStorage`. A popup (`?window=ID`) shows the window's title in the top bar, no side panel, and the
  canvas centred at the window's size, scaled down if the popup is smaller.
- **Application menu** (`Launcher.jsx`): the installed launchers from `GET /api/applications`, grouped by
  their freedesktop main category (`Network` shows as Internet, `Utility` as Accessories, and so on), with
  a search box that filters by name and comment and launches the first match on Enter; a click sends
  `launch`. Icons come through `fetch()` and blob URLs like thumbnails, cached per page, with a generic
  glyph for entries without one. The **power menu** confirms, then sends `quit`; once the server accepts
  it, the page shows "shut down" instead of reconnecting when the socket ends. Both are for sessions that
  act (not the viewer token, not a window popup); together with the window list they cover what a panel
  provides, so the desktop can run without one.
- **Windows tab**: one row per window, top-most first, minimized last: a thumbnail, a colour dot, the
  title, the app id and size, state badges, and (on hover) buttons to open the window in its own popup
  (a window stream), snapshot, maximize/restore, minimize/restore (restore uses `activate`, so the window
  also gets the keyboard), close. Clicking a row activates the window. The command box at the top spawns
  programs; focusing it releases any key held in the compositor, and keys typed into any text field of
  the page never reach the desktop.
- Thumbnails reload only when a window's `updated_ms` changed. `<img>` can't send the bearer header, so
  thumbnails and the full-size snapshot come through `fetch()` and blob URLs, one at a time (the server
  allows one snapshot in flight); the old picture stays until the new one is in.
- **Borders**: an overlay with one rectangle per visible window, positioned from the geometry scaled by
  the stage's CSS size over the logical stream size, hue hashed from the app id (the same hue as the
  row's dot), thicker for the focused window, app id label in the corner. React redraws it from the
  window list, the stream config and the stage size.
- **Statistics tab**: stream, per-stage timings, frame counters, audio and connection numbers once a
  second; frames in flight are tracked by pts only while the tab is shown, so it costs nothing otherwise.
- **Elements**: the focused window's elements as thin rectangles coloured by role, positioned like the
  borders from the window's current geometry, so a moving window needs no refetch. Fetched when the
  focused window's id, title, `updated_ms`, geometry or the stream scale changes, or a popup opens or
  closes, 300 ms after the last change; an answer that no longer matches the current state is dropped, a
  failed request is retried on the next list update. A note under the window says why there are none
  (`501`, `503`, or the `level`).
- **Banners** on the stage: reconnecting, a window stream whose window is gone; a modal asks for the
  token when there is none or it was rejected.
- `window.bw()` returns the numbers; `bw.windows()`, `bw.control()`, `bw.activate()`, `bw.spawn()`,
  `bw.snapshot()`, `bw.elements()` and `bw.clipboard.read()/write()` act on the desktop.

## Security model

There are no cookies, so there is no ambient credential to ride on: every HTTP request carries a
bearer token and the WebSocket authenticates with its first message. Two tokens: the control token can
do everything (`spawn` is remote code execution for whoever holds it, which the viewer already implied,
since it can type into a terminal; `launch` runs an installed program's `Exec` line, `quit` ends the
desktop); the viewer token is for showing the desktop to someone who should
only watch: it gets the video, the window list, elements, snapshots and the clipboard's text, and `403`
(or a tool error) for anything that acts, including taking control. The bearer middleware tags each
request with which token it carried; handlers and MCP tools check the tag. Snapshot
rendering is bounded by the one-in-flight rule and the pixel cap. The viewer receives the token once in
its URL fragment (so it never reaches the server's or a proxy's log), moves it into `sessionStorage` (this
tab only) and strips it from the address bar, so the URL can be shared or bookmarked without it; a tab with no token shows a dialog asking for one. `POST /api/token/rotate`
(control token) replaces both tokens everywhere at once and closes every session with `4001 token
rotated`; the server prints the new URLs. Per-person tokens with individual revocation are not implemented. Window streams (`/ws/window/{id}`)
cost an encoder and a swapchain each and are not limited: the token holder is trusted with that.

## Deferred

- Window streams: audio, a bitrate scaled to the window's size, sharing one render between two
  viewers of the same window.
- Viewers: showing the controller's pointer to the others; a bitrate scaled to each stream's size;
  the shared swapchain has four slots, so a viewer whose pipeline stalls holds up to three of them for
  the ten seconds until its session is dropped.
- Decorations: hover highlights on the buttons, a right-click menu, borders around the client area.
- Elements: acting on an element through AT-SPI (activate, set text) instead of clicking its rectangle;
  element states (checked, focused, disabled); Flatpak applications, whose pid on the bus is the sandbox's.
