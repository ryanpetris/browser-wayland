# browser-wayland

A headless Wayland compositor whose screen is a browser tab. Clients render on the GPU, the
composited frame is hardware-encoded with VA-API (AV1, HEVC, VP9 or H.264, whichever the browser
decodes best and the GPU encodes) and streamed over a WebSocket, and the browser decodes it with
WebCodecs. Mouse, keyboard, audio and the clipboard travel the same way.

Design notes: [docs/architecture.md](docs/architecture.md), [docs/protocol.md](docs/protocol.md),
[docs/panels.md](docs/panels.md), [docs/desktop-api.md](docs/desktop-api.md), [docs/mcp.md](docs/mcp.md).

## Install

Releases (made from `vX.Y.Z` tags; the tag is the version) carry a Debian package built on Debian
stable that also installs on Ubuntu 24.04 and later, an Arch package, and a tarball with the binary.
Building from source needs Rust stable, Node 24 (for the viewer) and the development packages for
GStreamer (core and base), libgbm, libEGL and libxkbcommon; `make` builds the viewer and then
`target/release/browser-wayland`, which reports `0.0.0-dev` unless `BW_VERSION` is set. The Arch
`PKGBUILD` is in `packaging/arch`.

## Requirements

- Linux with a GPU render node (`/dev/dri/renderD128`) and Mesa.
- GStreamer 1.24+ with the VA plugin: `gst-plugin-va` on Arch (`vapostproc`, `vah264enc`), for
  hardware encoding; or `--software-encoding` with the vpx (good), x264 (ugly), x265 or svtav1 (bad)
  plugins, which encodes on the CPU (the desktop then runs at 30 Hz) for machines without a usable GPU encoder.
- `xorg-xwayland` for X11 clients, and PipeWire or PulseAudio with `pactl` for audio (both optional).
- Rust stable and Node 24 to build. The browser needs WebCodecs (Chromium, Firefox 130+, Safari 26+).

## Run

```sh
make run ARGS="--exec foot"                   # any Wayland client; WAYLAND_DISPLAY is set for it
```

(`make web` once, then plain `cargo run --release -- --exec foot` works too.)

The server prints the certificate fingerprint and two URLs like `https://<lan-ip>:8443/#token=…`: one
with the control token, one with the view-only token. Open the first in a browser on the LAN, compare
the fingerprint before accepting the self-signed certificate, and the desktop appears. The desktop takes
the size of the controlling viewer's display area; the fullscreen button hands it the whole screen, with
keyboard lock so shortcuts like Ctrl+W reach the desktop.

Any number of people can watch at once, each with a stream scaled to their own window. The first to
connect with the control token drives the pointer and keyboard; anyone else with that token sees a
"Take control" button, and the desktop then takes their window's size. Whoever opens the view-only URL
can watch, read the window list and elements, and take snapshots, but not act.
Each viewer picks its own codec and quality in the status bar: the codec list is what both the server
and that browser can do ("Auto (HEVC)" shows the pick), and the quality is Low (2 Mbit/s, 30 fps), Medium,
High, Max, or Auto, which starts at `--bitrate` and backs off while the connection can't keep up, then
climbs back; `GET /api/codecs` lists the server's side. After the picture stops changing one more frame
goes out, at four times the bitrate with the software encoders, so text left rough by motion sharpens.
Frames are painted on a 2D canvas. `?renderer=webgpu` in the URL uses a WebGPU external-texture path
instead; it is opt-in because Chromium on Linux occasionally presents a blank frame that way, which looks like flicker.

Windows that don't draw their own title bar (X11 applications, Vulkan and SDL programs, anything that
asks for server-side decorations) get one from the compositor: drag it, double-click it to maximize,
resize from the edges, close, maximize and minimize with its buttons.

Other clients can join later: `WAYLAND_DISPLAY=wayland-browser some-app`. X11 apps work too: an
Xwayland is started automatically and the log prints its `DISPLAY`. Super (or Alt) + left drag moves
any window from anywhere in it.

