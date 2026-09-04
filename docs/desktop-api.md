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
| "Focused" | The compositor's intent: the window `focus_window` last activated (or that was just mapped), not the client-acknowledged xdg state, which lags a round trip and is wrong for a hung client. |
| Update timestamps | Whole-second resolution. It is part of the diffed list, so finer resolution would turn a 60 fps client into sixty lists a second. |
| Snapshot content | The window's xdg geometry (shadows clipped), popups included, minimized windows included, rendered offscreen at `scale` × output scale. The full screenshot uses the same path with the whole space at the output's own scale. |
| Snapshot format | PNG, straight alpha, encoded on the server's blocking pool; the compositor only renders and reads back. JPEG/WebP later if size matters. |
| Concurrency | One snapshot in flight; more get `429`. A queued request can't be cancelled once it is on the compositor's channel. |
| Elements | Behind `--elements`; read live from AT-SPI per request, never cached; the compositor is not involved beyond exporting the geometry offset. |

## Window list

`desktop.rs`. `window_info` builds a `WindowInfo` from a `Window`: title and app id from the xdg
toplevel data or the X11 surface, geometry from the space (or the saved position for a minimized
window), states from the acked xdg state or the X11 flags, `focused` from `State::active`, the pid from
the client's socket credentials or `_NET_WM_PID`, `updated_ms` from a per-window `LastCommit` cell set
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
- **Getting trees at all.** GTK always connects to the bus. Firefox connects when the bus reports
  accessibility enabled or `GNOME_ACCESSIBILITY=1` is set; Qt has `QT_LINUX_ACCESSIBILITY_ALWAYS_ON`.
  Both variables are added to the `--exec` environment when the flag is on, and they propagate to
  everything a panel launches. Chromium registers only an empty toplevel unless started with
  `--force-renderer-accessibility` (the bus's screen-reader flag does nothing for it), so that stays a
  documented requirement rather than something browser-wayland tries to inject.

## Browser UI (`web/desktop.js`)

- **Window panel** (☰ button, hidden in fullscreen): one row per window, top-most first, minimized
  last: a thumbnail, a colour dot, the title, badges (`full`, `max`, `min`), and buttons for snapshot,
  maximize/restore, minimize/restore (restore uses `activate`, so the window also gets the keyboard),
  close. Clicking a row activates the window. A command box at the top spawns programs; focusing it
  releases any key held in the compositor and its keys never reach the desktop.
- Rows are kept per window id across list updates, so a thumbnail reloads only when its `updated_ms`
  changed. `<img>` can't send the bearer header, so thumbnails and the full-size snapshot come through
  `fetch()` and blob URLs, one at a time (the server allows one snapshot in flight). Nothing is fetched
  while the panel is closed.
- **Borders** (▢ button, remembered in `localStorage`): an overlay with one rectangle per visible
  window, positioned from the geometry scaled by canvas CSS size over logical stream size, hue hashed
  from the app id (the same hue as the row's dot), thicker for the focused window, app id label in the
  corner. Redrawn on every list, on resize, and when the stream config arrives (the list usually comes
  first).
- **Elements** (⌖ button, remembered in `localStorage`): the focused window's elements as thin
  rectangles coloured by role, positioned like the borders from the window's current geometry, so a
  moving window needs no refetch. Fetched when the focused window's id, title or `updated_ms` changes,
  300 ms after the last change, one request at a time with a superseded answer dropped. A note under
  the window says why there are none (`501`, `503`, or the `level`).
- `window.bw` gains `windows()`, `control()`, `activate()`, `spawn()`, `snapshot()`, `elements()`.

## Security model

There are no cookies, so there is no ambient credential to ride on: every HTTP request carries the
bearer token and the WebSocket authenticates with its first message. `spawn` is remote code execution
for whoever holds the token, which the viewer already implied (it can type into a terminal). Snapshot
rendering is bounded by the one-in-flight rule and the pixel cap. The token lives in the viewer's URL;
moving it out (sessionStorage, a paste box, rotation) is future work.

## Deferred

- Per-window video streams (a browser tab per application): one encoder per window and per-stream
  negotiation.
- Clipboard read/write from the browser and the API.
- New windows are activated but don't take the keyboard until clicked; the API exposes this
  pre-existing behaviour as `focused: true` on a window without keyboard focus.
- GL failures in a snapshot answer `404` rather than `500`.
- Elements: acting on an element through AT-SPI (activate, set text) instead of clicking its rectangle;
  element states (checked, focused, disabled); Flatpak applications, whose pid on the bus is the sandbox's.
