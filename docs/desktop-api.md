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
| Two tokens | The control token acts; the viewer token (`viewer-token`, printed as "view only") reads: window list, elements, snapshots, the clipboard (text, image, copied files), the video. Acting routes answer `403`, acting MCP tools a tool error. |
| Several viewers | Each session has its own encoder (codec and size of its own); one controller at a time drives input and sizes the output, the first control-token session or whoever took control last; the rest watch letterboxed. |
| "Focused" | The compositor's intent: the window `focus_window` last activated (or that was just mapped), not the client-acknowledged xdg state, which lags a round trip and is wrong for a hung client. |
| Update timestamps | Whole-second resolution. It is part of the diffed list, so finer resolution would turn a 60 fps client into sixty lists a second. |
| Snapshot content | The window's xdg geometry (shadows clipped), popups included, minimized windows included, rendered offscreen with uniform scaling to the requested image dimensions. The full screenshot builds the output's elements itself (`output_elements`). |
| Snapshot format | PNG, straight alpha, encoded on the server's blocking pool; the compositor only renders and reads back. JPEG/WebP later if size matters. |
| Concurrency | One capture or PNG encode in flight; more get `429`. The slot remains held through queued capture and blocking encoding even if the caller cancels. |
| Elements | Behind `--elements`; read live from AT-SPI per request, never cached; the compositor is not involved beyond exporting the geometry offset, the open popups and its own decorations. |

## One implementation, several fronts

`api.rs` holds the operations as `App` methods: `windows`, `elements`, `snapshot`, `control`, `input`,
each returning a typed result or an `ApiError` (disabled, not found, busy, unavailable). The HTTP
routes in `lib.rs` parse and serialize; the MCP tools in `mcp.rs` do the same for an agent; both sit
behind one bearer middleware. Nothing that talks to the compositor lives in a handler, so the two
fronts cannot drift. See [mcp.md](mcp.md).

## Screenshot sizing

HTTP `/api/screenshot.png`, `/api/windows/{id}/snapshot.png`, and MCP `screenshot`
and `snapshot` share one optional sizing contract. Without sizing, captures use
native dimensions. Supply at most one of `width`, `height`, or `percentage`.
Width and height are whole output-image pixels from 1 through 16384, independent
of compositor logical coordinates. The other dimension follows the source aspect
ratio. `percentage=50` halves native width and height; percentages must be finite,
greater than zero, and at most 200. Every sizing form is limited to twice the native
width and height.

Dimensions round to the nearest integer, with half pixels rounded up and a minimum
of one pixel per side. A result may not exceed 16384 pixels per side or 67,108,864 pixels
in total. Values whose scale cannot be represented are rejected. Source geometry and compositor scale are resolved on the compositor
thread before rendering or allocating capture buffers. Rounding, including the
one-pixel minimum, can change the integer aspect ratio of very small images.
Unknown fields, malformed values, repeated query fields, multiple sizing inputs, and out-of-range
sizes return HTTP 400 or an MCP error. Authentication and PNG output are unchanged.

The browser snapshot helpers accept a sizing object with the same fields.

List thumbnails use the same endpoint and sizing implementation as full-size
captures. Their activity and visibility scheduling is covered separately by #39.
Run `node web/checks/screenshot-sizing.mjs` from the repository root in the Docker rig after building the release
binary to check both transports and record dimensions, PNG bytes, and debug-level
capture and encoding timings.

### Sizing measurements

One software-rendered Docker run with two Foot windows produced the following
scale-1 results. Capture time includes compositor dispatch, rendering and readback;
encoding time includes blocking-pool dispatch. Times are the median of the HTTP and
MCP samples at each size, not a throughput benchmark.