Audio from clients goes to the browser instead of the host speakers: the server creates a private
`browser-wayland-<pid>` sink (printed in the log) and captures it as Opus. Clients started with
`--exec` get `PULSE_SINK` set; for others use `PULSE_SINK=<that name> some-app`. The other way, the
microphone button in the viewer's status bar sends the browser's microphone (Opus, with echo
cancellation and noise suppression) into a virtual source `browser-wayland-microphone-<pid>` that
applications record from (`PULSE_SOURCE` for `--exec` children; a video call sees it as a microphone);
only the controlling session's is taken, and stopping it ends the capture, so the browser's recording
indicator goes off. `--no-audio` turns both off; both devices are unloaded when the server exits (Ctrl+C
or SIGTERM).

The browser's webcam works the same way through a `v4l2loopback` device, the one kind of camera every
application understands: on the host, `modprobe v4l2loopback exclusive_caps=1 card_label=browser-wayland`
(the package is `v4l2loopback-dkms` on most distributions) and start the server with `--webcam
/dev/videoN` (the device it made; in Docker add `--device /dev/videoN --group-add video` to `docker run`).
The camera button in the status bar then sends the webcam as VP8, 720p, to that device, which video
calls in the desktop pick as a camera; without `--webcam` (or if the device can't be opened, which the
log says) there is no button.

`--exec` runs at startup with the environment of a Wayland session (`XDG_SESSION_TYPE`, the toolkits'
backend switches, `DISPLAY` for X11 programs), and `--kiosk` fullscreens every window. Together they run a whole nested desktop; with the
`mutter-devkit` package installed, GNOME:

```sh
cargo run --release -- --kiosk --exec 'dbus-run-session -- gnome-shell --devkit'
```

