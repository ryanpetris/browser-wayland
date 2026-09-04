# browser-wayland

A headless Wayland compositor whose screen is a browser tab. Clients render on the GPU, the
composited frame is hardware-encoded to H.264 (VA-API) and streamed over a WebSocket, and the
browser decodes it with WebCodecs. Mouse and keyboard input flow back the same way.

## Requirements

- Linux with a GPU render node (`/dev/dri/renderD128`) and Mesa.
- GStreamer 1.24+ with the VA plugin: `gst-plugin-va` on Arch (`vapostproc`, `vah264enc`).
- `xorg-xwayland` for X11 clients, and PipeWire or PulseAudio with `pactl` for audio (both optional).
- Rust stable. The browser needs WebCodecs (Chromium, Firefox 130+, Safari 26+).

## Run

```sh
cargo run --release -- --exec 'foot'          # any Wayland client; WAYLAND_DISPLAY is set for it
```

The server prints the certificate fingerprint and a URL like `https://<lan-ip>:8443/?token=…`.
Open it in a browser on the LAN, compare the fingerprint before accepting the self-signed
certificate, and the desktop appears. The browser viewport size becomes the output size.
The ⛶ button enters fullscreen with keyboard lock so shortcuts like Ctrl+W reach the desktop.
Frames are painted on a 2D canvas. `?renderer=webgpu` in the URL uses a WebGPU external-texture path
instead; it is opt-in because Chromium on Linux occasionally presents a blank frame that way, which looks like flicker.

Other clients can join later: `WAYLAND_DISPLAY=wayland-browser some-app`. X11 apps work too: an
Xwayland is started automatically and the log prints its `DISPLAY`. Super (or Alt) + left drag moves
any window, which is how undecorated X11 windows get moved.

Audio from clients goes to the browser instead of the host speakers: the server creates a private
`browser-wayland-<pid>` sink (printed in the log) and captures it as Opus. Clients started with
`--exec` get `PULSE_SINK` set; for others use `PULSE_SINK=<that name> some-app`. `--no-audio` turns this off.

`--exec` runs when the first viewer connects, with `BW_WIDTH`/`BW_HEIGHT` set to the browser's size,
and `--kiosk` fullscreens every window. Together they run a whole nested desktop; with the
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
Its pager stays empty (no ext-workspace: there is one workspace). Waybar's default config draws its
icons with Font Awesome (`otf-font-awesome`); GTK only shows icons in the Xfce menus with
`gtk-menu-images=1` in `~/.config/gtk-3.0/settings.ini`, which xfsettingsd normally sets.

The `Dockerfile` packages all of that on Arch Linux: browser-wayland, the Xfce panel and apps, and
Firefox, with PipeWire for audio. Build and run instructions are in its header.

## Desktop API

The compositor is the window manager, so the viewer and outside scripts can see and drive the desktop.
Everything is behind the same token as the stream: the cookie the `?token=` URL sets, `?token=` on the
request, or `Authorization: Bearer <token>`.

```sh
curl -s -H "Authorization: Bearer $T" http://host:8443/api/windows | jq        # the window list
curl -X POST -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
     http://host:8443/api/control -d '{"id":3,"op":"minimize"}'                # act on a window
curl -X POST ... /api/control -d '{"op":"spawn","cmd":"firefox"}'             # start a program
curl -o w.png -H "Authorization: Bearer $T" 'http://host:8443/api/windows/3/snapshot.png?scale=0.5'
curl -o screen.png -H "Authorization: Bearer $T" http://host:8443/api/screenshot.png
```

Each window reports `id`, `title`, `app_id`, `x11`, `pid`, its geometry `x y w h` in logical pixels, its
stacking index `z` (`null` while minimized), `maximized`, `fullscreen`, `minimized`, `focused`, and
`updated_ms`, the time of its last commit. Ops: `activate`, `close`, `minimize`, `unminimize`, `maximize`,
`unmaximize`, `fullscreen`, `unfullscreen`, `move` (`x`, `y`), `resize` (`w`, `h`), `spawn` (`cmd`, run
with `sh -c` in the same environment as `--exec`). Requests are fire-and-forget; unknown ids are ignored.
Snapshots are lossless PNGs of a window's own buffers, so they include covered and minimized windows;
`scale` (0.05 to 2) is relative to the output scale and applies to windows only.

The viewer page uses the same data over its WebSocket: the ☰ button opens a window list with thumbnails
and maximize/minimize/close buttons (click a row to bring the window forward, type in the box to start a
program), and ▢ draws colour-coded borders with the app id over every window. In the browser console,
`bw.windows()`, `bw.activate(id)`, `bw.control({...})`, `bw.spawn(cmd)` and `bw.snapshot(id)` do the same.

Useful flags: `--no-tls` (localhost development), `--listen`, `--bitrate <kbps>`,
`--codec auto|h264|hevc|vp9` (auto prefers whatever the browser decodes in hardware: HEVC, then VP9,
then H.264), `--fake-source` (a test pattern instead of the compositor), `--socket-name`, `--render-node`.

Games and other clients that lock the pointer get raw mouse deltas: the page mirrors the lock with the
Pointer Lock API; Escape releases it.

Certificate and token live in `$XDG_CONFIG_HOME/browser-wayland/`; delete them to regenerate.