| Target | Requested size | PNG dimensions | PNG bytes | Capture ms | Encode ms |
|---|---|---|---:|---:|---:|
| Desktop | Native | 1000×610 | 35087 | 3.812 | 0.678 |
| Desktop | Width 64, list preview | 64×39 | 933 | 2.670 | 0.037 |
| Desktop | Width 320, grid preview | 320×195 | 6717 | 2.133 | 0.129 |
| Landscape window | Native | 636×351 | 10380 | 1.762 | 0.503 |
| Landscape window | Width 64 | 64×35 | 448 | 0.787 | 0.064 |
| Landscape window | Width 320 | 320×177 | 3587 | 0.831 | 0.271 |
| Portrait window | Native | 300×598 | 11209 | 1.290 | 0.248 |
| Portrait window | Width 64 | 64×128 | 1376 | 0.954 | 0.073 |
| Portrait window | Width 320 | 320×638 | 13007 | 1.391 | 0.255 |

The run checked 63 target/size combinations over both HTTP and MCP, including
compositor scales 1, 1.5 and 2, portrait output, and 1×1 results. Preview dimensions
use the same capture path; no grid UI is implied.

## Window list

`desktop.rs`. `window_info` builds a `WindowInfo` from a `Window`: title and app id from the xdg
toplevel data or the X11 surface, geometry from the space (or the saved position for a minimized
window), states from the acked xdg state or the X11 flags, `focused` from `State::active`, the pid from
the client's socket credentials or `_NET_WM_PID`, `icon` from the name the client set through
xdg-toplevel-icon (its picture, or the pixels it set instead, or its launcher's icon by app id, is at
`GET /api/windows/{id}/icon`), `content` from content-type-v1 (`photo`, `video`, `game`, else `null`),
`updated_ms` from the last applied commit in milliseconds, and `content_revision` from a monotonic
per-window counter. Applied commits, including popup/subsurface content and minimized windows, advance
the revision. Captured geometry and surface-tree membership changes also invalidate the image. `windows()`
walks the space bottom to top, skipping X11 override-redirect surfaces, then the minimized list.
`refresh_windows` runs once per loop iteration. Structural changes publish immediately; content-only
changes publish at most four times per second, retaining the final pending revision after drawing stops. The server caches the encoded message for `/api/windows` and replays it to a new
viewer; window lists to a slow viewer are coalesced to the newest.


Thumbnail verification runs in the Docker rig with `node web/checks/thumbnail-scheduler.mjs` and
`node web/checks/thumbnails.mjs` after building the viewer and release binary. The live check needs
`wayland-protocols`, a C compiler, and Python GObject/GTK 3 support in the image. It compiles a controlled
Wayland client and also paints through X11, checking retained PNGs against fresh captures. It covers
synchronized and desynchronized subsurfaces, popup children and lifecycle, a two-commit burst,
minimized windows, resize, visibility, batched observer records, and disposal during a capture.


Measured thumbnail workload in the Docker software-rendering rig, before/after scheduling changes,
with a 1280×1100 browser viewport at DPR 1 and 640×360 terminal requests. The animated terminal
writes a timestamp every 50 ms. These are single-run observations, not latency guarantees.

| Scenario | HTTP responses, before → after | Response bytes, before → after | Capture/readback ms total, before → after | PNG ms total, before → after |
| --- | ---: | ---: | ---: | ---: |
| Idle window, 4 s after a 1.5 s startup wait | 0 → 1 | 0 → 448 | 0.000 → 0.786 | 0.000 → 0.194 |
| Animated window, 10 s | 13 → 5 | 14,858 → 4,837 | 8.579 → 4.883 | 0.827 → 0.339 |
| Hidden pane, 4 s | 5 → 0 | 6,207 → 0 | 4.581 → 0.000 | 0.633 → 0.000 |
| Ten additional windows starting, 5 s | 19 → 22 | 12,363 → 10,853 | 30.925 → 29.529 | 0.982 → 1.512 |

The first sample includes a trailing initial-size update in the new scheduler; a separate settled-idle
check produced no repeat requests. The startup sample includes initial buffers and size changes, so its
request count increased even though transferred bytes fell. The baseline had one failed response in
each animated/startup sample; the new run had none. Response counts and completed captures are assigned
by their timestamps, so a request crossing a sample boundary can contribute to adjacent samples.
Capture time includes compositor queueing, rendering and readback; PNG time includes blocking-pool
dispatch. These timings do not isolate window-list bookkeeping CPU cost. Hidden-pane capture work fell
to zero. The measurements do not justify adding preview streams or encoders.

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

