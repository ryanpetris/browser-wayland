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

- Linux, with a GPU render node (`/dev/dri/renderD128`) and Mesa for hardware rendering and encoding, or
  none at all (Mesa's llvmpipe renders and the CPU encodes; see below).
- GStreamer 1.24+ with the VA plugin: `gst-plugin-va` on Arch (`vapostproc`, `vah264enc`), for
  hardware encoding; or `--software-encoding` with the vpx (good), x264 (ugly), x265 or svtav1 (bad)
  plugins, which encodes on the CPU (the desktop then runs at 30 Hz) for machines without a usable GPU encoder.
- `xorg-xwayland` for X11 clients. Audio requires PipeWire 1.4.2+, its Pulse compatibility service,
  WirePlumber 0.5.6+, the native GStreamer PipeWire plugin and `pactl`. See [session audio](docs/session-audio.md)
  for packages and host-service compatibility. `--no-audio` needs no audio services; the native PipeWire client library remains a runtime dependency.
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

Use `--screen-size 1920x1080` to set a fixed desktop resolution from startup. Browser resizes and
control handoffs keep that resolution; each browser scales the picture to fit while preserving its
aspect ratio. Both dimensions must be even and between 2 and 8192 pixels. Without this option, the
desktop follows the controlling browser's size.

Each viewer picks its own codec and quality in the status bar: the codec list is what both the server
and that browser can do ("Auto (HEVC)" shows the pick). All five quality levels adapt under their
selected ceiling: Very Low (2 Mbit/s), Low (5), Medium (`--bitrate`, 8 by default), High (12), and
Max (25). Max is the default. The stream starts at its ceiling; under pressure the server halves the
bitrate and holds it, then climbs back a quarter at a time. Targets below 3 Mbit/s have a 30 fps cap.
The status bar shows the selected ceiling and current encoder target separately from measured video
throughput. A custom Medium ceiling is displayed as configured, even if it exceeds another level.
`GET /api/codecs` lists the server's codecs. After the picture stops changing one more frame
goes out, at four times the bitrate with the software encoders, so text left rough by motion sharpens.
Frames are painted on a 2D canvas. `?renderer=webgpu` in the URL uses a WebGPU external-texture path
instead; it is opt-in because Chromium on Linux occasionally presents a blank frame that way, which looks like flicker.

Windows that don't draw their own title bar (X11 applications, Vulkan and SDL programs, anything that
asks for server-side decorations) get one from the compositor: drag it, double-click it to maximize,
resize from the edges, close, maximize and minimize with its buttons.

Other clients can join later: `WAYLAND_DISPLAY=wayland-browser some-app`. X11 apps work too: an
Xwayland is started automatically and the log prints its `DISPLAY`. Super (or Alt) + left drag moves
any window from anywhere in it.

Each desktop owns a private PipeWire server, Pulse compatibility service and WirePlumber policy.
Startup commands, menu/API launches and their descendants receive the private audio socket selectors.
Applications play into the session output, which is encoded as 48 kHz stereo Opus for the browser.
The viewer's microphone button sends the controlling browser's microphone into the session microphone.
Stopping capture stops the browser's recording indicator and leaves silence for recording applications.

Open **Mixer** in the status bar for session devices and individual application streams, with actual
peak meters, volume and mute. Read-only viewers can inspect; the controlling viewer changes the shared
session. Muting the session microphone does not stop browser capture. With multiple session endpoints,
the mixer also offers routing and default selection through WirePlumber.

`--no-audio` starts no audio services. Audio startup or service failure leaves the desktop running with
audio unavailable. Shutdown stops the audio pipelines before the private services. For clients launched
separately, use the connection variables printed in the log. See [session audio](docs/session-audio.md)
for the device names, environment and lifecycle.