Clients from `--exec` also get `GSK_RENDERER=ngl`: GTK 4.22's default Vulkan renderer intermittently
flashes thin dark triangles (the nested shell's viewer is GTK too). Set it yourself for GTK apps you start by hand.

The devkit's window follows the browser size. Don't add `--virtual-monitor`: that adds a second
monitor, and GNOME puts its top bar only on the first one.

Panels work without a nested desktop: the compositor speaks wlr-layer-shell and
wlr-foreign-toplevel-management, so waybar and xfce4-panel run as ordinary clients with working
taskbars (maximized windows stay clear of the panels, minimize goes through the taskbar).

```sh
cargo run --release -- --exec 'waybar & exec foot'                              # add "wlr/taskbar" to modules-left for a taskbar
cargo run --release -- --exec 'dbus-run-session -- sh -c "xfce4-panel & exec foot"'   # first run asks for the default config
```

xfce4-panel needs a D-Bus session bus for xfconfd; the wrapper is only needed where there is none.
 Its pager shows the single workspace. Waybar's default config draws its
icons with Font Awesome (`otf-font-awesome`); GTK only shows icons in the Xfce menus with
`gtk-menu-images=1` in `~/.config/gtk-3.0/settings.ini`, which xfsettingsd normally sets. Windows
that draw their own title bar (GTK applications, Firefox, Chromium) take its buttons from the GSettings
key `org.gnome.desktop.wm.preferences button-layout`, whose GNOME default is `appmenu:close`; without
a desktop that sets it, minimize and maximize are missing until you do:

```sh
gsettings set org.gnome.desktop.wm.preferences button-layout 'menu:minimize,maximize,close'
```

The `Dockerfile` packages all of that on Arch Linux: browser-wayland, the Xfce panel and apps,
Firefox and Chromium, with PipeWire for audio and Mesa's OpenGL and Vulkan drivers for Intel and AMD
(`glxgears`, `vkcube` and the info tools are included to check them), and the two GTK settings above.
The desktop starts empty; the viewer's menu launches the applications, and `--exec xfce4-panel` after
the image name adds the panel. `make docker-run` builds the image and runs it; the details are
in the Dockerfile's header.

The page says when the server closed its socket with a token dialog ("wrong token" or "token
rotated"; the tokens change with the data directory, e.g. a fresh container without a volume).

## Files

Drop a file on the page and it lands in the desktop's Downloads folder (`--files-dir` for another);
the Files tab of the side panel lists that folder, with download and delete, and an Upload button for
browsers without drag and drop. The same folder is `GET`, `PUT` and `DELETE /api/files/{name}` on the API.
Drag a file over the desktop itself and the application under the pointer sees a drag coming; let go and,
once the file is uploaded, it is dropped there as a `file://` URI (Thunar and Nautilus copy it into the
folder shown), so the file lands in the transfer folder either way.
Files copied in a file manager inside the desktop show up in the status bar with a download button, and
files copied on your machine paste into the desktop's file manager (they land in the folder first).

## Phones and tablets

The page works on a phone. Fingers reach applications as real touch points (`wl_touch`; X11
applications get XI2 touch through Xwayland), so a map pans with one finger and zooms with two, and a
drawn title bar drags with a finger. The hand button switches to "touch as mouse" for applications that
handle touch badly: a tap clicks, a finger drags, a hold of half a second right-clicks, two fingers
scroll, and a pinch zooms the picture on the phone (the desktop keeps the phone's size; two fingers pan
while zoomed, pinching back undoes it). The side panel slides over the stage, the top bar
keeps its icons, and on touch devices a keyboard button opens a row with a field that brings up the
phone's keyboard, whose text goes through the desktop's keyboard layout, and the keys such keyboards
lack: Esc, Tab, Ctrl, Alt and Super (sticky, for the next key), the arrows, Del. Fullscreen works where
the browser allows it (Android; iOS Safari has no fullscreen for pages, and needs Safari 26 for
WebCodecs).

## Window streams

The ↗ button on a window's row in the panel opens it in a popup of its own, streaming only that window
(`/?window=ID`): the pointer, keyboard and clipboard work there as in the viewer, resizing the popup
resizes the window, and the popup reports when the window closes. Each such popup has its own encoder.

## Desktop API

The compositor is the window manager, so the viewer and outside scripts can see and drive the desktop.
HTTP calls send the token as `Authorization: Bearer <token>`; the viewer page takes it from its URL
fragment once (`#token=`, never sent to the server), keeps it in `sessionStorage` and drops it from the
address bar, and sends it as the first message on its WebSocket. A tab without a token shows a dialog
asking for one. There are no cookies and a token is never in a URL the server sees. The view-only token
works for everything below that reads (the window list, elements, snapshots, the clipboard, copied files
included) and
gets `403` for everything that acts. `POST /api/token/rotate` (with the control token) issues new
tokens: the files, the API and every viewer switch at once and the server prints the new URLs.

```sh
T=$(cat ~/.config/browser-wayland/token)
curl -s -H "Authorization: Bearer $T" http://host:8443/api/windows | jq        # the window list
curl -X POST -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
     http://host:8443/api/control -d '{"id":3,"op":"minimize"}'                # act on a window
curl -X POST ... /api/control -d '{"op":"spawn","cmd":"firefox"}'             # start a program
curl -o w.png -H "Authorization: Bearer $T" 'http://host:8443/api/windows/3/snapshot.png?scale=0.5'
curl -o screen.png -H "Authorization: Bearer $T" http://host:8443/api/screenshot.png
curl -s -H "Authorization: Bearer $T" http://host:8443/api/windows/3/elements | jq   # with --elements
curl -X POST ... /api/input -d '{"type":"click","window":3,"x":549,"y":47}'  # click an element
curl -X POST ... /api/input -d '{"type":"text","text":"hello"}'                 # type; also key, scroll, move, button
```

Each window reports `id`, `title`, `app_id`, `icon` (with the picture at `/api/windows/{id}/icon`), `content`
(`video`, `game` or `photo` when the client says so), `x11`, `pid`, its geometry `x y w h` in logical pixels, where
that geometry sits in the client's surface (`geo_x geo_y`), its open `popups` (`[x, y, w, h]` relative to
the geometry), the height of the title bar the compositor draws above it (`decoration`, 0 when the
application draws its own), its stacking index `z` (`null` while minimized), `maximized`, `fullscreen`,
`minimized`, `focused`, and `updated_ms`, the time of its last commit to the second. Ops: `activate`, `close`, `minimize`, `unminimize`, `maximize`,
`unmaximize`, `fullscreen`, `unfullscreen`, `move` (`x`, `y`), `resize` (`w`, `h`), `spawn` (`cmd`, run
with `sh -c` in the same environment as `--exec`), `launch` (`app`, an id from `GET /api/applications`,
the installed `.desktop` launchers, whose icons are at `/api/applications/{id}/icon`) and `quit`, which
ends browser-wayland. Requests are fire-and-forget; unknown ids are ignored.
Snapshots are lossless PNGs of a window's own buffers, so they include covered and minimized windows;
`scale` (0.05 to 2) is relative to the output scale.

With `--elements`, `/api/windows/{id}/elements` lists a window's UI elements (buttons, links, text fields,
tabs, menu items, …) with role, name and rectangle relative to the window, so a script or an agent can
target a control instead of interpreting pixels. It reads the toolkits' accessibility trees over the D-Bus
session browser-wayland was started in (`dbus-run-session -- browser-wayland --elements …` if there is
none; the container does this). GTK and Qt applications and Firefox publish their trees when started from
`--exec` or `spawn`; Chromium and Electron apps need `--force-renderer-accessibility`. Without the flag
the route answers `501`; `503` means the tree couldn't be read (no bus, or the application went away).

