---
name: browser-wayland
description: Drive a browser-wayland desktop (Wayland compositor streamed to a browser) through its HTTP API or MCP tools: list windows, read a window's UI elements, click and type, take snapshots, start programs.
---

# Working a browser-wayland desktop

browser-wayland is a Wayland compositor whose screen is a browser tab. It is also the window manager,
so it can tell you what is on the screen and act on it. You talk to it over HTTP with a bearer token
(the same token the server prints in the viewer URL as `#token=`), or through its MCP tools, which are the same
operations under the same names. `reference.md` next to this file lists every route, tool and field.

```sh
T=...                                   # the token; usually in $BW_TOKEN or ~/.config/browser-wayland/token
H="Authorization: Bearer $T"
curl -s -H "$H" https://host:8443/api/windows | jq
```

## The loop that works

1. **Look at the window list first** (`GET /api/windows`, tool `windows`). Each window has an `id`, a
   `title`, an `app_id`, `focused`, `minimized`, and its geometry `x y w h` in logical pixels. Ids are
   stable for the window's life. Menus and tooltips are not windows; they show up as `popups` on the
   window that owns them.
2. **Read the window's elements** (`GET /api/windows/{id}/elements`, tool `elements`) instead of
   guessing from pixels. You get buttons, links, text fields, menu items, tabs, checkboxes and so on,
   each with `role`, `name` and a rectangle `x y w h` **relative to the window's own `x y`**. The
   answer's `level` tells you how much the application exposes: `full` is the normal case; `none`
   means the toolkit publishes nothing (terminals, games); `frame` means Chromium or Electron running
   without `--force-renderer-accessibility`. Below `full` the application's part of the list is empty
   (the compositor's title bar and buttons, when it draws them, are still there) and a snapshot is
   your only view of the content.
3. **Act on an element** with the input operations (`POST /api/input`, tools `click`, `type`, `key`,
   `scroll`, `move_pointer`). Pass the window id together with element-relative coordinates, for
   example the centre of the element's rectangle, and the server adds the window position for you.
   Without a window id the coordinates are output coordinates.
4. **Read the elements again** after acting; the tree reflects the new state (menus that opened,
   dialogs that appeared as new windows). Menus are placed at their popup, so their items have
   correct rectangles too.
5. **Take a snapshot when you need to see** (`GET /api/windows/{id}/snapshot.png`, tool `snapshot`;
   `/api/screenshot.png`, tool `screenshot`). Window snapshots are lossless PNGs of the window's own
   buffers, so they work for covered and minimized windows; `scale` shrinks either kind (0.5 is usually
   plenty for reading a layout). Prefer elements for finding things and snapshots for confirming.

## Input details

- `click` moves the pointer there first. `button` is `left` (default), `right` or `middle`; `count`
  2 double-clicks. For a drag use `button` (press), `move`, `button` (release).
- `text` types a string through the keyboard layout, including punctuation and capitals; `\n` is
  Return. Click into the field first so it has focus. For longer text, put it on the clipboard
  (`PUT /api/clipboard`, tool `clipboard_write`) and press `ctrl+v` in the field; `GET /api/clipboard`
  (tool `clipboard_read`) returns what an application last copied.
- `key` presses a chord and releases it: `ctrl+s`, `ctrl+shift+t`, `alt+F4`, `Return`, `Escape`,
  `Tab`, `Down`, `Prior` (Page Up), `F5`. Modifier names: `ctrl`, `shift`, `alt`, `super`. Anything
  else is an X keysym name or a single character; `ctrl+T` is the same as `ctrl+t` (write `shift` when
  you mean it). A chord with a key the layout doesn't have does nothing.
- `scroll` takes wheel lines; positive `dy` scrolls down.
- Input goes to whatever is under the pointer or has keyboard focus, exactly as a user's would. Click a
  window (or `activate` it) before typing into it. A human viewer may be connected at the same time;
  you share one pointer and keyboard with them.

## Windows and programs

- `activate` raises and focuses a window (and restores it if minimized); `close`, `minimize`,
  `unminimize`, `maximize`, `unmaximize`, `fullscreen`, `unfullscreen` do what they say; `move` and
  `resize` work on floating windows. These requests are fire-and-forget: check the window list
  afterwards.
- `spawn` runs a shell command as a client of this desktop (`sh -c`), with the display variables set.
  Programs take a moment to map their window; poll the window list. A program that is already
  running elsewhere may just open a new window in that instance.
- `updated_ms` on a window is the time of its last redraw to the second. If it stops changing, the
  application is idle.

## What the answers mean when they fail

| Status | Meaning | What to do |
|---|---|---|
| 401 | missing or wrong bearer token | check `Authorization: Bearer` |
| 404 | no such window | the window closed; list again |
| 429 | another snapshot is in flight | one at a time; retry after it returns |
| 500 | the snapshot render failed, or the requested size is out of range (the body says which) | a retry only helps the former; lower `scale` for the latter |
| 501 | the server runs without `--elements` | use snapshots instead |
| 503 | the compositor or the accessibility bus didn't answer | retry once; if the body says there is no D-Bus session, elements are not available on this server |
| 400, 415, 422 | the body wasn't JSON, lacked `Content-Type: application/json`, or had the wrong shape (plain-text message) | see `reference.md` for the shape |

MCP tools return the same failures as tool errors with the same text.

## Things that surprise people

- Coordinates from `elements` are relative to the window; the window's own `x y` are output
  coordinates. Pass the window id to the input operations and you never have to add them.
- A window with `decoration: 32` has a title bar drawn by the compositor above it. Its elements list
  ends with that bar (`title bar`) and its `Close`, `Maximize`/`Restore` and `Minimize` buttons at
  `y: -32`: click those like any element (negative `y` is fine), or use the `window_control` tool.
- Two windows of the same application look alike in the list; use `title` and `focused`.
- A dialog is a new window with its own id. A file chooser is often a separate window too.
- Chromium and Electron applications need `--force-renderer-accessibility` on their command line to
  expose their content; without it you get `level: frame`. Firefox and GTK and Qt applications work as
  started by `spawn`.
- The screen is whatever size the connected viewer is; with no viewer it is 1920×1080.