The browser's webcam works the same way through a `v4l2loopback` device, the one kind of camera every
application understands: on the host, `modprobe v4l2loopback exclusive_caps=1 card_label=browser-wayland`
(the package is `v4l2loopback-dkms` on most distributions) and start the server with `--webcam
/dev/videoN` (the device it made; in Docker add `--device /dev/videoN --group-add $(stat -c %g /dev/videoN)`
to `docker run`). The camera button in the status bar then sends the webcam as VP8, scaled to 720p, to
that device, which video calls in the desktop pick as a camera. With `exclusive_caps=1`, the loopback
advertises only video output until it receives frames. Turn the browser camera on before starting a
call so the application can discover it.
In the Docker image, the guvcview menu entry uses the device selected by `--webcam`. Enable the
browser camera before launching guvcview. If it was opened early, close it and relaunch after enabling
the camera. Applications launched by the compositor receive the configured device in `BW_WEBCAM_DEVICE`.
Without `--webcam` (or if the device can't be opened, which the log says) there is no button.

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
Firefox and Chromium, applications for what the desktop can do (guvcview for the webcam, Audacity, GIMP,
mpv with VA-API decode, Ristretto, pavucontrol), nano and a passwordless sudo for the `bw` user, with PipeWire for audio and
Mesa's OpenGL and Vulkan drivers for Intel and AMD (`glxgears`, `vkcube` and the info tools are included
to check them), and the two GTK settings above. Programs launched from the desktop start in the home folder.
The desktop starts empty; the viewer's menu launches the applications, and `--exec xfce4-panel` after
the image name adds the panel. `make docker-run` builds the image and runs it; the details are
in the Dockerfile's header.