`/api/input` clicks, types, presses key chords and scrolls as a user would, with coordinates relative to a
window when you pass its id, so element rectangles can be used as they are. `/api/clipboard` reads what an
application last copied, text or a PNG, and sets what it will paste; the viewer page bridges the same
clipboard to the browser's (copy text or an image in an application, paste locally; Ctrl+V in the page
pastes the browser's clipboard, screenshots included).

## Agents: MCP and skill

The same operations are MCP tools at `/mcp` (Streamable HTTP, same bearer token), so a coding agent can
drive the desktop: `windows`, `elements`, `snapshot`, `screenshot`, `window_control`, `move_window`,
`resize_window`, `applications`, `launch`, `spawn`, `click`, `move_pointer`, `button`, `scroll`, `key`,
`type`, `clipboard_read`, `clipboard_write`.

```sh
claude mcp add --transport http bw https://host:8443/mcp --header "Authorization: Bearer $T"
BW_TOKEN=$T codex mcp add bw --url https://host:8443/mcp --bearer-token-env-var BW_TOKEN
```

The server hands the agent its manual on connection (`skills/browser-wayland/SKILL.md`) and the
generated `reference.md` with every route, body and tool schema; both are also served at `/skill/`
without a token and can be copied into an agent's skills directory. With a self-signed certificate,
point the agent at the fingerprint the server prints, or run `--no-tls` behind a reverse proxy.

The viewer page uses the same data over its WebSocket: its application menu lists the installed
programs by category, with a search box, and starts them; its side panel lists the windows with thumbnails
and open-in-popup, snapshot, maximize, minimize and close buttons (click a row to bring the window
forward, type in the box at the top to run a command), its Statistics tab shows the stream's numbers
(per-stage timings, drops, audio lead) once a second, and the top bar toggles colour-coded borders with
the app id over every window and an outline of the focused window's elements; its power menu quits
browser-wayland; desktop notifications (browser-wayland is the session's notification daemon when no
other runs) appear as toasts with their actions. A panel is optional. In the browser console,
`bw.windows()`, `bw.activate(id)`, `bw.control({...})`, `bw.spawn(cmd)`, `bw.snapshot(id)` and
`bw.elements(id)` do the same.

The viewer lives in `web/` (React, Tailwind CSS, Vite) and is built into `web/dist`, which the binary
embeds at compile time; `make` builds the viewer (Node 24) and then the binary, `make web` only the
viewer, and a `cargo build` without `web/dist` stops with that hint. `npm run dev` in `web/` serves the
page with hot reload, proxying `/ws` and `/api` to a server started with `--no-tls --listen 127.0.0.1:8080`.

Useful flags: `--no-tls` (localhost development), `--listen`, `--bitrate <kbps>`,
`--codec auto|h264|hevc|vp9|av1|vp8` (what Auto resolves to when the browser decodes it; a codec this
machine can't encode stops startup; auto prefers whatever the browser decodes in hardware, among what
this machine encodes: AV1, then HEVC, VP9, H.264 on the GPU; VP8 first on the CPU),
`--software-encoding`, `--exec`, `--kiosk`, `--elements`, `--no-audio`, `--webcam`, `--socket-name`, `--render-node`. `--help`
lists them all.

Games and other clients that lock the pointer get raw mouse deltas: the page mirrors the lock with the
Pointer Lock API; Escape releases it.

Certificate and tokens live in `$XDG_CONFIG_HOME/browser-wayland/`; delete them to regenerate.