`snapshot(id, sizing)` renders one window's elements
(`Window::render_elements`, popups included) into an offscreen `GlesTexture` sized to the geometry at
the resolved sizing ratio × output scale, with the element origin at minus the geometry offset so the geometry lands at
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

`decor.rs`. A title bar (32 logical px, the layout in `elsewhere_core::decoration` so the server agrees on
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
  Elsewhere runs in. Nothing is launched: without a session the route answers `503`, and the
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
  documented requirement rather than something Elsewhere tries to inject (the Docker image sets
  it in Chromium's flags file).

## Files

`files.rs` implements operations on explicit paths. The server has no selected-directory state.
`--files-dir` chooses the initial transfer folder, otherwise the XDG download directory or `~/Downloads`.
It is not an access boundary: the process's Unix permissions determine access to the remote filesystem,
including mounted volumes in a container. `/proc` must be mounted for descriptor-based operations.

Every file endpoint, including clipboard file lists/downloads and equivalent MCP tools, requires a control
token. Participants may use files without taking desktop control.

`GET /api/files?path=…` returns `FileListing`: the resolved absolute path, entries, total, offset, limit,
and the count of omitted non-UTF-8 names. Paths are absolute UTF-8 strings, percent-encoded once as query
parameters; `@home` and `@transfer` are exact shortcuts. Entry names are separately encoded path
segments, may be hidden, and cannot be empty, `.` or `..`, contain `/`, or contain NUL. Non-UTF-8 names
are omitted instead of being changed lossily. Symlinks are marked separately: entering a directory
link resolves its destination, and downloading a link follows it only when the opened object is a
regular file. Broken links can be renamed or unlinked. Devices, sockets and FIFOs are never streamed.

Listings sort folders first, then `sort=name|size|modified`; name order is UTF-8 byte order, with optional `desc=true` and `hidden=true`.
`offset` and `limit` paginate each independent listing, default 100 entries and at most 500. Concurrent
filesystem changes can shift pages. Enumeration and sorting run on blocking workers; the UI renders
one page. Reserved `.upload-*.part` entries are omitted even when hidden files are shown.

`GET`, `PUT`, and `DELETE /api/files/{name}?path=…` download, upload, or unlink the named entry.
`POST /api/files` takes `{"op":"mkdir","path":…, "name":…}` or
`{"op":"rename","path":…, "name":…, "new_name":…}`. Rename stays within its directory and never
replaces an existing entry. Delete is nonrecursive and refuses directories. Rename and unlink act on
the named directory entry without following its final symlink. Concurrent external replacement can
change which entry that name denotes. There is no recursive transfer, search, preview, or remote copy/move.

Successful uploads and management return `SavedFile` with `name`, `path`, and `directory`. File errors
carry `error` text and a `code`, including `missing`, `permission_denied`, `exists`, `not_directory`,
`invalid_path`, and `unsupported_type`. A filesystem lacking hard links or atomic no-replace rename
returns `unsupported_operation`; collision protection is never replaced with a racy existence check.
Downloads and uploads stream. Normal nonempty file downloads stop at their size when opened. Empty
or virtual procfs/sysfs files stream to EOF without Content-Length because their reported size may
not describe their contents. Uploads hold an opened directory,
write a temporary file with mode `0666` filtered by the process umask, then publish the opened inode under the first free name, adding ` (2)`
before the extension on collisions. Failure/cancellation cleans up its temporary entry. A renamed
destination remains anchored and its current path is reported; a removed destination fails. Only the
`@transfer` shortcut and staging intentionally create their destination directory.
An explicitly selected absolute path is never recreated.

Listing, upload, download, and delete require `path`. The MCP `files` tool accepts the same
listing arguments and returns the same `FileListing`. Staging keeps its stricter batch/name
validation and name-only reply.

Each viewer starts with `@transfer` and keeps its navigation in client state. The first visible Files
panel fetches a listing; navigation, sorting, pagination, hidden-file changes and Refresh request new
listings. Reopening or focusing it does not. Successful local operations refresh only their affected
current directory. Stale responses are cancelled. Every queued upload batch captures its directory
before any asynchronous work and reports final saved names, destination and partial failures.

Desktop drops and pastes retain `PUT /api/drop/{batch}/{name}` cache staging, carried through the drag
or clipboard operation by a client-generated batch ID. The Wayland recipient chooses its destination.
Unclaimed drops and cancelled partial batches link validated regular files to the transfer folder,
copying through a temporary file across filesystems. Publication is collision-safe. `FILE_RESULT` reports saved paths and failures to control-token
sessions; only the client remembering that batch displays the result and offers Open folder. This
operation result is not a directory update subscription. Staged sources remain for the hourly sweep,
which removes batches older than a day. No navigation or unrelated refresh follows a late result.

Run `node web/checks/file-browser.mjs` in the Docker rig after building the viewer and release binary to
check two viewers, authorization, navigation and upload races, special files, pagination and actions.

Dragging local files over the stage is carried on as a drag on the desktop (`Drag` message; `State::drag`
in `clipboard.rs`; the source is `FileSource` there, the outcome comes to `DndGrabHandler` in `handlers.rs`).
`dragenter` starts a compositor-owned drag (a `DnDGrab` on the pointer with our source) offering `text/uri-list` to copy or to move (Thunar takes the move, so the staged file
leaves for the folder shown; Nautilus copies, as GTK 4 prefers when both are offered, and so does an
application that only copies, leaving the staged file to the sweep), from a synthetic left-button press made
over nothing so no client sees a press without its release; `dragover` is ordinary pointer motion, which
the drag grab turns into `wl_data_device` enter/motion for the application under the pointer; `dragleave`
lets go over nothing (`cancel`). The browser gives file contents only on `drop`, so the files are
uploaded then, staged (the drag holds still; the page shows the upload), and `drop` names them: the compositor
leaves and re-enters the target with a fresh offer whose list it can read now (Thunar reads it during
the drag to decide, once per offer, and refuses without it; Nautilus preloads it and keeps what it read;
a request before the drop gets EOF at once, because GTK 3 never asks again if the pointer leaves while a
read is pending), then releases the button once the target has accepted a mime and chosen an action,
sending a motion every 100 ms so it looks again, or after 1.5 s regardless. The release happens on the
next loop turn: the accept and action callbacks run inside the offer's request handler, which holds the
lock the drop takes. The page is told whether the application took the files (`Notice`); a refused drop
sends them to the transfer folder. A blur, disconnect or handover mid-drag cancels it (`release_all`),
and a drop whose upload outlived the grab is answered as not taken. X11 applications get no drop
(Smithay's XWM bridges XDND on master, but our pointer focus is the Wayland surface, and drops on X11
windows are not verified). A drag an application starts itself (a file out of Thunar) comes to
`WaylandDndGrabHandler::dnd_requested`, which puts Smithay's `DnDGrab` on the pointer or the touch: its icon surface is drawn at the pointer, offset by its
buffer offsets, while the drag lasts (`dnd_icon`), and pointer motion renders a frame so it moves.

## Notifications

`notify.rs`. Applications send desktop notifications to `org.freedesktop.Notifications` on the session
bus; with no panel nobody would answer, so the server requests that name at startup with
`DoNotQueue` (an owner keeps it: on a host with a real daemon we log and do nothing) and serves the
interface with zbus: `Notify` stores the notification (a nonzero `replaces_id` is its id, with `rev`
counting the replacements), sends every viewer and window session the whole open list as one
`Notifications` message (a snapshot, so a dropped message loses nothing), and arms its expiry
(`expire_timeout` −1 becomes 5 s, 0 or a critical urgency means until closed); `CloseNotification` and
the expiry remove it and send the list again; `GetCapabilities` says `actions`, `body`, `icon-static`. A
viewer's `Notify` message or `POST /api/notifications/{id}` with an action key the application offered
emits `ActionInvoked` and closes the notification (reason 2); no action just closes it; `default` when the
application offered no such action brings its newest window forward instead, matched by the
`desktop-entry` hint or the application name against `app_id`. The picture is resolved when the
notification arrives, in the specification's order (`image-data`, `image-path`, `app_icon` as a name, path
or `file://` URI, the launcher's icon from `desktop-entry`, the legacy `icon_data`), and served at `GET
/api/notifications/{id}/icon`. The page stacks the notifications top-right on the stage with the icon,
summary, body (markup stripped) and action buttons; the ✕ dismisses (a view-only session only hides it
locally).

## Clipboard

`clipboard.rs`. When a client takes the clipboard offering `text/uri-list`, a text mime type or `image/png`
(`new_selection`, or the Xwayland path in `xwayland.rs`), the compositor asks the owner for it (text
first) through a pipe on the next loop idle (the request is deferred out of the selection handler and
any read still in progress is dropped), reads it on the event loop (calloop `Generic` on the
non-blocking read end; 1 MiB cap for text, 16 MiB for a PNG) and sends `Event::Clipboard { mime, data }`;
the server keeps the last one for `GET /api/clipboard`, whose Content-Type says which it is, and tells
the viewers with the `Clipboard` message (the text itself) or `ClipboardData` (the mime only; the page
fetches the bytes). Neither is replayed to a connecting viewer, whose browser clipboard may be newer.
`Command::SetClipboard { mime, data }` makes a compositor-owned selection whose user data carries the
bytes (text is offered under every text mime, a PNG as `image/png`); `send_selection` writes them from a
calloop source on the non-blocking pipe, so a slow reader never blocks the compositor, and X11 clients
are offered it too. The selection user data distinguishes relayed X11 selections from our own. Setting
the clipboard drops a read still in flight, so an application's older clipboard can't land after the
new one. Either pipe is closed after ten seconds if its peer stalls. The primary selection is not
bridged; other image formats aren't read (GTK, Qt, Firefox and Chromium all offer PNG), and an X11
client's PNG isn't either: Smithay's Xwayland selection code resolves only text targets. Our PNG is
offered to X11 clients.

Files: a file manager's copy offers `text/uri-list` and `x-special/gnome-copied-files` (the same list
with a `copy` first line) besides the paths as text; the compositor reads the URI list then, the page
shows "N files copied" with a download button, and `GET /api/clipboard/files/{index}` streams the
`index`th file of the list currently on the clipboard (only that list: the route can't read anything
else). The other way, files pasted into the page are staged first (`PUT /api/drop/{batch}/{name}`), then
`POST /api/clipboard/files` with that `"batch"` makes them the desktop clipboard as a URI list offered under both mimes (the
gnome one rewritten the way file managers write it: `copy`, then one URI per line, LF only, no trailing
newline; Nautilus refuses a CR or an empty line), and the paste chord follows through the API; Thunar
and Nautilus paste them as copies.

The page writes received text to the browser clipboard at once when it may, otherwise on the next
gesture; a received image is fetched from the API and written as a `ClipboardItem`. Ctrl+V and
Shift+Insert are not forwarded immediately: the browser's `paste` event (which needs no permission)
delivers the text, which goes to the desktop as `SetClipboard`, and the key press and release follow, so
the application pastes the browser's content; if no paste event comes within 150 ms the key goes
through on its own. A pasted image goes by `PUT /api/clipboard` instead, and the user's chord is dropped
(its modifier may be released before the upload ends): once the upload succeeded the same chord is
pressed through `POST /api/input`, and not at all if it failed. The compositor reports its own
clipboard back as an `Event::Clipboard` like an application's, so the server's cache and every viewer
follow one ordered stream.

## Viewers

`ws.rs`. `Viewers` holds every session (`ViewerSession`: which token, its event and audio senders, its
last size, and the `StreamControl` of its own encoder) and the shared state they all see (cursor,
pointer lock, window list, clipboard text, the output as the controller last sized it). A session
authenticates, sends `Hello` (which picks its codec), gets an encoder from the server's `SinkFactory`
and hands the sink to the compositor with `Command::ViewerStream`; the compositor submits every
output frame to every viewer sink (each with its own dup of the dmabuf fd and a share of the swapchain
lease, so the slot is free when the last encoder is done) and encodes nothing while there is none.
Frames go from each session's own channel to its socket in order, with a ten-second send deadline, the
way window streams do; audio and events are broadcast with `try_send`. A session's video goes to its
WebRTC data channel instead while one is open (`rtc.rs`: the page's offer arrives as an `Rtc` message,
the page includes its hostname and port in the offer, which the server resolves for the answer
unless `--rtc-addr` sets the advertised endpoint. The hub answers as an ICE-lite str0m peer. The hub drives every
session's peer connection over one UDP socket per local address, and fragments frames into 16 kB
channel messages; `Hub::is_open` says whether a session's channel is up; window sessions use the same
hub under keys with the top bit set); a channel that closes, a `{"close": true}`, or the session's end
drops the peer. The controller's `Mic` packets
go to `Config::mic`, the channel `elsewhere-stream`'s `audio_sink` plays into the microphone sink (`elsewhere`
creates the sink and the remapped source next to the audio sink), and its `Cam` frames to `Config::cam`,
which `video_sink` decodes (VP8) and scales to 720p YUY2 for the `--webcam` loopback device (which keeps
the first format it is given); a session whose frame didn't fit the channel sends nothing more until a
keyframe (a VP8 frame tag's low bit is clear), and a pipeline that dies (its bus says so; the feeder
logs it and ends) withdraws the feature and tells the controller; the `Role` message's second byte tells
sessions which of the two exist.

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
encoder sink the server made for it (`SinkFactory` in `elsewhere-server`, a `GstSink` per stream). After every
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

## Document picture-in-picture

The top-bar Picture-in-picture action presents the desktop, or the current window in a window viewer.
Each window row also has a Picture-in-picture action alongside its ordinary popup action. One PiP
window belongs to an opener: choosing its current content focuses it, and choosing another target
replaces the iframe and disposes the previous viewer. Return to main viewer closes the presentation,
not the remote application. Closing the remote application closes its window PiP.

The PiP document hosts a same-origin iframe with the existing viewer. Its keyboard, pointer, clipboard,
renderer, resize observer and media objects all belong to that iframe's document. Authentication uses
the existing URL fragment and session storage. The compact toolbar shows title, connection/control
status and Return. Desktop controllers can open the existing keyboard row for composition input.
Fullscreen remains available in the normal viewer.

Desktop control transfers only from its current owner to a live control-token connection, using the
server's conditional `Handoff` message. A participant opening PiP keeps watching until explicitly
claiming control. Closing PiP conditionally returns control to the opener; a third party's intervening
claim is preserved. Taking control in the opener closes its desktop PiP first. If the opener is
disconnected when PiP closes, the server uses its normal oldest-session election. Only the controller sizes the desktop; other presentations scale their stream to
their actual viewport and DPR. Each presentation has its own decoder.

Desktop PiP owns playback while it is open. The opener stops its playback graph and restores it on
return. Losing control stops microphone and camera capture; PiP never starts either automatically.
This playback ownership covers the opener/PiP pair; unrelated viewers still have their own audio.
Window streams keep their existing video-only behavior. Reconnecting stays inside the existing PiP;
closing it never schedules another browser window. Opener navigation, authentication failure and
remote window closure dispose owned content.

The action requires a secure context and a browser exposing Document PiP, and opens directly from a
click. Requests can fail and requested dimensions can be clamped. The browser controls placement,
origin identification, close controls and always-on-top behavior. PiP cannot enter fullscreen or
navigate its top-level document. See the [Document PiP specification](https://wicg.github.io/document-picture-in-picture/)
and [Chrome implementation guide](https://developer.chrome.com/docs/web-platform/document-picture-in-picture/).

Docker checks used headed Chromium 152.0.7977.82 and Firefox 155.0.1 on a virtual X display with a window
manager. Both opened desktop and window viewers, accepted real keyboard input, rendered changing
content while the opener was backgrounded and minimized, and returned control. Native Wayland event
logs confirmed clicks and wheel input in both browsers, plus touch in Chromium. Both browsers accepted
pointer lock, composition commits through the desktop keyboard row, and unclaimed file drops into the
transfer folder. Composition checks inject a browser composition commit; they do not exercise every
installed operating-system IME. Window PiP keeps the ordinary window viewer's keyboard behavior.

Chromium checks additionally cover pairwise audio ownership, stopped microphone capture, third-party
control, read-only tokens and handoff targets, rejected/unsupported APIs, ordinary popups/fullscreen,
content replacement, reconnection, viewport resize, held-key release, navigation and token rotation.
Browser size hints remain subject to clamping; the viewer uses the dimensions it actually receives.
Requests for a 100,000-pixel square PiP were clamped to 1280×720 in Chromium and 1280×800 in Firefox
in the same rig. DPR checks cover 1, 1.5 and 2. CDP omits the iframe media-query event for DPR-only overrides, so that
check supplies the event and verifies rearming and output scale. It does not simulate physically moving
a window between monitors. Pointer-lock failure notices are exercised with error events.

The focused canvas becomes editable only for a pending paste shortcut, so Firefox can dispatch
Ctrl+Shift+V. The paste handler prevents insertion into the canvas and clears that attribute;
key release, timeout and input release clear it too. The viewer's own fields keep native paste.
`web/checks/clipboard-firefox.mjs` uses headed Firefox native key actions to verify trusted paste
events and a terminal round trip in desktop, window and both PiP viewers. It also checks local-field
isolation, pointer capture, read-only roles and fallback timer cleanup. A desktop participant can
synchronize the clipboard without forwarding a paste key to the application.
Clipboard shortcuts reserved by the browser or operating system
still follow that platform's rules.
Copied-file download controls and microphone/camera controls remain in the normal viewer; Return takes
you there. Pointer capture has an indicator and reports failure, but PiP cannot capture fullscreen
browser shortcuts. The checks live in `web/checks/picture-in-picture.mjs` and
`web/checks/picture-in-picture-firefox.mjs`; they require the Docker release build, display `:95`,
`wev`, and a geckodriver on port 4445 for Firefox.

## Browser UI (`web/src`)

React and Tailwind, built by Vite into `web/dist` by `make web` and embedded (see the README). The engine
`viewer.js` owns the canvas and the connection and publishes state through `store.js`; the components
read it with `useSyncExternalStore` and send actions back through the engine.

- **Layout** (`App.jsx`): a top bar (name, the application menu, connection status, codec and size, the
  toggles, About, fullscreen, the power menu),
  the stage (`Stage.jsx`: the canvas, centred and fitted to it, plus the overlays and the status banners), the
  side panel (`Sidebar.jsx`) and a status bar (`StatusBar.jsx`: fps, bandwidth, input-to-paint latency,
  loss counters, clipboard, pointer lock, audio). The controller's stage sizes the desktop's output; the
  other sessions get it fitted into theirs. Fullscreen
  is requested on the stage element, so the chrome is gone while it lasts. Toggles are remembered in
  `localStorage`. A popup (`?window=ID`) shows the window's title in the top bar, no side panel, and the
  canvas centred at the window's size, scaled down if the popup is smaller.
- **Touch and phones** (`viewer.js` `onTouch`, `Keyboard.jsx`): pointer events with `pointerType`
  `touch` take their own path. By default (a desktop page, "touch as mouse" off) every event goes out as
  a `Touch` message and the compositor's seat has a `wl_touch`: `State::touch` in `input.rs` turns it
  into `down`/`motion`/`up` on Smithay's touch handle with the surface under the finger (the same
  hit-test as the pointer), one frame per event (a cancelled finger is lifted: Smithay's `cancel` skips
  points already framed), so applications get real touch points and
  Xwayland gives X11 clients XI2 touch (with its own pointer emulation). A down focuses and raises like
  a click (`focus_at`; a panel above the windows gets the keyboard if it asked, as with a click); on a
  drawn title bar or resize band it starts `TouchMoveGrab`/`TouchResizeGrab` (`grabs.rs`: the pointer
  grabs' `drag`/`finish` bodies under Smithay's `TouchGrab`, ended by the finger that began them), and a
  bar button acts at the down (which goes where the finger was, not where a window moved). The
  compositor keeps the slots down and `release_all` lifts them, so a blur, a disconnect, a handover or
  the page's mode switch (which releases all input first) leaves no finger on an application. With
  the switch on (or in a window tab, whose session resolves pointer positions itself), one finger is a
  pointer with one button: `MOTION_ABS` at the contact,
  a tap (up within 500 ms, under 10 px of movement) is a left press and release, a hold of 500 ms a
  right one, and movement presses the left button first and drags (so a hold never clicks and a drag
  starts after GTK's own threshold). Two fingers cancel the one-finger gesture: the centre's movement
  goes out as pixel `AXIS` (finger scroll), unless the distance changed by 15 % since the two fingers
  came down (or the view is already zoomed), which pinches: a CSS `translate(…) scale(k)` on the canvas,
  1 to 5, about the fingers' centre, panned by the centre and kept covering the canvas's own box, snapping
  back under 1.05; a third finger, or one of three lifting, starts the two-finger gesture over. The
  desktop's size is the phone's; the zoom is only on the phone, and any session may zoom (it acts on
  nothing), while only the controller's fingers drive. Pointer positions go through the canvas's on-screen rectangle
  (`toDesktop`), which follows the transform, so a zoomed tap lands where it points; the overlays don't
  follow it. Layout: under 48 rem the side panel is a drawer over the stage (off by default there), the
  top bar and status bar keep icons and the few numbers that fit, `#root` is `100dvh` and the viewport
  meta says `interactive-widget=resizes-content`, so the phone's keyboard shrinks the stage (a desktop
  resize) instead of covering it. The keyboard button (touch devices, the controller) opens a row: a
  field that keeps the phone's keyboard up (taking the focus releases any key held on the stage), whose
  native `beforeinput` turns `insertText` and `insertFromPaste` into `{"type": "text"}` (composition
  waits for `compositionend`), `insertLineBreak` into Return and the deletions into BackSpace, Delete
  and their Ctrl word forms; physical keys that aren't text (`KEYSYM`) and modifier chords go
  as `{"type": "key"}`; and buttons for Esc, Tab, sticky Ctrl/Alt/Super (the next key or character is a
  chord), the arrows and Del. Both go on the WebSocket as `Input` (`0x91`), the `InputMsg` of
  `POST /api/input`, so they type through the compositor's keymap in order with the pointer.
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
- Thumbnails use `content_revision` and their required image dimensions to detect pending updates.
  A row is eligible only while the sidebar and Windows tab are open, the document is visible, fullscreen
  is not hiding the pane, and the row intersects the list viewport. Unchanged rows reuse their image.
  Each window has a three-second minimum between request starts and one coalesced trailing update;
  continuous activity does not postpone that update. The shared queue serializes thumbnail requests and
  rechecks eligibility before each start. Requests have a fifteen-second client deadline. A final failed
  update gets one retry while eligible; new content activity permits another attempt, still throttled.
  Hidden or removed rows cancel timers and abort obsolete client requests. Already-dispatched compositor
  work may finish. Old images remain until a successful replacement; replacement and disposal revoke
  owned blob URLs. Explicit full-size snapshots bypass thumbnail scheduling.
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
- `window.elsewhere()` returns the numbers; `elsewhere.windows()`, `elsewhere.control()`, `elsewhere.activate()`, `elsewhere.spawn()`,
  `elsewhere.snapshot()`, `elsewhere.elements()` and `elsewhere.clipboard.read()/write()` act on the desktop.

## Security model

There are no cookies, so there is no ambient credential to ride on: every HTTP request carries a
bearer token and the WebSocket authenticates with its first message. Two tokens: the control token can
do everything (`spawn` is remote code execution for whoever holds it, which the viewer already implied,
since it can type into a terminal; `launch` runs an installed program's `Exec` line, `quit` ends the
desktop); the viewer token is for showing the desktop to someone who should
only watch: it gets the video, the window list, elements, snapshots and the clipboard (copied files
included), and `403`
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