The page says when the server closed its socket with a token dialog ("wrong token" or "token
rotated"; the tokens change with the data directory, e.g. a fresh container without a volume).

## Transport

The video travels on the WebSocket. A viewer can move it to a WebRTC data channel (UDP, ordered and
reliable) with the Transport select in the status bar. The page offers and the server answers with
candidates for the page's hostname and port, unless `--rtc-addr` overrides them. The frames move to the channel once it opens; input, audio, events and the signalling
stay on the WebSocket either way, so the socket is needed whatever carries the video. Measured against
each other on a container link with a 4 Mbit/s desktop stream under an 8 Mbit/s Medium ceiling, the two
are even on a clean link and the channel is behind under packet loss, which is why the socket carries
the video unless the channel is picked:

| link | WebSocket | data channel |
| --- | --- | --- |
| clean, or 20 ms delay | 56 fps, 23–37 ms longest gap | 56 fps, 23 ms |
| 0.5 % loss | 56 fps, 36 ms | 52–56 fps, a one-second gap in one run of two |
| 2 % loss, 20 ms delay | 56 fps, 60 ms | 24–26 fps, gaps over a second, then given up |
| 1 % loss, 100 ms delay | 26–38 fps, 277 ms, at the 1 Mbit/s floor | 16–21 fps, gaps of a second, then given up |

TCP retransmits a lost packet and the picture barely notices; the channel's SCTP waits a second at the
least before it retransmits (sctp-proto's minimum RTO), its send buffer fills, and a keyframe behind it
is a stall of seconds. So a channel that loses or holds up
frames (half a second and more) in three of ten seconds, or holds one frame for three seconds, is given up:
the video returns to the socket and recovery starts, with WebRTC still selected. What the channel offers is smoother pacing on a clean link (the socket's frames bunch behind
TCP's acknowledgements; Firefox showed a 58 ms longest gap on the socket against 23 ms on the channel)
and a path through a TURN relay when direct UDP is blocked.

The selected transport stays WebRTC during fallback. Socket video continues while a fresh channel
connects. Failed attempts retry with jittered exponential delays of about 1 to 30 seconds; ten seconds
without channel loss or stalls resets the backoff. Each attempt has a ten-second total budget.
The status bar shows the actual path and recovery state, with Retry now while waiting. Statistics
also shows the reason, retry count and next attempt. Selecting WebSocket cancels recovery. A server
or browser without WebRTC support stays on the socket without retrying, preserving the preference.
Quality selection and its adaptive ceiling are independent of transport recovery.

The server listens on UDP on the listen port's number (`--rtc-port` changes the local UDP port).
By default, the browser sends WebRTC traffic to the page's hostname and port. The backend resolves
the hostname to IP addresses for ICE, so the hostname must resolve to the reachable address from
inside the container too. Use `--rtc-addr` if the browser and backend see different DNS results.
An omitted page port means 443 for HTTPS or 80 for HTTP. Docker needs both TCP and UDP published, for example `-p 8443:8443 -p 8443:8443/udp`. If the page uses a different
external port, publish UDP on that same external port too.

When UDP uses a different endpoint from the page, such as behind an HTTPS-only proxy, set
`--rtc-addr <reachable IP>` after the image name. This advertises that IP with `--rtc-port`,
or the listen port's number if omitted; the advertised UDP port must be forwarded unchanged.
For browsers behind a strict NAT give them a STUN server (`--stun stun:host:3478`) or a TURN server (`--turn turn:host:3478 --turn-user …
--turn-pass …`, its address as the browsers reach it). coturn is one container to self-host:

```sh
docker run -d --name turn -p 3478:3478/udp -p 3478:3478 -p 49160-49200:49160-49200/udp coturn/coturn \
  -n --lt-cred-mech --user=USER:PASS --realm=browser-wayland --listening-port=3478 \
  --min-port=49160 --max-port=49200 --relay-ip=<the address the server reaches it at>
```

`--no-rtc` leaves the option out entirely.

## Without a GPU

On a machine with no `/dev/dri` at all (a VPS, a CI runner, a container started without devices) the
server renders with Mesa's llvmpipe through the surfaceless EGL platform and encodes on the CPU: pass
`--render-node none`, or nothing (a default node that isn't there means the same). Frames are read back
into memory for the software encoders, clients draw into shared memory (there is no dmabuf global to
offer them GPU buffers), and X11 clients render in software too. The same binary with the node present
behaves as before. If the surfaceless platform picks a GPU driver you don't want, `LIBGL_ALWAYS_SOFTWARE=1`
in the environment forces llvmpipe.

## Files

The Files tab browses the remote filesystem with the server process's Unix permissions. It starts in
Downloads (`--files-dir` selects another transfer folder). Each viewer chooses its own directory.
Upload and page drops outside the desktop use that directory; navigation during a batch cannot change
its destination. Browse folders, download files, create directories, rename entries, and confirm file
deletion. Home, transfer-folder shortcuts, breadcrumbs, sorting, and hidden files are available.
Listings load on navigation, Refresh, and this viewer's own changes to the displayed directory.
Reopening the panel or changes from another viewer do not refresh it. File access requires a control
token, including the API; read-only viewers have no Files tab or file download controls.
Drag a file over the desktop itself and the application under the pointer sees a drag coming; let go and,
once the file is uploaded, it is dropped there as a `file://` URI, to copy or to move. Since the
application picks the folder, such a file is not uploaded to Downloads but staged under the cache
directory (`~/.cache/browser-wayland/drops`, a folder with a random name per drop): Thunar moves it out into
the folder shown, Nautilus copies, an editor opens it where it is, and whatever is left there is swept after
a day (an editor still showing the file loses it then).
A drop no application took is copied to the transfer folder. The result shows saved names and offers
Open folder without changing the selected directory. Files copied in a file manager inside the desktop
show up in the status bar with a download button, and files copied on your machine paste into the
desktop's file manager, staged the same way.

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

Picture-in-picture is also available in supporting browsers on a secure origin. The top-bar action
opens the whole desktop; a window row's action opens that application. One PiP window belongs to each
opener, with a compact title/status bar and Return to main viewer. Choosing another target replaces its
content. Ordinary popups remain available.

When you control the desktop, control and its dimensions follow the PiP viewport. Returning, or taking
control in the main viewer, returns that presentation to the main window. Another viewer's control is
not displaced just by opening or closing PiP. Desktop playback moves to PiP; microphone and camera
capture stop on handoff and must be restarted explicitly in the main viewer. Window PiP has no audio.
PiP cannot use fullscreen keyboard capture. Use the keyboard row for desktop composition input and
return to the normal viewer for copied-file downloads or capture controls. Browser checks and known
interaction limits are recorded in [the desktop UI notes](docs/desktop-api.md#document-picture-in-picture).

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
curl -o w.png -H "Authorization: Bearer $T" 'http://host:8443/api/windows/3/snapshot.png?percentage=50'
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
`minimized`, `focused`, `updated_ms` on the compositor clock, and monotonic `content_revision` for preview invalidation. Ops: `activate`, `close`, `minimize`, `unminimize`, `maximize`,
`unmaximize`, `fullscreen`, `unfullscreen`, `move` (`x`, `y`), `resize` (`w`, `h`), `spawn` (`cmd`, run
with `sh -c` in the same environment as `--exec`), `launch` (`app`, an id from `GET /api/applications`,
the installed `.desktop` launchers, whose icons are at `/api/applications/{id}/icon`) and `quit`, which
ends browser-wayland. Requests are fire-and-forget; unknown ids are ignored.
Snapshots are lossless PNGs of a window's own buffers, so they include covered and minimized windows.
Omitted sizing returns native dimensions. Supply one of `width`, `height`, or
`percentage`; see [sizing limits and migration](docs/desktop-api.md#screenshot-sizing).

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
(per-stage timings, drops, audio lead) once a second. **Settings → Overlays** in the top bar controls
local coloured window outlines and the focused window's accessibility-element outlines. Both apply
immediately and are remembered. UI elements requires server accessibility support (`--elements`).
The power menu quits browser-wayland. Desktop notifications appear as toasts with their actions;
browser-wayland is the session's notification daemon when no other runs. A panel is optional. In the browser console,
`bw.windows()`, `bw.activate(id)`, `bw.control({...})`, `bw.spawn(cmd)`, `bw.snapshot(id)` and
`bw.elements(id)` do the same.

The viewer lives in `web/` (React, Tailwind CSS, Vite) and is built into `web/dist`, which the binary
embeds at compile time; `make` builds the viewer (Node 24) and then the binary, `make web` only the
viewer, and a `cargo build` without `web/dist` stops with that hint. `npm run dev` in `web/` serves the
page with hot reload, proxying `/ws` and `/api` to a server started with `--no-tls --listen 127.0.0.1:8080`.

Useful flags: `--no-tls` (localhost development), `--listen`, `--bitrate <kbps>` (Medium ceiling),
`--codec auto|h264|hevc|vp9|av1|vp8` (what Auto resolves to when the browser decodes it; a codec this
machine can't encode stops startup; auto prefers whatever the browser decodes in hardware, among what
this machine encodes: AV1, then HEVC, VP9, H.264 on the GPU; VP8 first on the CPU),
`--software-encoding`, `--exec`, `--kiosk`, `--elements`, `--no-audio`, `--webcam`, `--no-rtc`, `--rtc-port`, `--rtc-addr`, `--stun`,
`--turn`, `--turn-user`, `--turn-pass`, `--socket-name`, `--render-node` (`none` for no GPU). `--help`
lists them all.

Games and other clients that lock the pointer get raw mouse deltas: the page mirrors the lock with the
Pointer Lock API; Escape releases it.

Certificate and tokens live in `$XDG_CONFIG_HOME/browser-wayland/`; delete them to regenerate.

The [session audio visualiser](docs/audio-visualiser.md) loads when opened.
The About dialog links to license notices and the source repository.

## Source code

Source and build instructions are available in the
[GitHub repository](https://github.com/ryanpetris/browser-wayland).
For a release, run `browser-wayland --version` and check out the matching
`vX.Y.Z` tag. For a development or modified build, use the revision and any local
changes supplied by its distributor; the current default branch may differ.

Install the build dependencies listed under [Install](#install) and
[Requirements](#requirements), then run
`make`. This installs the locked npm dependencies, builds the viewer, and builds
the release binary. The audioMotion source comes from the version pinned in
`web/package-lock.json`; the viewer's modification is in `web/vite.config.js`.
The lockfile records the dependency download URL and integrity hash.

Distributors must keep the matching source and required dependency sources
available. A fork with additional changes must link to its own corresponding
source rather than this repository.
